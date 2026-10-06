// Reference inner actor (std/actor/README.md): a key-value store. A pure function
// of (state tree, message) -> {state, reply}. Messages are idempotent:
//
//	put <key> <value>   set key (applying twice is the same as once)
//	get <key>           reply with the value, state unchanged
//	getcheck <key>      like get, and fail if any OTHER entry's content was
//	                    materialized: the inner sees the state lazily
//
// It runs as an actor's inner on std/go, and probe.go runs it too: probe copies
// this file into the prelude module as its own command, so keep it a single
// self-contained `package main`.
package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"caos/w"
)

func caos(args ...string) {
	cmd := exec.Command("caos", args...)
	cmd.Stderr = os.Stderr
	w.True(cmd.Run() == nil, "caos %s failed", strings.Join(args, " "))
}

func main() {
	w.Main(func() {
		caos("get", "/cas/args/message")
		line := strings.TrimRight(string(w.Check(os.ReadFile("/cas/args/message"))), "\n")
		fields := strings.SplitN(line, " ", 3)
		w.True(len(fields) >= 2, "kv: bad message: %q", line)
		op, key, value := fields[0], fields[1], ""
		if len(fields) == 3 {
			value = strings.TrimSpace(fields[2])
		}
		w.True(key != "" && !strings.Contains(key, "/") && !strings.HasPrefix(key, "."), "kv: bad key: %s", key)

		// List the state's entries without reading their content.
		caos("get", "/cas/args/state")
		entries := w.Check(os.ReadDir("/cas/args/state"))

		w.Must(os.RemoveAll("/tmp/out"))
		w.Must(os.MkdirAll("/tmp/out", 0o755))
		switch op {
		case "put":
			w.Must(os.Mkdir("/tmp/out/state", 0o755))
			for _, e := range entries {
				if e.Name() != key {
					w.Must(os.Symlink(filepath.Join("/cas/args/state", e.Name()), filepath.Join("/tmp/out/state", e.Name())))
				}
			}
			w.Must(os.WriteFile(filepath.Join("/tmp/out/state", key), []byte(value+"\n"), 0o644))
			w.Must(os.WriteFile("/tmp/out/reply", []byte("ok\n"), 0o644))
		case "get", "getcheck":
			w.Must(os.Symlink("/cas/args/state", "/tmp/out/state"))
			reply := []byte{}
			if _, err := os.Lstat(filepath.Join("/cas/args/state", key)); err == nil {
				caos("get", filepath.Join("/cas/args/state", key))
				reply = w.Check(os.ReadFile(filepath.Join("/cas/args/state", key)))
			}
			w.Must(os.WriteFile("/tmp/out/reply", reply, 0o644))
			if op == "getcheck" {
				for _, e := range entries {
					if e.Name() == key {
						continue
					}
					info := w.Check(os.Stat(filepath.Join("/cas/args/state", e.Name())))
					w.True(info.Size() == 0, "kv: %s was materialized by a read of %s", e.Name(), key)
				}
			}
		default:
			w.True(false, "kv: unknown op: %s", op)
		}
		caos("put", "/tmp/out", "/cas/out")
	})
}
