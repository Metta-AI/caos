// Drive a Claude Code cloud session from a terminal (or another Claude session).
//
//	go run drive.go --env Caos --repo Metta-AI/caos-session 'a prompt'
//
// A NEW SESSION NAMES ITS ENVIRONMENT AND ITS REPOSITORY, and neither is a
// property of the directory this runs in. `claude --cloud` cannot do that:
// its `--environment` takes only a self-hosted `ccpool_…` id, it has no repo
// flag at all, and the repository it sends is whatever `git remote get-url
// origin` says in the current directory. So the create goes straight to the
// API underneath it — POST /v1/code/sessions, which takes `environment_id`
// and a `git_repository` source — and the prompt is delivered afterwards over
// the ordinary continue path. The caos checkout stops being the wrong place
// to start a session from: there is nothing left for the cwd to decide.
//
// Without --env/--repo the old behaviour is unchanged: `claude --cloud` under
// script(1), opening the cwd's repo, which then has to be a caos CLIENT repo.
//
//	drive --env Caos --repo Metta-AI/caos-session 'prompt'
//	drive --env Caos --repo Metta-AI/caos-session@some-branch 'prompt'
//	drive 'a brand new session prompt'      # cwd's repo, default environment
//	drive -c <session> 'a follow-up'        # inject into an existing session
//	drive -f prompt.txt                     # read the prompt from a file
//	drive --probe                           # dump the cloud env (new session)
//	drive -c <session> --probe              # dump the env of a running session
//	drive --list                            # recent sessions (id, env, status)
//	drive --archive <session>               # end a session and free its container
//	drive --info <session>                  # a session's env, from OUTSIDE
//	drive --conv <session>                  # the session's CONVERSATION on caosd
//	drive --env-config                      # LIST cloud environments (id + name)
//	drive --env-config Caos                 # print an env's setup script + vars
//	drive --env-update Caos --init-script F # replace an env's setup script
//	drive --env-update Caos --set-env K=V   # set/unset env vars (--unset-env K)
//	drive --env-create NAME --from Caos     # create (optionally cloning an env)
//	drive --env-delete NAME                 # delete an environment
//	drive --mint                            # mint the token a caos secret holds
//
// A SESSION ID HAS TWO SPELLINGS of the same session: the API and this tool
// print `cse_<suffix>`, and claude.ai/code URLs carry `session_<suffix>`. The
// API accepts either, so anything here takes either, or the URL — see
// sessionID. A pattern that matches only `session_` finds nothing in an id the
// API itself minted.
//
// `--info` reads a session's environment from OUTSIDE the session, over the
// code API -- no prompt, no in-container shell. It prints the environment id
// and kind, the container Claude Code version, the repo branches, the worker's
// tool set, the auto summary, and the env_manager_log trace (sandbox alloc,
// repo clone, mounted-vs-installed client, setup-script run). NOTE: this API
// does NOT serve the environment DEFINITION -- the setup-script text and
// env-var VALUES are not exposed to a session-scoped token; for those use
// --env-config, or --probe.
//
// `--probe` reads the ENVIRONMENT from INSIDE the session, with no dependence
// on caos being installed or healthy: it asks the session's own Claude Code to
// report the container. On any env that allows a shell (the default) it dumps
// env vars, the Claude/MCP config, installed binaries, processes, listening
// ports and the caos logs; on an env that denies the shell it falls back to
// listing the MCP servers, tools, skills and cwd it can see. The answer comes
// back in the run log, not here -- read it with the run-log API, or, in Claude
// Code, RemoteTrigger get_run_log.
//
// ONE API, ONE CREDENTIAL. Sessions and environments are both api.anthropic.com,
// and both answer to an Anthropic OAuth token carrying the
// `user:sessions:claude_code` scope — creating a session, prompting it,
// listing, archiving, and reading or writing an environment's whole definition
// including its setup script and its variables. An API key is not one of
// these; see readCreds for where the token is looked for.
//
// An environment's variables are not secrets ("visible to anyone using this
// environment"), so they print in full.
//
// IT IS ALSO A CAOS WORKER. Run with no command line and /cas/args present, it
// reads the same arguments from there and its token from /secret — see
// runWorker at the bottom. Stdlib only, so the prelude std/go copies beside it
// is never consulted, there is no vendorHash, and no module to keep in step.
package main

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
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
	days       string // --mint: how long the minted token should last
	checkReuse bool   // --mint: spend the refresh token twice, to learn if it is single-use
	probe      bool
	anyRepo    bool
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
			fmt.Print(usage)
			return nil
		case "--probe":
			o.probe = true
		case "--any-repo":
			o.anyRepo = true
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
		case "--mint":
			o.mode = "mint"
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
		case "--authorize":
			o.mode = "authorize"
		case "--check-reuse":
			o.checkReuse = true
		case "--days":
			o.days, err = next(&i, a)
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
	case "authorize":
		return authorize(o)
	case "mint":
		return mint(o)
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
				"  The secret is granted only to a job whose ArgTree is a superset of one of\n"+
				"  its readers, so check `reader=` in .caos-secrets/%s.", secretsDir, secretName, secretName)
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
	warnUnpinned(o)
	envID, err := chooseEnv(c, o)
	if err != nil {
		return err
	}
	repo := o.repo
	if repo == "" {
		if repo, err = originRepo(); err != nil {
			return err
		}
	}
	repoURL, ref, err := parseRepo(repo, o.ref)
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

// chooseEnv resolves --env, or picks the one a bare `claude --cloud` would:
// the `remote.defaultEnvironmentId` settings key, else the first hosted
// environment the account has. A worker has no settings, so there it is the
// list or nothing.
func chooseEnv(c creds, o opts) (string, error) {
	if o.env != "" {
		return resolveEnv(c, o.env)
	}
	if id := defaultEnvFromSettings(); id != "" {
		return id, nil
	}
	envs, err := environments(c)
	if err != nil {
		return "", err
	}
	for _, e := range envs {
		if e.Kind == "anthropic_cloud" {
			fmt.Fprintf(os.Stderr, "env:  %s (%s), the first hosted one — name it with --env\n", e.Name, e.ID)
			return e.ID, nil
		}
	}
	return "", errors.New("--env was not given and this account has no hosted environment to fall back on")
}

// defaultEnvFromSettings reads the key `/remote-env` writes. Only the user and
// policy files are consulted: a project or local settings file travels with a
// checkout, and where a session RUNS is not a property of the code it opens.
func defaultEnvFromSettings() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	var s struct {
		Remote struct {
			DefaultEnvironmentID string `json:"defaultEnvironmentId"`
		} `json:"remote"`
	}
	for _, p := range []string{
		"/Library/Application Support/ClaudeCode/managed-settings.json",
		"/etc/claude-code/managed-settings.json",
		filepath.Join(home, ".claude", "settings.json"),
	} {
		if readJSON(p, &s) == nil && s.Remote.DefaultEnvironmentID != "" {
			return s.Remote.DefaultEnvironmentID
		}
	}
	return ""
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

// originRepo is the repository of the checkout this runs in, used when --repo
// is not given. It is the one thing the cwd still decides, and it decides it
// explicitly rather than by being the directory a subprocess happened to run in.
func originRepo() (string, error) {
	out, err := exec.Command("git", "remote", "get-url", "origin").Output()
	if err != nil {
		wd, _ := os.Getwd()
		return "", fmt.Errorf("--repo was not given and %s has no `origin` remote to fall back on", wd)
	}
	return strings.TrimSpace(string(out)), nil
}

// warnUnpinned is advice, not a gate: a session on a repository that does not
// pin caos dies in its setup phase, four minutes and one container later, and
// that is worth saying before the container is paid for rather than after.
//
// The pin test is bootstrap.go's own (readLock): nodes.<root>.inputs.caos
// names a node KEY, and only that node's locked.rev is authoritative. A
// `follows` input is an array rather than a key and carries no lock, so it is
// not a pin — which is why this asks for the rev rather than for the input's
// mere presence. It can only be asked of a repository that is checked out
// here, so a named --repo is never checked.
func warnUnpinned(o opts) {
	if o.anyRepo || o.repo != "" {
		return
	}
	root, err := exec.Command("git", "rev-parse", "--show-toplevel").Output()
	if err != nil {
		return
	}
	dir := strings.TrimSpace(string(root))
	if lockRev(filepath.Join(dir, "flake.lock")) != "" {
		return
	}
	fmt.Fprintf(os.Stderr, `warning: %s does not pin caos, so its setup script will fail.
  A caos client repo has flake.lock pinning a 'caos' input by revision, a root
  .caos-expr mounting that input's std, AGENTS.md and .caos-secrets. The code to
  work on is imported into the conversation, not cloned, so --repo names the
  client repo — e.g. --repo Metta-AI/caos-session.
`, dir)
}
func lockRev(path string) string {
	var lock struct {
		Root  string `json:"root"`
		Nodes map[string]struct {
			Inputs map[string]json.RawMessage `json:"inputs"`
			Locked struct {
				Rev string `json:"rev"`
			} `json:"locked"`
		} `json:"nodes"`
	}
	if err := readJSON(path, &lock); err != nil {
		return ""
	}
	root := lock.Root
	if root == "" {
		root = "root"
	}
	raw, ok := lock.Nodes[root].Inputs["caos"]
	if !ok {
		return ""
	}
	var key string
	if json.Unmarshal(raw, &key) != nil {
		return "" // an array is a `follows`, which carries no lock
	}
	return lock.Nodes[key].Locked.Rev
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
	server := os.Getenv("CAOS_SERVER_URL")
	if server == "" {
		server = "http://localhost:9090"
	}
	fmt.Println("session:      " + id)
	fmt.Println("internal id:  " + internal)
	fmt.Println("conversation: " + name)
	fmt.Println("ref:          " + ref)
	ls, _ := exec.Command("git", "ls-remote", server, ref).Output()
	head := strings.Fields(string(ls))
	if len(head) == 0 {
		fmt.Println("head:         (not on " + server + " — wrong server, or no turn yet)")
		return errors.New("conversation head not found on " + server)
	}
	fmt.Println("head:         " + head[0])
	fmt.Printf(convNote, server, head[0], head[0], server)
	return nil
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

const usage = `drive — start and inspect Claude Code cloud sessions.

  drive --env <env> --repo <owner/repo[@ref]> 'prompt'   start one (no cwd needed)
  drive 'prompt'                    start one on the cwd's repo, default env
  drive -c <session> 'follow-up'    inject a prompt into a running session
  drive -f <file>                   read the prompt from a file (with either)
  drive --probe [-c <session>]      ask a session to dump its container

  drive --list                      recent sessions (id, status, env, title)
  drive --archive <session>         end a session and free its container
  drive --info <session>            a session's environment, from outside
  drive --conv <session>            the conversation it records into, on caosd

  drive --env-config [<env>]        list environments, or print one's definition
  drive --env-update <env> [--init-script F] [--set-env K=V] [--unset-env K] [--rename N]
  drive --env-create <name> [--from <env>]
  drive --env-delete <env>

  drive --authorize [--days N]      consent at claude.ai for a long-lived token
  drive --mint [--days N]           exchange the CLI login's refresh token instead

  --title T    the session's title (default: the prompt's first line)
  --ref R      the revision to check out (same as --repo owner/repo@R)
  --any-repo   skip the caos-pin warning on the cwd fallback
  --days N     --mint only: the lifetime to ask for (default 365; the server decides)
  --check-reuse  --mint only: spend the refresh token twice, to learn if it is single-use

<env> is an environment's name or its env_… id; <session> is cse_…, session_…,
or a claude.ai/code URL. Omitting --repo falls back to the cwd's origin remote,
and omitting --env to the default environment.

Every mode needs one Anthropic OAuth token: $CLAUDE_CODE_OAUTH_TOKEN, else
/secret/claude-oauth-token, else the CLI's own ~/.claude/.credentials.json.
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

// AS A CAOS WORKER, this same program reads its arguments from /cas/args and
// its token from /secret. The runner gives a worker no command line, so the
// arguments the caller curried become files, one per name, and this turns them
// back into the flags above.
//
//	caos-cli run --base:@=integrations/claude-code/drive \
//	  --verb=start --env=Caos --repo=Metta-AI/caos-session --prompt:@=./p.txt \
//	  --at="$(date +%s)"
//
// EVERY VERB TAKES `at`, AND IT IS THE CURRENT TIME. Nothing here is a pure
// function of its arguments: `start` and `send` change a session, and `list`,
// `info` and the env reads ask a service that moves on its own. A worker's
// result is memoized on its ArgTree, so without something that differs, a
// second `start` answers with the FIRST session's id and creates nothing —
// the session looks started and no container exists — and a second `list`
// answers with a listing from whenever the first one ran.
//
// It is a TIME rather than a nonce so that staleness is the caller's to
// choose: `date +%s` is always fresh, `date +%Y%m%d%H%M` reuses an answer for
// up to a minute, and a fixed value deliberately pins one. Refused when
// missing rather than defaulted — a default that varies would take that choice
// away, and one that does not vary would fix nothing.
//
// THE READ VERBS ARE ACCOUNT-SCOPED, which a memoized result may only be
// because the secret's entropy pins that identity in `secret-hash` (SPEC.md,
// "Correctness requirements"). Rotate the entropy when the token starts naming
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
	"start": "", "send": "", "list": "--list", "info": "--info", "conv": "--conv",
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
		{"from", "--from"}, {"rename", "--rename"}, {"init-script", "--init-script"},
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
	if verb == "send" {
		session, err := workerArg("session")
		if err != nil {
			return err
		}
		if session == "" {
			return errors.New("verb \"send\" needs a `session` argument")
		}
		argv = append(argv, "-c", session)
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
	return "reading of state as it was then, not now"
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

// ---------------------------------------------------------------- minting

// mint trades the CLI's refresh token for a LONG-LIVED access token carrying
// the scopes this needs, and prints it for a caos secret to hold.
//
// `claude setup-token` is the obvious thing and produces the wrong token: its
// flow asks for the console scopes, and every session and environment route
// answers a token without `user:sessions:claude_code` with
//
//	401 OAuth token lacks a scope this endpoint accepts (oauth_scope_insufficient)
//
// The CLI's own login has the right scopes but a short life and a refresh
// token beside it. This asks the token endpoint for the same scope set with a
// long `expires_in`, which is what makes the result something a secret can
// hold rather than something that goes stale in hours.
//
// THE REFRESH TOKEN MAY ROTATE. The response carries a new one when it does,
// and this writes it back to ~/.claude/.credentials.json — without that, the
// running CLI's login would be the thing this broke. The write is skipped
// when the value is unchanged, and it is temp-and-rename so a crash cannot
// leave a half-written credentials file.
func mint(o opts) error {
	home, err := os.UserHomeDir()
	if err != nil {
		return err
	}
	path := filepath.Join(home, ".claude", ".credentials.json")
	var creds map[string]any
	if err := readJSON(path, &creds); err != nil {
		return fmt.Errorf("%s: %v — run `claude /login` first", path, err)
	}
	oauth, _ := creds["claudeAiOauth"].(map[string]any)
	first, _ := dig(oauth, "refreshToken").(string)
	if first == "" {
		return fmt.Errorf("no refreshToken in %s — run `claude /login` first", path)
	}

	days := 365
	if o.days != "" {
		if _, err := fmt.Sscanf(o.days, "%d", &days); err != nil || days < 1 {
			return fmt.Errorf("--days %q is not a positive number of days", o.days)
		}
	}

	tok, err := exchange(first, days)
	if err != nil {
		return err
	}
	// The write-back comes FIRST: a rotated refresh token that reaches nothing
	// is a broken CLI login, and that matters more than printing a token.
	if err := writeRefresh(path, creds, oauth, first, tok.RefreshToken); err != nil {
		return err
	}
	rotated := tok.RefreshToken != "" && tok.RefreshToken != first

	// THE SERVER DECIDES THE LIFETIME, and it has ignored `expires_in` here —
	// a 365-day request came back as a token good for about a day. Say so
	// rather than let a secret be filled with something that dies tomorrow.
	life := time.Duration(tok.ExpiresIn) * time.Second
	fmt.Fprintf(os.Stderr, "granted: %s\nexpires: %s (%s from now; %d days requested)\n",
		tok.Scope, time.Now().Add(life).Format(time.RFC3339), life.Round(time.Minute), days)
	if !slicesContains(strings.Fields(tok.Scope), sessionsScope) {
		return fmt.Errorf("the token endpoint granted %q, which does not include %s.\n"+
			"  A token without it is refused by every session and environment route, so this\n"+
			"  is not the credential to store.", tok.Scope, sessionsScope)
	}

	// IS A ROTATED-AWAY REFRESH TOKEN STILL USABLE? The whole shape of this
	// depends on the answer. If the one just spent still works, a secret can
	// hold a refresh token and a worker can exchange it for an access token on
	// every run, never storing a credential anywhere. If it is single-use,
	// only something that can PERSIST the replacement can do the exchanging,
	// and a worker cannot.
	//
	// Spending it again is the only way to ask, and it is safe: whichever
	// refresh token is newest and valid ends up in the credentials file.
	if o.checkReuse {
		if !rotated {
			fmt.Fprintln(os.Stderr, "\nreuse: the refresh token did not rotate, so it stays usable.")
		} else if again, err := exchange(first, days); err != nil {
			fmt.Fprintf(os.Stderr, "\nreuse: the spent refresh token is REFUSED (%v).\n"+
				"  It is single-use, so whatever exchanges it must be able to store the\n"+
				"  replacement — which a worker cannot.\n", err)
		} else {
			if err := writeRefresh(path, creds, oauth, tok.RefreshToken, again.RefreshToken); err != nil {
				return err
			}
			fmt.Fprintln(os.Stderr, "\nreuse: the spent refresh token STILL WORKS, so it is not single-use.\n"+
				"  A secret can hold one and a worker can exchange it every run.")
		}
	}

	fmt.Fprintf(os.Stderr, "\nput this in .caos-secrets/%s as `value=`, keeping its\n"+
		"reader= and entropy= lines:\n\n", secretName)
	fmt.Println(tok.AccessToken)
	return nil
}

// oauthToken is what the token endpoint answers with.
type oauthToken struct {
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
	ExpiresIn    int64  `json:"expires_in"`
	Scope        string `json:"scope"`
}

// exchange trades a refresh token for an access token carrying the scopes this
// needs. `expires_in` is a REQUEST, not a setting: the server has answered a
// 365-day one with a token good for about a day.
func exchange(refresh string, days int) (oauthToken, error) {
	var tok oauthToken
	body := map[string]any{
		"grant_type":    "refresh_token",
		"refresh_token": refresh,
		"client_id":     oauthClientID,
		"scope":         strings.Join(wantedScopes, " "),
		"expires_in":    days * 24 * 60 * 60,
	}
	raw, err := json.Marshal(body)
	if err != nil {
		return tok, err
	}
	resp, err := (&http.Client{Timeout: 60 * time.Second}).Post(
		oauthTokenURL, "application/json", bytes.NewReader(raw))
	if err != nil {
		return tok, err
	}
	defer resp.Body.Close()
	answer, err := io.ReadAll(resp.Body)
	if err != nil {
		return tok, err
	}
	if resp.StatusCode != 200 {
		return tok, fmt.Errorf("token endpoint: HTTP %d: %s", resp.StatusCode, trunc(string(answer), 300))
	}
	if err := json.Unmarshal(answer, &tok); err != nil {
		return tok, err
	}
	if tok.AccessToken == "" {
		return tok, errors.New("the token endpoint returned no access_token")
	}
	return tok, nil
}

// writeRefresh stores a rotated refresh token, temp-and-rename so a crash
// cannot leave a half-written credentials file. Unchanged is a no-op.
func writeRefresh(path string, creds, oauth map[string]any, old, new string) error {
	if new == "" || new == old {
		return nil
	}
	oauth["refreshToken"] = new
	updated, err := json.MarshalIndent(creds, "", "  ")
	if err != nil {
		return err
	}
	tmp := path + ".drive-mint"
	if err := os.WriteFile(tmp, updated, 0o600); err != nil {
		return fmt.Errorf("writing %s: %v", tmp, err)
	}
	if err := os.Rename(tmp, path); err != nil {
		return fmt.Errorf("replacing %s: %v (the new refresh token is in %s)", path, err, tmp)
	}
	fmt.Fprintln(os.Stderr, "the refresh token rotated; wrote the new one back to "+path)
	return nil
}

const (
	oauthTokenURL = "https://platform.claude.com/v1/oauth/token"
	// Consent at CLAUDE.AI, not the console: both authorize URLs request the
	// same scope set, and it is the grant that narrows. The console grants the
	// console scopes, which is why setup-token's token has no sessions scope.
	claudeAIAuthorizeURL = "https://claude.com/cai/oauth/authorize"
	// The MANUAL redirect, so no local port has to be opened.
	oauthRedirectURL = "https://platform.claude.com/oauth/code/callback"
	// The PROD client. Claude Code carries three OAuth configs and the other
	// two are a localhost dev factory and a staging one; the prod block is the
	// one with an empty OAUTH_FILE_SUFFIX and mcp-proxy.anthropic.com. A dev
	// client id reaches the real token endpoint and is answered "Client with id
	// … not found", which reads like a revoked client rather than a wrong one.
	oauthClientID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
	sessionsScope = "user:sessions:claude_code"
)

// wantedScopes is the set the CLI's own claude.ai login carries. The sessions
// one is the only one this program needs; the rest ride along so the minted
// token is not narrower than the login it came from.
var wantedScopes = []string{
	"user:profile", "user:inference", sessionsScope,
	"user:mcp_servers", "user:file_upload", "user:plugins",
}

func slicesContains(haystack []string, needle string) bool {
	for _, s := range haystack {
		if s == needle {
			return true
		}
	}
	return false
}

// ------------------------------------------------------- the browser flow

// authorize runs the OAuth authorization-code flow against CLAUDE.AI and asks
// for a long-lived token. It is the only route to a credential a worker can
// hold, and every other one is measured shut:
//
//   - `claude setup-token` consents at the CONSOLE, which grants the console
//     scopes; the session and environment routes refuse the result with
//     401 oauth_scope_insufficient.
//   - a refresh exchange consents nowhere and grants the claude.ai scopes,
//     but the server ignores `expires_in` there — 365 days asked, 8 hours
//     given — and the refresh token is SINGLE-USE: spending it rotates it, and
//     the spent one is answered `invalid_grant`. So nothing that cannot store
//     the replacement can do the exchanging, and a worker cannot store.
//   - a claude.ai browser `sessionKey` is a cookie, not a bearer; the session
//     routes answer it 401 whichever host it is presented to.
//
// `expires_in` rides the CODE exchange rather than the authorize URL, which is
// where setup-token's year comes from. Consenting at claude.ai rather than the
// console is what should keep `user:sessions:claude_code` on the grant. If the
// answer comes back short-lived or narrow anyway, this says so and stores
// nothing.
//
// The redirect is the MANUAL one, so no local port is opened and nothing has
// to survive a browser handing back to a listener: claude.ai shows a code,
// and it is pasted here.
func authorize(o opts) error {
	days := 365
	if o.days != "" {
		if _, err := fmt.Sscanf(o.days, "%d", &days); err != nil || days < 1 {
			return fmt.Errorf("--days %q is not a positive number of days", o.days)
		}
	}
	verifier, err := randomURLSafe(32)
	if err != nil {
		return err
	}
	state, err := randomURLSafe(16)
	if err != nil {
		return err
	}
	sum := sha256.Sum256([]byte(verifier))
	challenge := base64.RawURLEncoding.EncodeToString(sum[:])

	// BUILT IN THE CLIENT'S OWN ORDER, and with its own scope list. The
	// endpoint answers "Authorization failed / Invalid request format" to a
	// narrowed scope set — a subset of what the client is registered for is not
	// a smaller ask, it is a malformed one — so the whole of `authorizeScopes`
	// goes out and the grant narrows it. Order is preserved for the same reason:
	// url.Values.Encode sorts, and nothing here needs to find out whether that
	// matters.
	var q strings.Builder
	for i, kv := range [][2]string{
		{"code", "true"},
		{"client_id", oauthClientID},
		{"response_type", "code"},
		{"redirect_uri", oauthRedirectURL},
		{"scope", strings.Join(authorizeScopes, " ")},
		{"code_challenge", challenge},
		{"code_challenge_method", "S256"},
		{"state", state},
	} {
		if i > 0 {
			q.WriteByte('&')
		}
		q.WriteString(kv[0] + "=" + url.QueryEscape(kv[1]))
	}

	fmt.Fprintf(os.Stderr, "Open this, approve, and paste back what the page shows:\n\n%s?%s\n\ncode: ",
		claudeAIAuthorizeURL, q.String())
	line, err := bufio.NewReader(os.Stdin).ReadString('\n')
	if err != nil && strings.TrimSpace(line) == "" {
		return fmt.Errorf("reading the pasted code: %v", err)
	}
	// The callback page shows `<code>#<state>`; either half alone is accepted
	// so a paste that drops the fragment still works.
	code, pastedState, _ := strings.Cut(strings.TrimSpace(line), "#")
	if code == "" {
		return errors.New("no code pasted")
	}
	if pastedState != "" {
		state = pastedState
	}

	body := map[string]any{
		"grant_type":    "authorization_code",
		"code":          code,
		"redirect_uri":  oauthRedirectURL,
		"client_id":     oauthClientID,
		"code_verifier": verifier,
		"state":         state,
		"expires_in":    days * 24 * 60 * 60,
	}
	raw, err := json.Marshal(body)
	if err != nil {
		return err
	}
	resp, err := (&http.Client{Timeout: 60 * time.Second}).Post(
		oauthTokenURL, "application/json", bytes.NewReader(raw))
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	answer, err := io.ReadAll(resp.Body)
	if err != nil {
		return err
	}
	if resp.StatusCode != 200 {
		return fmt.Errorf("token endpoint: HTTP %d: %s", resp.StatusCode, trunc(string(answer), 300))
	}
	var tok oauthToken
	if err := json.Unmarshal(answer, &tok); err != nil {
		return err
	}
	if tok.AccessToken == "" {
		return errors.New("the token endpoint returned no access_token")
	}

	life := time.Duration(tok.ExpiresIn) * time.Second
	fmt.Fprintf(os.Stderr, "\ngranted: %s\nexpires: %s (%s from now; %d days requested)\n",
		tok.Scope, time.Now().Add(life).Format(time.RFC3339), life.Round(time.Minute), days)
	if !slicesContains(strings.Fields(tok.Scope), sessionsScope) {
		return fmt.Errorf("the grant does not include %s, so this token is refused by every\n"+
			"  session and environment route. Consenting at claude.ai did not widen it.", sessionsScope)
	}
	if life < 24*time.Hour {
		fmt.Fprintf(os.Stderr, "\nNOTE: %s is not a lifetime a secret can hold — the server ignored\n"+
			"the request here too, and a worker-held token is not reachable this way.\n", life.Round(time.Minute))
	}
	fmt.Fprintf(os.Stderr, "\nput this in .caos-secrets/%s as `value=`, keeping its\n"+
		"reader= and entropy= lines:\n\n", secretName)
	fmt.Println(tok.AccessToken)
	return nil
}

func randomURLSafe(n int) (string, error) {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(b), nil
}

// authorizeScopes is the set the client itself requests at either authorize
// URL — the console scope and the claude.ai ones together. It is sent whole
// because the endpoint refuses a subset, and the GRANT is what narrows it:
// consenting at claude.ai yields the claude.ai scopes and drops
// org:create_api_key, which is exactly the set a CLI login carries.
var authorizeScopes = append([]string{"org:create_api_key"}, wantedScopes...)
