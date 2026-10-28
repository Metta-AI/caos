// tests/stack-sync — design/stack-publish.md's stack-sync operation, run
// through llm-step's real tool loop with a scripted model, in one turn: build
// a stack with the shell, write its .pr.md descriptions, then call
// caos-std/stack-sync on it.
//
// The repository is a `.invalid` host, so the remote cannot be read: the sync
// must get through every check, stop at reading the trunk, and change
// nothing. The refusals (no trunk, no such directory, an unknown argument)
// must come before the remote is read. The merge logic itself has unit tests
// in llm-step (stack_sync.rs) over a real git store.
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

	invalid = "https://caos-stack-sync.invalid/repo.git"
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

// startStub starts llm-stub on a free port and returns the port.
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

func sync(id string, args map[string]any) map[string]any {
	return use(id, "run_tool", map[string]any{"path": "tools/stack-sync", "arguments": args})
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

func says(claim, out string, phrases ...string) {
	for _, p := range phrases {
		w.True(strings.Contains(out, p), "%s: does not say %q:\n%s", claim, p, out)
	}
}

func sourceTree(head, name string) string {
	return fields(run(tool, "source-tree", "--repo", repo, "--head", head, "--name", name))["commit"]
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
		conv := fmt.Sprintf("%d-%d-stack-sync", time.Now().UnixNano(), rand.Int())
		w.Must(os.WriteFile("/tmp/system.txt", []byte("You are a coding agent."), 0o644))
		caos("put", "/tmp/system.txt", "/cas/system")
		llm := caos("curry", "--base:hash="+caos("hash", "/cas/args/llm-step"), "--system:@=/cas/system",
			"--model=test-model", fmt.Sprintf("--base-url=http://%s:%d", stubHost(), port), "--conversation="+conv)

		respond(1,
			write("tu_sh", "tools/sh/.caos-expr", "curry --base:hash="+caos("hash", "/cas/args/bash-tool")),
			write("tu_sx", "tools/stack-sync/.caos-expr", "curry --base:hash="+caos("hash", "/cas/args/stack-sync")),
			shell("tu_l1", "mkdir -p mystack && cp -a main mystack/00-base && cp -a main mystack/01-feature && echo feature > mystack/01-feature/feature.txt", "main"))
		respond(2,
			shell("tu_l2", "cp -a mystack/01-feature mystack/02-tests && echo tests > mystack/02-tests/tests.txt && cp -a mystack/02-tests mystack/99-dirty", "mystack"),
			write("tu_d1", "mystack/01-feature.pr.md", "Add the feature ("+salt+")\n"),
			write("tu_d2", "mystack/02-tests.pr.md", "Test the feature ("+salt+")\n"))
		respond(3,
			sync("tu_sync", map[string]any{"stack": "mystack", "repository": invalid, "trunk": "main"}),
			sync("tu_dry", map[string]any{"stack": "mystack", "repository": invalid, "trunk": "main", "dry-run": "true"}),
			sync("tu_notrunk", map[string]any{"stack": "mystack", "repository": invalid}),
			sync("tu_nowhere", map[string]any{"stack": "nowhere", "repository": invalid, "trunk": "main"}),
			sync("tu_badarg", map[string]any{"stack": "mystack", "repository": invalid, "trunk": "main", "force": "true"}),
			use("tu_ls", "ls", map[string]any{"path": "mystack"}))
		respond(4, map[string]any{"type": "text", "text": "synced"})

		w.Step("the turn")
		admitted := fields(run(tool, "turn", "--repo", repo, "--id", conv, "--user", "tester", "--title", conv,
			"--actor", "tester", "--text", "build a stack and sync it",
			"--secret-hash", caos("hash", "/cas/args/secret-hash"), "--model", "test-model",
			"--configuration", llm, "--source-tree", "main="+base))
		caos("sub-run", admitted["request"])
		done := fields(run(tool, "wait-terminal", "--repo", repo, "--ref", admitted["ref"],
			"--request", admitted["request"], "--timeout-secs", "240"))
		w.True(done["status"] == "idle", "the turn ended with status %q", done["status"])
		head := done["head"]

		w.Step("the stack was built")
		for _, c := range []struct {
			n  int
			id string
		}{{2, "tu_l1"}, {3, "tu_l2"}, {3, "tu_d1"}, {3, "tu_d2"}} {
			out := seen(c.n, c.id)
			w.True(!strings.HasPrefix(out, "ERROR:"), "call %s failed: %s", c.id, out)
		}

		w.Step("a sync gets through every check and stops at the remote, changing nothing")
		for _, id := range []string{"tu_sync", "tu_dry"} {
			out := seen(4, id)
			note("%s: %s", id, out)
			w.True(strings.HasPrefix(out, "ERROR:"), "%s: an unreachable remote was not an error: %s", id, out)
			says(id, out, "remote branch lookup failed", "for the trunk main", "nothing changed")
		}

		w.Step("refusals, each before the remote is read")
		for _, c := range []struct {
			id   string
			says []string
		}{
			{"tu_notrunk", []string{"no trunk", "pass trunk"}},
			{"tu_nowhere", []string{"stack nowhere"}},
			{"tu_badarg", []string{"invalid stack-sync arguments", "force"}},
		} {
			out := seen(4, c.id)
			note("%s: %s", c.id, out)
			w.True(strings.HasPrefix(out, "ERROR:"), "%s was not refused: %s", c.id, out)
			w.True(!strings.Contains(out, "remote branch lookup"), "%s reached the remote before refusing: %s", c.id, out)
			says(c.id, out, c.says...)
		}

		w.Step("the stack is as it was")
		listing := seen(4, "tu_ls")
		says("ls", listing, "00-base/", "01-feature/", "01-feature.pr.md", "02-tests/", "02-tests.pr.md", "99-dirty/")
		w.True(sourceTree(head, "mystack/00-base") == base, "00-base moved although the remote could not be read")

		w.Report(narration.String() + "stack-sync: ALL PASS\n")
	})
}
