// The `read-trace` tool's worker. Its DOCS live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "Tools").
//
// It reads a run's trace by the hash of its ArgTree. The trace is NOT a git
// structure: the server keeps one redis list per ArgTree (`caos:trace:<hash>`,
// SPEC "Tracing") and renders it at `GET /status/<hash>?all=1`. A trace key
// carries no cache namespace, so a dev stack's records are served by the host
// server too — which is why a `caos-test` run is readable from here. Only the
// perf data workers leave at /cas/out-trace is a git object, fetched by hash.
//
// Every job has CAOS_SERVER_URL, so this needs nothing from the `caos` client.
// The URL is used as given: design/faster-tests.md traces seconds of stall to a
// hostname where the address it already carries is instant.
//
// EVERY OUTCOME IS THE VALUE, as in caos-test-result: a bad hash, an empty
// trace or an unreachable server comes back as text the caller can read, never
// as a job error — this tool is called when something already went wrong or ran
// slowly.
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"caos/w"

	"github.com/bitfield/script"
)

var out strings.Builder

func say(format string, a ...any) { fmt.Fprintf(&out, format+"\n", a...) }

// optArg reads a curried argument, or def when it was never bound.
func optArg(name, def string) string {
	path := "/cas/args/" + name
	if _, err := os.Stat(path); err != nil {
		return def
	}
	w.Do(script.Exec("caos get " + path))
	return strings.TrimSpace(string(w.Check(os.ReadFile(path))))
}

// node is one node of the server's `/status` JSON (status.rs `Node`). Times are
// milliseconds relative to the root request; work reused from an earlier run
// reads negative. A missing time is a pointer left nil.
type node struct {
	Name      string   `json:"name"`
	Requested *int64   `json:"requested"`
	Started   *int64   `json:"started"`
	Ended     *int64   `json:"ended"`
	OutTrace  []string `json:"out_trace"`
	Reused    bool     `json:"reused"`
	Children  []node   `json:"children"`
}

// flat is a node with its depth, for listing.
type flat struct {
	node
	depth int
}

func flatten(n node, depth int, into *[]flat) {
	*into = append(*into, flat{n, depth})
	for _, c := range n.Children {
		flatten(c, depth+1, into)
	}
}

func (f flat) duration() (int64, bool) {
	if f.Started == nil || f.Ended == nil {
		return 0, false
	}
	return *f.Ended - *f.Started, true
}

func ms(p *int64) string {
	if p == nil {
		return "-"
	}
	return strconv.FormatInt(*p, 10)
}

// fetchStatus asks the server for the trace. A nil body with no error means the
// server has nothing recorded.
func fetchStatus(hash string) ([]byte, string) {
	base := strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
	if base == "" {
		return nil, "CAOS_SERVER_URL is not set in this worker, so there is no server to ask."
	}
	// No proxy: the server is on the container network, and a proxy variable
	// would send it somewhere that cannot resolve it.
	client := &http.Client{
		Timeout:   60 * time.Second,
		Transport: &http.Transport{Proxy: nil},
	}
	resp, err := client.Get(base + "/status/" + hash + "?all=1")
	if err != nil {
		return nil, "could not reach the server at " + base + ":\n" + err.Error()
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, "reading the server's reply failed:\n" + err.Error()
	}
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Sprintf("the server answered %s for /status/%s:\n%s",
			resp.Status, hash, strings.TrimSpace(string(body)))
	}
	text := strings.TrimSpace(string(body))
	if text == "" || text == "null" {
		return nil, ""
	}
	return []byte(text), ""
}

func main() {
	w.Main(func() {
		read()
		w.Report(out.String())
	})
}

func read() {
	hash := optArg("hash", "")
	if !regexp.MustCompile(`^[0-9a-f]{40}$`).MatchString(hash) {
		say("not a hash: %s\n\nPass the 40-character ArgTree hash from the `caos-test` report's \"full trace\" line.", hash)
		return
	}
	top, err := strconv.Atoi(optArg("top", "25"))
	if err != nil || top < 0 {
		top = 25
	}
	depth, err := strconv.Atoi(optArg("depth", "3"))
	if err != nil || depth < 0 {
		depth = 3
	}
	perf := optArg("perf", "") != ""
	wantJSON := optArg("json", "") != ""

	body, problem := fetchStatus(hash)
	if problem != "" {
		say("%s", problem)
		return
	}
	if body == nil {
		say("nothing recorded under %s\n\nTrace records live in the server's redis, one per ArgTree. This hash has none: it never ran on this server, or it is not the ArgTree hash (the report prints it in its \"full trace\" line).", hash)
		return
	}
	var root node
	if err := json.Unmarshal(body, &root); err != nil {
		say("the server's trace for %s is not the JSON this tool reads (%v). Raw reply:\n\n%s", hash, err, body)
		return
	}

	var all []flat
	flatten(root, 0, &all)
	reused, unfinished := 0, 0
	for _, f := range all {
		if f.Reused {
			reused++
		}
		if f.Ended == nil {
			unfinished++
		}
	}
	say("trace of %s", hash)
	say("nodes: %d, reused: %d, unfinished: %d", len(all), reused, unfinished)
	if root.Ended != nil {
		say("root: %s ms end to end", ms(root.Ended))
	}
	say("")

	// The expensive nodes. A promising node's `ended` covers its whole subtree,
	// so its duration includes its children's: read the list as "where did the
	// time sit", not as a partition of it.
	var timed []flat
	for _, f := range all {
		if _, ok := f.duration(); ok {
			timed = append(timed, f)
		}
	}
	sort.SliceStable(timed, func(i, j int) bool {
		a, _ := timed[i].duration()
		b, _ := timed[j].duration()
		return a > b
	})
	if top < len(timed) {
		timed = timed[:top]
	}
	say("---- slowest %d nodes (ms; start/end relative to the root's request; a node's duration includes its children) ----", len(timed))
	say("%8s %8s %8s  %s", "dur", "wait", "start", "name")
	for _, f := range timed {
		d, _ := f.duration()
		wait := "-"
		if f.Requested != nil && f.Started != nil {
			wait = strconv.FormatInt(*f.Started-*f.Requested, 10)
		}
		mark := ""
		if f.Reused {
			mark = "  [reused]"
		}
		say("%8d %8s %8s  %s%s", d, wait, ms(f.Started), f.Name, mark)
	}
	say("")

	say("---- tree (requested/started/ended ms) ----")
	for _, f := range all {
		mark := ""
		if f.Reused {
			mark = " [reused]"
		}
		say("%s%s  %s/%s/%s%s", strings.Repeat("  ", f.depth), f.Name,
			ms(f.Requested), ms(f.Started), ms(f.Ended), mark)
	}
	say("")

	if perf {
		perfData(all)
	}

	var pretty []byte
	var anyJSON any
	if json.Unmarshal(body, &anyJSON) == nil {
		pretty, _ = json.MarshalIndent(anyJSON, "", "  ")
	} else {
		pretty = body
	}
	say("---- full trace JSON ----")
	say("%s", pretty)
}

// perfData prints every distinct out-trace object the tree names. They are git
// objects workers left at /cas/out-trace, fetched by hash.
func perfData(all []flat) {
	seen := map[string]bool{}
	var oids []string
	for _, f := range all {
		for _, oid := range f.OutTrace {
			if !seen[oid] {
				seen[oid] = true
				oids = append(oids, oid)
			}
		}
	}
	if len(oids) == 0 {
		say("---- perf data: none ----")
		say("No node in this trace has an out-trace: no worker in this run left perf data at")
		say("/cas/out-trace (most don't; tests/tracing's driver does).")
		say("")
		return
	}
	say("---- perf data: %d out-trace object(s) ----", len(oids))
	for i, oid := range oids {
		say("")
		say("-- out-trace %s --", oid)
		dest := fmt.Sprintf("/cas/ot%d", i)
		if errText, code := w.Try(script.Exec("caos get-hash " + oid + " " + dest)); code != 0 {
			say("could not fetch: %s", strings.TrimSpace(errText))
			continue
		}
		info, err := os.Stat(dest)
		if err != nil {
			say("could not read %s: %v", dest, err)
			continue
		}
		if !info.IsDir() {
			text, _ := os.ReadFile(dest)
			say("%s", strings.TrimRight(string(text), "\n"))
			continue
		}
		w.Do(script.Exec("caos get -r " + dest))
		_ = filepath.Walk(dest, func(p string, fi os.FileInfo, err error) error {
			if err != nil || fi.IsDir() {
				return nil
			}
			text, _ := os.ReadFile(p)
			say("## %s", strings.TrimPrefix(p, dest+"/"))
			say("%s", strings.TrimRight(string(text), "\n"))
			return nil
		})
	}
	say("")
}
