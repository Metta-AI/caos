// The `github` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// The one door to GitHub (design/tool-contract.md, "One grant"): the
// github-token is granted to this image alone, and every other git tool hands
// its remote work here. Args, under /cas/args:
//
//	method, path, body  one GitHub API call, answered as text
//	batch               a JSON array of operations, answered as a JSON array
//	request-id          `@request-id`; it only keys the result
//
// The github-token secret, when granted, is at /secret/github-token. It is
// sent to api.github.com, and to a github.com remote by git, and nowhere else.
//
// THIS PROGRAM MUST STAY CLOSED: its arguments are data, never code. A grant
// also covers a job whose arg tree is a superset of the granted image's (SPEC,
// "Isolation"), so an image that ran something a caller curried onto it would
// hand that caller the token.
//
// A response of any status is a result: the model reads a 404 as an answer.
// Only a single call that got no response fails the job; in a batch, an
// operation that could not be done says so in its own result.
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"slices"
	"strings"
	"time"

	"caos/w"
)

// The one host API calls reach. A path is appended to it, never parsed into a
// URL of its own, and redirects are not followed, so the token is sent here
// and nowhere else.
const api = "https://api.github.com"

// A single call's response longer than this is cut, with a note saying where.
const maxBody = 100_000

// A batch operation's response longer than this is an error rather than cut:
// its reader is a program, and half a JSON document is no answer.
const maxBatchBody = 4_000_000

const maxOperations = 100

const tokenFile = "/secret/github-token"

var client = &http.Client{
	Timeout: 60 * time.Second,
	// A redirect would carry the request to wherever it points.
	CheckRedirect: func(*http.Request, []*http.Request) error {
		return http.ErrUseLastResponse
	},
}

// arg reads an argument, or reports it absent.
func arg(name string) (string, bool) {
	path := "/cas/args/" + name
	if _, err := os.Stat(path); err != nil {
		return "", false
	}
	out, err := exec.Command("caos", "get", path).CombinedOutput()
	w.True(err == nil, "caos get %s: %v: %s", path, err, out)
	return string(w.Check(os.ReadFile(path))), true
}

func present(name string) bool {
	value, ok := arg(name)
	return ok && strings.TrimSpace(value) != ""
}

func token() string {
	bytes, err := os.ReadFile(tokenFile)
	if os.IsNotExist(err) {
		return ""
	}
	t := strings.TrimRight(string(w.Check(bytes, err)), "\r\n")
	// A line break inside a header value would end the header block early.
	w.True(!strings.ContainsAny(t, "\r\n"), "github: the github-token secret has a line break inside it")
	return t
}

// apiRequest checks one call and forms it. Nothing is sent until every part
// is well-formed.
func apiRequest(method, raw, body, token string) (*http.Request, error) {
	method = strings.ToUpper(strings.TrimSpace(method))
	switch method {
	case "GET", "POST", "PATCH", "PUT", "DELETE":
	default:
		return nil, fmt.Errorf("method must be GET, POST, PATCH, PUT or DELETE, not %q", method)
	}
	raw = strings.TrimSpace(raw)
	// A model often copies a full `url` out of an earlier response.
	path := strings.TrimPrefix(raw, api)
	if !strings.HasPrefix(path, "/") || strings.ContainsFunc(path, func(r rune) bool {
		return r <= ' ' || r == 0x7f
	}) {
		return nil, fmt.Errorf("path must be an API path such as /repos/owner/repo/pulls, not %q", raw)
	}
	var reader io.Reader
	if strings.TrimSpace(body) != "" {
		if method == "GET" {
			return nil, errors.New("a GET takes no body; put its parameters in the query string")
		}
		if !json.Valid([]byte(body)) {
			return nil, errors.New("body is not JSON")
		}
		reader = strings.NewReader(body)
	}
	req, err := http.NewRequest(method, api+path, reader)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("X-GitHub-Api-Version", "2022-11-28")
	req.Header.Set("User-Agent", "caos-github")
	if reader != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	return req, nil
}

// noResponse says that a call got no answer, and that a write may still have
// arrived.
func noResponse(req *http.Request, err error) string {
	unconfirmed := ""
	if req.Method != "GET" {
		unconfirmed = " The request may still have arrived: read the affected state with a GET before sending it again."
	}
	return fmt.Sprintf("%s %s got no response: %v.%s", req.Method, req.URL.RequestURI(), err, unconfirmed)
}

// render is the status line, then the body, cut if long.
func render(resp *http.Response, hasToken bool) string {
	var out strings.Builder
	out.WriteString(resp.Status)
	if location := resp.Header.Get("Location"); location != "" {
		fmt.Fprintf(&out, "\nlocation: %s", location)
	}
	if resp.StatusCode == http.StatusUnauthorized && !hasToken {
		out.WriteString("\nNo github-token secret is granted to std/github, so only public reads work.")
	}
	out.WriteString("\n\n")
	body := w.Check(io.ReadAll(io.LimitReader(resp.Body, maxBody+1)))
	if len(body) <= maxBody {
		out.Write(body)
		return out.String()
	}
	cut := maxBody
	for cut > 0 && !utf8Start(body[cut]) {
		cut--
	}
	out.Write(body[:cut])
	fmt.Fprintf(&out, "\n[truncated after %d bytes. Narrow the query, or select fields with GraphQL.]", cut)
	return out.String()
}

// utf8Start reports whether b begins a UTF-8 character, so a cut there leaves
// none split.
func utf8Start(b byte) bool { return b&0xC0 != 0x80 }

// single is one API call, answered as text.
func single(t string) string {
	method, _ := arg("method")
	path, _ := arg("path")
	w.True(strings.TrimSpace(method) != "", "github: method is required (or pass batch)")
	w.True(strings.TrimSpace(path) != "", "github: path is required (or pass batch)")
	body, _ := arg("body")
	req, err := apiRequest(method, path, body, t)
	w.True(err == nil, "github: %v", err)
	resp, err := client.Do(req)
	w.True(err == nil, "github: %s", noResponse(req, err))
	defer resp.Body.Close()
	return render(resp, t != "")
}

// operation is one entry of a batch. Which fields apply depends on `op`.
type operation struct {
	Op string `json:"op"`

	// api
	Method   string          `json:"method"`
	Path     string          `json:"path"`
	Body     json.RawMessage `json:"body"`
	OKStatus []int           `json:"ok_status"`

	// head, push, import
	Repository   string `json:"repository"`
	Branch       string `json:"branch"`
	Commit       string `json:"commit"`
	Expected     string `json:"expected"`
	Force        bool   `json:"force"`
	AllowIgnored bool   `json:"allow_ignored"`
	Revision     string `json:"revision"`
}

// result is one operation's answer.
type result map[string]any

// batch runs the operations in order, stopping at the first that is not ok,
// and answers with one result per operation.
func batch(text, t string) string {
	decoder := json.NewDecoder(strings.NewReader(text))
	decoder.DisallowUnknownFields()
	var ops []operation
	err := decoder.Decode(&ops)
	w.True(err == nil, "github: batch is not a JSON array of operations: %v", err)
	w.True(len(ops) > 0 && len(ops) <= maxOperations,
		"github: a batch holds 1 to %d operations, not %d", maxOperations, len(ops))
	results := make([]result, 0, len(ops))
	stopped := false
	for _, op := range ops {
		if stopped {
			results = append(results, result{"op": op.Op, "ok": false, "skipped": true})
			continue
		}
		r := perform(op, t)
		results = append(results, r)
		stopped = r["ok"] != true
	}
	return string(w.Check(json.MarshalIndent(results, "", "  "))) + "\n"
}

func perform(op operation, t string) result {
	r := result{"op": op.Op}
	var err error
	switch op.Op {
	case "api":
		err = callAPI(op, t, r)
	case "head":
		err = readHead(op, t, r)
	case "push":
		err = push(op, t, r)
	case "import":
		err = importRevision(op, t, r)
	default:
		err = fmt.Errorf("op must be api, head, push or import, not %q", op.Op)
	}
	if err != nil {
		r["ok"] = false
		r["error"] = err.Error()
	}
	return r
}

// callAPI is ok on a 2xx status or one the operation lists in ok_status.
func callAPI(op operation, t string, r result) error {
	body := ""
	if len(op.Body) > 0 && string(op.Body) != "null" {
		body = string(op.Body)
	}
	req, err := apiRequest(op.Method, op.Path, body, t)
	if err != nil {
		return err
	}
	r["method"], r["path"] = req.Method, req.URL.RequestURI()
	resp, err := client.Do(req)
	if err != nil {
		return errors.New(noResponse(req, err))
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(io.LimitReader(resp.Body, maxBatchBody+1))
	if err != nil {
		return fmt.Errorf("reading the response to %s %s: %v", req.Method, req.URL.RequestURI(), err)
	}
	if len(data) > maxBatchBody {
		return fmt.Errorf("the response to %s %s is longer than %d bytes: narrow the query",
			req.Method, req.URL.RequestURI(), maxBatchBody)
	}
	r["status"] = resp.StatusCode
	if location := resp.Header.Get("Location"); location != "" {
		r["location"] = location
	}
	if len(bytes.TrimSpace(data)) > 0 && json.Valid(data) {
		r["body"] = json.RawMessage(data)
	} else {
		r["body"] = string(data)
	}
	r["ok"] = resp.StatusCode/100 == 2 || slices.Contains(op.OKStatus, resp.StatusCode)
	return nil
}

// readHead is the branch's commit, or null when the branch does not exist.
func readHead(op operation, t string, r result) error {
	if err := checkRemote(op.Repository); err != nil {
		return err
	}
	if err := checkBranch(op.Branch); err != nil {
		return err
	}
	head, err := branchHead(op.Repository, op.Branch, t)
	if err != nil {
		return err
	}
	r["repository"], r["branch"] = op.Repository, op.Branch
	if head == "" {
		r["head"] = nil
	} else {
		r["head"] = head
	}
	r["ok"] = true
	return nil
}

// push publishes one commit to one branch with `caos push-git`, leased on
// `expected` (design/agent-publish.md), and reconciles an answer that is not
// definite against the remote before returning. It is ok when the branch is
// at the commit.
func push(op operation, t string, r result) error {
	if err := checkRemote(op.Repository); err != nil {
		return err
	}
	if err := checkBranch(op.Branch); err != nil {
		return err
	}
	if !isHash(op.Commit) {
		return fmt.Errorf("push needs commit, a full commit hash, not %q", op.Commit)
	}
	if op.Expected != "absent" && !isHash(op.Expected) {
		return fmt.Errorf("push needs expected, the branch's head as you saw it: a full commit hash, or \"absent\" for a new branch; not %q", op.Expected)
	}
	commit := strings.ToLower(op.Commit)
	expected := strings.ToLower(op.Expected)
	args := []string{"push-git", op.Repository, commit, op.Branch, "--expected=" + expected}
	if op.Force {
		args = append(args, "--force")
	}
	if op.AllowIgnored {
		args = append(args, "--allow-ignored")
	}
	args = append(args, tokenFlag(op.Repository, t)...)
	cmd := exec.Command("caos", args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	runErr := cmd.Run()
	receipt := result{}
	var exit *exec.ExitError
	switch {
	case runErr == nil:
		if json.Unmarshal(stdout.Bytes(), &receipt) != nil {
			receipt = uncertain()
		}
	case errors.As(runErr, &exit) && exit.ExitCode() == 1:
		// A local refusal: nothing was sent.
		receipt = result{"status": "conflict", "kind": "validation-rejected",
			"diagnostic": strings.TrimSpace(stderr.String()), "observed": nil}
	default:
		receipt = uncertain()
	}
	// A remote can accept a push before the connection drops, and git's retry
	// can then report a stale lease: read the branch. The commit there means it
	// landed. For an uncertain answer, any value other than the lease means
	// someone else moved it; the lease itself, or a failed lookup, stays
	// uncertain, because a push may still be running.
	if receipt["status"] != "complete" && receipt["kind"] != "validation-rejected" {
		if head, err := branchHead(op.Repository, op.Branch, t); err == nil {
			lease := expected
			if lease == "absent" {
				lease = ""
			}
			switch {
			case head == commit:
				receipt = result{"status": "complete", "kind": "ref-converged", "observed": head}
			case receipt["status"] == "uncertain" && head != lease:
				receipt = result{"status": "conflict", "kind": "ref-drift", "observed": nullable(head),
					"diagnostic": "The branch moved to a commit other than the lease while the push was unconfirmed."}
			}
		}
	}
	for _, key := range []string{"status", "kind", "observed", "diagnostic"} {
		if value, ok := receipt[key]; ok {
			r[key] = value
		}
	}
	r["repository"], r["branch"], r["commit"], r["expected"] = op.Repository, op.Branch, commit, expected
	r["ok"] = receipt["status"] == "complete"
	return nil
}

func uncertain() result {
	return result{"status": "uncertain", "kind": "ambiguous", "observed": nil,
		"diagnostic": "No confirmed push result. Inspect the remote before sending it again."}
}

func nullable(head string) any {
	if head == "" {
		return nil
	}
	return head
}

// importRevision resolves a revision on the remote and fetches that exact
// commit into caos with `caos import-git`. It is ok with the commit.
func importRevision(op operation, t string, r result) error {
	if err := checkRemote(op.Repository); err != nil {
		return err
	}
	commit, defaultBranch, err := resolve(op.Repository, op.Revision, t)
	if err != nil {
		return err
	}
	args := append([]string{"import-git", op.Repository, commit}, tokenFlag(op.Repository, t)...)
	cmd := exec.Command("caos", args...)
	var stdout bytes.Buffer
	cmd.Stdout = &stdout
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("importing %s at %s failed. Check the repository, commit and token, then retry", op.Repository, commit)
	}
	if strings.TrimSpace(stdout.String()) != commit {
		return fmt.Errorf("import-git returned %q, not %s", strings.TrimSpace(stdout.String()), commit)
	}
	r["repository"], r["commit"] = op.Repository, commit
	if op.Revision != "" {
		r["revision"] = op.Revision
	}
	if defaultBranch != "" {
		r["default_branch"] = defaultBranch
	}
	r["ok"] = true
	return nil
}

func isHash(value string) bool {
	if len(value) != 40 {
		return false
	}
	for _, c := range value {
		if !strings.ContainsRune("0123456789abcdefABCDEF", c) {
			return false
		}
	}
	return true
}

// checkRemote is git-locator's import::remote: an HTTPS URL with no
// credentials, query or fragment, and no path tricks.
func checkRemote(source string) error {
	_, _, err := remote(source)
	return err
}

func remote(source string) (string, string, error) {
	invalid := fmt.Errorf("repository must be an HTTPS URL without credentials, query or fragment, not %q", source)
	rest, ok := strings.CutPrefix(source, "https://")
	if !ok {
		return "", "", invalid
	}
	host, path, ok := strings.Cut(rest, "/")
	if !ok || host == "" || path == "" || strings.HasPrefix(host, ".") || strings.HasPrefix(host, "-") {
		return "", "", invalid
	}
	for _, c := range host {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || strings.ContainsRune(".-:", c)) {
			return "", "", invalid
		}
	}
	for _, c := range source {
		if c <= ' ' || c >= 127 || strings.ContainsRune("@?#%\\", c) {
			return "", "", invalid
		}
	}
	for _, part := range strings.Split(path, "/") {
		if part == "" || part == "." || part == ".." {
			return "", "", invalid
		}
	}
	if name, port, ok := strings.Cut(host, ":"); ok {
		if name == "" || port == "" || port == "0" || strings.Trim(port, "0123456789") != "" || len(port) > 5 {
			return "", "", invalid
		}
	}
	return host, path, nil
}

func checkBranch(branch string) error {
	if branch == "" || strings.HasPrefix(branch, "-") || strings.HasPrefix(branch, "refs/") ||
		strings.ContainsFunc(branch, func(r rune) bool { return r <= ' ' || r == 0x7f || strings.ContainsRune("~^:?*[\\", r) }) {
		return fmt.Errorf("branch must be a branch name without refs/heads/, not %q", branch)
	}
	return nil
}

func isGitHub(host string) bool {
	host = strings.ToLower(host)
	return host == "github.com" || host == "github.com:443"
}

// tokenFlag hands caos the token file for a github.com remote only: the token
// is a GitHub credential, not one for every repository a caller can name.
func tokenFlag(source, t string) []string {
	host, _, err := remote(source)
	if err != nil || t == "" || !isGitHub(host) {
		return nil
	}
	return []string{"--github-token-file=" + tokenFile}
}

// helper answers git's credential request for exactly this host and path,
// and nothing else (git-locator's import::git).
const helper = `!f() { test "$1" = get || exit 0; protocol= host= path=; while IFS='=' read -r k v; do case "$k" in protocol) protocol=$v;; host) host=$v;; path) path=$v;; esac; done; if test "$protocol" = https && test "$host" = "$CAOS_IMPORT_HOST" && test "$path" = "$CAOS_IMPORT_PATH"; then printf 'username=x-access-token\npassword=%s\n' "$CAOS_IMPORT_TOKEN"; fi; }; f`

// lsRemote lists the remote refs matching `refs`, with git-locator's
// hardening: a cleared environment, no ambient credentials, redirects or
// protocols other than https, and git's stderr (remote-controlled, and able
// to carry credentials) never echoed.
func lsRemote(source string, t string, refs ...string) (string, error) {
	host, path, err := remote(source)
	if err != nil {
		return "", err
	}
	env := []string{
		"GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null", "GIT_TERMINAL_PROMPT=0",
		"GIT_ALLOW_PROTOCOL=https", "GIT_NO_LAZY_FETCH=1", "GIT_NO_REPLACE_OBJECTS=1",
	}
	for _, name := range []string{"PATH", "SSL_CERT_FILE", "GIT_SSL_CAINFO"} {
		if value, ok := os.LookupEnv(name); ok {
			env = append(env, name+"="+value)
		}
	}
	args := []string{"--kill-after=5", "300", "git",
		"-c", "credential.helper=", "-c", "credential.useHttpPath=true",
		"-c", "http.followRedirects=false", "-c", "http.extraHeader="}
	if t != "" && isGitHub(host) {
		if len(t) > 8192 || strings.ContainsFunc(t, func(r rune) bool { return r <= ' ' || r >= 127 }) {
			return "", errors.New("the github-token secret is not a valid git credential")
		}
		env = append(env, "CAOS_IMPORT_TOKEN="+t, "CAOS_IMPORT_HOST="+host, "CAOS_IMPORT_PATH="+path)
		args = append(args, "-c", "credential."+source+".helper="+helper)
	}
	args = append(args, "ls-remote", "--symref", "--", source)
	args = append(args, refs...)
	cmd := exec.Command("timeout", args...)
	cmd.Env = env
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("reading the refs of %s failed: check the repository and the token", source)
	}
	return string(out), nil
}

// branchHead is the branch's commit on the remote, or "" when it does not
// exist.
func branchHead(source, branch, t string) (string, error) {
	ref := "refs/heads/" + branch
	out, err := lsRemote(source, t, ref)
	if err != nil {
		return "", err
	}
	for _, line := range strings.Split(out, "\n") {
		if left, right, ok := strings.Cut(line, "\t"); ok && right == ref && isHash(left) {
			return strings.ToLower(left), nil
		}
	}
	return "", nil
}

// resolve is a revision's commit on the remote, peeled through an annotated
// tag, and the remote's default branch when the revision was left out.
func resolve(source, revision, t string) (string, string, error) {
	if revision == "" {
		revision = "HEAD"
	}
	if isHash(revision) {
		return strings.ToLower(revision), "", nil
	}
	if revision != "HEAD" && !strings.HasPrefix(revision, "refs/") {
		if err := checkBranch(revision); err != nil {
			return "", "", fmt.Errorf("revision must be a branch, a full refs/... name or a full commit hash, not %q", revision)
		}
		revision = "refs/heads/" + revision
	}
	out, err := lsRemote(source, t, revision, revision+"^{}")
	if err != nil {
		return "", "", err
	}
	var hash, peeled, defaultBranch string
	for _, line := range strings.Split(out, "\n") {
		left, right, ok := strings.Cut(line, "\t")
		if !ok {
			continue
		}
		switch {
		case right == revision:
			if branch, ok := strings.CutPrefix(left, "ref: refs/heads/"); ok {
				if revision == "HEAD" {
					defaultBranch = branch
				}
			} else if isHash(left) {
				hash = strings.ToLower(left)
			}
		case right == revision+"^{}" && isHash(left):
			peeled = strings.ToLower(left)
		}
	}
	if peeled != "" {
		hash = peeled
	}
	if hash == "" {
		return "", "", fmt.Errorf("%s has no %s", source, revision)
	}
	return hash, defaultBranch, nil
}

func main() {
	w.Main(func() {
		t := token()
		if text, ok := arg("batch"); ok && strings.TrimSpace(text) != "" {
			for _, name := range []string{"method", "path", "body"} {
				w.True(!present(name), "github: pass either batch or method/path/body, not both (%s was given)", name)
			}
			w.Report(batch(text, t))
			return
		}
		w.Report(single(t))
	})
}
