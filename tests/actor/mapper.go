// One concurrent writer for tests/actor. Used as a map-then `map`, it is called
// with --in=<message blob>; it sends that message to the actor and, when the
// request loses the race for the branch (the wrapper fails it, uncached), sends
// it again with a new request-id, up to maxAttempts. Its callback is this same
// program with --attempt and --msg curried on and --result or --error supplied.
package main

import (
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"

	"caos/w"
)

const maxAttempts = 24

func caos(args ...string) string {
	cmd := exec.Command("caos", args...)
	cmd.Stderr = os.Stderr
	out, err := cmd.Output()
	w.True(err == nil, "caos %s: %v", strings.Join(args, " "), err)
	return strings.TrimSpace(string(out))
}

func exists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

func readArg(name string) string {
	path := "/cas/args/" + name
	caos("get", path)
	return strings.TrimSpace(string(w.Check(os.ReadFile(path))))
}

func main() {
	w.Main(func() {
		if exists("/cas/args/result") {
			caos("forward", "/cas/args/result", "/cas/out")
			return
		}

		attempt := 0
		if exists("/cas/args/attempt") {
			attempt = w.Check(strconv.Atoi(readArg("attempt")))
		}
		if exists("/cas/args/error") {
			attempt++
			w.True(attempt < maxAttempts, "still losing the race after %d attempts: %s", maxAttempts, readArg("error"))
		}

		msg := "/cas/args/msg"
		if !exists(msg) {
			caos("get", "/cas/args/in")
			msg = "/cas/args/in"
		}
		stateRef, salt := readArg("state-ref"), readArg("test-salt")
		requestID := fmt.Sprintf("%s-%d-%s", caos("hash", msg), attempt, salt)

		inner := caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/kv")
		request := caos("prepare-request", "--base:@=/cas/args/actor", "--state-ref="+stateRef,
			"--inner:hash="+inner, "--request-id="+requestID, "--message:@="+msg)
		callback := caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
			"--actor:@=/cas/args/actor", "--kv:@=/cas/args/kv",
			"--state-ref="+stateRef, "--test-salt:@=/cas/args/test-salt",
			"--attempt="+strconv.Itoa(attempt), "--msg:@="+msg)
		caos("run-request-then", request, "--then:hash="+callback, "--catch")
	})
}
