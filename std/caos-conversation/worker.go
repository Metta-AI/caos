// The `caos-conversation` tool's worker. Its DOCS live in the sibling
// `.caos-expr` here-string, not in this header (SPEC, "Tools").
//
// It reads a recorded conversation by the hash of its tip commit, and prints it
// as text for an agent that is reviewing how a session went.
//
// WHERE A CONVERSATION LIVES (design/chat.md, v3/events.rs, v3/paths.rs):
//   - the TIP'S TREE holds the transcript: `.caos/transcript/<n>-<id>.json`, one
//     entry per message, and beside it `<n>-<id>/args-<call>.json`, the arguments
//     of the tool calls that message declared.
//   - the COMMIT MESSAGES hold everything about execution, on the first-parent
//     chain: `<kind>\n\n{"events":[...]}`. A tool's result is NOT in the tree. It
//     is a `payload` event (`{path, bytes:[<u8>...]}`, the bytes as a JSON array
//     of numbers) plus a `tool` event whose `result.observation` names that path.
//
// So this walks the chain root-ward with `caos get-hash` (a worker fetches any
// object by hash from the same git the conversations are recorded in), gathers
// the events oldest-first, and reads the transcript out of the tip's tree.
//
// EVERY OUTCOME IS THE VALUE, as in caos-test-result: a bad hash comes back as
// text the caller can correct, never as a job error.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"

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

// event is one entry of a commit message's `events`. Payload bytes are a JSON
// array of NUMBERS, which encoding/json would read into a []byte only from
// base64 — hence []int.
type event struct {
	Event string          `json:"event"`
	Value json.RawMessage `json:"value"`
}

type payload struct {
	Path  string `json:"path"`
	Bytes []int  `json:"bytes"`
}

type toolResult struct {
	Kind        string `json:"kind"`
	Observation string `json:"observation"`
	Error       string `json:"error"`
	Reason      string `json:"reason"`
}

type call struct {
	ID     string      `json:"id"`
	Status string      `json:"status"`
	Result *toolResult `json:"result"`
}

// A request's ending is in `outcome`: `{kind: idle, interrupted}` or
// `{kind: failed, error}`.
type request struct {
	ID      string `json:"id"`
	Status  string `json:"status"`
	Outcome *struct {
		Kind        string `json:"kind"`
		Interrupted bool   `json:"interrupted"`
		Error       string `json:"error"`
	} `json:"outcome"`
}

type block struct {
	Type      string `json:"type"`
	Text      string `json:"text"`
	Path      string `json:"path"`
	ID        string `json:"id"`
	Name      string `json:"name"`
	Arguments struct {
		Path string `json:"path"`
	} `json:"arguments"`
}

type entry struct {
	Role   string  `json:"role"`
	Round  *uint64 `json:"round"`
	Blocks []block `json:"blocks"`
}

// The conversation as read: later records of the same id replace earlier ones,
// so the walk's oldest-first replay leaves each call and request at its last
// state.
var (
	payloads = map[string][]byte{}
	calls    = map[string]call{}
	requests = map[string]request{}
	reqOrder []string
	notes    []string
)

// splitMessage returns a raw commit's header and message.
func splitMessage(raw []byte) (string, []byte) {
	head, msg, _ := bytes.Cut(raw, []byte("\n\n"))
	return string(head), msg
}

func header(head, key string) string {
	for _, line := range strings.Split(head, "\n") {
		if v, ok := strings.CutPrefix(line, key+" "); ok {
			return v
		}
	}
	return ""
}

// walk reads the first-parent chain from tip to root and returns the tip's
// tree, the number of commits walked, and each commit's events tip-first. A
// failure at the tip is an answer for the caller, returned as the message.
func walk(tip string) (tree string, n int, perCommit [][]event, answer string) {
	for cur := tip; cur != ""; n++ {
		dest := fmt.Sprintf("/cas/c%d", n)
		errText, code := w.Try(script.Exec("caos get-hash " + cur + " " + dest))
		if code != 0 {
			if n == 0 {
				return "", 0, nil, fmt.Sprintf("no object %s on this server:\n\n%s", tip, errText)
			}
			notes = append(notes, "history walk stopped: could not fetch "+cur)
			break
		}
		raw, err := os.ReadFile(dest)
		if err != nil || !bytes.HasPrefix(raw, []byte("tree ")) {
			if n == 0 {
				return "", 0, nil, tip + " is not a commit.\n\n" +
					"Pass the hash of a conversation's tip commit, not of a tree or a file."
			}
			notes = append(notes, "history walk stopped: "+cur+" is not a commit")
			break
		}
		head, msg := splitMessage(raw)
		if n == 0 {
			tree = header(head, "tree")
		}
		parent := header(head, "parent")
		// `<kind>\n\n<canonical JSON>`. The root commit's message is a bare
		// genesis marker with no events, and says so by having no parent.
		_, body, _ := bytes.Cut(msg, []byte("\n\n"))
		var rec struct {
			Events []event `json:"events"`
		}
		if err := json.Unmarshal(body, &rec); err != nil {
			if parent != "" {
				end := string(msg)
				if len(end) > 300 {
					end = end[len(end)-300:]
				}
				notes = append(notes, fmt.Sprintf("commit %s: no readable event record; message ends: %s",
					cur, strings.ReplaceAll(end, "\n", "|")))
			}
		}
		perCommit = append(perCommit, rec.Events)
		cur = parent
	}
	return tree, n, perCommit, ""
}

// replay applies events oldest-first.
func replay(perCommit [][]event) {
	for i := len(perCommit) - 1; i >= 0; i-- {
		for _, e := range perCommit[i] {
			switch e.Event {
			case "payload":
				var p payload
				w.Must(json.Unmarshal(e.Value, &p))
				b := make([]byte, len(p.Bytes))
				for j, v := range p.Bytes {
					b[j] = byte(v)
				}
				payloads[p.Path] = b
			case "tool":
				var c call
				w.Must(json.Unmarshal(e.Value, &c))
				calls[c.ID] = c
			case "request":
				var r request
				w.Must(json.Unmarshal(e.Value, &r))
				if _, seen := requests[r.ID]; !seen {
					reqOrder = append(reqOrder, r.ID)
				}
				requests[r.ID] = r
			}
		}
	}
}

// resolve reads a record's string field: the text of the payload it names,
// or the string itself when it names none.
func resolve(ref string) string {
	b, ok := payloads[ref]
	if !ok {
		return ref
	}
	var r struct {
		Content json.RawMessage `json:"content"`
		IsError bool            `json:"is_error"`
	}
	if json.Unmarshal(b, &r) != nil || r.Content == nil {
		return string(b)
	}
	prefix := ""
	if r.IsError {
		prefix = "[is_error]\n"
	}
	var s string
	if json.Unmarshal(r.Content, &s) == nil {
		return prefix + s
	}
	var parts []struct {
		Type string `json:"type"`
		Text string `json:"text"`
	}
	if json.Unmarshal(r.Content, &parts) != nil {
		return prefix + string(r.Content)
	}
	var texts []string
	for _, p := range parts {
		if p.Type == "text" {
			texts = append(texts, p.Text)
		} else {
			texts = append(texts, "["+p.Type+" block]")
		}
	}
	return prefix + strings.Join(texts, "\n")
}

// trunc cuts s to width characters, 0 meaning no cut.
func trunc(s string, width int, id string) string {
	s = strings.TrimRight(s, "\n")
	r := []rune(s)
	if width == 0 || len(r) <= width {
		return s
	}
	return fmt.Sprintf("%s… [%d more chars; pass call=%s to see all]", string(r[:width]), len(r)-width, id)
}

func indent(s string) {
	for _, line := range strings.Split(s, "\n") {
		say("      %s", line)
	}
}

func renderCall(b block, width int) {
	status := "no result recorded"
	c, ok := calls[b.ID]
	if ok {
		status = c.Status
	}
	say("  [call %s] %s -> %s", b.ID, b.Name, status)
	args, err := os.ReadFile(filepath.Join("/cas/tree", b.Arguments.Path))
	switch {
	case err != nil:
		say("    args: (not in the transcript: %s)", b.Arguments.Path)
	case width == 0:
		var pretty bytes.Buffer
		if json.Indent(&pretty, args, "", "  ") != nil {
			pretty.Reset()
			pretty.Write(args)
		}
		say("    args:")
		indent(strings.TrimRight(pretty.String(), "\n"))
	default:
		var compact bytes.Buffer
		if json.Compact(&compact, args) != nil {
			compact.Reset()
			compact.Write(args)
		}
		say("    args: %s", trunc(compact.String(), width, b.ID))
	}
	if !ok || c.Result == nil {
		return
	}
	ref := c.Result.Observation + c.Result.Error + c.Result.Reason
	if ref == "" {
		return
	}
	say("    result (%s):", c.Result.Kind)
	indent(trunc(resolve(ref), width, b.ID))
}

// renderEntries prints the transcript, or with only set, the one entry and
// call it names.
func renderEntries(files []string, width int, only string, o opts) (shown int) {
	for _, f := range files {
		var e entry
		w.Must(json.Unmarshal(w.Check(os.ReadFile(f)), &e))
		if only != "" && !hasCall(e, only) {
			continue
		}
		ord, _, _ := strings.Cut(filepath.Base(f), "-")
		idx := int(w.Check(strconv.ParseUint(ord, 10, 64)))
		if only == "" && !o.wants(idx, e) {
			continue
		}
		shown++
		// With only=failed the assistant's closing text is left out too: the
		// failing calls are what was asked for.
		quiet := o.only == "failed" && e.Role != "user"
		round := ""
		if e.Round != nil {
			round = fmt.Sprintf(" (round %d)", *e.Round)
		}
		say("[%d] %s%s", w.Check(strconv.ParseUint(ord, 10, 64)), strings.ToUpper(e.Role), round)
		for _, b := range e.Blocks {
			switch b.Type {
			case "text":
				if only == "" && !quiet {
					say("%s", cutMsg(b.Text, o.msgWidth, idx))
				}
			case "payload":
				if only == "" && !quiet {
					if text, err := os.ReadFile(filepath.Join("/cas/tree", b.Path)); err == nil {
						say("%s", cutMsg(string(text), o.msgWidth, idx))
					} else {
						say("[payload block: %s]", b.Path)
					}
				}
			case "tool_use":
				if (only == "" && (o.only != "failed" || isFailed(b.ID))) || b.ID == only {
					renderCall(b, width)
				}
			}
		}
		say("")
	}
	return shown
}

// opts selects which transcript entries print and how message text is cut.
// from and to are entry numbers, as printed in `[n]`, both inclusive; to < 0
// means no upper bound.
type opts struct {
	width, msgWidth, from, to int
	only                      string // "", "user", "assistant" or "failed"
}

func (o opts) wants(idx int, e entry) bool {
	if idx < o.from || (o.to >= 0 && idx > o.to) {
		return false
	}
	switch o.only {
	case "user":
		return e.Role == "user"
	case "assistant":
		return e.Role != "user"
	case "failed":
		return e.Role == "user" || hasFailed(e)
	}
	return true
}

// cutMsg cuts message text to width characters, 0 meaning no cut.
func cutMsg(s string, width, idx int) string {
	r := []rune(s)
	if width == 0 || len(r) <= width {
		return s
	}
	return fmt.Sprintf("%s… [%d more chars; pass from=%d to=%d msg_width=0 to see all]",
		string(r[:width]), len(r)-width, idx, idx)
}

// isFailed is true for a call that did not complete, or that completed with an
// error result (a tool's own `[is_error]` still has status complete).
func isFailed(id string) bool {
	c, ok := calls[id]
	if !ok {
		return false
	}
	if c.Status != "complete" {
		return true
	}
	if c.Result == nil {
		return false
	}
	ref := c.Result.Observation + c.Result.Error + c.Result.Reason
	return ref != "" && strings.HasPrefix(resolve(ref), "[is_error]")
}

func hasFailed(e entry) bool {
	for _, b := range e.Blocks {
		if b.Type == "tool_use" && isFailed(b.ID) {
			return true
		}
	}
	return false
}

// overview reads every entry once for the header: calls per tool, and the
// numbers of the entries holding a failed call.
func overview(files []string) (tools, failed string) {
	counts := map[string]int{}
	var bad []string
	for _, f := range files {
		var e entry
		if json.Unmarshal(w.Check(os.ReadFile(f)), &e) != nil {
			continue
		}
		ord, _, _ := strings.Cut(filepath.Base(f), "-")
		n, _ := strconv.Atoi(ord)
		hit := false
		for _, b := range e.Blocks {
			if b.Type != "tool_use" {
				continue
			}
			counts[b.Name]++
			hit = hit || isFailed(b.ID)
		}
		if hit {
			bad = append(bad, strconv.Itoa(n))
		}
	}
	names := make([]string, 0, len(counts))
	for n := range counts {
		names = append(names, n)
	}
	sort.Strings(names)
	parts := make([]string, len(names))
	for i, n := range names {
		parts[i] = fmt.Sprintf("%s %d", n, counts[n])
	}
	return strings.Join(parts, ", "), strings.Join(bad, ", ")
}

func hasCall(e entry, id string) bool {
	for _, b := range e.Blocks {
		if b.Type == "tool_use" && b.ID == id {
			return true
		}
	}
	return false
}

// fetch materializes a path under the tip's tree when the tree has it.
func fetch(path string, recursive bool) bool {
	if _, err := os.Stat(path); err != nil {
		return false
	}
	flag := ""
	if recursive {
		flag = "-r "
	}
	w.Do(script.Exec("caos get " + flag + path))
	return true
}

func main() {
	w.Main(func() {
		read()
		w.Report(out.String())
	})
}

// read renders the conversation into out. Every answer, including a hash that
// names nothing, is a return here rather than a failure.
func read() {
	hash := optArg("hash", "")
	if !regexp.MustCompile(`^[0-9a-f]{40}$`).MatchString(hash) {
		say("not a hash: %s\n\nPass the 40-character hash of the commit at the tip of a conversation.", hash)
		return
	}
	want := optArg("call", "")
	width, err := strconv.Atoi(optArg("width", "300"))
	if err != nil || width < 0 {
		width = 300
	}
	o := opts{width: width, to: -1, only: optArg("only", "")}
	if o.msgWidth, err = strconv.Atoi(optArg("msg_width", "0")); err != nil || o.msgWidth < 0 {
		o.msgWidth = 0
	}
	if o.from, err = strconv.Atoi(optArg("from", "0")); err != nil || o.from < 0 {
		o.from = 0
	}
	if t, err := strconv.Atoi(optArg("to", "-1")); err == nil && t >= 0 {
		o.to = t
	}
	switch o.only {
	case "", "user", "assistant", "failed":
	default:
		say("only=%q is not one of user, assistant, failed.", o.only)
		return
	}

	tree, n, perCommit, answer := walk(hash)
	if answer != "" {
		say("%s", answer)
		return
	}
	replay(perCommit)

	w.Do(script.Exec("caos get-hash " + tree + " /cas/tree"))
	title := ""
	if fetch("/cas/tree/.caos", false) {
		if fetch("/cas/tree/.caos/title", false) {
			title = strings.TrimRight(string(w.Check(os.ReadFile("/cas/tree/.caos/title"))), "\n")
		}
		fetch("/cas/tree/.caos/transcript", true)
	}
	files, _ := filepath.Glob("/cas/tree/.caos/transcript/*.json")
	sort.Strings(files)

	if want != "" {
		if _, ok := calls[want]; ok {
			renderEntries(files, 0, want, opts{})
			return
		}
		say("no call %s in this conversation.\n\nCalls it recorded (ids as the default listing prints them):", want)
		ids := make([]string, 0, len(calls))
		for id := range calls {
			ids = append(ids, id)
		}
		sort.Strings(ids)
		for _, id := range ids {
			say("  %s", id)
		}
		return
	}

	if title != "" {
		say("Conversation: %s", title)
	} else {
		say("Conversation")
	}
	say("tip %s (%d commits walked, %d transcript entries)", hash, n, len(files))
	say("calls: %s", callSummary())
	say("")
	say("NOT RECORDED: the model's text and reasoning between tool calls, calls to tools")
	say("that are not caos's (and calls that never reached one), and timings. Gaps in the")
	say("story below may be unrecorded steps.")
	say("")
	if len(notes) > 0 {
		say("Commits skipped while reading the history:")
		for _, note := range notes {
			say("  %s", note)
		}
		say("")
	}
	say("----")
	renderEntries(files, width, "")

	var bad []string
	for _, id := range reqOrder {
		r := requests[id]
		line := id + ": " + r.Status
		switch {
		case r.Outcome != nil && r.Outcome.Interrupted:
			line += " interrupted"
		case r.Outcome != nil && r.Outcome.Error != "":
			line += " — " + resolve(r.Outcome.Error)
		case r.Status == "idle":
			continue
		}
		bad = append(bad, line)
	}
	if len(bad) > 0 {
		say("----")
		say("Requests that did not end idle:")
		for _, line := range bad {
			say("  %s", line)
		}
	}
}

// callSummary counts calls by status, as `complete 12, failed 1`.
func callSummary() string {
	if len(calls) == 0 {
		return "none"
	}
	counts := map[string]int{}
	for _, c := range calls {
		counts[c.Status]++
	}
	statuses := make([]string, 0, len(counts))
	for s := range counts {
		statuses = append(statuses, s)
	}
	sort.Strings(statuses)
	parts := make([]string, len(statuses))
	for i, s := range statuses {
		parts[i] = fmt.Sprintf("%s %d", s, counts[s])
	}
	return strings.Join(parts, ", ")
}
