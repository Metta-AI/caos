package main

import (
	"fmt"
	"net"
	"os"
	"os/exec"
	"strings"
	"time"

	"caos/w"
)

// serviceCheck reports one pass/fail to the local Datadog agent over
// DogStatsD. A service check rather than a metric because that is what these
// are: a named thing that is OK or CRITICAL right now. The monitors over them
// live in metta (devops/tf/datadog-monitors/caos_monitors.tf).
func serviceCheck(c conf, name string, ok bool, message string) {
	status := 2 // CRITICAL
	if ok {
		status = 0
	}
	// A newline or a pipe would split the datagram; the message is one line.
	message = strings.NewReplacer("\n", " ", "|", "/").Replace(strings.TrimSpace(message))
	if len(message) > 400 {
		message = message[:400]
	}
	datagram := fmt.Sprintf("_sc|%s|%d|#service:caos,env:production,target:%s|m:%s", name, status, c.target, message)
	conn := w.Check(net.Dial("udp", c.dogstatsd))
	defer conn.Close()
	w.Check(conn.Write([]byte(datagram)))
	fmt.Fprintf(os.Stderr, "%-16s %s  %s\n", name, map[bool]string{true: "OK", false: "CRITICAL"}[ok], message)
}

// gauge reports one number to the local agent over DogStatsD, for the
// dashboard: how long the probes took, next to whether they passed.
func gauge(c conf, name string, value float64) {
	conn := w.Check(net.Dial("udp", c.dogstatsd))
	defer conn.Close()
	w.Check(fmt.Fprintf(conn, "%s:%.3f|g|#service:caos,env:production,target:%s", name, value, c.target))
}

// lsRemote exercises the whole client path to the server: git-remote-caos,
// caos-iroh, the relay or a direct hole-punch, and the server's git transport.
// It is what every `git fetch caos` does, so if it fails nobody can use the
// server, whatever the host metrics say.
func lsRemote(ticket string) (bool, string, time.Duration) {
	started := time.Now()
	cmd := exec.Command("timeout", "60", "git", "ls-remote", ticket)
	out, err := cmd.CombinedOutput()
	elapsed := time.Since(started).Round(100 * time.Millisecond)
	if err != nil {
		return false, fmt.Sprintf("git ls-remote failed after %s: %s", elapsed, strings.TrimSpace(string(out))), elapsed
	}
	return true, fmt.Sprintf("%d refs in %s", strings.Count(string(out), "\n"), elapsed), elapsed
}

// hello runs std/hello on the server through a real client, with a fresh
// --date so nothing is cached: a client forms an ArgTree, the server
// schedules it, a runner starts a container, a worker reads its arguments and
// a result comes back. ls-remote proves the server answers; this proves it
// computes. Measured 2026-10-29 against caosd-prod: 0.3 s cached, 2.2 s
// fresh, 6 s when hello itself had to be compiled first.
//
// caos-cli needs a git worktree with the server as its `caos` remote, so one
// is kept under the state dir and its remote is pointed at the current ticket.
func hello(c conf, ticket string) (bool, string, time.Duration) {
	repo := c.stateDir + "/probe-repo"
	if _, err := os.Stat(repo + "/.git"); err != nil {
		w.Must(os.MkdirAll(repo, 0o700))
		w.Must(exec.Command("git", "-C", repo, "init", "-q").Run())
		w.Must(exec.Command("git", "-C", repo, "-c", "user.name=caos-watchdog", "-c", "user.email=watchdog@caos",
			"commit", "-q", "--allow-empty", "-m", "probe worktree").Run())
		w.Must(exec.Command("git", "-C", repo, "remote", "add", "caos", ticket).Run())
	}
	w.Must(exec.Command("git", "-C", repo, "remote", "set-url", "caos", ticket).Run())

	started := time.Now()
	cmd := exec.Command("timeout", "600", "caos-cli", "run",
		"--base:@@=git+https://github.com/Metta-AI/caos?rev="+c.helloRev+"&dir=std/hello",
		"--date="+started.UTC().Format(time.RFC3339))
	cmd.Dir = repo
	out, err := cmd.CombinedOutput()
	elapsed := time.Since(started).Round(100 * time.Millisecond)
	if err != nil {
		return false, fmt.Sprintf("caos-cli run std/hello failed after %s: %s", elapsed, strings.TrimSpace(string(out))), elapsed
	}
	w.True(strings.Contains(string(out), "date = "), "std/hello returned without echoing its argument: %s", out)
	return true, fmt.Sprintf("std/hello round trip %s", elapsed), elapsed
}

// probe is what the timer runs. It never exits non-zero for a failing target:
// the result IS the service check, and a probe that died would read as the
// watchdog being broken rather than the server. Only a broken probe (no
// agent to talk to, no AWS credentials) fails the unit.
func probe(c conf) {
	// Client reachability, with one retry on a fresh ticket: a stack wipe
	// re-keys the server, and a stale cached ticket must not read as an outage.
	t := ticket(c, false)
	ok, msg, elapsed := lsRemote(t)
	if !ok {
		t = ticket(c, true)
		ok, msg, elapsed = lsRemote(t)
		if ok {
			msg = "ok after refreshing the cached ticket: " + msg
		}
	}
	serviceCheck(c, "caos.probe.iroh", ok, msg)
	if ok {
		gauge(c, "caos.probe.iroh.seconds", elapsed.Seconds())
		// Only worth asking if the server answers at all; a dead server
		// would otherwise cost a 10-minute timeout on every probe.
		ok, msg, elapsed = hello(c, t)
		serviceCheck(c, "caos.probe.run", ok, msg)
		if ok {
			gauge(c, "caos.probe.run.seconds", elapsed.Seconds())
		}
	} else {
		serviceCheck(c, "caos.probe.run", false, "not attempted: ls-remote failed")
	}

	// Operator reachability. The one path into the box is an SSM worker, and
	// workers die when a container veth steals 169.254.0.0/16 (the dhcpcd
	// guard in prod/base.nix exists for this) while the agent still pings. A
	// RunCommand round trip is the test that catches it.
	started := time.Now()
	inv := c.run("true", 60*time.Second)
	elapsed = time.Since(started).Round(time.Second)
	if inv.Status == "Success" {
		serviceCheck(c, "caos.probe.ssm", true, fmt.Sprintf("RunCommand round trip %s", elapsed))
	} else {
		serviceCheck(c, "caos.probe.ssm", false,
			fmt.Sprintf("RunCommand %s after %s: %s", inv.Status, elapsed, inv.StandardErrorContent))
	}
}
