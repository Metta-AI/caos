// tests/actor-check: std/actor-check against the lock actor in
// examples/actor-check (lock/lock.sh) under its mutual-exclusion invariant
// (mutex/mutex.sh), with clients A and B (clients/). In stages, each
// tail-calling the next with run-request-then:
//
//	start     check the lock whose release frees it whoever holds it
//	naive     -> a violation, and the counterexample is the one that matters: A's
//	          release is applied, its reply is lost, B claims the free lock, and
//	          A's RETRY of the same release frees B's lock. Every step agrees
//	          with an independent model of the lock (below), down to the state
//	          oids. Then re-run that retry step's own request
//	replayed  -> the request the trace names really produces the trace's state.
//	          Then deliver the whole counterexample to a REAL actor
//	live      one std/actor request per step, on a fresh branch, a lost reply
//	          being nothing more than the same message sent again later -- which
//	          is what a caller's retry after a crash is. Every reply and every
//	          branch state is the trace's, so A and B really are both granted the
//	          lock; and each step's inner request is the checker's own, found
//	          REUSED in the server's trace of the live request. Then check the
//	          lock whose release frees only the holder's
//	fixed     -> ok, and the exploration matches the independent model exactly:
//	          every reachable state, every transition, every complete execution
//
// THE ORACLE SHARES NOTHING WITH THE CHECKER BUT THE CLIENT FILES. It is the
// lock and the invariant rewritten in Go, explored depth-first with no
// deduplication at all -- every execution walked to its end -- so a checker
// that dropped a state, merged two that differ, or miscounted a path disagrees
// with it here.
package main

import (
	"bytes"
	"crypto/sha1"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/big"
	"math/rand"
	"net/http"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"

	"caos/w"
)

var salt string

func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	return strings.TrimSpace(string(out))
}

func caos(args ...string) string { return run("caos", args...) }

func exists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

func read(path string) string {
	caos("get", path)
	return string(w.Check(os.ReadFile(path)))
}

func arg(name string) string { return strings.TrimSpace(read("/cas/args/" + name)) }

func next(stage string, extra ...string) string {
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage=" + stage, "--test-salt:@=/cas/args/test-salt",
		"--any:@=/cas/args/any", "--holder:@=/cas/args/holder", "--clients:@=/cas/args/clients",
		"--actor:@=/cas/args/actor"}
	return caos(append(args, extra...)...)
}

// inner is the lock the release-<release> check curries onto the checker, with
// this run's salt curried on too (lock.sh ignores it).
//
// THE SALT GOES ON THE INNER, NOT ON THE CHECKER. The live stage proves that
// std/actor runs the checker's own transition requests by finding them REUSED
// in its trace, and a request an EARLIER run cached is reused just the same: a
// checker whose transitions had drifted from std/actor's shape passed that
// check on any warm cache. A salted inner makes every transition of this run a
// new request, so only this run's checker can have run them first.
func inner(release string) string {
	caos("get", "/cas/args/"+release)
	caos("get", "/cas/args/"+release+"/args")
	return caos("curry", "--base:@=/cas/args/"+release+"/args/inner", "--test-salt=actor-check-"+salt)
}

// check runs one of the two checks (examples/actor-check/release-<release>),
// over this run's inner, and continues at stage with the result.
func check(release, stage string) {
	checker := caos("curry", "--unbind=inner", "--base:@=/cas/args/"+release, "--inner:hash="+inner(release))
	request := caos("prepare-request", "--base:hash="+checker)
	caos("run-request-then", request, "--then:hash="+next(stage))
}

// ---- the oracle -----------------------------------------------------------

type omsg struct{ text, expect string }

type ocs struct {
	PC    int
	Lost  int
	Acked []string
}

type ostate struct {
	Holder  string
	Clients []ocs
}

func (s ostate) key() string { return string(w.Check(json.Marshal(s))) }

func clone(s ostate) ostate {
	c := ostate{Holder: s.Holder}
	for _, cs := range s.Clients {
		c.Clients = append(c.Clients, ocs{cs.PC, cs.Lost, append([]string{}, cs.Acked...)})
	}
	return c
}

var (
	names   []string
	scripts [][]omsg
)

func loadClients() {
	caos("get", "/cas/args/clients")
	for _, e := range w.Check(os.ReadDir("/cas/args/clients")) {
		var script []omsg
		for _, line := range strings.Split(read("/cas/args/clients/"+e.Name()), "\n") {
			if line = strings.TrimSpace(line); line == "" {
				continue
			}
			text, expect, _ := strings.Cut(line, " => ")
			script = append(script, omsg{strings.TrimSpace(text), strings.TrimSpace(expect)})
		}
		names = append(names, e.Name())
		scripts = append(scripts, script)
	}
}

// apply is lock.sh, rewritten.
func apply(holder, text, release string) (string, string) {
	op, who, _ := strings.Cut(text, " ")
	switch op {
	case "claim":
		if holder == "" || holder == who {
			return who, "granted"
		}
		return holder, "busy"
	case "release":
		if release == "holder" && holder != who {
			return holder, "not-held"
		}
		return "", "ok"
	}
	w.True(false, "oracle: unknown message %q", text)
	return "", ""
}

// mutexOK is mutex.sh, rewritten.
func mutexOK(s ostate) bool {
	in := 0
	for i, cs := range s.Clients {
		held := false
		for _, a := range cs.Acked {
			if strings.HasPrefix(a, "claim ") && strings.HasSuffix(a, " => granted") {
				held = true
			} else if strings.HasPrefix(a, "release ") {
				held = false
			}
		}
		if held && cs.PC < len(scripts[i]) && strings.HasPrefix(scripts[i][cs.PC].text, "release ") && cs.Lost > 0 {
			held = false
		}
		if held {
			in++
		}
	}
	return in <= 1
}

type move struct {
	client      int
	lost        bool
	reply, text string
	to          ostate
}

// moves is every step from s, self-loops dropped, in no particular order.
func moves(s ostate, release string, maxLost int) (all []move, transitions int) {
	for i, cs := range s.Clients {
		if cs.PC >= len(scripts[i]) {
			continue
		}
		m := scripts[i][cs.PC]
		holder, reply := apply(s.Holder, m.text, release)
		heard := clone(s)
		heard.Holder = holder
		if m.expect == "" || reply == m.expect {
			heard.Clients[i].PC++
			heard.Clients[i].Lost = 0
			heard.Clients[i].Acked = append(heard.Clients[i].Acked, m.text+" => "+reply)
		}
		outs := []move{{i, false, reply, m.text, heard}}
		if cs.Lost < maxLost {
			lost := clone(s)
			lost.Holder = holder
			lost.Clients[i].Lost++
			outs = append(outs, move{i, true, reply, m.text, lost})
		}
		for _, o := range outs {
			transitions++
			if o.to.key() != s.key() {
				all = append(all, o)
			}
		}
	}
	return all, transitions
}

type census struct {
	states, transitions, finals, pairs int
	executions                         *big.Int
	firstViolation                     int // shortest path to a violating state, or -1
}

func explore(release string, maxLost int) census {
	init := ostate{}
	for range names {
		init.Clients = append(init.Clients, ocs{Acked: []string{}})
	}
	seen := map[string]bool{}
	finals := map[string]bool{}
	pairs := map[string]bool{}
	c := census{executions: big.NewInt(0), firstViolation: -1}
	var walk func(s ostate, depth int)
	walk = func(s ostate, depth int) {
		k := s.key()
		if !mutexOK(s) && (c.firstViolation < 0 || depth < c.firstViolation) {
			c.firstViolation = depth
		}
		ms, transitions := moves(s, release, maxLost)
		if !seen[k] {
			seen[k] = true
			c.transitions += transitions
			done := true
			for i, cs := range s.Clients {
				if cs.PC < len(scripts[i]) {
					done = false
					pairs[s.Holder+" "+scripts[i][cs.PC].text] = true
				}
			}
			if done {
				finals[s.Holder] = true
			}
		}
		if len(ms) == 0 {
			c.executions.Add(c.executions, big.NewInt(1))
			return
		}
		for _, m := range ms {
			walk(m.to, depth+1)
		}
	}
	walk(init, 0)
	c.states, c.finals, c.pairs = len(seen), len(finals), len(pairs)
	return c
}

// oid is the git tree a lock state is: empty, or one 0644 file `holder`.
func oid(holder string) string {
	if holder == "" {
		return "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	}
	content := holder + "\n"
	blob := sha1.Sum([]byte(fmt.Sprintf("blob %d\x00%s", len(content), content)))
	entry := append([]byte("100644 holder\x00"), blob[:]...)
	tree := sha1.Sum(append([]byte(fmt.Sprintf("tree %d\x00", len(entry))), entry...))
	return hex.EncodeToString(tree[:])
}

// ---- reading the checker's answer ----------------------------------------

type tedge struct {
	Client, Message, Reply, Before, After, Request string
	Lost                                           bool
}

func result() (verdict, report string, stats map[string]any, trace []tedge) {
	caos("get", "/cas/args/result")
	verdict = strings.TrimSpace(read("/cas/args/result/verdict"))
	report = read("/cas/args/result/report")
	w.Must(json.Unmarshal([]byte(read("/cas/args/result/stats.json")), &stats))
	if exists("/cas/args/result/trace.json") {
		w.Must(json.Unmarshal([]byte(read("/cas/args/result/trace.json")), &trace))
	}
	return
}

func number(stats map[string]any, k string) string {
	switch v := stats[k].(type) {
	case float64:
		return fmt.Sprintf("%.0f", v)
	case string:
		return v
	}
	w.True(false, "stats.json has no %s", k)
	return ""
}

// ---- the live actor -------------------------------------------------------

func readTrace() []tedge {
	var trace []tedge
	w.Must(json.Unmarshal([]byte(read("/cas/args/trace")), &trace))
	w.True(len(trace) > 0, "no trace to deliver")
	return trace
}

// deliver sends step i of the trace to the actor on ref, through std/actor,
// and continues at live with its reply. The inner is the very one the check
// ran.
func deliver(trace []tedge, i int, ref string) {
	path := fmt.Sprintf("/tmp/live-%d", i)
	w.Must(os.WriteFile(path, []byte(trace[i].Message+"\n"), 0o644))
	caos("put", path, "/cas/live-message")
	request := caos("prepare-request", "--base:@=/cas/args/actor", "--state-ref="+ref,
		"--inner:hash="+inner("any"), fmt.Sprintf("--nonce=live-%s-%d", salt, i),
		"--message:@=/cas/live-message")
	caos("run-request-then", request, "--then:hash="+next("live", "--trace:@=/cas/args/trace",
		"--ref="+ref, fmt.Sprintf("--i=%d", i+1), "--req="+request))
}

// liveState is the state tree at the head of the actor's branch.
func liveState(ref string) string {
	url := strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
	w.True(url != "", "this test needs CAOS_SERVER_URL from the runner")
	out := run("git", "ls-remote", "--refs", url, ref)
	w.True(out != "", "the actor's branch %s does not exist", ref)
	head := strings.Fields(out)[0]
	if !exists("/tmp/live-repo") {
		run("git", "init", "-q", "/tmp/live-repo")
	}
	run("git", "-C", "/tmp/live-repo", "fetch", "-q", url, head)
	return run("git", "-C", "/tmp/live-repo", "rev-parse", head+":state")
}

type snode struct {
	ArgTree  string  `json:"arg_tree"`
	Reused   bool    `json:"reused"`
	Children []snode `json:"children"`
}

// status is the server's record of what a request did (SPEC, "Tracing").
func status(request string) snode {
	url := strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
	resp := w.Check(http.Get(url + "/status/" + request + "?all=1"))
	defer resp.Body.Close()
	w.True(resp.StatusCode == http.StatusOK, "GET /status/%s: %s", request, resp.Status)
	var n snode
	w.Must(json.NewDecoder(resp.Body).Decode(&n))
	return n
}

func find(n snode, argTree string) *snode {
	if n.ArgTree == argTree {
		return &n
	}
	for _, c := range n.Children {
		if f := find(c, argTree); f != nil {
			return f
		}
	}
	return nil
}

func main() {
	w.Main(func() {
		stage := "start"
		if exists("/cas/args/stage") {
			stage = arg("stage")
		}
		salt = arg("test-salt")
		loadClients()

		switch stage {
		case "start":
			w.Step("check the lock whose release frees it, whoever holds it")
			check("any", "naive")

		case "naive":
			verdict, report, _, trace := result()
			w.True(verdict == "violation", "verdict %q, want a violation:\n%s", verdict, report)
			want := explore("any", 1).firstViolation
			w.True(want > 0, "the oracle finds no violation for release=any")
			w.True(len(trace) == want, "a %d-step counterexample; the shortest is %d:\n%s", len(trace), want, report)
			w.True(strings.Contains(report, "critical section"), "the report does not carry the invariant's words:\n%s", report)

			// Replay the trace on the oracle, state oid by state oid.
			s := ostate{}
			for range names {
				s.Clients = append(s.Clients, ocs{Acked: []string{}})
			}
			bug := -1
			for n, e := range trace {
				i := -1
				for j, name := range names {
					if name == e.Client {
						i = j
					}
				}
				w.True(i >= 0, "step %d names no client: %q", n+1, e.Client)
				m := scripts[i][s.Clients[i].PC]
				w.True(e.Message == m.text, "step %d: %s sends %q, but its script says %q", n+1, e.Client, e.Message, m.text)
				holder, reply := apply(s.Holder, m.text, "any")
				w.True(e.Reply == reply, "step %d: reply %q, the lock says %q", n+1, e.Reply, reply)
				w.True(e.Before == oid(s.Holder) && e.After == oid(holder),
					"step %d: state %s -> %s, the lock says %s -> %s", n+1, e.Before, e.After, oid(s.Holder), oid(holder))
				// The bug: a release applied to a lock someone ELSE holds. It
				// is a retry, so this client's release was applied before.
				if strings.HasPrefix(m.text, "release ") && s.Holder != "" && s.Holder != e.Client {
					w.True(s.Clients[i].Lost > 0, "step %d frees %s's lock without being a retry", n+1, s.Holder)
					bug = n
				}
				var to ostate
				for _, mv := range func() []move { ms, _ := moves(s, "any", 1); return ms }() {
					if mv.client == i && mv.lost == e.Lost {
						to = mv.to
					}
				}
				w.True(to.Clients != nil, "step %d is not a move the lock allows", n+1)
				s = to
			}
			w.True(!mutexOK(s), "the trace ends in a state where mutual exclusion holds")
			w.True(bug >= 0, "no step of the counterexample is a stale release:\n%s", report)

			// The counterexample is a list of REQUESTS: run the bad step's.
			w.Step("re-run the stale release's own request")
			caos("run-request-then", trace[bug].Request,
				"--then:hash="+next("replayed", "--after="+trace[bug].After,
					"--trace:@=/cas/args/result/trace.json"))

		case "replayed":
			caos("get", "/cas/args/result")
			got := caos("hash", "/cas/args/result/state")
			w.True(got == arg("after"), "re-running the step gave state %s, the trace says %s", got, arg("after"))
			w.True(strings.TrimSpace(read("/cas/args/result/reply")) == "ok", "re-running the step did not reply ok")
			w.Step("deliver the counterexample to a real std/actor branch")
			deliver(readTrace(), 0, fmt.Sprintf("refs/heads/actors/actor-check-%d-%d-%d",
				time.Now().UnixNano(), os.Getpid(), rand.Intn(32768)))

		case "live":
			trace, ref := readTrace(), arg("ref")
			i := w.Check(strconv.Atoi(arg("i")))
			e := trace[i-1]
			reply := strings.TrimSpace(read("/cas/args/result"))
			w.True(reply == e.Reply, "live step %d: std/actor replied %q, the trace says %q", i, reply, e.Reply)
			got := liveState(ref)
			w.True(got == e.After, "live step %d: the branch's state is %s, the trace says %s", i, got, e.After)
			n := find(status(arg("req")), e.Request)
			w.True(n != nil, "live step %d: std/actor did not run the checker's request %s", i, e.Request)
			w.True(n.Reused, "live step %d: the checker's request %s was not reused by std/actor", i, e.Request)
			if i < len(trace) {
				deliver(trace, i, ref)
				return
			}
			w.Step("check the lock whose release frees only the holder's")
			check("holder", "fixed")

		case "fixed":
			verdict, report, stats, trace := result()
			w.True(verdict == "ok", "verdict %q, want ok:\n%s", verdict, report)
			w.True(len(trace) == 0, "an ok check carries a trace")
			o := explore("holder", 1)
			w.True(o.firstViolation < 0, "the oracle finds a violation the checker missed")
			for k, want := range map[string]string{
				"states": fmt.Sprint(o.states), "transitions": fmt.Sprint(o.transitions),
				"executions": o.executions.String(), "inner-runs": fmt.Sprint(o.pairs),
				"invariant-runs": fmt.Sprint(o.states), "final-states": fmt.Sprint(o.finals),
			} {
				w.True(number(stats, k) == want, "%s: the checker says %s, the oracle %s\n%s", k, number(stats, k), want, report)
			}
			w.Report(fmt.Sprintf("actor-check: ALL PASS (%s states, %s executions, %s inner runs)\n",
				number(stats, "states"), number(stats, "executions"), number(stats, "inner-runs")))

		default:
			w.True(false, "unknown --stage: %s", stage)
		}
	})
}
