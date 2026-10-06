// tests/actor: the actor wrapper + a reference key-value inner, in stages (a
// worker cannot block on a run, so each stage tail-calls the next with
// run-request-then). Every program here runs on std/go, which has git.
//
//	start       put a=1             -> one commit, state/a == 1
//	after-put   get a               -> reply 1, head unchanged (a read commits nothing)
//	after-get   put a=1 again       -> head unchanged (same state, no commit, no push).
//	                                   This is also the crash-after-push case: a retry
//	                                   re-applies the message and reaches the same head.
//	after-idem  put b=2             -> a second commit whose parent is the first
//	after-b     fresh branch, impure inner pushes a competing commit mid-request
//	raced       the request FAILED (lost race, not cached); the competing head stands;
//	            send the identical request again
//	retried     the retry succeeded on top of the winner: state has x (winner) and a
//	after-conc  12 concurrent puts (map-then, retried on a lost race) all landed
//	after-lazy  a read touched one entry and the inner saw the others unmaterialized
//	after-hit1/after-hit2
//	            the same read twice with different nonces ran the inner once
package main

import (
	"bytes"
	"fmt"
	"math/rand"
	"os"
	"os/exec"
	"strings"
	"time"

	"caos/w"
)

var (
	salt     string
	stateRef string
	url      string
)

// try runs a command, returning its trimmed stdout and whether it succeeded.
func try(dir, name string, args ...string) (string, bool) {
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), "GIT_TERMINAL_PROMPT=0")
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	return strings.TrimSpace(string(out)), err == nil
}

func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	cmd.Env = append(os.Environ(), "GIT_TERMINAL_PROMPT=0")
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	return strings.TrimSpace(string(out))
}

func caos(args ...string) string { return run("caos", args...) }

func git(args ...string) string { return run("git", append([]string{"-C", "/tmp/repo"}, args...)...) }

func exists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

func readArg(name string) string {
	path := "/cas/args/" + name
	w.True(exists(path), "reading --%s", name)
	caos("get", path)
	return strings.TrimSpace(string(w.Check(os.ReadFile(path))))
}

// remoteHead is the branch's head on the server, or "".
func remoteHead(ref string) string {
	out, ok := try("", "git", "ls-remote", "--refs", url, ref)
	w.True(ok, "ls-remote %s", ref)
	if out == "" {
		return ""
	}
	return strings.Fields(out)[0]
}

func fetch(oid string) {
	git("fetch", "-q", "caos", oid)
}

func stateFile(commit, name string) string {
	fetch(commit)
	return git("show", commit+":state/"+name)
}

// next is the ArgTree of the following stage; extra are more --name=value args.
func next(stage string, extra ...string) string {
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage=" + stage, "--test-salt:@=/cas/args/test-salt",
		"--actor:@=/cas/args/actor", "--kv:@=/cas/args/kv",
		"--probe:@=/cas/args/probe", "--mapper:@=/cas/args/mapper",
		"--state-ref=" + stateRef}
	return caos(append(args, extra...)...)
}

func kvInner() string {
	return caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/kv")
}

// probeInner is the impure inner, in this image: --race-ref=R, --count-ref=C.
func probeInner(opts ...string) string {
	return caos(append([]string{"curry", "--base:@=/cas/args/base",
		"--worker1:@=/cas/args/probe", "--kv:@=/cas/args/kv"}, opts...)...)
}

// actorRequest is the complete request for one message.
func actorRequest(message, nonce, inner string) string {
	w.Must(os.WriteFile("/tmp/msg", []byte(message+"\n"), 0o644))
	_ = os.Remove("/cas/msg")
	caos("put", "/tmp/msg", "/cas/msg")
	return caos("prepare-request", "--base:@=/cas/args/actor", "--state-ref="+stateRef,
		"--inner:hash="+inner, "--nonce="+nonce+"-"+salt, "--message:@=/cas/msg")
}

// call sends message and continues at nextStage. With catch, a failed request
// reaches the next stage as --error instead of failing the test.
func call(message, nonce, inner, nextStage string, catch bool, extra ...string) {
	request := actorRequest(message, nonce, inner)
	args := []string{"run-request-then", request, "--then:hash=" + next(nextStage, extra...)}
	if catch {
		args = append(args, "--catch")
	}
	caos(args...)
}

func freshRef(tag string) string {
	return fmt.Sprintf("refs/heads/actors/test-%s-%d-%d-%d", tag, time.Now().UnixNano(), os.Getpid(), rand.Intn(32768))
}

func main() {
	w.Main(func() {
		stage := "start"
		if exists("/cas/args/stage") {
			stage = readArg("stage")
		}
		salt = readArg("test-salt")
		url = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(url != "", "this test needs CAOS_SERVER_URL from the runner")

		w.Must(os.RemoveAll("/tmp/repo"))
		run("git", "init", "-q", "/tmp/repo")
		git("config", "user.email", "test@caos")
		git("config", "user.name", "caos")
		git("config", "gc.auto", "0")
		git("remote", "add", "caos", url)

		if stage == "start" {
			stateRef = freshRef("main")
		} else {
			stateRef = readArg("state-ref")
		}

		switch stage {
		case "start":
			w.True(remoteHead(stateRef) == "", "fresh ref already exists")
			call("put a 1", "n1", kvInner(), "after-put", false)

		case "after-put":
			h1 := remoteHead(stateRef)
			w.True(h1 != "", "put created no branch")
			w.True(stateFile(h1, "a") == "1", "state/a is not 1")
			w.True(git("rev-list", "--count", h1) == "1", "first update is not a root commit")
			call("get a", "n2", kvInner(), "after-get", false, "--h1="+h1)

		case "after-get":
			h1 := readArg("h1")
			w.True(remoteHead(stateRef) == h1, "a read changed the head")
			reply := readArg("result")
			w.True(reply == "1", "get a replied '%s'", reply)
			call("put a 1", "n3", kvInner(), "after-idem", false, "--h1="+h1)

		case "after-idem":
			h1 := readArg("h1")
			w.True(remoteHead(stateRef) == h1, "an unchanged put made a commit")
			call("put b 2", "n4", kvInner(), "after-b", false, "--h1="+h1)

		case "after-b":
			h1 := readArg("h1")
			h2 := remoteHead(stateRef)
			w.True(h2 != "", "branch vanished")
			w.True(h2 != h1, "put b made no commit")
			fetch(h2)
			w.True(git("rev-parse", h2+"^1") == h1, "second update is not on the first")
			w.True(git("rev-list", "--count", h2) == "2", "history is not a linear chain of two")
			w.True(stateFile(h2, "a") == "1", "state/a lost")
			w.True(stateFile(h2, "b") == "2", "state/b missing")
			// A forced lost race: on a fresh branch the impure inner pushes a
			// competing commit while the request is in flight, so the wrapper's
			// lease (no head) fails.
			stateRef = freshRef("race")
			call("put a 1", "n5", probeInner("--race-ref="+stateRef), "raced", true)

		case "raced":
			w.True(exists("/cas/args/error"), "the raced request did not fail (--error missing)")
			winner := remoteHead(stateRef)
			w.True(winner != "", "the competing writer left no branch")
			w.True(stateFile(winner, "x") == "0", "the head is not the competing commit")
			_, has := try("/tmp/repo", "git", "cat-file", "-e", winner+":state/a")
			w.True(!has, "the lost request published anyway")
			// The identical request again (same nonce): a cached failure would
			// replay the failure; instead it re-runs against the new head.
			call("put a 1", "n5", probeInner("--race-ref="+stateRef), "retried", false, "--winner="+winner)

		case "retried":
			winner := readArg("winner")
			head := remoteHead(stateRef)
			w.True(head != "", "branch vanished")
			w.True(head != winner, "the retry published nothing")
			fetch(head)
			w.True(git("rev-parse", head+"^1") == winner, "the retry is not on top of the winner")
			w.True(stateFile(head, "x") == "0", "the winner's entry was lost")
			w.True(stateFile(head, "a") == "1", "state/a missing after the retry")
			// Concurrent writers, each retrying a lost race, must converge with
			// no lost update.
			stateRef = freshRef("conc")
			w.Must(os.RemoveAll("/tmp/msgs"))
			w.Must(os.MkdirAll("/tmp/msgs", 0o755))
			for n := 1; n <= 12; n++ {
				w.Must(os.WriteFile(fmt.Sprintf("/tmp/msgs/m%d", n), []byte(fmt.Sprintf("put c%d v%d\n", n, n)), 0o644))
			}
			caos("put", "/tmp/msgs", "/cas/msgs")
			mapper := caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/mapper",
				"--actor:@=/cas/args/actor", "--kv:@=/cas/args/kv",
				"--state-ref="+stateRef, "--test-salt:@=/cas/args/test-salt")
			caos("map-then", "/cas/msgs", "--map:hash="+mapper, "--then:hash="+next("after-conc"))

		case "after-conc":
			head := remoteHead(stateRef)
			w.True(head != "", "no branch after the concurrent puts")
			for n := 1; n <= 12; n++ {
				w.True(stateFile(head, fmt.Sprintf("c%d", n)) == fmt.Sprintf("v%d", n), "update c%d was lost", n)
			}
			w.True(git("rev-list", "--count", head) == "12", "expected a linear chain of 12 commits")
			call("getcheck c3", "n6", kvInner(), "after-lazy", false, "--h="+head)

		case "after-lazy":
			head := readArg("h")
			w.True(remoteHead(stateRef) == head, "a read changed the head")
			reply := readArg("result")
			w.True(reply == "v3", "getcheck replied '%s'", reply)
			// The same read twice, different nonces: the inner (pure, so
			// cached) runs once.
			countRef := fmt.Sprintf("refs/heads/actors-count/%d-%d-%d", time.Now().UnixNano(), os.Getpid(), rand.Intn(32768))
			call("get c4", "n7", probeInner("--count-ref="+countRef), "after-hit1", false, "--count-ref="+countRef)

		case "after-hit1":
			countRef := readArg("count-ref")
			w.True(remoteHead(countRef) != "", "the inner did not run")
			call("get c4", "n8", probeInner("--count-ref="+countRef), "after-hit2", false, "--count-ref="+countRef)

		case "after-hit2":
			countRef := readArg("count-ref")
			last := remoteHead(countRef)
			w.True(last != "", "count ref vanished")
			fetch(last)
			runs := git("rev-list", "--count", last)
			w.True(runs == "1", "the inner ran %s times; the repeat should hit the cache", runs)
			w.Report("actor: ALL PASS\n")

		default:
			w.True(false, "unknown --stage: %s", stage)
		}
	})
}
