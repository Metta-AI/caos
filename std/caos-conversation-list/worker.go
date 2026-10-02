// The `caos-conversation-list` tool's worker. Its DOCS live in the sibling
// `.caos-expr` here-string, not in this header (SPEC, "Tools").
//
// It lists the conversations recorded on the server it runs against, so a
// caller has a tip hash to give `caos-conversation`.
//
// WHERE A CONVERSATION LIVES (v3/refs.rs): in two kinds of git ref on the server,
// with every id hex-encoded into its ref path:
//   - `refs/caos/v3/conversations/<key>/head`: the tip commit.
//   - `refs/caos/v3/users/<user key>/conversations/{active,archived}/<key>`:
//     one per user who has it. Its value is not read here; the name is the fact.
//
// caos has no verb for refs (`caos get-hash` fetches an object BY hash), so this
// asks git, which std/go carries for exactly that, at $CAOS_SERVER_URL, the same
// remote every worker already pushes to.
//
// A tip's title is `.caos/title` in its tree, read the way caos-conversation
// reads it. It is best-effort: a conversation whose tip cannot be read is still
// listed, with a note, because a listing that drops what it cannot describe is
// how a conversation goes missing without a trace.
//
// EVERY OUTCOME IS THE VALUE, as in caos-conversation: a server that cannot be
// reached comes back as text, never as a job error.
package main

import (
	"bytes"
	"encoding/hex"
	"fmt"
	"os"
	"sort"
	"strconv"
	"strings"

	"caos/w"

	"github.com/bitfield/script"
)

const (
	headPrefix  = "refs/caos/v3/conversations/"
	headSuffix  = "/head"
	usersPrefix = "refs/caos/v3/users/"
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

// unkey reverses refs.rs `key_of`: lowercase hex of the id's bytes.
func unkey(key string) (string, bool) {
	b, err := hex.DecodeString(key)
	if err != nil || hex.EncodeToString(b) != key {
		return "", false
	}
	return string(b), true
}

type conversation struct {
	id      string
	tip     string
	members []string
}

// refs returns ref name -> hash for every ref under the given patterns, or the
// error text when git could not ask.
func refs(server string, patterns ...string) (map[string]string, string) {
	cmd := "git ls-remote --refs " + server
	for _, p := range patterns {
		cmd += " '" + p + "'"
	}
	text, code := w.Try(script.Exec(cmd))
	if code != 0 {
		return nil, text
	}
	found := map[string]string{}
	for _, line := range strings.Split(text, "\n") {
		if hash, name, ok := strings.Cut(strings.TrimSpace(line), "\t"); ok {
			found[name] = hash
		}
	}
	return found, ""
}

// title reads `.caos/title` from the tip's tree. The second result says why
// there is none, when there is none.
func title(n int, tip string) (string, string) {
	dest := fmt.Sprintf("/cas/c%d", n)
	if errText, code := w.Try(script.Exec("caos get-hash " + tip + " " + dest)); code != 0 {
		return "", "tip not fetchable: " + firstLine(errText)
	}
	raw, err := os.ReadFile(dest)
	if err != nil || !bytes.HasPrefix(raw, []byte("tree ")) {
		return "", "tip is not a commit"
	}
	head, _, _ := bytes.Cut(raw, []byte("\n\n"))
	if os.Getenv("PROBE") != "" || optArg("debug", "") != "" {
		say("DEBUG %s:\n%s\n", tip, head)
	}
	tree := ""
	for _, line := range strings.Split(string(head), "\n") {
		if v, ok := strings.CutPrefix(line, "tree "); ok {
			tree = v
		}
	}
	root := fmt.Sprintf("/cas/t%d", n)
	if errText, code := w.Try(script.Exec("caos get-hash " + tree + " " + root)); code != 0 {
		return "", "tree not fetchable: " + firstLine(errText)
	}
	for _, p := range []string{root + "/.caos", root + "/.caos/title"} {
		if _, err := os.Stat(p); err != nil {
			return "", "no title recorded"
		}
		if _, code := w.Try(script.Exec("caos get " + p)); code != 0 {
			return "", "title not fetchable"
		}
	}
	b, err := os.ReadFile(root + "/.caos/title")
	if err != nil {
		return "", "title not readable"
	}
	return strings.TrimSpace(string(b)), ""
}

func firstLine(s string) string {
	line, _, _ := strings.Cut(strings.TrimSpace(s), "\n")
	return line
}

func main() {
	w.Main(func() {
		list()
		w.Report(out.String())
	})
}

// list renders the conversations into out. Every answer, including a server
// that cannot be asked, is a return here rather than a failure.
func list() {
	server := os.Getenv("CAOS_SERVER_URL")
	if server == "" {
		say("CAOS_SERVER_URL is not set, so there is no server to ask.\n\nThis tool must run as a worker under a caos server.")
		return
	}
	filter := strings.ToLower(optArg("filter", ""))
	limit, err := strconv.Atoi(optArg("limit", "50"))
	if err != nil || limit < 0 {
		limit = 50
	}
	wantTitles := optArg("titles", "1") != "0"

	heads, failure := refs(server, headPrefix+"*"+headSuffix)
	if failure != "" {
		say("could not list refs at %s:\n\n%s", server, failure)
		return
	}
	// Every ref under users/ is asked for and filtered here: git's pattern
	// matching has no way to say "a user, then /conversations/".
	memberRefs, failure := refs(server, usersPrefix+"*")
	if failure != "" {
		say("listed the conversations but not who has them (%s): %s", server, firstLine(failure))
	}

	byID := map[string]*conversation{}
	var skipped []string
	for name, tip := range heads {
		key, ok := strings.CutSuffix(strings.TrimPrefix(name, headPrefix), headSuffix)
		id, decoded := unkey(key)
		if !ok || !decoded || strings.Contains(key, "/") {
			skipped = append(skipped, name)
			continue
		}
		byID[id] = &conversation{id: id, tip: tip}
	}
	for name := range memberRefs {
		rest := strings.TrimPrefix(name, usersPrefix)
		userKey, rest, ok := strings.Cut(rest, "/conversations/")
		if !ok {
			continue
		}
		status, convKey, ok := strings.Cut(rest, "/")
		if !ok || (status != "active" && status != "archived") {
			continue
		}
		user, ok1 := unkey(userKey)
		id, ok2 := unkey(convKey)
		if !ok1 || !ok2 {
			skipped = append(skipped, name)
			continue
		}
		if c, found := byID[id]; found {
			c.members = append(c.members, user+":"+status)
		}
	}

	all := make([]*conversation, 0, len(byID))
	for _, c := range byID {
		sort.Strings(c.members)
		all = append(all, c)
	}
	sort.Slice(all, func(i, j int) bool { return all[i].id < all[j].id })

	titles := map[string]string{}
	whys := map[string]string{}
	if wantTitles {
		// Titles are read for the conversations that will be printed, not for
		// all of them, but the filter may be on the title, so with one set the
		// whole list is read.
		reading := all
		if filter == "" && limit > 0 && len(reading) > limit {
			reading = reading[:limit]
		}
		for i, c := range reading {
			titles[c.id], whys[c.id] = title(i, c.tip)
		}
	}

	var shown []*conversation
	for _, c := range all {
		hay := strings.ToLower(c.id + "\n" + titles[c.id])
		if filter == "" || strings.Contains(hay, filter) {
			shown = append(shown, c)
		}
	}

	say("%d conversations on %s", len(all), server)
	if filter != "" {
		say("%d match %q", len(shown), filter)
	}
	say("")
	cut := 0
	if limit > 0 && len(shown) > limit {
		cut = len(shown) - limit
		shown = shown[:limit]
	}
	for _, c := range shown {
		line := c.id + "  " + c.tip
		if t := titles[c.id]; t != "" {
			line += "  " + strconv.Quote(t)
		} else if why := whys[c.id]; why != "" {
			line += "  (" + why + ")"
		}
		if len(c.members) > 0 {
			line += "  [" + strings.Join(c.members, ", ") + "]"
		}
		say("%s", line)
	}
	if cut > 0 {
		say("")
		say("… %d more; pass limit=0 for all, or filter=<text> to narrow", cut)
	}
	if len(skipped) > 0 {
		say("")
		say("Refs skipped as unreadable (not a canonical id key):")
		for _, name := range skipped {
			say("  %s", name)
		}
	}
}
