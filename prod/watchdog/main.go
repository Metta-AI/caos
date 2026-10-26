// caos-watchdog watches the caosd-prod host from a machine that shares none of
// its failure modes, and runs a Claude Code session against what Datadog says
// is wrong.
//
// Detection is NOT this program's job, and not Claude's: the Datadog agent on
// each host and the probes below feed Datadog, whose monitors
// (metta: devops/tf/datadog-monitors/caos_monitors.tf) decide what "wrong"
// means. This program reads that decision back:
//
//	caos-watchdog probe             every two minutes, from a timer: can a
//	                                client reach the server (iroh), can it
//	                                compute something (std/hello), can an
//	                                operator reach the box (SSM)? Reported as
//	                                Datadog service checks plus the two
//	                                latencies as gauges, nothing else.
//	caos-watchdog respond           the daemon: polls Datadog for alerting
//	                                caos monitors; when the set becomes
//	                                non-empty, posts to Slack, waits for the
//	                                storm to settle, and runs ONE Claude
//	                                session over the whole set. See respond.go
//	                                for the single-flight and cap rules.
//	caos-watchdog ssh -- <command>  run a command on the prod host over SSM.
//	                                What the Claude session uses for every
//	                                read and every fix; it refuses the
//	                                commands the runbook reserves for a human.
//	caos-watchdog notify <text>     post to the #proj-caos Slack webhook.
//	caos-watchdog ticket [--refresh] the prod iroh ticket, cached from the host.
//
// Configuration is environment, set by the systemd unit (prod/watchdog/
// configuration.nix); see conf().
package main

import (
	"fmt"
	"os"
	"strings"

	"caos/w"
)

type conf struct {
	stateDir   string // ledger, ticket cache, Claude's HOME and workspace
	secretsDir string // /run/caos-secrets, populated by caos-secrets.service
	target     string // the prod instance id
	region     string
	monitorTag string // Datadog monitor tag selecting the caos monitors
	dogstatsd  string // host:port of the local agent's DogStatsD
	runbookDir string // CLAUDE.md and .claude/settings.json for the session
	model      string
	helloRev   string // caos revision whose std/hello the run probe executes
}

func env(name, fallback string) string {
	if v, ok := os.LookupEnv(name); ok {
		return v
	}
	w.True(fallback != "", "%s is not set", name)
	return fallback
}

func loadConf() conf {
	return conf{
		stateDir:   env("CAOS_WATCHDOG_STATE", ""),
		secretsDir: env("CAOS_WATCHDOG_SECRETS", "/run/caos-secrets"),
		target:     env("CAOS_WATCHDOG_TARGET", ""),
		region:     env("CAOS_WATCHDOG_REGION", "us-east-1"),
		monitorTag: env("CAOS_WATCHDOG_MONITOR_TAG", "service:caos"),
		dogstatsd:  env("CAOS_WATCHDOG_DOGSTATSD", "127.0.0.1:8125"),
		runbookDir: env("CAOS_WATCHDOG_RUNBOOK", ""),
		model:      env("CAOS_WATCHDOG_MODEL", "claude-opus-5-5"),
		helloRev:   env("CAOS_WATCHDOG_HELLO_REV", ""),
	}
}

// secret reads one file caos-secrets.service wrote. The value is used, never
// printed: callers put it in a header or an environment, not on a command line.
func (c conf) secret(name string) string {
	b := w.Check(os.ReadFile(c.secretsDir + "/" + name))
	s := strings.TrimSpace(string(b))
	w.True(s != "", "secret %s is empty", name)
	return s
}

func usage() {
	fmt.Fprintln(os.Stderr, strings.TrimSpace(`
usage: caos-watchdog probe
       caos-watchdog respond
       caos-watchdog ssh [--timeout SECONDS] -- <command...>
       caos-watchdog notify <text>
       caos-watchdog ticket [--refresh]`))
	os.Exit(2)
}

func main() {
	w.Main(func() {
		if len(os.Args) < 2 {
			usage()
		}
		c := loadConf()
		args := os.Args[2:]
		switch os.Args[1] {
		case "probe":
			probe(c)
		case "respond":
			respond(c)
		case "ssh":
			sshMain(c, args)
		case "notify":
			w.True(len(args) == 1, "notify takes exactly one argument, the text")
			notify(c, args[0])
		case "ticket":
			refresh := len(args) == 1 && args[0] == "--refresh"
			w.True(len(args) == 0 || refresh, "ticket takes only --refresh")
			fmt.Println(ticket(c, refresh))
		default:
			usage()
		}
	})
}
