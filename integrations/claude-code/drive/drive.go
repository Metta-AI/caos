// The caos worker behind the `integrations/claude-code/drive` entry. std/go
// runs this file as /worker: it reads its arguments from /cas/args and its
// token from /secret/claude-oauth-token, and runWorker at the bottom turns
// them back into the command line the rest of this file parses.
//
// WHAT IT TAKES, AND WHAT EACH VERB DOES, IS IN `.caos-expr` — bound there as
// the tool's `help`, which is the copy a harness reads and the copy a person
// reads. This comment is about how it works, not about how to call it.
//
// It REFUSES to run on the host (requireWorker), so that entry is the only way
// in. The token is minted by authorize.go beside this, which for the opposite
// reason runs ONLY on the host.
//
// WHY IT SPEAKS THE API RATHER THAN `claude --cloud`: a session names its
// environment and its repository, and the CLI can express neither — its
// `--environment` takes only a self-hosted `ccpool_…` id, and it has no
// repository flag at all, so the repository is whatever `git remote get-url
// origin` says where it ran. POST /v1/code/sessions takes `environment_id` and
// a `git_repository` source directly.
//
// ONE CREDENTIAL REACHES ALL OF IT. Sessions and environments are both
// api.anthropic.com and both answer to an OAuth token carrying
// `user:sessions:claude_code`; readCreds is where it is looked for, and
// authorize.go is why no other kind of token will do. An environment's
// variables are not secrets ("visible to anyone using this environment"), so
// they print in full.
//
// A SESSION ID HAS TWO SPELLINGS of the same session: the API and this print
// `cse_<suffix>`, and claude.ai/code URLs carry `session_<suffix>`. Either
// resolves, so anything here takes either, or the URL — see sessionID. A
// pattern matching only `session_` finds nothing in an id the API itself
// minted.
//
// Stdlib only, so the prelude module std/go copies beside it is never
// consulted and there is no vendorHash to keep in step.
package main

import (
	"bytes"
	"crypto/rand"
	"encoding/binary"
	"encoding/json"
	"errors"
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
)

const apiBase = "https://api.anthropic.com"

func main() {
	// A worker gets no command line, so the mode is decided by whether the
	// runner laid out /cas/args rather than by a flag a caller must remember.
	runner := run
	if inWorker() && len(os.Args) == 1 {
		runner = func([]string) error { return runWorker() }
	}
	if err := runner(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "drive: "+err.Error())
		os.Exit(1)
	}
}

// opts is the whole command line. A bare token is the prompt normally, and the
// environment or session name for the modes that take one.
type opts struct {
	mode       string // "", "info", "conv", "list", "archive", "env-config", "env-update", "env-create", "env-delete"
	prompt     string
	promptSet  bool
	file       string
	session    string // -c
	env        string // --env: the environment a new session runs on
	repo       string // --repo: owner/repo[@ref]
	ref        string // --ref
	title      string
	probe      bool
	arg        string // the bare token
	initScript string
	from       string
	rename     string
	setEnv     []string
	unsetEnv   []string
}

func run(argv []string) error {
	var o opts
	// One flag takes a value only when it is not the last token, so a missing
	// value is named rather than silently swallowing the next flag.
	next := func(i *int, flag string) (string, error) {
		*i++
		if *i >= len(argv) {
			return "", fmt.Errorf("%s needs a value", flag)
		}
		return argv[*i], nil
	}
	for i := 0; i < len(argv); i++ {
		var err error
		a := argv[i]
		switch a {
		case "-h", "--help":
			os.Stdout.WriteString(usage)
			return nil
		case "--probe":
			o.probe = true
		case "--info", "--env-of":
			o.mode = "info"
		case "--conv":
			o.mode = "conv"
		case "--list":
			o.mode = "list"
		case "--archive":
			o.mode = "archive"
		case "--env-config":
			o.mode = "env-config"
		case "--env-update":
			o.mode = "env-update"
		case "--env-create":
			o.mode = "env-create"
		case "--env-delete":
			o.mode = "env-delete"
		case "--env":
			// --env is BOTH spellings' entry point, and the argument says
			// which: an environment is named or `env_…`/`ccpool_…`, a session
			// is `cse_…`/`session_…` or a claude.ai URL. The two can't be
			// confused, so the older "read this session's environment" reading
			// keeps working without a second flag to remember.
			if o.env, err = next(&i, a); err != nil {
				return err
			}
			if id, ok := sessionID(o.env); ok {
				o.mode, o.arg, o.env = "info", id, ""
			}
		case "--repo":
			o.repo, err = next(&i, a)
		case "--ref":
			o.ref, err = next(&i, a)
		case "--title":
			o.title, err = next(&i, a)
		case "--init-script":
			o.initScript, err = next(&i, a)
		case "--from":
			o.from, err = next(&i, a)
		case "--rename":
			o.rename, err = next(&i, a)
		case "--set-env":
			var kv string
			if kv, err = next(&i, a); err == nil {
				o.setEnv = append(o.setEnv, kv)
			}
		case "--unset-env":
			var k string
			if k, err = next(&i, a); err == nil {
				o.unsetEnv = append(o.unsetEnv, k)
			}
		case "-c":
			o.session, err = next(&i, a)
		case "-f":
			o.file, err = next(&i, a)
		default:
			if strings.HasPrefix(a, "-") {
				return fmt.Errorf("unknown flag %s", a)
			}
			o.prompt, o.promptSet, o.file, o.arg = a, true, "", a
		}
		if err != nil {
			return err
		}
	}

	if err := requireWorker(o); err != nil {
		return err
	}
	switch o.mode {
	case "env-config":
		return envConfig(o)
	case "env-update":
		return envUpdate(o)
	case "env-create":
		return envCreate(o)
	case "env-delete":
		return envDelete(o)
	case "conv":
		return conv(o)
	case "info":
		return info(o)
	case "list":
		return list()
	case "archive":
		return archive(o)
	}
	return newOrContinue(o)
}

// ---------------------------------------------------------------- session ids

// A session's two spellings share a suffix; either prefix resolves on the API.
var idRe = regexp.MustCompile(`(?:cse_|session_)[A-Za-z0-9_-]+`)

// sessionID pulls a session id out of an id or a claude.ai/code URL.
func sessionID(s string) (string, bool) {
	m := idRe.FindString(s)
	return m, m != ""
}

// wantSession resolves the -c flag or the bare token to a session id.
func wantSession(o opts, what string) (string, error) {
	for _, s := range []string{o.session, o.arg} {
		if id, ok := sessionID(s); ok {
			return id, nil
		}
	}
	return "", fmt.Errorf("%s: need a session id (cse_… or session_…) or a claude.ai/code URL", what)
}

// sessionURL is the claude.ai spelling, which is the one a person can open.
func sessionURL(id string) string {
	return "https://claude.ai/code/session_" + strings.TrimPrefix(strings.TrimPrefix(id, "cse_"), "session_")
}

// ------------------------------------------------------------- anthropic api

// ONE CREDENTIAL RUNS ALL OF THIS: an Anthropic OAuth token with the
// `user:sessions:claude_code` scope. Sessions and environments are both served
// by api.anthropic.com to that token — creating, prompting, listing, archiving,
// and reading or writing an environment's whole definition including its setup
// script and its variables. An API key (`sk-ant-api03-…`) is NOT one of these:
// the service answers "Cloud sessions are only available on the first-party
// Anthropic API provider" to it.
//
// `x-organization-uuid` is optional. Sent when known, it selects the org; left
// out, the account's default answers.
type creds struct{ token, org string }

// secretName is the caos secret this reads its token from. A worker gets it at
// /secret/<name>, dropped there by the runner when the job's ArgTree is a
// superset of one of the secret's readers.
const secretName = "claude-oauth-token"

// readCreds finds the token in the first of three places that holds one.
// A worker has only the secret; a terminal usually has only the CLI's own
// login. Neither is required to know about the other.
//
// The CLI's token EXPIRES (`expiresAt`, refreshed by the running CLI), so it
// is the wrong thing to copy into a secret. `claude setup-token` mints the
// long-lived one that belongs there.
func readCreds() (creds, error) {
	var c creds
	var from string
	switch {
	case os.Getenv("CLAUDE_CODE_OAUTH_TOKEN") != "":
		c.token, from = os.Getenv("CLAUDE_CODE_OAUTH_TOKEN"), "$CLAUDE_CODE_OAUTH_TOKEN"
	default:
		if b, err := os.ReadFile(filepath.Join(secretsDir, secretName)); err == nil {
			// Verbatim, no trimming: a token is used as-is (design/secrets.md).
			c.token, from = string(b), secretsDir+"/"+secretName
		}
	}
	if c.token == "" {
		if tok, err := tokenFromCLI(); err == nil {
			c.token, from = tok, "~/.claude/.credentials.json"
		} else if inWorker() {
			return creds{}, fmt.Errorf("no token at %s/%s, and $CLAUDE_CODE_OAUTH_TOKEN is unset.\n"+
				"  The secret is granted only to an image a reader matches, so check the\n"+
				"  readers of %s in your secret store.", secretsDir, secretName, secretName)
		} else {
			return creds{}, fmt.Errorf("no token: %v.\n"+
				"  Set $CLAUDE_CODE_OAUTH_TOKEN (`claude setup-token` mints a long-lived one),\n"+
				"  or run `claude /login` so the CLI has one.", err)
		}
	}
	_ = from
	if c.org = os.Getenv("CLAUDE_ORG_ID"); c.org == "" {
		var cfg struct {
			OauthAccount struct {
				OrganizationUUID string `json:"organizationUuid"`
			} `json:"oauthAccount"`
		}
		if home, err := os.UserHomeDir(); err == nil {
			readJSON(filepath.Join(home, ".claude.json"), &cfg)
			c.org = cfg.OauthAccount.OrganizationUUID
		}
	}
	return c, nil
}

// tokenFromCLI reads the token the running CLI maintains, and refuses an
// expired one rather than letting the first call fail as a 401.
func tokenFromCLI() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	var credFile struct {
		ClaudeAiOauth struct {
			AccessToken string `json:"accessToken"`
			ExpiresAt   int64  `json:"expiresAt"`
		} `json:"claudeAiOauth"`
	}
	path := filepath.Join(home, ".claude", ".credentials.json")
	if err := readJSON(path, &credFile); err != nil {
		return "", fmt.Errorf("~/.claude/.credentials.json: %v", err)
	}
	if credFile.ClaudeAiOauth.AccessToken == "" {
		return "", errors.New("~/.claude/.credentials.json holds no accessToken")
	}
	if exp := credFile.ClaudeAiOauth.ExpiresAt; exp > 0 && time.UnixMilli(exp).Before(time.Now()) {
		return "", errors.New("the token in ~/.claude/.credentials.json has expired")
	}
	return credFile.ClaudeAiOauth.AccessToken, nil
}

func readJSON(path string, v any) error {
	b, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	return json.Unmarshal(b, v)
}

// api calls the code API. body nil is a GET. A non-2xx answer is returned as
// an error carrying the server's own message, which is the only place that
// says WHICH of the two lookups failed — the environment (404) or the
// repository (github_repo_access_denied).
func api(c creds, method, path string, body any, out any) error {
	var rdr io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		rdr = bytes.NewReader(b)
	}
	req, err := http.NewRequest(method, apiBase+path, rdr)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+c.token)
	req.Header.Set("anthropic-version", "2023-06-01")
	// The byoc beta gates the environment writes; the session routes ignore it.
	req.Header.Set("anthropic-beta", "ccr-byoc-2025-07-29")
	req.Header.Set("anthropic-client-platform", "cli")
	if c.org != "" {
		req.Header.Set("x-organization-uuid", c.org)
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := (&http.Client{Timeout: 60 * time.Second}).Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return err
	}
	if resp.StatusCode/100 != 2 {
		var e struct {
			Error struct{ Message, Reason, Type string } `json:"error"`
		}
		json.Unmarshal(raw, &e)
		msg := e.Error.Message
		if msg == "" {
			msg = strings.TrimSpace(string(raw))
		}
		if e.Error.Reason != "" {
			msg += " (" + e.Error.Reason + ")"
		}
		return fmt.Errorf("%s %s: HTTP %d: %s", method, path, resp.StatusCode, msg)
	}
	if out == nil {
		return nil
	}
	return json.Unmarshal(raw, out)
}

// ------------------------------------------------------------- environments

type environment struct {
	ID    string `json:"environment_id"`
	Name  string `json:"name"`
	Kind  string `json:"kind"`
	State string `json:"state"`
}

// environments lists what this account can run a session on. The listing
// answers with ids, names and state but a null
// `config`, so it resolves a NAME and nothing more. A definition still needs
// --env-config.
func environments(c creds) ([]environment, error) {
	var out struct {
		Environments []environment `json:"environments"`
	}
	if err := api(c, "GET", "/v1/environment_providers", nil, &out); err != nil {
		return nil, err
	}
	return out.Environments, nil
}

// resolveEnv turns a name or an id into an id. An id is passed through
// unchecked so a pool or a brand-new environment does not need to be listable.
func resolveEnv(c creds, want string) (string, error) {
	if want == "" {
		return "", errors.New("`env` is required: a session runs in a cloud environment, and a\n" +
			"  worker has no settings to read a default from — verb=env-list lists them")
	}
	if strings.HasPrefix(want, "env_") || strings.HasPrefix(want, "ccpool_") {
		return want, nil
	}
	envs, err := environments(c)
	if err != nil {
		return "", err
	}
	var names []string
	for _, e := range envs {
		if e.Name == want {
			return e.ID, nil
		}
		names = append(names, e.Name)
	}
	sort.Strings(names)
	return "", fmt.Errorf("no environment named %q (have: %s)", want, strings.Join(names, ", "))
}

// ------------------------------------------------------- new session, or -c

func newOrContinue(o opts) error {
	prompt, err := promptText(o)
	if err != nil {
		return err
	}
	c, err := readCreds()
	if err != nil {
		return err
	}
	if o.session != "" {
		id, err := wantSession(o, "-c")
		if err != nil {
			return err
		}
		return sendPrompt(c, id, prompt)
	}
	return start(c, o, prompt)
}

// start creates the session, then delivers the prompt.
//
// The prompt is a SECOND call rather than an initial event in the create body,
// so there is one way for a prompt to reach a session rather than two. An
// `events` entry in the create body works too.
func start(c creds, o opts, prompt string) error {
	envID, err := resolveEnv(c, o.env)
	if err != nil {
		return err
	}
	if o.repo == "" {
		return errors.New("`repo` is required: a session opens a repository, and a worker has no\n" +
			"  checkout to fall back on — name it, e.g. repo=Metta-AI/caos-session")
	}
	repoURL, ref, err := parseRepo(o.repo, o.ref)
	if err != nil {
		return err
	}

	title := o.title
	if title == "" {
		title = firstLine(prompt, 60)
	}
	source := map[string]any{"type": "git_repository", "url": repoURL}
	if ref != "" {
		source["revision"] = ref
	}
	body := map[string]any{
		"title":          title,
		"environment_id": envID,
		"events":         []any{},
		"config": map[string]any{
			"sources":  []any{source},
			"outcomes": []any{},
		},
	}
	var created struct {
		Session struct{ ID, Title string } `json:"session"`
	}
	if err := api(c, "POST", "/v1/code/sessions", body, &created); err != nil {
		return err
	}
	id := created.Session.ID
	if id == "" {
		return errors.New("the API created a session but returned no id")
	}
	fmt.Println(id)
	fmt.Fprintf(os.Stderr, "env:  %s\nrepo: %s", envID, repoURL)
	if ref != "" {
		fmt.Fprintf(os.Stderr, "@%s", ref)
	}
	fmt.Fprintf(os.Stderr, "\nview: %s\n", sessionURL(id))
	return sendPrompt(c, id, prompt)
}

// parseRepo accepts owner/repo, owner/repo@ref, or a full URL. An omitted ref
// is omitted from the request, which leaves the choice of branch to the
// server rather than guessing a name for the default.
func parseRepo(repo, ref string) (string, string, error) {
	if at := strings.LastIndex(repo, "@"); at > 0 && !strings.Contains(repo[at:], "/") {
		if ref == "" {
			ref = repo[at+1:]
		}
		repo = repo[:at]
	}
	if strings.HasPrefix(repo, "http://") || strings.HasPrefix(repo, "https://") {
		return strings.TrimSuffix(repo, ".git"), ref, nil
	}
	repo = strings.TrimSuffix(strings.Trim(repo, "/"), ".git")
	if strings.Count(repo, "/") != 1 || strings.HasPrefix(repo, "/") {
		return "", "", fmt.Errorf("--repo %q is not owner/repo or a URL", repo)
	}
	return "https://github.com/" + repo, ref, nil
}

func firstLine(s string, n int) string {
	s = strings.TrimSpace(s)
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		s = s[:i]
	}
	if len(s) > n {
		s = s[:n] + "…"
	}
	return s
}

// sendPrompt posts a message into an existing session. `-p` needs no TTY, and
// sendPrompt posts a user message into an existing session. The event is the
// same shape the CLI sends, and the session takes a turn on it.
//
// An ARCHIVED session refuses messages; the API says so, and that is the usual
// reason a follow-up lands nowhere.
func sendPrompt(c creds, id, prompt string) error {
	payload := map[string]any{
		"uuid":               uuidV4(),
		"session_id":         id,
		"type":               "user",
		"parent_tool_use_id": nil,
		"message":            map[string]any{"role": "user", "content": prompt},
	}
	body := map[string]any{"events": []any{map[string]any{"payload": payload}}}
	var out struct {
		Results []struct {
			Duplicate bool   `json:"duplicate"`
			EventID   string `json:"event_id"`
		} `json:"results"`
	}
	if err := api(c, "POST", "/v1/code/sessions/"+id+"/events", body, &out); err != nil {
		return err
	}
	if len(out.Results) > 0 && out.Results[0].Duplicate {
		fmt.Fprintln(os.Stderr, "note: the server treated this as a duplicate event")
	}
	fmt.Fprintln(os.Stderr, "sent to "+id)
	return nil
}

// uuidV4 names the event. The server rejects a repeat of one, which is what
// makes a retried send idempotent rather than a second turn.
func uuidV4() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		// A UUID that is not random is still unique enough to name one event.
		binary.BigEndian.PutUint64(b[0:], uint64(time.Now().UnixNano()))
		binary.BigEndian.PutUint64(b[8:], uint64(os.Getpid()))
	}
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

func promptText(o opts) (string, error) {
	switch {
	case o.probe:
		return probePrompt, nil
	case o.file != "":
		b, err := os.ReadFile(o.file)
		if err != nil {
			return "", err
		}
		return string(b), nil
	case o.promptSet && strings.TrimSpace(o.prompt) != "":
		return o.prompt, nil
	}
	b, err := io.ReadAll(os.Stdin)
	if err != nil {
		return "", err
	}
	if strings.TrimSpace(string(b)) == "" {
		return "", errors.New("empty prompt")
	}
	return string(b), nil
}

// --------------------------------------------------- reading a session back

func list() error {
	c, err := readCreds()
	if err != nil {
		return err
	}
	var out struct {
		Data []struct {
			ID, Title, Status string
			EnvironmentID     string `json:"environment_id"`
			CreatedAt         string `json:"created_at"`
		} `json:"data"`
	}
	if err := api(c, "GET", "/v1/code/sessions?limit=20", nil, &out); err != nil {
		return err
	}
	names := map[string]string{}
	if envs, err := environments(c); err == nil {
		for _, e := range envs {
			names[e.ID] = e.Name
		}
	}
	for _, s := range out.Data {
		env := names[s.EnvironmentID]
		if env == "" {
			env = s.EnvironmentID
		}
		fmt.Printf("%s  %-8s  %-12s  %s\n", s.ID, s.Status, env, firstLine(s.Title, 60))
	}
	return nil
}

// archive ends a session, which is what frees its container. A headless create
// makes this necessary: nothing else reclaims a session nobody is watching.
func archive(o opts) error {
	id, err := wantSession(o, "--archive")
	if err != nil {
		return err
	}
	c, err := readCreds()
	if err != nil {
		return err
	}
	if err := api(c, "POST", "/v1/code/sessions/"+id+"/archive", map[string]any{}, nil); err != nil {
		return err
	}
	fmt.Println("archived " + id)
	return nil
}

func info(o opts) error {
	id, err := wantSession(o, "--info")
	if err != nil {
		return err
	}
	c, err := readCreds()
	if err != nil {
		return err
	}
	var raw map[string]any
	if err := api(c, "GET", "/v1/code/sessions/"+id, nil, &raw); err != nil {
		return err
	}
	s := shape(raw)
	p := func(label string, v any) { fmt.Printf("%-16s%v\n", label, v) }
	p("session:", fmt.Sprintf("%v   status: %v", s["id"], s["status"]))
	p("title:", s["title"])
	p("environment:", fmt.Sprintf("%v  (%v)", s["environment_id"], s["environment_kind"]))
	meta, _ := s["external_metadata"].(map[string]any)
	p("container cc:", dig(meta, "container_cc_version"))
	p("branches:", render(dig(meta, "current_branches")))
	if h, ok := dig(meta, "turn_handoff").(map[string]any); ok {
		p("worker tools:", joinAny(h["tools"]))
	}
	if cfg, ok := s["config"].(map[string]any); ok {
		p("model:", fmt.Sprintf("%s   origin: %s", render(cfg["model"]), render(cfg["origin"])))
		if srcs, ok := cfg["sources"].([]any); ok {
			for _, x := range srcs {
				m, _ := x.(map[string]any)
				rev := render(m["revision"])
				p("source:", fmt.Sprintf("%v @ %s", m["url"], rev))
			}
		}
	}
	if sum, ok := s["post_turn_summary"].(map[string]any); ok {
		p("summary:", sum["recent_action"])
		p("", sum["status_detail"])
	}
	p("view:", sessionURL(fmt.Sprint(s["id"])))

	fmt.Println("--- provisioning (env_manager_log, oldest first) ---")
	var ev struct {
		Data []struct {
			EventType string      `json:"event_type"`
			Sequence  json.Number `json:"sequence_num"`
			Payload   struct {
				Data struct{ Level, Content string } `json:"data"`
			} `json:"payload"`
		} `json:"data"`
	}
	if err := api(c, "GET", "/v1/code/sessions/"+id+"/events", nil, &ev); err != nil {
		return err
	}
	rows := ev.Data[:0]
	for _, e := range ev.Data {
		if e.EventType == "env_manager_log" {
			rows = append(rows, e)
		}
	}
	// sequence_num arrives as a STRING, so it orders numerically only once parsed.
	sort.SliceStable(rows, func(i, j int) bool {
		a, _ := rows[i].Sequence.Int64()
		b, _ := rows[j].Sequence.Int64()
		return a < b
	})
	if len(rows) == 0 {
		fmt.Println("  (no env_manager_log events)")
	}
	for _, e := range rows {
		fmt.Printf("  [%s] %s\n", e.Payload.Data.Level, e.Payload.Data.Content)
	}
	return nil
}

// shape unwraps the two envelopes this API has used for a session object.
func shape(raw map[string]any) map[string]any {
	for _, k := range []string{"response_shape", "session"} {
		if v, ok := raw[k].(map[string]any); ok {
			return v
		}
	}
	return raw
}

func dig(m map[string]any, k string) any {
	if m == nil {
		return nil
	}
	return m[k]
}

func joinAny(v any) string {
	xs, _ := v.([]any)
	var out []string
	for _, x := range xs {
		out = append(out, fmt.Sprint(x))
	}
	return strings.Join(out, ", ")
}

// conv finds the CONVERSATION a session records into, on the caos server.
//
// THE ID IS NOT THE ONE IN THE URL. A conversation is `cc/<the Claude Code
// INTERNAL session uuid>`, which appears only in hook payloads, and the ref
// spells that HEX-ENCODED — so neither the session id you can see nor a plain
// grep of the refs will find it.
//
// Why bother: the conversation tree is the one place that says what a session
// actually evaluates. A checkout can be rewritten (dev mode rewrites
// .caos-expr) while the conversation still carries the committed file, and
// from inside the session the two are indistinguishable — mcp__caos__read
// shows the conversation's copy, which is the one that matters and the one
// nobody thinks to doubt.
func conv(o opts) error {
	id, err := wantSession(o, "--conv")
	if err != nil {
		return err
	}
	c, err := readCreds()
	if err != nil {
		return err
	}
	var ev struct {
		Data []struct {
			Payload struct {
				SessionID string `json:"session_id"`
			} `json:"payload"`
		} `json:"data"`
	}
	if err := api(c, "GET", "/v1/code/sessions/"+id+"/events", nil, &ev); err != nil {
		return err
	}
	internal := ""
	for _, e := range ev.Data {
		if e.Payload.SessionID != "" {
			internal = e.Payload.SessionID
			break
		}
	}
	if internal == "" {
		return fmt.Errorf("no hook payload in %s yet, so no internal session id.\n"+
			"  A session that has not run a turn has no conversation.", id)
	}
	name := "cc/" + internal
	ref := "refs/caos/v3/conversations/" + fmt.Sprintf("%x", name) + "/head"
	server, from := sessionServer(c, id)
	shown := redactServer(server)
	fmt.Println("session:      " + id)
	fmt.Println("internal id:  " + internal)
	fmt.Println("conversation: " + name)
	fmt.Println("ref:          " + ref)
	fmt.Println("server:       " + shown + "   (" + from + ")")
	lsRef := ref
	if strings.HasPrefix(server, "caos://") {
		if _, err := exec.LookPath("git-remote-caos"); err != nil {
			// This worker cannot speak a ticket, so it cannot ask the stack. What
			// it can ask is the HOST server, which holds the copy that
			// `caos-stack harvest` exports: refs/stacks/<instance>/<the ref>.
			// `git ls-remote` matches a pattern by its tail, so the ref without
			// its leading "refs/" finds it whatever the instance.
			fmt.Println("note:         git-remote-caos is not in this worker, so the stack cannot be asked.")
			fmt.Println("              Looking instead for the copy `caos-stack harvest` exported to the host server.")
			server = hostServer()
			shown = redactServer(server)
			lsRef = strings.TrimPrefix(ref, "refs/")
			fmt.Println("server:       " + shown + "   (this worker's $CAOS_SERVER_URL)")
		}
	}
	cmd := exec.Command("git", "ls-remote", server, lsRef)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	ls, err := cmd.Output()
	if err != nil {
		// A failed query is not an absent conversation, and git's message can
		// quote the URL, which for a ticket is a credential.
		msg := strings.TrimSpace(strings.ReplaceAll(stderr.String(), server, shown))
		fmt.Println("head:         (could not ask " + shown + ")")
		return fmt.Errorf("git ls-remote %s: %v: %s", shown, err, msg)
	}
	head := strings.Fields(string(ls))
	if len(head) == 0 {
		fmt.Println("head:         (not on " + shown + " — no turn recorded yet, or not harvested from the stack)")
		return errors.New("conversation head not found on " + shown)
	}
	fmt.Println("head:         " + head[0])
	if len(head) > 1 {
		fmt.Println("found as:     " + head[1])
	}
	// A ticket stays out of the transcript: the commands below name it by
	// placeholder, and the person substitutes it.
	cmdServer := server
	if strings.HasPrefix(server, "caos://") {
		cmdServer = "<ticket>"
		fmt.Println("\n<ticket> is the caos:// URL after --server= in the setup script that `drive env-show` prints for the session's environment.")
	}
	fmt.Printf(convNote, cmdServer, head[0], head[0], cmdServer)
	return nil
}

// sessionServer finds the caos server a session records into, and says where
// it found it. The only place a cloud session is told its server is the
// `--server=` on its environment's setup line (integrations/claude-code/cloud),
// so that is read first: this worker's own $CAOS_SERVER_URL is the HOST's
// server, and a session driven at a `caos-stack` test stack records into the
// stack, so asking the host for its conversation finds nothing — or, if an id
// ever collided, the wrong one. The host's is the fallback for a session whose
// environment names none.
func sessionServer(c creds, id string) (string, string) {
	host := hostServer()
	var raw map[string]any
	if err := api(c, "GET", "/v1/code/sessions/"+id, nil, &raw); err != nil {
		return host, "this worker's $CAOS_SERVER_URL; the session could not be read: " + err.Error()
	}
	envID, _ := shape(raw)["environment_id"].(string)
	if envID == "" {
		return host, "this worker's $CAOS_SERVER_URL; the session names no environment"
	}
	env, err := envGet(c, envID)
	if err != nil {
		return host, "this worker's $CAOS_SERVER_URL; " + err.Error()
	}
	script, _ := dig(env["config"].(map[string]any), "init_script").(string)
	if m := serverFlagRe.FindStringSubmatch(script); m != nil {
		return m[1], "--server= in the setup script of " + envID
	}
	return host, "this worker's $CAOS_SERVER_URL; the setup script of " + envID + " names no --server="
}

var serverFlagRe = regexp.MustCompile(`--server=(\S+)`)

// hostServer is the server this worker itself runs against.
func hostServer() string {
	if s := os.Getenv("CAOS_SERVER_URL"); s != "" {
		return s
	}
	return "http://localhost:9090"
}

// redactServer is a server URL fit to print. A `caos://` ticket is the
// capability to drive that server (integrations/claude-code/cloud/README.md),
// and this output is read by a model, so only its first characters show.
func redactServer(s string) string {
	if rest, ok := strings.CutPrefix(s, "caos://"); ok {
		return "caos://" + trunc(rest, 12) + "…"
	}
	return s
}

// ---------------------------------------------- environment definitions

// An environment's DEFINITION — its setup script, its variables, its network
// config — is served to the same token as everything else. The listing answers
// `config: null`; the per-environment route is what carries it.
//
//	GET  /v1/environment_providers            list (id, name, kind, state)
//	GET  /v1/environment_providers/<id>       the whole definition
//	POST /v1/environment_providers/cloud/create
//	POST /v1/environment_providers/<id>       replace name/description/config
//	POST /v1/environment_providers/<id>/delete
func envGet(c creds, id string) (map[string]any, error) {
	var env map[string]any
	if err := api(c, "GET", "/v1/environment_providers/"+id, nil, &env); err != nil {
		return nil, err
	}
	if _, ok := env["config"].(map[string]any); !ok {
		return nil, fmt.Errorf("%s came back without a config", id)
	}
	return env, nil
}

func envConfig(o opts) error {
	c, err := readCreds()
	if err != nil {
		return err
	}
	if o.arg == "" {
		envs, err := environments(c)
		if err != nil {
			return err
		}
		if len(envs) == 0 {
			return errors.New("--env-config: this account has no environments")
		}
		for _, e := range envs {
			fmt.Printf("%s   %s   [%s]   %s\n", e.ID, e.Name, e.State, e.Kind)
		}
		return nil
	}
	id, err := resolveEnv(c, o.arg)
	if err != nil {
		return err
	}
	env, err := envGet(c, id)
	if err != nil {
		return err
	}
	envPrint(env)
	return nil
}

func envUpdate(o opts) error {
	if o.arg == "" {
		return errors.New("--env-update: give the name or id to update")
	}
	c, err := readCreds()
	if err != nil {
		return err
	}
	id, err := resolveEnv(c, o.arg)
	if err != nil {
		return err
	}
	env, err := envGet(c, id)
	if err != nil {
		return err
	}
	if err := envMutate(env, o); err != nil {
		return err
	}
	// The update REPLACES name, description and config, so it is sent the
	// definition just read with the requested edits folded in. A field left
	// out of `config` is a field cleared.
	body := map[string]any{"name": env["name"], "description": describe(env), "config": env["config"]}
	var out map[string]any
	if err := api(c, "POST", "/v1/environment_providers/"+id, body, &out); err != nil {
		return err
	}
	fmt.Println("updated " + id + ":")
	envPrint(out)
	return nil
}

func envCreate(o opts) error {
	if o.arg == "" {
		return errors.New("--env-create: give a name (and optionally --from <env> to clone)")
	}
	c, err := readCreds()
	if err != nil {
		return err
	}
	var seed map[string]any
	if o.from != "" {
		id, err := resolveEnv(c, o.from)
		if err != nil {
			return fmt.Errorf("--from %q: %v", o.from, err)
		}
		if seed, err = envGet(c, id); err != nil {
			return err
		}
	} else if err := json.Unmarshal([]byte(defaultEnvSeed), &seed); err != nil {
		return err
	}
	o.rename = o.arg // envMutate names the new environment
	if err := envMutate(seed, o); err != nil {
		return err
	}
	body := map[string]any{
		"name": seed["name"], "kind": "anthropic_cloud",
		"description": describe(seed), "config": seed["config"],
	}
	var resp map[string]any
	if err := api(c, "POST", "/v1/environment_providers/cloud/create", body, &resp); err != nil {
		return err
	}
	if resp["environment_id"] == nil {
		return fmt.Errorf("--env-create: no environment_id in the answer: %v", resp)
	}
	fmt.Printf("created %v (%s)\n", resp["environment_id"], o.arg)
	envPrint(resp)
	return nil
}

func envDelete(o opts) error {
	if o.arg == "" {
		return errors.New("--env-delete: give the name or id to delete")
	}
	c, err := readCreds()
	if err != nil {
		return err
	}
	id, err := resolveEnv(c, o.arg)
	if err != nil {
		return err
	}
	if err := api(c, "POST", "/v1/environment_providers/"+id+"/delete", map[string]any{}, nil); err != nil {
		return err
	}
	fmt.Println("deleted " + id)
	return nil
}

// envMutate applies --init-script/--set-env/--unset-env/--rename in place.
func envMutate(env map[string]any, o opts) error {
	cfg, ok := env["config"].(map[string]any)
	if !ok {
		cfg = map[string]any{}
		env["config"] = cfg
	}
	if o.initScript != "" {
		b, err := os.ReadFile(o.initScript)
		if err != nil {
			return err
		}
		cfg["init_script"] = string(b)
	}
	if len(o.setEnv)+len(o.unsetEnv) > 0 {
		vars, ok := cfg["environment"].(map[string]any)
		if !ok {
			vars = map[string]any{}
			cfg["environment"] = vars
		}
		for _, kv := range o.setEnv {
			k, v, found := strings.Cut(kv, "=")
			if !found {
				return fmt.Errorf("--set-env %q is not KEY=VALUE", kv)
			}
			vars[k] = v
		}
		for _, k := range o.unsetEnv {
			delete(vars, k)
		}
	}
	if o.rename != "" {
		env["name"] = o.rename
	}
	return nil
}
func describe(env map[string]any) any {
	if d, ok := env["description"].(string); ok && d != "" {
		return d
	}
	if n, ok := env["name"]; ok {
		return n
	}
	return "created by drive"
}

func envPrint(env map[string]any) {
	cfg, _ := env["config"].(map[string]any)
	net, _ := dig(cfg, "network_config").(map[string]any)
	fmt.Printf("name:    %v   id: %v   state: %v\n", env["name"], env["environment_id"], env["state"])
	fmt.Printf("type:    %v/%v   cwd: %v\n", dig(cfg, "environment_type"), dig(cfg, "sub_type"), dig(cfg, "cwd"))
	fmt.Printf("network: hosts=%s  mcp=%v\n", render(dig(net, "allowed_hosts")), dig(net, "allow_mcp_servers"))
	fmt.Println("--- environment variables ---")
	if vars, ok := dig(cfg, "environment").(map[string]any); ok {
		keys := make([]string, 0, len(vars))
		for k := range vars {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			fmt.Printf("  %s=%v\n", k, vars[k])
		}
	}
	fmt.Println("--- setup / init script ---")
	if s, ok := dig(cfg, "init_script").(string); ok && s != "" {
		fmt.Println(s)
	} else {
		fmt.Println("(none)")
	}
}

func trunc(s string, n int) string {
	if len(s) > n {
		return s[:n]
	}
	return s
}

// ------------------------------------------------------------------- text

// usage is the INTERNAL command line runWorker builds, for reading the code
// and nothing else — every one of these is refused on the host. What the verbs
// mean, and which arguments each takes, is the `help` in `.caos-expr`.
const usage = `drive — the worker's own flags. Callers use the entry:
  caos-cli run --base:@=integrations/claude-code/drive --verb=… --at="$(date +%s)"

  --env --repo --ref --title --prompt-bearing positional | -f <file> | -c <session>
  --probe --list --archive --info --conv
  --env-config --env-update --env-create --env-delete
  --init-script --set-env --unset-env --rename --from
`

// defaultEnvSeed is what --env-create starts from with no --from: the same
// shape the CLI creates a default environment with.
const defaultEnvSeed = `{"config":{"environment_type":"anthropic","sub_type":"ccr","cwd":"/home/user","init_script":"","environment":{},"languages":[{"name":"python","version":"3.11"},{"name":"node","version":"20"}],"network_config":{"allowed_hosts":["*"],"allow_default_hosts":true,"allow_mcp_servers":true}}}`

const convNote = `
Read its tree in a SCRATCH BARE REPO -- never your working checkout. A
--depth=1 fetch into a repo you push from records a shallow boundary, and this
server refuses shallow pushes ("shallow pushes are not accepted"), so every
later push from that checkout dies for an unrelated-looking reason.

  git init -q --bare /tmp/conv.git
  git -C /tmp/conv.git fetch -q --depth=1 %s %s
  git -C /tmp/conv.git ls-tree %s
  git -C /tmp/conv.git cat-file blob <blob-sha>       # e.g. .caos-expr

Every conversation, with the names decoded:

  git ls-remote %s 'refs/caos/v3/conversations/*' |
    while read -r sha ref; do
      hex=$(echo "$ref" | cut -d/ -f5)
      echo "$sha $(echo "$hex" | fold -w2 | while read -r b; do printf "\x$b"; done)"
    done
`

// probePrompt is a STANDALONE prompt: it names no caos config and assumes
// nothing is installed, so it reads a plain cloud env as readily as a caos
// one. The embedded script runs in ONE shell call and brackets its output so
// the reader can lift it out of the transcript; the no-shell branch keeps the
// probe useful on an env whose settings deny Bash.
const probePrompt = `Report the ENVIRONMENT of this cloud container verbatim -- do not summarize or
interpret, I need the raw facts.

If you have a Bash/shell tool: run the single script below in ONE Bash call and
paste its entire stdout+stderr, unedited, between the BEGIN/END markers it
prints.

If you do NOT have a Bash tool: say so plainly, then between markers
"===== CAOS-ENV-REPORT BEGIN =====" and "===== CAOS-ENV-REPORT END ====="
report instead (a) every MCP server and every tool you can find (search the tool
registry broadly, several queries), (b) any skills and connectors, (c) your
current working directory and model, and (d) anything else observable about the
environment. Do not stop at the first search.

------------------------------- run this -------------------------------
{
echo "===== CAOS-ENV-REPORT BEGIN ====="
echo "## identity"
uname -a; echo "user=$(whoami) uid=$(id -u) cwd=$(pwd) HOME=$HOME shell=$SHELL"
echo "## env (values of SECRET/TOKEN/KEY/PASSWORD/PAT/CRED vars masked)"
env | sort | awk -F= '{k=$1; v=substr($0,length(k)+2);
  if (k ~ /SECRET|TOKEN|KEY|PASSWORD|CRED/ || k ~ /(^|_)PAT(_|$)/) printf "%s=<redacted len=%d>\n",k,length(v);
  else print}'
echo "## caos install"
ls -la /usr/local/bin 2>/dev/null | grep -iE 'caos|dumbpipe' || echo "(none in /usr/local/bin)"
for b in caos caos-serve dumbpipe; do printf '%s: ' "$b"; command -v "$b" || echo "(absent)"; done
caos --version 2>&1 | head -1
echo "-- build record --"; cat /usr/local/share/caos/build 2>/dev/null || echo "(none)"
echo "-- setup stamp --"; cat /usr/local/share/caos/setup-stamp 2>/dev/null || echo "(none)"
echo "-- dev stamp --"; cat /usr/local/share/caos/dev-stamp 2>/dev/null || echo "(none)"
echo "## claude / mcp config"
for f in "$HOME/.claude/settings.json" "$HOME/.claude.json" "$HOME/.mcp.json" ".claude/settings.json" ".mcp.json"; do
  echo "-- $f --"
  if [ -f "$f" ]; then
    case "$f" in
      *.claude.json) jq '{mcpServers}' "$f" 2>/dev/null || grep -aA40 mcpServers "$f" || echo "(unreadable)";;
      *) cat "$f";;
    esac
  else echo "(absent)"; fi
done
echo "## processes"
ps -eo pid,ppid,pgid,sess,etime,args 2>/dev/null | grep -iE 'caos|dumbpipe|mcp serve' | grep -v grep || echo "(none)"
echo "## listening tcp (hex local addr:port, state 0A=LISTEN)"
awk 'NR>1 && $4=="0A"{print $2}' /proc/net/tcp 2>/dev/null; awk 'NR>1 && $4=="0A"{print $2}' /proc/net/tcp6 2>/dev/null
echo "## caos logs"
for l in /tmp/caos-hook.log /tmp/caos-warm.log /tmp/caos-tunnel.log; do echo "-- $l --"; cat "$l" 2>/dev/null || echo "(none)"; done
echo "## caos server reachability"
url="${CAOS_SERVER_URL:-}"; [ -z "$url" ] && url="$(git config --get remote.caos.url 2>/dev/null || true)"
echo "caos remote/url: ${url:-<none found>}"
[ -n "$url" ] && curl -sS -m 8 -o /dev/null -w "  $url -> HTTP %{http_code} in %{time_total}s\n" "$url" 2>&1 || echo "  (no url to probe)"
echo "===== CAOS-ENV-REPORT END ====="
} 2>&1
--------------------------------- end ----------------------------------

Paste everything from "===== CAOS-ENV-REPORT BEGIN =====" through
"===== CAOS-ENV-REPORT END =====" verbatim, then stop.
`

// render prints a field as it arrived. A string stays bare; anything else goes
// back through JSON, because Go's default for a decoded map ("map[:main]") is
// neither the shape the API sent nor one a reader can act on.
func render(v any) string {
	switch t := v.(type) {
	case nil:
		return "(none)"
	case string:
		if t == "" {
			return "(none)"
		}
		return t
	}
	b, err := json.Marshal(v)
	if err != nil {
		return fmt.Sprint(v)
	}
	return string(b)
}

// ------------------------------------------------------------------ worker

// AS A CAOS WORKER, this reads its arguments from /cas/args. The runner gives
// a worker no command line, so the arguments the caller curried arrive as
// files, one per name, and this turns them back into the flags above.
//
// `at` is refused when missing rather than defaulted: a default that varies
// would take the choice of staleness away from the caller, and one that does
// not vary would fix nothing. What it is for is in `.caos-expr`, with the rest
// of the calling contract.
//
// THE READ VERBS ARE ACCOUNT-SCOPED, which a memoized result may only be
// because the secret's entropy pins that identity in `secret-hash` (SPEC.md,
// "Correctness requirements"). Rotate the entropy when the token starts naming
// a different account.
// a different account.
const argsDir = "/cas/args"

const secretsDir = "/secret"

// inWorker is true when this is running as a caos worker: /cas/args is the
// runner's doing and exists nowhere else.
func inWorker() bool {
	st, err := os.Stat(argsDir)
	return err == nil && st.IsDir()
}

// workerArg reads one curried argument, absent as "". `caos get` materializes
// the placeholder the runner left; a name that was never bound has no file.
func workerArg(name string) (string, error) {
	path := filepath.Join(argsDir, name)
	if _, err := os.Stat(path); err != nil {
		return "", nil
	}
	if out, err := exec.Command("caos", "get", path).CombinedOutput(); err != nil {
		return "", fmt.Errorf("caos get %s: %v: %s", path, err, strings.TrimSpace(string(out)))
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("reading %s: %v", path, err)
	}
	return strings.TrimRight(string(b), "\n"), nil
}

// workerVerbs maps `verb` to the flags `run` already dispatches on. The worker
// surface is deliberately the same surface, so there is one set of behaviours
// to reason about and one set to document.
var workerVerbs = map[string]string{
	"start": "", "send": "", "probe": "--probe", "list": "--list", "info": "--info", "conv": "--conv",
	"archive": "--archive", "env-list": "--env-config", "env-show": "--env-config",
	"env-update": "--env-update", "env-create": "--env-create", "env-delete": "--env-delete",
}

// runWorker turns the curried args into a command line, runs it, and reports
// what it printed as the job's result.
func runWorker() error {
	verb, err := workerArg("verb")
	if err != nil {
		return err
	}
	if verb == "" {
		return errors.New("no `verb` argument: bind --verb=start|send|list|info|conv|archive|env-list|env-show|env-update|env-create|env-delete")
	}
	flag, known := workerVerbs[verb]
	if !known {
		return fmt.Errorf("unknown verb %q", verb)
	}

	// `at` is read and then dropped: it is in the ArgTree, which is where it
	// does its work, and the command line below never sees it.
	at, err := workerArg("at")
	if err != nil {
		return err
	}
	if at == "" {
		return fmt.Errorf("verb %q needs an `at` argument holding the current time, e.g. "+
			"--at=\"$(date +%%s)\".\n  Without one this job's ArgTree repeats and the memo answers "+
			"with the earlier run's result — a %s that never reached the API.", verb, verbDid(verb))
	}

	var argv []string
	if flag != "" {
		argv = append(argv, flag)
	}
	// The bare token carries the subject of the verb: a session for `info`,
	// `conv` and `archive`, an environment for the env verbs.
	subject := map[string]string{
		"info": "session", "conv": "session", "archive": "session",
		"env-show": "env", "env-update": "env", "env-create": "env", "env-delete": "env",
	}[verb]
	for _, pair := range [][2]string{
		{"env", "--env"}, {"repo", "--repo"}, {"ref", "--ref"}, {"title", "--title"},
		{"from", "--from"}, {"rename", "--rename"},
	} {
		v, err := workerArg(pair[0])
		if err != nil {
			return err
		}
		if v == "" || pair[0] == subject {
			continue
		}
		argv = append(argv, pair[1], v)
	}
	// `init-script` is the SCRIPT, not a path to one: it arrives as a blob like
	// any other argument, and --init-script reads a file, so what is passed is
	// where the runner put it.
	if _, err := workerArg("init-script"); err != nil {
		return err
	} else if _, err := os.Stat(filepath.Join(argsDir, "init-script")); err == nil {
		argv = append(argv, "--init-script", filepath.Join(argsDir, "init-script"))
	}
	for _, pair := range [][2]string{{"set-env", "--set-env"}, {"unset-env", "--unset-env"}} {
		v, err := workerArg(pair[0])
		if err != nil {
			return err
		}
		for _, line := range strings.Split(v, "\n") {
			if strings.TrimSpace(line) != "" {
				argv = append(argv, pair[1], line)
			}
		}
	}
	if subject != "" {
		v, err := workerArg(subject)
		if err != nil {
			return err
		}
		if v == "" {
			return fmt.Errorf("verb %q needs a `%s` argument", verb, subject)
		}
		argv = append(argv, v)
	}
	if verb == "send" || verb == "probe" {
		session, err := workerArg("session")
		if err != nil {
			return err
		}
		// `probe` carries its own prompt, so it may either target a session or
		// start one; `send` has nothing to say without a session.
		if session == "" && verb == "send" {
			return errors.New("verb \"send\" needs a `session` argument")
		}
		if session != "" {
			argv = append(argv, "-c", session)
		}
	}
	if verb == "start" || verb == "send" {
		// The prompt is a FILE argument, never a curried string: a prompt
		// carries newlines and quotes, and a blob holds them unchanged.
		if _, err := os.Stat(filepath.Join(argsDir, "prompt")); err != nil {
			return errors.New("verb " + verb + " needs a `prompt` argument")
		}
		if _, err := workerArg("prompt"); err != nil {
			return err
		}
		argv = append(argv, "-f", filepath.Join(argsDir, "prompt"))
	}

	// Both streams are the report: a worker's stderr is where the session id's
	// context and every warning goes, and a reader of the result wants them.
	var out strings.Builder
	stdout, stderr := os.Stdout, os.Stderr
	r, w, err := os.Pipe()
	if err != nil {
		return err
	}
	os.Stdout, os.Stderr = w, w
	done := make(chan struct{})
	go func() { io.Copy(&out, r); close(done) }()
	runErr := run(argv)
	w.Close()
	<-done
	os.Stdout, os.Stderr = stdout, stderr
	fmt.Fprint(os.Stderr, out.String())
	if runErr != nil {
		return runErr
	}
	return report(out.String())
}

// verbDid names what a memo hit would have silently skipped, so the error says
// what did not happen rather than only what is missing.
func verbDid(verb string) string {
	switch verb {
	case "start":
		return "session that was never created"
	case "send":
		return "prompt that was never delivered"
	case "archive":
		return "session that was never archived"
	case "env-update", "env-create", "env-delete":
		return "change that was never made"
	}
	return "reading"
}

// report writes the worker's result to /cas/out.
func report(body string) error {
	const path = "/tmp/drive-report"
	if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
		return err
	}
	if out, err := exec.Command("caos", "put", path, "/cas/out").CombinedOutput(); err != nil {
		return fmt.Errorf("caos put: %v: %s", err, strings.TrimSpace(string(out)))
	}
	return nil
}

// requireWorker refuses the verbs outside a caos job.
//
// Every one of them reaches the API with the token at /secret, and a run on
// the host reaches it with whatever ~/.claude/.credentials.json happens to
// hold — a DIFFERENT account's, a stale one, or one with the wrong scopes.
// Two ways to drive the same sessions is one more than there should be, and
// the second one leaves no job to look at afterwards.
//
// The host path is not a convenience worth keeping: `caos-cli run` is barely
// longer, and it records what it did.
func requireWorker(o opts) error {
	if inWorker() {
		return nil
	}
	verb := "start"
	for name, flag := range workerVerbs {
		if flag != "" && flag == "--"+o.mode {
			verb = name
		}
	}
	if o.mode == "" && o.session != "" {
		verb = "send"
	}
	return fmt.Errorf(`this runs as a caos worker, not on the host. The same call is

    caos-cli run --base:@=integrations/claude-code/drive \
      --verb=%s ... --at="$(date +%%s)"

  so the job is recorded, and the token comes from /secret/%s rather than
  from whichever login this machine happens to hold. Minting that token is
  the one thing that does run here: see authorize.go.`, verb, secretName)
}
