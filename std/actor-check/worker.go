// std/actor-check: model-check an actor (std/actor/README.md).
//
// An actor's inner is a pure function (state tree, message) -> {state, reply},
// and caos caches it on exactly those two. That is a transition relation, and
// the object store is already a hash table of states. So this explores EVERY
// order in which a set of clients' messages can be applied to one actor,
// including the one std/actor fault that changes state -- a crash after the
// push, which loses the reply, so the client sends the same message again and
// it is applied a second time, after whatever else landed in between -- and
// runs an invariant in each reachable state. It is breadth-first, so a
// violation it reports is a shortest one.
//
// Args:
//
//	inner       the inner actor, exactly as std/actor takes it
//	clients     a tree, one blob per client: its messages in order, one per
//	            line. `<message> => <reply>` makes that client WAIT: it resends
//	            the message until the reply is <reply> (a claim that must be
//	            granted). Without `=>` any reply moves the client on
//	invariant   optional: an image run on every reachable state, with that state
//	            at /cas/args/in = {state, terminal, clients/<name>/{acked,
//	            pending, lost}}. It returns a blob: `ok`, or what is wrong
//	lost        optional: how many replies to ONE message may be lost (default 1)
//	max-states  optional: give up past this many states (default 20000)
//	nonce       optional, opaque: re-runs these stages and nothing below them
//
// The result is a tree {report, verdict, stats.json[, trace.json]}. verdict is
// `ok`, `violation`, `stuck` (a client can never proceed) or `incomplete`.
//
// TRANSITIONS ARE NOT RUN HERE. Each one is the inner request std/actor would
// form (step.sh), dispatched through map-then, so it is cached like any job:
// a (state, message) pair runs once no matter how many interleavings reach it,
// a re-check with a different invariant runs no transition at all, and a
// transition a live actor already ran is a hit here. Only the bookkeeping is
// in this program.
//
// Three positions, by --stage, each tail-calling the next with its whole
// search state (--ctx) curried on: start; judged (the invariant map's then);
// stepped (the transition map's then).
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"caos/w"
)

type message struct {
	Text   string `json:"text"`
	Expect string `json:"expect,omitempty"`
}

type client struct {
	Name     string    `json:"name"`
	Messages []message `json:"messages"`
}

// cstate is what a client knows: where it is in its script, how many times the
// message it is sending has been applied without a reply reaching it, and the
// replies it has heard.
type cstate struct {
	PC    int      `json:"pc"`
	Lost  int      `json:"lost"`
	Acked []string `json:"acked"`
}

type edge struct {
	Client  string `json:"client"`
	Message string `json:"message"`
	Reply   string `json:"reply"`
	Lost    bool   `json:"lost"`
	Before  string `json:"before"`
	After   string `json:"after"`
	Request string `json:"request,omitempty"`
}

type node struct {
	Actor   string   `json:"actor"`
	Clients []cstate `json:"clients"`
	Parent  int      `json:"parent"`
	Edge    *edge    `json:"edge,omitempty"`
	Depth   int      `json:"depth"`
	Succ    []int    `json:"succ,omitempty"`
}

type step struct {
	State string `json:"state"`
	Reply string `json:"reply"`
}

type search struct {
	Clients   []client        `json:"clients"`
	MaxLost   int             `json:"max_lost"`
	MaxStates int             `json:"max_states"`
	Nodes     []node          `json:"nodes"`
	Frontier  []int           `json:"frontier"`
	Steps     map[string]step `json:"steps"`
	Pending   []string        `json:"pending"`
	Judged    int             `json:"judged"`
	Edges     int             `json:"edges"`
	index     map[string]int
}

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

func intArg(name string, def int) int {
	if !exists("/cas/args/" + name) {
		return def
	}
	n := w.Check(strconv.Atoi(arg(name)))
	w.True(n >= 0, "--%s must not be negative", name)
	return n
}

// The step key: a state and a message, which is all a transition depends on.
func stepKey(state, text string) string { return state + " " + text }

func key(actor string, clients []cstate) string {
	return string(w.Check(json.Marshal(struct {
		A string
		C []cstate
	}{actor, clients})))
}

func (s *search) reindex() {
	s.index = map[string]int{}
	for i, n := range s.Nodes {
		s.index[key(n.Actor, n.Clients)] = i
	}
}

// statePath is a /cas placeholder for a state tree, made once per oid, for
// binding by reference: nothing below it is fetched.
func statePath(oid string) string {
	path := "/cas/state-" + oid
	if !exists(path) {
		caos("get-hash", oid, path)
	}
	return path
}

var puts int

// putFile stores bytes as a blob at a fresh /cas path and returns that path.
func putFile(content string) string {
	puts++
	tmp := fmt.Sprintf("/tmp/put-%d", puts)
	w.Must(os.WriteFile(tmp, []byte(content), 0o644))
	path := fmt.Sprintf("/cas/put-%d", puts)
	caos("put", tmp, path)
	return path
}

func write(path, content string) {
	w.Must(os.MkdirAll(filepath.Dir(path), 0o755))
	w.Must(os.WriteFile(path, []byte(content), 0o644))
}

// mapper is the one-transition image: step.sh on std/bash, with the inner on.
var mapper string

// next is the ArgTree of the following position, carrying the search.
func next(stage string, s *search) string {
	ctx := putFile(string(w.Check(json.Marshal(s))))
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage=" + stage, "--ctx:@=" + ctx,
		"--inner:@=/cas/args/inner", "--mapper:hash=" + mapper}
	for _, opt := range []string{"invariant", "nonce"} {
		if exists("/cas/args/" + opt) {
			args = append(args, "--"+opt+":@=/cas/args/"+opt)
		}
	}
	return caos(args...)
}

func done(c *client, cs cstate) bool { return cs.PC >= len(c.Messages) }

func terminal(s *search, n node) bool {
	for i := range s.Clients {
		if !done(&s.Clients[i], n.Clients[i]) {
			return false
		}
	}
	return true
}

func parseClients() []client {
	caos("get", "/cas/args/clients")
	entries := w.Check(os.ReadDir("/cas/args/clients"))
	w.True(len(entries) > 0, "--clients names no client")
	var clients []client
	for _, e := range entries { // ReadDir sorts by name, which fixes client order
		c := client{Name: e.Name()}
		for _, line := range strings.Split(read(filepath.Join("/cas/args/clients", e.Name())), "\n") {
			line = strings.TrimSpace(line)
			if line == "" || strings.HasPrefix(line, "#") {
				continue
			}
			m := message{Text: line}
			if text, expect, ok := strings.Cut(line, " => "); ok {
				m = message{Text: strings.TrimSpace(text), Expect: strings.TrimSpace(expect)}
			}
			w.True(m.Text != "", "client %s: an empty message", c.Name)
			c.Messages = append(c.Messages, m)
		}
		clients = append(clients, c)
	}
	return clients
}

func start() {
	s := &search{
		Clients:   parseClients(),
		MaxLost:   intArg("lost", 1),
		MaxStates: intArg("max-states", 20000),
		Steps:     map[string]step{},
	}
	w.Must(os.MkdirAll("/tmp/empty", 0o755))
	caos("put", "/tmp/empty", "/cas/empty")
	init := node{Actor: caos("hash", "/cas/empty"), Parent: -1}
	for range s.Clients {
		init.Clients = append(init.Clients, cstate{Acked: []string{}})
	}
	s.Nodes = []node{init}
	s.Frontier = []int{0}
	s.reindex()
	advance(s)
}

// advance judges the frontier: the invariant over every new state, as one map.
func advance(s *search) {
	if !exists("/cas/args/invariant") {
		proceed(s)
		return
	}
	w.Must(os.RemoveAll("/tmp/batch"))
	for _, id := range s.Frontier {
		n := s.Nodes[id]
		dir := fmt.Sprintf("/tmp/batch/%06d", id)
		w.Must(os.MkdirAll(dir, 0o755))
		w.Must(os.Symlink(statePath(n.Actor), dir+"/state"))
		write(dir+"/terminal", map[bool]string{true: "yes\n", false: "no\n"}[terminal(s, n)])
		for i, c := range s.Clients {
			cs := n.Clients[i]
			acked := strings.Join(cs.Acked, "\n")
			if acked != "" {
				acked += "\n"
			}
			pending := ""
			if !done(&c, cs) {
				pending = c.Messages[cs.PC].Text + "\n"
			}
			write(dir+"/clients/"+c.Name+"/acked", acked)
			write(dir+"/clients/"+c.Name+"/pending", pending)
			write(dir+"/clients/"+c.Name+"/lost", fmt.Sprintf("%d\n", cs.Lost))
		}
	}
	caos("put", "/tmp/batch", "/cas/batch")
	s.Judged += len(s.Frontier)
	caos("map-then", "/cas/batch", "--map:hash="+caos("hash", "/cas/args/invariant"),
		"--then:hash="+next("judged", s))
}

func judged(s *search) {
	caos("get", "/cas/args/children")
	for _, id := range s.Frontier {
		verdict := strings.TrimSpace(read(fmt.Sprintf("/cas/args/children/%06d", id)))
		if verdict != "ok" {
			finish(s, "violation", id, verdict)
			return
		}
	}
	proceed(s)
}

// proceed runs every transition the frontier needs that has not run yet.
func proceed(s *search) {
	need := map[string]bool{}
	for _, id := range s.Frontier {
		n := s.Nodes[id]
		for i, c := range s.Clients {
			if done(&c, n.Clients[i]) {
				continue
			}
			k := stepKey(n.Actor, c.Messages[n.Clients[i].PC].Text)
			if _, ok := s.Steps[k]; !ok {
				need[k] = true
			}
		}
	}
	if len(need) == 0 {
		successors(s)
		return
	}
	s.Pending = make([]string, 0, len(need))
	for k := range need {
		s.Pending = append(s.Pending, k)
	}
	sort.Strings(s.Pending)
	w.Must(os.RemoveAll("/tmp/steps"))
	for j, k := range s.Pending {
		state, text, _ := strings.Cut(k, " ")
		dir := fmt.Sprintf("/tmp/steps/%06d", j)
		w.Must(os.MkdirAll(dir, 0o755))
		w.Must(os.Symlink(statePath(state), dir+"/state"))
		// The message's bytes are the blob the inner reads, so they are part
		// of the transition's key: one line, newline-terminated, always.
		write(dir+"/message", text+"\n")
	}
	caos("put", "/tmp/steps", "/cas/steps")
	caos("map-then", "/cas/steps", "--map:hash="+mapper, "--then:hash="+next("stepped", s))
}

func stepped(s *search) {
	caos("get", "/cas/args/children")
	for j, k := range s.Pending {
		dir := fmt.Sprintf("/cas/args/children/%06d", j)
		caos("get", dir)
		w.True(exists(dir+"/state") && exists(dir+"/reply"),
			"the inner's result for %q is not {state, reply}", k)
		s.Steps[k] = step{State: caos("hash", dir+"/state"), Reply: strings.TrimSpace(read(dir + "/reply"))}
	}
	s.Pending = nil
	successors(s)
}

// successors expands the frontier by one step of every client, in both ways a
// step can end: the reply arrives, or the message was applied and its reply
// lost (so the client will send it again).
func successors(s *search) {
	var frontier []int
	for _, id := range s.Frontier {
		n := s.Nodes[id]
		moved := false
		for i, c := range s.Clients {
			cs := n.Clients[i]
			if done(&c, cs) {
				continue
			}
			m := c.Messages[cs.PC]
			r := s.Steps[stepKey(n.Actor, m.Text)]
			heard := cs
			heard.Acked = append(append([]string{}, cs.Acked...), m.Text+" => "+r.Reply)
			heard.PC, heard.Lost = cs.PC+1, 0
			if m.Expect != "" && r.Reply != m.Expect {
				heard = cs // not the reply it waits for: it will ask again
			}
			outcomes := []cstate{heard}
			if cs.Lost < s.MaxLost {
				lost := cs
				lost.Lost++
				outcomes = append(outcomes, lost)
			}
			for o, next := range outcomes {
				clients := append([]cstate{}, n.Clients...)
				clients[i] = next
				k := key(r.State, clients)
				s.Edges++
				if k == key(n.Actor, n.Clients) {
					continue // a reply that changes nothing, e.g. busy
				}
				moved = true
				to, seen := s.index[k]
				if !seen {
					to = len(s.Nodes)
					s.Nodes = append(s.Nodes, node{
						Actor: r.State, Clients: clients, Parent: id, Depth: n.Depth + 1,
						Edge: &edge{Client: c.Name, Message: m.Text, Reply: r.Reply, Lost: o == 1,
							Before: n.Actor, After: r.State},
					})
					s.index[k] = to
					frontier = append(frontier, to)
				}
				s.Nodes[id].Succ = append(s.Nodes[id].Succ, to)
			}
		}
		if !moved && !terminal(s, n) {
			var waiting []string
			for i, c := range s.Clients {
				if !done(&c, n.Clients[i]) {
					waiting = append(waiting, fmt.Sprintf("%s waits on %q", c.Name, c.Messages[n.Clients[i].PC].Text))
				}
			}
			finish(s, "stuck", id, "no client can proceed: "+strings.Join(waiting, ", "))
			return
		}
	}
	if len(frontier) == 0 {
		finish(s, "ok", -1, "")
		return
	}
	if len(s.Nodes) > s.MaxStates {
		finish(s, "incomplete", -1, fmt.Sprintf("more than %d states", s.MaxStates))
		return
	}
	s.Frontier = frontier
	advance(s)
}

// executions counts the complete executions: paths from the initial state to a
// state with no successor. The state graph is acyclic when every message moves
// its client on, which is what makes this a number at all.
func executions(s *search) (*big.Int, bool) {
	memo := make([]*big.Int, len(s.Nodes))
	visiting := make([]bool, len(s.Nodes))
	acyclic := true
	var count func(int) *big.Int
	count = func(id int) *big.Int {
		if memo[id] != nil {
			return memo[id]
		}
		if visiting[id] {
			acyclic = false
			return big.NewInt(0)
		}
		visiting[id] = true
		total := big.NewInt(0)
		if len(s.Nodes[id].Succ) == 0 {
			total.SetInt64(1)
		}
		for _, to := range s.Nodes[id].Succ {
			total.Add(total, count(to))
		}
		visiting[id] = false
		memo[id] = total
		return total
	}
	return count(0), acyclic
}

func finish(s *search, verdict string, at int, detail string) {
	var trace []edge
	for id := at; id > 0; id = s.Nodes[id].Parent {
		trace = append([]edge{*s.Nodes[id].Edge}, trace...)
	}
	// Each step names the request that ran it, so a counterexample is something
	// to RE-RUN, not just read: it is std/actor's request, formed the same way.
	for i := range trace {
		trace[i].Request = caos("prepare-request", "--base:@=/cas/args/inner",
			"--state:@="+statePath(trace[i].Before), "--message:@="+putFile(trace[i].Message+"\n"))
	}

	depth := 0
	for _, n := range s.Nodes {
		if n.Depth > depth {
			depth = n.Depth
		}
	}
	stats := map[string]any{
		"complete": verdict == "ok", "states": len(s.Nodes), "transitions": s.Edges,
		"depth": depth, "inner-runs": len(s.Steps), "invariant-runs": s.Judged,
	}
	// What only a finished search can say. A stopped one has a frontier it
	// never expanded, which would count as dead ends.
	execs, finals := "", 0
	if verdict == "ok" {
		runs, acyclic := executions(s)
		execs = runs.String()
		if !acyclic {
			execs = "unbounded (the state graph has a cycle)"
		}
		seen := map[string]bool{}
		for _, n := range s.Nodes {
			if terminal(s, n) {
				seen[n.Actor] = true
			}
		}
		finals = len(seen)
		stats["executions"], stats["final-states"] = execs, finals
	}

	var r strings.Builder
	switch verdict {
	case "ok":
		fmt.Fprintf(&r, "actor-check: OK -- the invariant holds in every one of %d reachable states\n", len(s.Nodes))
	case "incomplete":
		fmt.Fprintf(&r, "actor-check: INCOMPLETE -- %s; nothing past it was checked\n", detail)
	default:
		fmt.Fprintf(&r, "FAILED: actor-check: %s after %d steps (breadth-first, so no shorter run fails)\n", strings.ToUpper(verdict), len(trace))
		fmt.Fprintf(&r, "  %s\n\n", detail)
		for i, e := range trace {
			outcome := "-> " + e.Reply
			if e.Lost {
				outcome = "-> " + e.Reply + "  (reply lost: it will be sent again)"
			}
			fmt.Fprintf(&r, "  %2d. %-3s %-12s %s\n", i+1, e.Client, e.Message, outcome)
			fmt.Fprintf(&r, "      state %.10s -> %.10s   request %.12s\n", e.Before, e.After, e.Request)
		}
		r.WriteString("\n")
	}
	if verdict == "ok" {
		fmt.Fprintf(&r, "explored %d states and %d transitions to depth %d: %s complete executions\n",
			len(s.Nodes), s.Edges, depth, execs)
	} else {
		fmt.Fprintf(&r, "stopped after %d states and %d transitions, at depth %d\n", len(s.Nodes), s.Edges, depth)
	}
	fmt.Fprintf(&r, "ran the inner on %d distinct (state, message) pairs and the invariant on %d states\n",
		len(s.Steps), s.Judged)
	if verdict == "ok" {
		fmt.Fprintf(&r, "the clients can leave the actor in %d distinct final state(s)\n", finals)
	}

	w.Must(os.RemoveAll("/tmp/result"))
	write("/tmp/result/report", r.String())
	write("/tmp/result/verdict", verdict+"\n")
	write("/tmp/result/stats.json", string(w.Check(json.MarshalIndent(stats, "", "  ")))+"\n")
	if len(trace) > 0 {
		write("/tmp/result/trace.json", string(w.Check(json.MarshalIndent(trace, "", "  ")))+"\n")
	}
	fmt.Fprint(os.Stderr, r.String())
	caos("put", "/tmp/result", "/cas/out")
}

func main() {
	w.Main(func() {
		stage := "start"
		if exists("/cas/args/stage") {
			stage = arg("stage")
		}
		switch stage {
		case "start":
			// The mapper is curried ONCE and carried, so every transition of
			// this check is the same image over a different child.
			mapper = caos("curry", "--base:@=/cas/args/bash", "--worker1:@=/cas/args/step",
				"--inner:@=/cas/args/inner")
			start()
		case "judged", "stepped":
			mapper = caos("hash", "/cas/args/mapper")
			var s search
			w.Must(json.Unmarshal([]byte(read("/cas/args/ctx")), &s))
			s.reindex()
			if stage == "judged" {
				judged(&s)
			} else {
				stepped(&s)
			}
		default:
			w.True(false, "unknown --stage: %s", stage)
		}
	})
}
