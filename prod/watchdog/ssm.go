package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"regexp"
	"strconv"
	"strings"
	"time"

	"caos/w"
)

// aws runs one AWS CLI command against the configured region and returns its
// stdout, failing with the CLI's own stderr. The CLI rather than the SDK: the
// instance role is the credential either way, and the CLI is already on the
// host for the operator, so this binary stays one small module.
func (c conf) aws(args ...string) string {
	cmd := exec.Command("aws", append([]string{"--region", c.region, "--output", "json"}, args...)...)
	var errb strings.Builder
	cmd.Stderr = &errb
	out, err := cmd.Output()
	w.True(err == nil, "aws %s: %v: %s", strings.Join(args, " "), err, strings.TrimSpace(errb.String()))
	return string(out)
}

// invocation is the part of GetCommandInvocation this program reads.
type invocation struct {
	Status                string
	ResponseCode          int
	StandardOutputContent string
	StandardErrorContent  string
}

// reserved names the commands the runbook keeps for a human. The Claude session
// reaches the host only through `caos-watchdog ssh`, so this is where the
// line is enforced rather than merely described: a reboot, a redeploy, or
// anything that rewrites stack state is refused here, and the session is told
// to ask for it in Slack instead. A deny list is not a sandbox; it is one
// guard, with the IAM role (no ec2:RebootInstances) and the runbook as the
// other two.
var reserved = regexp.MustCompile(strings.Join([]string{
	`\breboot\b`, `\bshutdown\b`, `\bpoweroff\b`, `\bhalt\b`, `\binit [06]\b`,
	`\bsystemctl\s+(kexec|poweroff|halt|reboot)\b`,
	`\bnixos-rebuild\b`, `\bnix-env\b`, `\bswitch-to-configuration\b`, `deploy-caosd`,
	`\bmkfs\b`, `\be2label\b`, `\bdd\s+(if|of)=`, `\bwipefs\b`,
	`rm\s+(-\w+\s+)*/data/caos/stack`, `\bchown\s+-R\b`,
	`\bcaosd\s+down\b`,
	`\bdocker\s+(system\s+prune\s+(-a|--all|--volumes)|volume\s+prune|rm\b|rmi\b)`,
}, "|"))

// run executes a shell command on the prod host through SSM RunShellScript
// and waits for it, up to timeout. SSM runs it as root. It returns the
// invocation, including a non-Success status, so the caller decides whether
// a failing command is a finding (probe) or a failure (ssh).
func (c conf) run(command string, timeout time.Duration) invocation {
	params := w.Check(json.Marshal(map[string][]string{
		"commands":         {command},
		"executionTimeout": {strconv.Itoa(int(timeout.Seconds()))},
	}))
	var sent struct {
		Command struct{ CommandId string }
	}
	w.Must(json.Unmarshal([]byte(c.aws("ssm", "send-command",
		"--instance-ids", c.target,
		"--document-name", "AWS-RunShellScript",
		"--comment", "caos-watchdog",
		"--parameters", string(params))), &sent))

	deadline := time.Now().Add(timeout + 30*time.Second)
	for {
		time.Sleep(2 * time.Second)
		// InvocationDoesNotExist for the first second or two after send is
		// normal; it is "not yet", not "broken".
		cmd := exec.Command("aws", "--region", c.region, "--output", "json",
			"ssm", "get-command-invocation", "--command-id", sent.Command.CommandId, "--instance-id", c.target)
		out, err := cmd.Output()
		if err == nil {
			var inv invocation
			w.Must(json.Unmarshal(out, &inv))
			switch inv.Status {
			case "Pending", "InProgress", "Delayed":
			default:
				return inv
			}
		}
		w.True(time.Now().Before(deadline), "ssm command %s still %s after %s",
			sent.Command.CommandId, "running", timeout)
	}
}

func sshMain(c conf, args []string) {
	timeout := 120 * time.Second
	for len(args) > 0 && args[0] != "--" {
		w.True(args[0] == "--timeout" && len(args) >= 2, "ssh: unknown argument %q", args[0])
		timeout = time.Duration(w.Check(strconv.Atoi(args[1]))) * time.Second
		args = args[2:]
	}
	w.True(len(args) >= 2 && args[0] == "--", "ssh: write the command after `--`")
	command := strings.Join(args[1:], " ")
	if m := reserved.FindString(command); m != "" {
		fmt.Fprintf(os.Stderr, "REFUSED: %q is reserved for a human. Ask for it with\n"+
			"  caos-watchdog notify \"NEEDS APPROVAL: <what and why>\"\n"+
			"and stop; do not look for another way to do it.\n", m)
		os.Exit(3)
	}
	inv := c.run(command, timeout)
	fmt.Fprint(os.Stdout, inv.StandardOutputContent)
	fmt.Fprint(os.Stderr, inv.StandardErrorContent)
	if inv.Status != "Success" {
		fmt.Fprintf(os.Stderr, "[ssm: %s, exit %d]\n", inv.Status, inv.ResponseCode)
		if inv.ResponseCode > 0 {
			os.Exit(inv.ResponseCode)
		}
		os.Exit(1)
	}
}

// ticket is the prod server's iroh ticket, the one string a client needs. It
// is read off the host once and cached: it survives caosd restarts, so the
// cache is only wrong after a stack wipe, and the probe refreshes it before
// calling a failed ls-remote an outage.
func ticket(c conf, refresh bool) string {
	path := c.stateDir + "/ticket"
	if !refresh {
		if b, err := os.ReadFile(path); err == nil && len(strings.TrimSpace(string(b))) > 0 {
			return strings.TrimSpace(string(b))
		}
	}
	inv := c.run("cat /data/caos/stack/iroh/ticket", 30*time.Second)
	w.True(inv.Status == "Success", "reading the ticket on %s: %s: %s", c.target, inv.Status,
		strings.TrimSpace(inv.StandardErrorContent))
	t := strings.TrimSpace(inv.StandardOutputContent)
	w.True(strings.HasPrefix(t, "caos://"), "the ticket file does not hold a caos:// ticket: %q", t[:min(len(t), 12)])
	w.Must(os.WriteFile(path, []byte(t+"\n"), 0o600))
	return t
}
