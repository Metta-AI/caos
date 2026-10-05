// Throwaway measurement: how long does reading an rgrep result take?
// Mirrors std/llm-step/src/tools.rs grep_result_block: one `caos get` per
// directory and per matching file, depth-first, sequential, until 100000 bytes.
package main

import (
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"

	"caos/w"
)

var out strings.Builder

func say(f string, a ...any) { fmt.Fprintf(&out, f+"\n", a...) }

var gets int

func caosGet(p string) {
	gets++
	_ = exec.Command("caos", "get", p).Run()
}

var (
	files, dirs, matchBytes, rendered int
	overflow                          int
)

const max = 100000

func walk(dir string) {
	caosGet(dir)
	dirs++
	es, _ := os.ReadDir(dir)
	sort.Slice(es, func(i, j int) bool { return es[i].Name() < es[j].Name() })
	for _, e := range es {
		p := filepath.Join(dir, e.Name())
		if fi, err := os.Stat(p); err == nil && fi.IsDir() {
			walk(p)
			continue
		}
		files++
		if rendered >= max {
			overflow++
			continue
		}
		caosGet(p)
		b, _ := os.ReadFile(p)
		matchBytes += len(b)
		rendered += len(b) + len(e.Name())
	}
}

func main() {
	w.Main(func() {
		reqB, _ := os.ReadFile("/cas/args/req")
		_ = exec.Command("caos", "get", "/cas/args/req").Run()
		reqB, _ = os.ReadFile("/cas/args/req")
		req := strings.TrimSpace(string(reqB))
		base := strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")

		t0 := time.Now()
		c := &http.Client{Timeout: 10 * time.Minute, Transport: &http.Transport{Proxy: nil}}
		resp, err := c.Get(base + "/run?req=" + req)
		if err != nil {
			say("GET /run failed: %v", err)
			w.Report(out.String())
			return
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		say("GET /run?req=%s -> %s in %v", req, resp.Status, time.Since(t0))
		hash := regexp.MustCompile(`[0-9a-f]{40}`).FindString(string(body))
		if len(body) < 300 {
			say("body: %q", string(body))
		}
		if hash == "" {
			w.Report(out.String())
			return
		}

		t1 := time.Now()
		if err := exec.Command("caos", "get-hash", hash, "/cas/res").Run(); err != nil {
			say("get-hash %s failed: %v", hash, err)
			w.Report(out.String())
			return
		}
		say("get-hash result %s: %v", hash, time.Since(t1))

		t2 := time.Now()
		walk("/cas/res")
		say("walk (render, sequential `caos get` per dir/file): %v", time.Since(t2))
		say("  caos get calls: %d, dirs: %d, matching files: %d, match bytes read: %d, files not rendered (over %d): %d",
			gets, dirs, files, matchBytes, max, overflow)
		// The alternative: one recursive get, then plain filesystem reads.
		_ = exec.Command("caos", "get-hash", hash, "/cas/res2").Run()
		t3 := time.Now()
		err = exec.Command("caos", "get", "-r", "/cas/res2").Run()
		say("one `caos get -r` of the whole result: %v (err=%v)", time.Since(t3), err)
		t4 := time.Now()
		n, bytes := 0, 0
		_ = filepath.Walk("/cas/res2", func(p string, fi os.FileInfo, e error) error {
			if e == nil && !fi.IsDir() {
				b, _ := os.ReadFile(p)
				n++
				bytes += len(b)
			}
			return nil
		})
		say("then reading %d files (%d bytes) from disk: %v", n, bytes, time.Since(t4))
		w.Report(out.String())
	})
}
