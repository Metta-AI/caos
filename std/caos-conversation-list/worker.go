// The `caos-conversation-list` tool's worker. Its DOCS live in the sibling
// `.caos-expr` here-string, not in this header (SPEC, "Tools").
//
// It lists the conversations recorded on the server it runs against, newest
// first, so a caller has a tip hash to give `caos-conversation`.
//
// WHERE A CONVERSATION LIVES (v3/refs.rs, design/ref-writers.md): in two kinds of
// git ref on the server, both inside ref-writer namespaces, with every id
// hex-encoded into its ref path:
//   - `refs/caos/w/<ns>/conversations/<key>/head`: the tip commit. The same id
//     can live in more than one namespace, so a conversation is its ADDRESS,
//     `<ns>/<id>`, and that is what it is listed by.
//   - `refs/caos/w/<personal>/memberships/{active,archived}/<ns>/<key>`: one per
//     writer who has it, in that writer's personal namespace. Its value is not
//     read here; the name is the fact.
//
// caos has no verb for refs (`caos get-hash` fetches an object BY hash), so this
// asks git, which std/go carries for exactly that, at $CAOS_SERVER_URL, the same
// remote every worker already pushes to.
//
// WHEN: a ref carries no time, but the tip COMMIT does. Its committer time is
// when the last event was recorded, which is the last message. (std/caos-conversation's
// recording-gaps.md says commit timestamps are all identical; on this server they
// are not, and that note is the stale one.) So listing newest-first means reading
// every tip commit, one `caos get-hash` each, BEFORE the limit is applied.
//
// A tip's title is `.caos/title` in its tree, read the way caos-conversation
// reads it. It is best-effort: a conversation whose tip cannot be read is still
// listed, with a note, because a listing that drops what it cannot describe is
// how a conversation goes missing without a trace. Such a conversation has no
// time and sorts last.
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
	"time"

	"caos/w"

	"github.com/bitfield/script"
)

const (
	nsPrefix = "refs/caos/w/"
	// ls-remote's `*` crosses `/`, so these only narrow the listing; each name
	// is parsed exactly below.
	headPattern       = nsPrefix + "*/conversations/*/head"
	membershipPattern = nsPrefix + "*/memberships/*"
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

// isNamespace matches writers.rs `is_namespace`: a namespace id is the hash of
// its first writers commit.
func isNamespace(s string) bool {
	if len(s) != 40 {
		return false
	}
	for _, c := range s {
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

// parseHead reads `refs/caos/w/<ns>/conversations/<key>/head`.
func parseHead(name string) (ns, id string, ok bool) {
	rest, ok := strings.CutPrefix(name, nsPrefix)
	if !ok {
		return "", "", false
	}
	ns, rest, ok = strings.Cut(rest, "/conversations/")
	if !ok || !isNamespace(ns) {
		return "", "", false
	}
	key, ok := strings.CutSuffix(rest, "/head")
	if !ok || strings.Contains(key, "/") {
		return "", "", false
	}
	id, ok = unkey(key)
	return ns, id, ok
}

// parseMembership reads
// `refs/caos/w/<personal>/memberships/{active,archived}/<ns>/<key>`.
func parseMembership(name string) (personal, status, ns, id string, ok bool) {
	rest, ok := strings.CutPrefix(name, nsPrefix)
	if !ok {
		return "", "", "", "", false
	}
	personal, rest, ok = strings.Cut(rest, "/memberships/")
	parts := strings.Split(rest, "/")
	if !ok || !isNamespace(personal) || len(parts) != 3 ||
		(parts[0] != "active" && parts[0] != "archived") || !isNamespace(parts[1]) {
		return "", "", "", "", false
	}
	id, ok = unkey(parts[2])
	return personal, parts[0], parts[1], id, ok
}

type conversation struct {
	address string // `<ns>/<id>`
	tip     string
	members []string

	tree  string
	when  int64 // committer time of the tip, 0 when it could not be read
	title string
	note  string // why there is no time or title, when there is none
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

// readTip fills in the tip's tree and time from its commit.
func (c *conversation) readTip(n int) {
	dest := fmt.Sprintf("/cas/c%d", n)
	if errText, code := w.Try(script.Exec("caos get-hash " + c.tip + " " + dest)); code != 0 {
		c.note = "tip not fetchable: " + firstLine(errText)
		return
	}
	raw, err := os.ReadFile(dest)
	if err != nil || !bytes.HasPrefix(raw, []byte("tree ")) {
		c.note = "tip is not a commit"
		return
	}
	head, _, _ := bytes.Cut(raw, []byte("\n\n"))
	for _, line := range strings.Split(string(head), "\n") {
		if v, ok := strings.CutPrefix(line, "tree "); ok {
			c.tree = v
		}
		// `committer Name <email> <unix> <tz>`: the time is the second-to-last field.
		if v, ok := strings.CutPrefix(line, "committer "); ok {
			fields := strings.Fields(v)
			if len(fields) >= 2 {
				if t, err := strconv.ParseInt(fields[len(fields)-2], 10, 64); err == nil {
					c.when = t
				}
			}
		}
	}
	if c.when == 0 {
		c.note = "tip has no readable time"
	}
}

// readTitle reads `.caos/title` from the tip's tree.
func (c *conversation) readTitle(n int) {
	if c.tree == "" {
		return
	}
	root := fmt.Sprintf("/cas/t%d", n)
	if errText, code := w.Try(script.Exec("caos get-hash " + c.tree + " " + root)); code != 0 {
		c.note = "tree not fetchable: " + firstLine(errText)
		return
	}
	for _, p := range []string{root + "/.caos", root + "/.caos/title"} {
		if _, err := os.Stat(p); err != nil {
			c.note = "no title recorded"
			return
		}
		if _, code := w.Try(script.Exec("caos get " + p)); code != 0 {
			c.note = "title not fetchable"
			return
		}
	}
	b, err := os.ReadFile(root + "/.caos/title")
	if err != nil {
		c.note = "title not readable"
		return
	}
	c.title = strings.TrimSpace(string(b))
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

	heads, failure := refs(server, headPattern)
	if failure != "" {
		say("could not list refs at %s:\n\n%s", server, failure)
		return
	}
	memberRefs, failure := refs(server, membershipPattern)
	if failure != "" {
		say("listed the conversations but not who has them (%s): %s", server, firstLine(failure))
	}

	byAddress := map[string]*conversation{}
	var skipped []string
	for name, tip := range heads {
		ns, id, ok := parseHead(name)
		if !ok {
			skipped = append(skipped, name)
			continue
		}
		address := ns + "/" + id
		byAddress[address] = &conversation{address: address, tip: tip}
	}
	for name := range memberRefs {
		personal, status, ns, id, ok := parseMembership(name)
		if !ok {
			skipped = append(skipped, name)
			continue
		}
		// A writer's personal namespace is all that names them here: its
		// writers list holds only their key, unlabelled.
		if c, found := byAddress[ns+"/"+id]; found {
			c.members = append(c.members, personal[:12]+":"+status)
		}
	}

	// Newest first. The address breaks ties, and orders the conversations whose
	// tip could not be read (time 0) among themselves, at the end.
	all := make([]*conversation, 0, len(byAddress))
	for _, c := range byAddress {
		sort.Strings(c.members)
		all = append(all, c)
	}
	sort.Slice(all, func(i, j int) bool { return all[i].address < all[j].address })
	for i, c := range all {
		c.readTip(i)
	}
	sort.SliceStable(all, func(i, j int) bool { return all[i].when > all[j].when })

	// With a filter the title is part of what is matched, so every title is
	// read; without one, only the conversations that will be printed.
	if wantTitles {
		reading := all
		if filter == "" && limit > 0 && len(reading) > limit {
			reading = reading[:limit]
		}
		for i, c := range reading {
			c.readTitle(i)
		}
	}

	var shown []*conversation
	for _, c := range all {
		if filter == "" || strings.Contains(strings.ToLower(c.address+"\n"+c.title), filter) {
			shown = append(shown, c)
		}
	}

	say("%d conversations on %s, newest first", len(all), server)
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
		when := "(no time)"
		if c.when != 0 {
			when = time.Unix(c.when, 0).UTC().Format("2006-01-02T15:04:05Z")
		}
		line := when + "  " + c.address + "  " + c.tip
		if c.title != "" {
			line += "  " + strconv.Quote(c.title)
		} else if c.note != "" {
			line += "  (" + c.note + ")"
		}
		if len(c.members) > 0 {
			line += "  [" + strings.Join(c.members, ", ") + "]"
		}
		say("%s", line)
	}
	if cut > 0 {
		say("")
		say("… %d older; pass limit=0 for all, or filter=<text> to narrow", cut)
	}
	if len(skipped) > 0 {
		say("")
		say("Refs skipped as unreadable (not a namespace and a canonical id key):")
		for _, name := range skipped {
			say("  %s", name)
		}
	}
}
