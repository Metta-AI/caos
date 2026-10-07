// tests/chat-squash-publish — design/stacks.md's "Publishing", run through
// llm-step's real tool loop with a scripted model, in one turn: build a stack
// with the shell, write a plan, run create-squashed-stack into publish/mystack,
// reword one message in the plan and run it again (a republish over what the
// first run wrote), then call publish_source.
//
// It checks that create-squashed-stack leaves publish/mystack/01-feature as a
// source tree that publish_source accepts. The repository is a `.invalid`
// host, so no push happens: the call must get past the source-tree check and
// fail reading the remote. The same call on publish/mystack, a plain folder,
// must be refused at that check.
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"math/rand"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"caos/w"
)

const (
	repo = "/tmp/repo"
	stub = "/tmp/stub"
	tool = "/tmp/llm-test-tool"
)

func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	if err != nil {
		var exit *exec.ExitError
		w.True(errors.As(err, &exit), "running %s: %v", name, err)
	}
	w.True(err == nil, "%s %s: %s", name, strings.Join(args, " "), strings.TrimSpace(stderr.String()))
	return strings.TrimRight(stdout.String(), "\n")
}

func caos(args ...string) string { return run("caos", args...) }
func git(args ...string) string  { return run("git", append([]string{"-C", repo}, args...)...) }

// fields reads `key value` lines, as llm-test-tool prints them.
func fields(out string) map[string]string {
	m := map[string]string{}
	for _, line := range strings.Split(out, "\n") {
		if k, v, ok := strings.Cut(line, " "); ok {
			m[k] = v
		}
	}
	return m
}

var narration strings.Builder

// note keeps a line of what the model saw as this test's narration, so a pass
// prints the recipe's actual output.
func note(format string, a ...any) {
	line := fmt.Sprintf(format, a...)
	narration.WriteString(line + "\n")
	fmt.Fprintln(os.Stderr, line)
}

// install copies a test binary out of its CAS entry.
func install(arg, bin, dst string) {
	caos("get", "-r", "/cas/args/"+arg)
	w.Must(os.WriteFile(dst, w.Check(os.ReadFile("/cas/args/"+arg+"/bin/"+bin)), 0o755))
}

// stubHost is this container's address, which llm-step's worker dials.
func stubHost() string {
	self := strings.TrimSpace(string(w.Check(os.ReadFile("/etc/hostname"))))
	for _, line := range strings.Split(string(w.Check(os.ReadFile("/etc/hosts"))), "\n") {
		f := strings.Fields(line)
		for _, name := range f[min(1, len(f)):] {
			if name == self {
				return f[0]
			}
		}
	}
	w.True(false, "no /etc/hosts entry for %s", self)
	return ""
}

// startStub starts llm-stub on a free port and returns the port. The stub
// answers request N with stub/response-N.json and records it as request-N.json.
func startStub() int {
	w.Must(os.MkdirAll(stub, 0o755))
	install("stub", "llm-stub", "/tmp/llm-stub")
	for range 5 {
		port := 20000 + rand.Intn(20000)
		log := w.Check(os.Create(filepath.Join(stub, "log")))
		cmd := exec.Command("/tmp/llm-stub", fmt.Sprintf("0.0.0.0:%d", port), stub)
		cmd.Stderr = log
		w.Must(cmd.Start())
		for range 400 {
			if c, err := net.Dial("tcp", fmt.Sprintf("127.0.0.1:%d", port)); err == nil {
				c.Close()
				return port
			}
			time.Sleep(5 * time.Millisecond)
		}
		cmd.Process.Kill()
	}
	w.True(false, "could not start llm-stub: %s", w.Check(os.ReadFile(filepath.Join(stub, "log"))))
	return 0
}

// respond scripts the model's reply to request n.
func respond(n int, content ...map[string]any) {
	stop := "end_turn"
	for _, c := range content {
		if c["type"] == "tool_use" {
			stop = "tool_use"
		}
	}
	body := w.Check(json.Marshal(map[string]any{"content": content, "stop_reason": stop}))
	w.Must(os.WriteFile(filepath.Join(stub, fmt.Sprintf("response-%d.json", n)), body, 0o644))
}

func use(id, name string, input map[string]any) map[string]any {
	return map[string]any{"type": "tool_use", "id": id, "name": name, "input": input}
}

func shell(id, cmd, path string) map[string]any {
	return use(id, "run_tool", map[string]any{"path": "tools/sh",
		"arguments": map[string]any{"cmd": cmd, "paths": []string{path}}})
}

func write(id, path, content string) map[string]any {
	return use(id, "write", map[string]any{"file-path": path, "content": content})
}

// seen is what the model was shown for a call: request n's tool result for id,
// prefixed ERROR: when it was an error.
func seen(n int, id string) string {
	var req struct {
		Messages []struct {
			Content json.RawMessage `json:"content"`
		} `json:"messages"`
	}
	w.Must(json.Unmarshal(w.Check(os.ReadFile(filepath.Join(stub, fmt.Sprintf("request-%d.json", n)))), &req))
	var blocks []struct {
		Type      string          `json:"type"`
		ToolUseID string          `json:"tool_use_id"`
		IsError   bool            `json:"is_error"`
		Content   json.RawMessage `json:"content"`
	}
	w.Must(json.Unmarshal(req.Messages[len(req.Messages)-1].Content, &blocks))
	for _, b := range blocks {
		if b.Type != "tool_result" || b.ToolUseID != id {
			continue
		}
		var text string
		if json.Unmarshal(b.Content, &text) != nil {
			var parts []struct{ Text string }
			w.Must(json.Unmarshal(b.Content, &parts))
			for _, p := range parts {
				text += p.Text
			}
		}
		if b.IsError {
			text = "ERROR: " + text
		}
		return text
	}
	w.True(false, "request %d has no result for %s", n, id)
	return ""
}

// reported is the commit a squash reported for an entry, from its
// `  publish/mystack/<entry>  <oid>` line.
func reported(out, entry string) string {
	for _, line := range strings.Split(out, "\n") {
		if rest, ok := strings.CutPrefix(line, "  publish/mystack/"+entry+"  "); ok {
			return rest
		}
	}
	w.True(false, "no line for %s in: %s", entry, out)
	return ""
}

func sourceTree(head, name string) string {
	return fields(run(tool, "source-tree", "--repo", repo, "--head", head, "--name", name))["commit"]
}

// plan is a plan file: onto and into, then a layer= block per layer, each
// message line its own message= line.
func plan(m1, m2 string) string {
	var b strings.Builder
	b.WriteString("onto=mystack/00-base\ninto=publish/mystack\n")
	for _, l := range [][2]string{{"01-feature", m1}, {"02-tests", m2}} {
		fmt.Fprintf(&b, "layer=%s\ncommit=mystack/%s\n", l[0], l[0])
		for _, line := range strings.Split(l[1], "\n") {
			b.WriteString("message=" + line + "\n")
		}
	}
	return b.String()
}

func main() {
	w.Main(func() {
		w.Step("a source tree, a stub model, and a conversation")
		caos("get", "/cas/args/test-salt")
		salt := strings.TrimSpace(string(w.Check(os.ReadFile("/cas/args/test-salt"))))
		install("tool", "llm-test-tool", tool)
		w.Must(os.MkdirAll(repo, 0o755))
		git("init", "-q")
		git("config", "gc.auto", "0")
		git("remote", "add", "caos", os.Getenv("CAOS_SERVER_URL"))

		w.Must(os.MkdirAll("/tmp/ws", 0o755))
		w.Must(os.WriteFile("/tmp/ws/base.txt", []byte("base\n"), 0o644))
		caos("put", "/tmp/ws", "/cas/ws")
		commit := fmt.Sprintf("tree %s\nauthor caos <test@caos> 1700000000 +0000\ncommitter caos <test@caos> 1700000000 +0000\n\nbase (%s)\n",
			caos("hash", "/cas/ws"), salt)
		w.Must(os.WriteFile("/tmp/base-commit", []byte(commit), 0o644))
		base := caos("put-commit", "/tmp/base-commit", "/cas/base")

		port := startStub()
		conv := fmt.Sprintf("%d-%d-squash-publish", time.Now().UnixNano(), rand.Int())
		w.Must(os.WriteFile("/tmp/system.txt", []byte("You are a coding agent."), 0o644))
		caos("put", "/tmp/system.txt", "/cas/system")
		llm := caos("curry", "--base:hash="+caos("hash", "/cas/args/llm-step"), "--system:@=/cas/system",
			"--model=test-model", fmt.Sprintf("--base-url=http://%s:%d", stubHost(), port), "--conversation="+conv)

		// The tools, staged the way chat-tools-mixed stages its shell: a folder
		// whose .caos-expr names the image by hash. An agent reaches the same
		// images at caos-std/bash-tool and caos-std/create-squashed-stack.
		m1 := "Add the feature (" + salt + ")\n\nWhy it exists,\nover two lines."
		m2 := "Test the feature (" + salt + ")\n\nOne body line."
		m2b := "Test the feature, reworded (" + salt + ")\n\nA republish: only this message changed."
		squash := use("tu_squash", "run_tool", map[string]any{"path": "tools/squash", "arguments": map[string]any{"plan": "mystack.plan"}})
		squash2 := use("tu_squash2", "run_tool", map[string]any{"path": "tools/squash", "arguments": map[string]any{"plan": "mystack.plan"}})
		publish := func(id, source string) map[string]any {
			return use(id, "publish_source", map[string]any{"source-tree": source,
				"repository": "https://caos-squash-publish.invalid/repo.git", "branch": "feature", "force": true})
		}
		respond(1,
			write("tu_sh", "tools/sh/.caos-expr", "curry --base:hash="+caos("hash", "/cas/args/bash-tool")),
			write("tu_cx", "tools/squash/.caos-expr", "curry --base:hash="+caos("hash", "/cas/args/squash")),
			shell("tu_l1", "mkdir -p mystack && cp -a main mystack/00-base && cp -a main mystack/01-feature && echo feature > mystack/01-feature/feature.txt", "main"))
		respond(2,
			shell("tu_l2", "cp -a mystack/01-feature mystack/02-tests && echo tests > mystack/02-tests/tests.txt", "mystack"),
			write("tu_plan", "mystack.plan", plan(m1, m2)))
		respond(3, squash)
		// The republish: reword one message and squash again over the
		// publish/mystack the first call wrote.
		respond(4, write("tu_plan2", "mystack.plan", plan(m1, m2b)), squash2)
		respond(5, publish("tu_pub", "publish/mystack/01-feature"), publish("tu_dir", "publish/mystack"))
		respond(6, map[string]any{"type": "text", "text": "published"})

		w.Step("the turn")
		admitted := fields(run(tool, "turn", "--repo", repo, "--id", conv, "--user", "tester", "--title", conv,
			"--actor", "tester", "--text", "build a two-layer stack, squash it, republish it, publish it",
			"--secret-hash", caos("hash", "/cas/args/secret-hash"), "--model", "test-model",
			"--configuration", llm, "--source-tree", "main="+base))
		caos("sub-run", admitted["request"])
		done := fields(run(tool, "wait-terminal", "--repo", repo, "--ref", admitted["ref"],
			"--request", admitted["request"], "--timeout-secs", "120"))
		w.True(done["status"] == "idle", "the turn ended with status %q", done["status"])
		head := done["head"]

		w.Step("what the model saw")
		for _, c := range []struct {
			n  int
			id string
		}{{2, "tu_l1"}, {3, "tu_l2"}, {3, "tu_plan"}, {5, "tu_plan2"}} {
			out := seen(c.n, c.id)
			w.True(!strings.HasPrefix(out, "ERROR:"), "call %s failed: %s", c.id, out)
		}
		first, second := seen(4, "tu_squash"), seen(5, "tu_squash2")
		note("create-squashed-stack, as the model saw it:\n%s\nand on the republish:\n%s", first, second)
		w.True(!strings.Contains(first+second, "ERROR:") && !strings.Contains(first+second, "FAILED"), "a squash was refused")
		c1, c2, c2b := reported(first, "01-feature"), reported(first, "02-tests"), reported(second, "02-tests")
		w.True(reported(second, "01-feature") == c1, "an unchanged layer squashed to a different commit on the republish")
		w.True(c2b != c2, "a reworded message did not change its layer's commit")

		// The conversation holds exactly the republished pair at publish/mystack,
		// as gitlinks: the first run's folder replaced, not merged into.
		want := "160000 commit " + c1 + "\t01-feature\n160000 commit " + c2b + "\t02-tests"
		listing := git("ls-tree", head+":publish/mystack")
		w.True(listing == want, "publish/mystack is not exactly {01-feature: C1, 02-tests: C2'}:\n%s", listing)
		w.True(sourceTree(head, "publish/mystack/01-feature") == c1, "publish/mystack/01-feature is not a source tree at C1")

		// Each commit is its layer's tree on the one below, with the plan's message.
		l1, l2 := sourceTree(head, "mystack/01-feature"), sourceTree(head, "mystack/02-tests")
		git("-c", "fetch.negotiationAlgorithm=noop", "fetch", "-q", "caos", c2b, l2)
		w.True(git("rev-parse", c1+"^{tree}") == git("rev-parse", l1+"^{tree}"), "C1 is not layer 1's tree")
		w.True(git("rev-parse", c2b+"^{tree}") == git("rev-parse", l2+"^{tree}"), "C2' is not layer 2's tree")
		w.True(git("rev-parse", c1+"^@") == base, "C1's parents are not [onto]")
		w.True(git("rev-parse", c2b+"^@") == c1, "C2's parents are not [C1]")
		_, body, _ := strings.Cut(git("cat-file", "commit", c2b), "\n\n")
		w.True(body == m2b, "C2's message is not the plan's, verbatim: [%s]", body)
		note("ok: publish/mystack = {01-feature: %s, 02-tests: %s}, each a layer tree on the one below", c1, c2b)

		child, dir := seen(6, "tu_pub"), seen(6, "tu_dir")
		note("publish_source publish/mystack/01-feature: %s", child)
		note("publish_source publish/mystack:            %s", dir)
		w.True(strings.Contains(dir, "requires an existing source gitlink"), "the control call on a plain folder was not refused as one: %s", dir)
		// read_branch's error: the call found the source gitlink and went on to
		// read the remote.
		w.True(strings.Contains(child, "remote branch lookup failed"), "publish_source on the squashed layer failed somewhere unexpected: %s", child)
		note("ok: publish_source took the squashed layer and stopped only at the remote")

		w.Report(narration.String() + "chat-squash-publish: ALL PASS\n")
	})
}
