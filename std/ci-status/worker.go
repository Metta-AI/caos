// The `ci-status` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// What CI says about a branch on GitHub (design/stack-publish.md,
// "ci-status"), as an ordinary worker (design/tool-contract.md): it holds no
// token, and reads GitHub through its neighbour `DEEP-DEPS/github`, which the
// token is granted to. Each read is one batch there, the check runs and the
// combined status of the reference, and the verdict is worked out in the
// `then` this worker names for it. A wait (`wait-min`) is a chain of such
// reads, each with its own request-id, until CI settles or the time is up.
//
// Stages, by --stage: none (resolve the call and read), and `check` (read
// GitHub's answer, then answer or read again).
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"caos/w"
)

// How often a wait reads CI again. CI moves in minutes; each read is a run.
const poll = 15 * time.Second

type refusal struct{ msg string }

func refuse(format string, a ...any) { panic(refusal{fmt.Sprintf(format, a...)}) }

// arg is an optional argument, trimmed; "" when the caller left it out.
func arg(name string) string {
	path := "/cas/args/" + name
	if !w.Exists(path) {
		return ""
	}
	return strings.TrimSpace(w.Read(path))
}

// answer is the result under the root contract: `{stdout, failed?}`.
func answer(text string, failed bool) {
	const dir = "/tmp/ci-status-out"
	w.Must(os.MkdirAll(dir, 0o755))
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(text), 0o644))
	if failed {
		w.Must(os.WriteFile(filepath.Join(dir, "failed"), []byte("ci-status failed\n"), 0o644))
	}
	w.Put(dir, "/cas/out")
}

var (
	layerName = regexp.MustCompile(`^[0-9]+-(.+)$`)
	plain     = regexp.MustCompile(`^[A-Za-z0-9_./-]+$`)
	githubURL = regexp.MustCompile(`^https://github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(\.git)?/?$`)
)

// resolve is the repository and the reference to read: a layer's name (or its
// name without the number) in the stack at root means that layer's branch.
func resolve() (repository, reference string) {
	branch := arg("branch")
	if branch == "" || !plain.MatchString(branch) || strings.Contains(branch, "..") {
		refuse("branch must be a branch name, a layer name or a commit hash, not %q", branch)
	}
	repository = arg("repository")
	reference = branch
	const root = "/cas/args/root"
	if w.Exists(root) {
		if k := w.Caos("kind", root); k != "tree" {
			refuse("the root is a %s: ci-status takes a stack directory, such as sources/repo", k)
		}
		w.Get(root)
		if repository == "" && w.Exists(root+"/00-base.source.json") {
			var provenance struct {
				Repository string `json:"repository"`
			}
			if json.Unmarshal([]byte(w.Read(root+"/00-base.source.json")), &provenance) == nil {
				repository = provenance.Repository
			}
		}
		for _, entry := range w.Check(os.ReadDir(root)) {
			name := entry.Name()
			m := layerName.FindStringSubmatch(name)
			if m == nil || strings.HasPrefix(name, "00-") || (name != branch && m[1] != branch) {
				continue
			}
			if w.Caos("kind", filepath.Join(root, name)) == "commit" {
				reference = arg("branch-prefix") + m[1]
				break
			}
		}
	}
	if repository == "" {
		refuse("no repository: pass repository, or a stack directory as root whose 00-base.source.json names one")
	}
	return repository, reference
}

func ownerRepo(repository string) (string, string) {
	m := githubURL.FindStringSubmatch(repository)
	if m == nil {
		refuse("%s is not on github.com, which is where check runs are read", repository)
	}
	return m[1], m[2]
}

// read asks GitHub, through std/github, for the reference's check runs and
// combined status, and names the `check` stage to read the answer.
func read(repository, reference string, deadline int64, n int) {
	owner, repo := ownerRepo(repository)
	base := fmt.Sprintf("/repos/%s/%s/commits/%s", owner, repo, reference)
	batch := w.Check(json.Marshal([]map[string]any{
		{"op": "api", "method": "GET", "path": base + "/check-runs?per_page=100"},
		{"op": "api", "method": "GET", "path": base + "/status"},
	}))
	id := arg("request-id")
	request := w.Caos("prepare-request", "--base:@=/cas/args/github", "--batch="+string(batch),
		fmt.Sprintf("--request-id=%s-read-%d", id, n))
	then := w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--github:@=/cas/args/github", "--stage=check", "--request-id="+id,
		"--repository="+repository, "--reference="+reference,
		"--deadline="+strconv.FormatInt(deadline, 10), "--read="+strconv.Itoa(n),
		"--wait-min="+arg("wait-min"))
	w.Caos("run-request-then", request, "--then:hash="+then)
}

type check struct{ name, state, standing, url string }

func str(v any) string {
	s, _ := v.(string)
	return s
}

func asList(v any) []any {
	list, _ := v.([]any)
	return list
}

// runStanding: neutral and skipped pass, any other completed conclusion
// fails, and a run that has not completed is pending.
func runStanding(status, conclusion string) string {
	if status != "completed" {
		return "pending"
	}
	switch conclusion {
	case "success", "neutral", "skipped":
		return "passing"
	}
	return "failing"
}

func statusStanding(state string) string {
	switch state {
	case "success":
		return "passing"
	case "pending":
		return "pending"
	}
	return "failing"
}

// checkStage reads GitHub's answer: an error, or the checks, and then either
// the verdict or another read.
func checkStage() {
	var results []map[string]any
	text := w.Read("/cas/args/result")
	if json.Unmarshal([]byte(text), &results) != nil || len(results) != 2 {
		refuse("GitHub's answer was not two results: %s", text)
	}
	for _, r := range results {
		if r["ok"] != true {
			if message := str(r["error"]); message != "" {
				refuse("%s", message)
			}
			refuse("GitHub answered %v to %s: %v", r["status"], str(r["path"]), r["body"])
		}
	}
	runs, _ := results[0]["body"].(map[string]any)
	status, _ := results[1]["body"].(map[string]any)
	var checks []check
	for _, raw := range asList(runs["check_runs"]) {
		run, _ := raw.(map[string]any)
		conclusion := str(run["conclusion"])
		state := conclusion
		if state == "" {
			state = str(run["status"])
		}
		url := str(run["html_url"])
		if url == "" {
			url = str(run["details_url"])
		}
		checks = append(checks, check{str(run["name"]), state, runStanding(str(run["status"]), conclusion), url})
	}
	for _, raw := range asList(status["statuses"]) {
		s, _ := raw.(map[string]any)
		checks = append(checks, check{str(s["context"]), str(s["state"]), statusStanding(str(s["state"])), str(s["target_url"])})
	}
	named := map[string][]string{}
	for _, c := range checks {
		named[c.standing] = append(named[c.standing], c.name)
	}
	settled := len(named["failing"]) > 0 || len(named["pending"]) == 0
	deadline := w.Check(strconv.ParseInt(arg("deadline"), 10, 64))
	if !settled && time.Now().Unix() < deadline {
		time.Sleep(min(poll, time.Until(time.Unix(deadline, 0))))
		n := w.Check(strconv.Atoi(arg("read")))
		read(arg("repository"), arg("reference"), deadline, n+1)
		return
	}

	var summary string
	switch {
	case len(checks) == 0:
		summary = "no checks"
	case len(named["failing"]) > 0:
		summary = "FAILING: " + strings.Join(named["failing"], ", ")
		if len(named["pending"]) > 0 {
			summary += "; pending: " + strings.Join(named["pending"], ", ")
		}
	case len(named["pending"]) > 0:
		summary = fmt.Sprintf("pending: %s (%d passing)", strings.Join(named["pending"], ", "), len(named["passing"]))
	default:
		summary = fmt.Sprintf("%d passing", len(named["passing"]))
	}
	head := str(status["sha"])
	if len(head) > 12 {
		head = head[:12]
	}
	var out strings.Builder
	fmt.Fprintf(&out, "checks: %s\nrepository %s  branch %s  head %s\n", summary, arg("repository"), arg("reference"), head)
	order := map[string]int{"failing": 0, "pending": 1, "passing": 2}
	sort.SliceStable(checks, func(i, j int) bool { return order[checks[i].standing] < order[checks[j].standing] })
	for _, c := range checks {
		fmt.Fprintf(&out, "%-16s %s", c.state, c.name)
		if c.url != "" {
			out.WriteString("  " + c.url)
		}
		out.WriteString("\n")
	}
	if !settled {
		fmt.Fprintf(&out, "still pending after waiting %s min; run ci-status again to wait longer\n", arg("wait-min"))
	}
	answer(out.String(), false)
}

func main() {
	w.Main(func() {
		defer func() {
			r := recover()
			if r == nil {
				return
			}
			no, ok := r.(refusal)
			if !ok {
				panic(r)
			}
			answer("ci-status: "+no.msg+"\n", true)
		}()
		if arg("stage") == "check" {
			checkStage()
			return
		}
		minutes := 0
		if text := arg("wait-min"); text != "" {
			var err error
			if minutes, err = strconv.Atoi(text); err != nil || minutes < 0 {
				refuse("wait-min is a whole number of minutes, such as 20, not %q", text)
			}
		}
		repository, reference := resolve()
		ownerRepo(repository)
		read(repository, reference, time.Now().Add(time.Duration(minutes)*time.Minute).Unix(), 0)
	})
}
