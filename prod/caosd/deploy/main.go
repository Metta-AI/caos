// prod/caosd/deploy — turn the machine this runs on into a caosd-prod host.
//
//	nix run github:Metta-AI/caos#deploy-caosd-prod
//
// A flake is pure, so it cannot read this machine's public address or look at
// its disks. That is the whole reason this program exists: it gathers the facts
// a shared role config cannot know, refuses to proceed if the machine is not
// ready, and only then builds the role with those facts layered on.
//
// Commands are run through os/exec with an explicit argv rather than
// script.Exec, because the nix expression below carries quotes, braces and
// spaces — exactly the material that makes a shell command line ambiguous.
// Error policy is caos/w, so any failure ends the run loudly and names itself.
package main

import (
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"time"

	"caos/w"

	"github.com/bitfield/script"
)

const (
	hostAttr  = "caosd-prod"
	dataLabel = "caos-data"
	imds      = "http://169.254.169.254"
	irohPort  = 11204
)

// defaultFlake is the flake this binary was BUILT from, injected by the flake
// itself (-X main.defaultFlake=path:${self}). Without it the default was a
// branch name baked in by hand, so
//
//	nix run git+...?ref=my-branch#deploy-caosd-prod
//
// fetched the driver from my-branch and then built the host from main --
// silently, and fatally if main has no nixosConfigurations yet. The ref had to
// be repeated as an argument to get the obvious behaviour. Now the driver and
// the host config it applies always come from the same revision, and an
// argument is only needed to deliberately build from somewhere ELSE.
var defaultFlake = "github:Metta-AI/caos"

// run executes argv and returns its stdout, failing with the command's own
// stderr as the diagnostic. Explicit argv: no quoting, no word splitting.
func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	var errb strings.Builder
	cmd.Stderr = &errb
	out, err := cmd.Output()
	if err != nil {
		w.True(false, "%s %s: %v: %s",
			name, strings.Join(args, " "), err, strings.TrimSpace(errb.String()))
	}
	return strings.TrimRight(string(out), "\n")
}

// stream executes argv with the caller's stdout/stderr, for the steps whose
// progress the operator should watch as it happens.
func stream(name string, args ...string) {
	cmd := exec.Command(name, args...)
	cmd.Stdout, cmd.Stderr, cmd.Stdin = os.Stdout, os.Stderr, os.Stdin
	w.Must(cmd.Run())
}

// requireDataVolume refuses to touch the machine unless the bulk volume is
// there. It carries docker's data-root, the registry, redis and the server
// repo; without it they land on the small root disk, which is a slow failure
// discovered long after the switch. Checked here, before anything is built.
func requireDataVolume() {
	dev := "/dev/disk/by-label/" + dataLabel
	if _, err := os.Stat(dev); err == nil {
		fmt.Fprintf(os.Stderr, "data volume  %s\n", dev)
		return
	}
	// Informational only: Try, not Out — a machine without lsblk should still
	// get the instructions below rather than dying on the diagnostic.
	devices, _ := w.Try(script.Exec("lsblk -o NAME,SIZE,FSTYPE,LABEL,MOUNTPOINT"))
	w.True(false, "no volume labelled %q.\n\n"+
		"  Attach the data volume, then label it ONCE (this ERASES that disk):\n"+
		"      sudo mkfs.ext4 -L %s /dev/nvme1n1\n\n"+
		"  Present block devices:\n%s",
		dataLabel, dataLabel, indent(devices))
}

func indent(s string) string {
	if strings.TrimSpace(s) == "" {
		return "      (lsblk unavailable)"
	}
	return "      " + strings.ReplaceAll(s, "\n", "\n      ")
}

// imdsGet performs one IMDSv2 request. Separate from its callers so a failure
// says which of the two steps failed rather than "IMDS broke".
func imdsGet(method, url string, headers map[string]string) (string, error) {
	req, err := http.NewRequest(method, url, nil)
	if err != nil {
		return "", err
	}
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	resp, err := (&http.Client{Timeout: 5 * time.Second}).Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return "", err
	}
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("%s: %s", resp.Status, strings.TrimSpace(string(body)))
	}
	return strings.TrimSpace(string(body)), nil
}

// publicAddress is what goes into the iroh ticket. EC2 NATs it, so the host
// cannot find it by looking at its own interfaces; without it clients fall back
// to a relay (~4.6s vs ~100ms for a cached run), so an absent one is fatal
// rather than a warning.
func publicAddress() string {
	token, err := imdsGet(http.MethodPut, imds+"/latest/api/token",
		map[string]string{"X-aws-ec2-metadata-token-ttl-seconds": "60"})
	if err != nil {
		w.True(false, "cannot reach IMDS at %s: %v\n"+
			"  If a container veth has taken 169.254.0.0/16, check:\n"+
			"      ip -4 route | grep 169.254", imds, err)
	}
	addr, err := imdsGet(http.MethodGet, imds+"/latest/meta-data/public-ipv4",
		map[string]string{"X-aws-ec2-metadata-token": token})
	if err != nil {
		w.True(false, "this instance has no public IPv4: %v\n"+
			"  Associate an Elastic IP first; clients need a reachable address\n"+
			"  in the ticket, and it must not change under them.", err)
	}
	return addr
}

// buildSystem layers this machine's facts onto the shared role. extendModules
// is what keeps the committed flake pure: the role declares
// caos.advertiseAddress as a required option with no default, and the only
// impurity — reading it off this host — stays in this program.
func buildSystem(flakeRef, addr string) string {
	expr := fmt.Sprintf(
		`((builtins.getFlake "%s").nixosConfigurations.%s.extendModules {`+
			` modules = [ { caos.advertiseAddress = "%s"; } ]; })`+
			`.config.system.build.toplevel`,
		flakeRef, hostAttr, addr)
	return run("nix", "build", "--impure", "--no-link", "--print-out-paths", "--expr", expr)
}

func main() {
	w.Main(func() {
		flakeRef := defaultFlake
		if len(os.Args) > 1 {
			flakeRef = os.Args[1]
		}

		w.Step("checking this machine")
		requireDataVolume()

		w.Step("reading host facts")
		addr := publicAddress()
		fmt.Fprintf(os.Stderr, "advertising  %s:%d\n", addr, irohPort)
		fmt.Fprintf(os.Stderr, "flake        %s\n", flakeRef)

		w.Step("building " + hostAttr)
		system := buildSystem(flakeRef, addr)
		fmt.Fprintf(os.Stderr, "built        %s\n", system)

		w.Step("switching")
		stream("sudo", "nix-env", "-p", "/nix/var/nix/profiles/system", "--set", system)
		stream("sudo", system+"/bin/switch-to-configuration", "switch")

		fmt.Fprintln(os.Stderr, "\ndone. 'systemctl status caosd' for the stack.")
	})
}
