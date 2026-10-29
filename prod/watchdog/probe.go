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

// probe is what the timer runs. It never exits non-zero for a failing target:
// the result IS the service check, and a probe that died would read as the
// watchdog being broken rather than the server. Only a broken probe (no
// agent to talk to, no AWS credentials) fails the unit.
func probe(c conf) {
	// Client reachability, with one retry on a fresh ticket: a stack wipe
	// re-keys the server, and a stale cached ticket must not read as an outage.
	ok, msg, _ := lsRemote(ticket(c, false))
	if !ok {
		fresh := ticket(c, true)
		ok, msg, _ = lsRemote(fresh)
		if ok {
			msg = "ok after refreshing the cached ticket: " + msg
		}
	}
	serviceCheck(c, "caos.probe.iroh", ok, msg)

	// Operator reachability. The one path into the box is an SSM worker, and
	// workers die when a container veth steals 169.254.0.0/16 (the dhcpcd
	// guard in prod/base.nix exists for this) while the agent still pings. A
	// RunCommand round trip is the test that catches it.
	started := time.Now()
	inv := c.run("true", 60*time.Second)
	elapsed := time.Since(started).Round(time.Second)
	if inv.Status == "Success" {
		serviceCheck(c, "caos.probe.ssm", true, fmt.Sprintf("RunCommand round trip %s", elapsed))
	} else {
		serviceCheck(c, "caos.probe.ssm", false,
			fmt.Sprintf("RunCommand %s after %s: %s", inv.Status, elapsed, inv.StandardErrorContent))
	}
}
