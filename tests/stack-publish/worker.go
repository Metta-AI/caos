// tests/stack-publish — design/stack-publish.md's operation, run through
// llm-step's real tool loop with a scripted model, in one turn: build stacks
// with the shell, write their .pr.md descriptions, then call
// caos-std/stack-publish on each.
//
// The repository is a `.invalid` host, so no push happens: the good stack
// must get through every check and stop at reading the remote, and say that
// nothing was pushed. A dry run must report the plan: the layers, their
// branches with and without a prefix, the chained bases, the titles, and not
// the undescribed 99-dirty. The refusals (a described layer above an
// undescribed one, a layer that does not contain the one below it, a
// description with no layer, a stack with no descriptions, no trunk, no such
// directory) must come before the remote is read.
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

	invalid = "https://caos-stack-publish.invalid/repo.git"
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
// prints the operation's actual output.
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

func shell(id, cmd string, paths ...string) map[string]any {
	return use(id, "run_tool", map[string]any{"path": "tools/sh",
		"arguments": map[string]any{"cmd": cmd, "paths": paths}})
}

func write(id, path, content string) map[string]any {
	return use(id, "write", map[string]any{"file-path": path, "content": content})
}

// publish is one stack-publish call, reached by path like any std tool. Its
// arguments are strings, like every tool's.
func publish(id string, args map[string]any) map[string]any {
	return use(id, "run_tool", map[string]any{"path": "tools/stack-publish", "arguments": args})
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

// says checks that a result carries every phrase, failing with the whole text.
func says(claim, out string, phrases ...string) {
	for _, p := range phrases {
		w.True(strings.Contains(out, p), "%s: does not say %q:\n%s", claim, p, out)
	}
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
		conv := fmt.Sprintf("%d-%d-stack-publish", time.Now().UnixNano(), rand.Int())
		w.Must(os.WriteFile("/tmp/system.txt", []byte("You are a coding agent."), 0o644))
		caos("put", "/tmp/system.txt", "/cas/system")
		llm := caos("curry", "--base:hash="+caos("hash", "/cas/args/llm-step"), "--system:@=/cas/system",
			"--model=test-model", fmt.Sprintf("--base-url=http://%s:%d", stubHost(), port), "--conversation="+conv)

		// The tools, staged the way chat-tools-mixed stages its shell: a folder
		// whose .caos-expr names the image by hash. An agent reaches the same
		// images at caos-std/bash-tool and caos-std/stack-publish.
		title1 := "Add the feature (" + salt + ")"
		title2 := "Test the feature (" + salt + ")"
		respond(1,
			write("tu_sh", "tools/sh/.caos-expr", "curry --base:hash="+caos("hash", "/cas/args/bash-tool")),
			write("tu_px", "tools/stack-publish/.caos-expr", "curry --base:hash="+caos("hash", "/cas/args/stack-publish")),
			// The good stack: each layer copied from the one below, then edited,
			// so each descends from the one below; 99-dirty on top, undescribed.
			shell("tu_l1", "mkdir -p mystack && cp -a main mystack/00-base && cp -a main mystack/01-feature && echo feature > mystack/01-feature/feature.txt", "main"))
		respond(2,
			shell("tu_l2", "cp -a mystack/01-feature mystack/02-tests && echo tests > mystack/02-tests/tests.txt && cp -a mystack/02-tests mystack/99-dirty && echo scratch > mystack/99-dirty/scratch.txt", "mystack"),
			write("tu_d1", "mystack/01-feature.pr.md", title1+"\n\nWhy it exists,\nover two lines.\n"),
			write("tu_d2", "mystack/02-tests.pr.md", "# "+title2+"\n"),
			// The refusals' stacks. `gap`: a described layer above an undescribed
			// one. `unmerged`: 02-b from main, not from 01-a. `orphan`: a
			// description with no gitlink. `bare`: no descriptions at all.
			shell("tu_l3", "for s in gap unmerged orphan bare; do mkdir -p $s && cp -a main $s/00-base && cp -a main $s/01-a && echo a > $s/01-a/a.txt; done"+
				" && cp -a gap/01-a gap/02-b && echo b > gap/02-b/b.txt"+
				" && cp -a main unmerged/02-b && echo b > unmerged/02-b/b.txt", "main"),
			write("tu_d3", "gap/02-b.pr.md", "B\n"),
			write("tu_d4", "unmerged/01-a.pr.md", "A\n"),
			write("tu_d5", "unmerged/02-b.pr.md", "B\n"),
			write("tu_d6", "orphan/01-a.pr.md", "A\n"),
			write("tu_d7", "orphan/03-c.pr.md", "C\n"))
		respond(3,
			publish("tu_dry", map[string]any{"stack": "mystack", "repository": invalid, "trunk": "main", "dry-run": "true"}),
			publish("tu_prefix", map[string]any{"stack": "mystack", "repository": invalid, "trunk": "main", "dry-run": "true", "branch-prefix": "me/"}),
			publish("tu_pub", map[string]any{"stack": "mystack", "repository": invalid, "trunk": "main"}),
			publish("tu_notrunk", map[string]any{"stack": "mystack", "repository": invalid}),
			publish("tu_norepo", map[string]any{"stack": "mystack", "trunk": "main"}),
			publish("tu_gap", map[string]any{"stack": "gap", "repository": invalid, "trunk": "main"}),
			publish("tu_unmerged", map[string]any{"stack": "unmerged", "repository": invalid, "trunk": "main"}),
			publish("tu_orphan", map[string]any{"stack": "orphan", "repository": invalid, "trunk": "main"}),
			publish("tu_bare", map[string]any{"stack": "bare", "repository": invalid, "trunk": "main"}),
			publish("tu_nowhere", map[string]any{"stack": "nowhere", "repository": invalid, "trunk": "main"}),
			publish("tu_badarg", map[string]any{"stack": "mystack", "repository": invalid, "trunk": "main", "squash": "true"}))
		respond(4, map[string]any{"type": "text", "text": "published"})

		w.Step("the turn")
		admitted := fields(run(tool, "turn", "--repo", repo, "--id", conv, "--user", "tester", "--title", conv,
			"--actor", "tester", "--text", "build stacks and publish them",
			"--secret-hash", caos("hash", "/cas/args/secret-hash"),
			"--configuration", llm, "--source-tree", "main="+base))
		caos("sub-run", admitted["request"])
		done := fields(run(tool, "wait-terminal", "--repo", repo, "--ref", admitted["ref"],
			"--request", admitted["request"], "--timeout-secs", "240"))
		w.True(done["status"] == "idle", "the turn ended with status %q", done["status"])

		w.Step("the stacks were built")
		for _, c := range []struct {
			n  int
			id string
		}{{2, "tu_l1"}, {3, "tu_l2"}, {3, "tu_d1"}, {3, "tu_d2"}, {3, "tu_l3"}, {3, "tu_d3"}, {3, "tu_d7"}} {
			out := seen(c.n, c.id)
			w.True(!strings.HasPrefix(out, "ERROR:"), "call %s failed: %s", c.id, out)
		}

		w.Step("a dry run reports the plan")
		dry := seen(4, "tu_dry")
		note("stack-publish dry-run, as the model saw it:\n%s", dry)
		w.True(!strings.HasPrefix(dry, "ERROR:"), "the dry run was refused: %s", dry)
		says("dry run", dry, "dry run", "repository "+invalid, "trunk main",
			"01-feature  feature  ", "  base main  PR title: "+title1,
			"02-tests  tests  ", "  base feature  PR title: "+title2)
		w.True(!strings.Contains(dry, "99-dirty"), "the undescribed 99-dirty was planned: %s", dry)
		prefixed := seen(4, "tu_prefix")
		says("prefixed dry run", prefixed, "01-feature  me/feature  ", "  base main  ", "02-tests  me/tests  ", "  base me/feature  ")

		w.Step("a publish gets through every check and stops at the remote")
		pub := seen(4, "tu_pub")
		note("stack-publish against an unreachable remote: %s", pub)
		w.True(strings.HasPrefix(pub, "ERROR:"), "an unreachable remote was not an error: %s", pub)
		says("publish", pub, "remote branch lookup failed", "for branch feature", "nothing was pushed")

		w.Step("refusals, each before the remote is read")
		for _, c := range []struct {
			id   string
			says []string
		}{
			{"tu_notrunk", []string{"no trunk", "pass trunk"}},
			{"tu_norepo", []string{"no repository", "pass repository"}},
			{"tu_gap", []string{"gap/02-b has a description but gap/01-a below it has none", "gap/01-a.pr.md"}},
			{"tu_unmerged", []string{"unmerged/02-b does not contain unmerged/01-a", "caos-std/merge", `"theirs": "unmerged/01-a"`}},
			{"tu_orphan", []string{"orphan/03-c.pr.md describes no layer"}},
			{"tu_bare", []string{"no layer in bare has a .pr.md description"}},
			{"tu_nowhere", []string{"stack nowhere"}},
			{"tu_badarg", []string{"invalid stack-publish arguments", "squash"}},
		} {
			out := seen(4, c.id)
			note("%s: %s", c.id, out)
			w.True(strings.HasPrefix(out, "ERROR:"), "%s was not refused: %s", c.id, out)
			w.True(!strings.Contains(out, "remote branch lookup"), "%s reached the remote before refusing: %s", c.id, out)
			says(c.id, out, c.says...)
		}

		w.Report(narration.String() + "stack-publish: ALL PASS\n")
	})
}
