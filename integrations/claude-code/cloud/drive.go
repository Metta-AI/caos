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
// TWO APIS, TWO CREDENTIALS. Everything about SESSIONS and the environment
// LIST is api.anthropic.com with the CLI's own OAuth token, read from
// ~/.claude/.credentials.json. An environment DEFINITION (its setup script and
// env-var values) lives only behind the claude.ai WEB api, which is
// cookie-authed and Cloudflare-fronted: set $CLAUDE_SESSION_KEY (or
// $CLAUDE_SESSION_ID) to the browser's `sessionKey` cookie (DevTools >
// Application > Cookies > claude.ai > sessionKey -- it is HttpOnly, so it is
// NOT in document.cookie), and those requests go through curl-impersonate (a
// real Chrome TLS fingerprint) so Cloudflare does not serve its bot challenge.
// The env vars are not secrets ("visible to anyone using this environment"),
// so they print in full.
//
// Stdlib only, like the three programs beside it, so there is no vendorHash
// and no module to keep in step. The one exception is curl-impersonate, which
// exists precisely because net/http's TLS fingerprint is the thing Cloudflare
// rejects.
package main

import (
	"bytes"
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
	if err := run(os.Args[1:]); err != nil {
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

// creds are what api.anthropic.com wants: the CLI's own OAuth token, and the
// organization to act in. Both are read from the files the CLI maintains, so
// `claude /login` is the only thing that ever refreshes them.
type creds struct{ token, org string }

func readCreds() (creds, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return creds{}, err
	}
	var c creds
	var credFile struct {
		ClaudeAiOauth struct {
			AccessToken string `json:"accessToken"`
			ExpiresAt   int64  `json:"expiresAt"`
		} `json:"claudeAiOauth"`
	}
	if err := readJSON(filepath.Join(home, ".claude", ".credentials.json"), &credFile); err != nil {
		return creds{}, fmt.Errorf("no OAuth token in ~/.claude/.credentials.json (%v) — run `claude /login`", err)
	}
	c.token = credFile.ClaudeAiOauth.AccessToken
	if c.token == "" {
		return creds{}, errors.New("no OAuth token in ~/.claude/.credentials.json — run `claude /login`")
	}
	if exp := credFile.ClaudeAiOauth.ExpiresAt; exp > 0 && time.UnixMilli(exp).Before(time.Now()) {
		return creds{}, errors.New("the OAuth token in ~/.claude/.credentials.json has expired — run `claude /login`")
	}
	if c.org = os.Getenv("CLAUDE_ORG_ID"); c.org == "" {
		var cfg struct {
			OauthAccount struct {
				OrganizationUUID string `json:"organizationUuid"`
			} `json:"oauthAccount"`
		}
		if err := readJSON(filepath.Join(home, ".claude.json"), &cfg); err != nil {
			return creds{}, fmt.Errorf("no organization uuid in ~/.claude.json (%v); set CLAUDE_ORG_ID", err)
		}
		c.org = cfg.OauthAccount.OrganizationUUID
	}
	if c.org == "" {
		return creds{}, errors.New("no organization uuid in ~/.claude.json; set CLAUDE_ORG_ID")
	}
	return c, nil
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
	req.Header.Set("anthropic-beta", "oauth-2025-04-20")
	req.Header.Set("anthropic-client-platform", "cli")
	req.Header.Set("x-organization-uuid", c.org)
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

// environments lists what this account can run a session on. This is the OAuth
// route, not the cookie one: it answers with ids, names and state but a null
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
	if o.session != "" {
		id, err := wantSession(o, "-c")
		if err != nil {
			return err
		}
		return sendPrompt(id, prompt)
	}
	if o.env == "" && o.repo == "" {
		return legacyStart(o, prompt)
	}
	return apiStart(o, prompt)
}

// apiStart creates the session with the environment and the repository named
// here, then delivers the prompt over the same path `-c` uses.
//
// The prompt is a SECOND call rather than an initial event in the create body,
// so there is one way for a prompt to reach a session rather than two. An
// `events` entry in the create body works too.
func apiStart(o opts, prompt string) error {
	c, err := readCreds()
	if err != nil {
		return err
	}
	if o.env == "" {
		return errors.New("--repo needs --env: an API-created session names its environment (see --env-config for the list)")
	}
	envID, err := resolveEnv(c, o.env)
	if err != nil {
		return err
	}
	if o.repo == "" {
		return errors.New("--env needs --repo: an API-created session names its repository, e.g. --repo Metta-AI/caos-session")
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
	fmt.Fprintf(os.Stderr, "env:  %s (%s)\nrepo: %s", o.env, envID, repoURL)
	if ref != "" {
		fmt.Fprintf(os.Stderr, "@%s", ref)
	}
	fmt.Fprintf(os.Stderr, "\nView: %s\n", sessionURL(id))
	return sendPrompt(id, prompt)
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
// it is a different route from the interactive attach, which is not enabled
// for this account ("Attaching to an existing cloud session is not enabled").
func sendPrompt(id, prompt string) error {
	cmd := exec.Command("claude", "--cloud", id, "-p", prompt)
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	return cmd.Run()
}

// legacyStart is `claude --cloud` with no environment or repository named: the
// session opens the CURRENT directory's repo, on the account's default
// environment. `--cloud` refuses a pipe and would silently run locally, so it
// runs under script(1) for a TTY, and the prompt is passed through the
// environment rather than through the command string script(1) parses.
func legacyStart(o opts, prompt string) error {
	if err := requireClientRepo(o); err != nil {
		return err
	}
	pf, err := os.CreateTemp("", "drive-prompt-*")
	if err != nil {
		return err
	}
	defer os.Remove(pf.Name())
	if _, err := pf.WriteString(prompt); err != nil {
		return err
	}
	pf.Close()
	out, err := os.CreateTemp("", "drive-out-*")
	if err != nil {
		return err
	}
	defer os.Remove(out.Name())
	out.Close()

	cmd := exec.Command("script", "-qec", `claude --cloud "$(cat "$CAOS_DRIVE_PROMPT")"`, out.Name())
	cmd.Env = append(os.Environ(), "CAOS_DRIVE_PROMPT="+pf.Name())
	cmd.Run() // the banner is the result; script(1)'s own status is not.

	banner, _ := os.ReadFile(out.Name())
	if id, ok := sessionID(string(banner)); ok {
		fmt.Println(id)
	}
	for _, line := range strings.Split(string(banner), "\n") {
		if strings.Contains(line, "session_") || strings.Contains(line, "cse_") ||
			strings.Contains(line, "View:") || strings.Contains(line, "Resume") {
			fmt.Fprintln(os.Stderr, strings.TrimRight(line, "\r"))
		}
	}
	return nil
}

// requireClientRepo guards the legacy path only. A session started that way
// opens the cwd's repository, and for caos that has to be a caos CLIENT repo:
// caos itself is not one, so the session would die in its setup phase four
// minutes and one container later.
//
// The pin test is bootstrap.go's own (readLock): nodes.<root>.inputs.caos
// names a node KEY, and only that node's locked.rev is authoritative. A
// `follows` input is an array rather than a key and carries no lock, so it is
// not a pin — which is why this asks for the rev rather than for the input's
// mere presence.
func requireClientRepo(o opts) error {
	if o.anyRepo {
		return nil
	}
	root, err := exec.Command("git", "rev-parse", "--show-toplevel").Output()
	if err != nil {
		wd, _ := os.Getwd()
		return fmt.Errorf("%s is not a git repository, so there is nothing to open", wd)
	}
	dir := strings.TrimSpace(string(root))
	if lockRev(filepath.Join(dir, "flake.lock")) != "" {
		return nil
	}
	return fmt.Errorf(`%s does not pin caos, so a session started here cannot run.
  Name the environment and the repository instead, and the cwd stops mattering:
      drive --env Caos --repo Metta-AI/caos-session 'prompt'
  Started from a directory, a cloud session opens THAT repo, and it must be a
  caos CLIENT repo: flake.lock pinning a 'caos' input by revision, a root
  .caos-expr mounting that input's std, AGENTS.md and .caos-secrets.
  Pass --any-repo to start one anyway (it will fail in setup; that is the
  point of doing it deliberately).`, dir)
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

// ------------------------------------------- claude.ai web api (definitions)

// web runs one cookie-authed request against claude.ai through
// curl-impersonate. A plain client gets a 200 whose body is Cloudflare's "Just
// a moment..." interstitial, so the TLS fingerprint has to be a real Chrome's
// — which is the whole reason this is not net/http.
func web(args ...string) ([]byte, error) {
	cookie := os.Getenv("CLAUDE_SESSION_KEY")
	if cookie == "" {
		cookie = os.Getenv("CLAUDE_SESSION_ID")
	}
	if cookie == "" {
		return nil, errors.New(`set CLAUDE_SESSION_KEY (or CLAUDE_SESSION_ID) to your claude.ai
  'sessionKey' cookie — DevTools > Application > Cookies > claude.ai >
  sessionKey (HttpOnly, so it is not in document.cookie).`)
	}
	base := []string{"--impersonate", "chrome110", "--compressed", "-sS", "-m", "25",
		"-H", "Cookie: sessionKey=" + cookie}
	var cmd *exec.Cmd
	if p, err := exec.LookPath("curl-impersonate-chrome"); err == nil {
		cmd = exec.Command(p, append(base, args...)...)
	} else {
		cmd = exec.Command("nix", append([]string{"run", "nixpkgs#curl-impersonate", "--"}, append(base, args...)...)...)
	}
	cmd.Stderr = os.Stderr
	return cmd.Output()
}

func webJSON(v any, args ...string) error {
	b, err := web(args...)
	if err != nil {
		return err
	}
	return json.Unmarshal(b, v)
}

// orgsAll is every org the account belongs to: an environment can live in a
// non-default one, so a single guessed org is wrong.
func orgsAll() []string {
	if o := os.Getenv("CLAUDE_ORG_ID"); o != "" {
		return []string{o}
	}
	var orgs []struct {
		UUID string `json:"uuid"`
	}
	if err := webJSON(&orgs, "https://claude.ai/api/organizations"); err != nil {
		return nil
	}
	var out []string
	for _, o := range orgs {
		out = append(out, o.UUID)
	}
	return out
}

func envBase(org string) string {
	return "https://claude.ai/v1/environment_providers/private/organizations/" + url.PathEscape(org)
}

// envLocate resolves a name or an id to its org and id, across every org.
func envLocate(want string) (org, id string, err error) {
	for _, o := range orgsAll() {
		var page struct {
			Environments []environment `json:"environments"`
		}
		if err := webJSON(&page, envBase(o)+"/environments?limit=1000"); err != nil {
			continue
		}
		for _, e := range page.Environments {
			if e.ID == want || e.Name == want {
				return o, e.ID, nil
			}
		}
	}
	return "", "", fmt.Errorf("no environment matching %q", want)
}

func envGet(org, id string) (map[string]any, error) {
	var env map[string]any
	if err := webJSON(&env, envBase(org)+"/environments/"+url.PathEscape(id)); err != nil {
		return nil, fmt.Errorf("could not read %s: %v", id, err)
	}
	if _, ok := env["config"].(map[string]any); !ok {
		return nil, fmt.Errorf("could not read %s (cookie expired?)", id)
	}
	return env, nil
}

func envConfig(o opts) error {
	if o.arg == "" {
		any := false
		for _, org := range orgsAll() {
			var page struct {
				Environments []environment `json:"environments"`
			}
			if webJSON(&page, envBase(org)+"/environments?limit=1000") != nil {
				continue
			}
			for _, e := range page.Environments {
				fmt.Printf("%s   %s   [%s]   org=%s\n", e.ID, e.Name, e.State, org)
				any = true
			}
		}
		if !any {
			return errors.New("--env-config: no environments found (cookie expired?)")
		}
		return nil
	}
	org, id, err := envLocate(o.arg)
	if err != nil {
		return err
	}
	env, err := envGet(org, id)
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
	org, id, err := envLocate(o.arg)
	if err != nil {
		return err
	}
	env, err := envGet(org, id)
	if err != nil {
		return err
	}
	if err := envMutate(env, o); err != nil {
		return err
	}
	// Update takes name+description+config; the API rejects an extra `kind`.
	body := map[string]any{"name": env["name"], "description": describe(env), "config": env["config"]}
	if err := webPost(envBase(org)+"/environments/"+url.PathEscape(id), body, nil); err != nil {
		return err
	}
	fmt.Println("updated " + id + ":")
	if env, err := envGet(org, id); err == nil {
		envPrint(env)
	}
	return nil
}

func envCreate(o opts) error {
	if o.arg == "" {
		return errors.New("--env-create: give a name (and optionally --from <env> to clone)")
	}
	org := os.Getenv("CLAUDE_ORG_ID")
	var seed map[string]any
	if o.from != "" {
		var id string
		var err error
		if org, id, err = envLocate(o.from); err != nil {
			return fmt.Errorf("--from %q not found", o.from)
		}
		if seed, err = envGet(org, id); err != nil {
			return err
		}
	} else {
		if org == "" {
			if all := orgsAll(); len(all) > 0 {
				org = all[0]
			}
		}
		if err := json.Unmarshal([]byte(defaultEnvSeed), &seed); err != nil {
			return err
		}
	}
	if org == "" {
		return errors.New("--env-create: could not resolve an org; set CLAUDE_ORG_ID")
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
	if err := webPost(envBase(org)+"/cloud/create", body, &resp); err != nil {
		return err
	}
	if resp["environment_id"] == nil {
		return fmt.Errorf("--env-create failed: %v", resp)
	}
	fmt.Printf("created %v (%s)\n", resp["environment_id"], o.arg)
	envPrint(resp)
	return nil
}

func envDelete(o opts) error {
	if o.arg == "" {
		return errors.New("--env-delete: give the name or id to delete")
	}
	org, id, err := envLocate(o.arg)
	if err != nil {
		return err
	}
	if err := webPost(envBase(org)+"/environments/"+url.PathEscape(id)+"/delete", nil, nil); err != nil {
		return err
	}
	fmt.Println("deleted " + id)
	return nil
}

func webPost(u string, body any, out any) error {
	args := []string{"-o", "/dev/stdout", "-w", "\n%{http_code}", "-X", "POST", u}
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		f, err := os.CreateTemp("", "drive-body-*.json")
		if err != nil {
			return err
		}
		defer os.Remove(f.Name())
		if _, err := f.Write(b); err != nil {
			return err
		}
		f.Close()
		args = append([]string{"-H", "Content-Type: application/json", "--data", "@" + f.Name()}, args...)
	}
	raw, err := web(args...)
	if err != nil {
		return err
	}
	cut := bytes.LastIndexByte(raw, '\n')
	if cut < 0 {
		return fmt.Errorf("no status in response: %s", raw)
	}
	payload, code := raw[:cut], strings.TrimSpace(string(raw[cut+1:]))
	if code != "200" {
		return fmt.Errorf("HTTP %s: %s", code, trunc(string(payload), 300))
	}
	if out != nil {
		return json.Unmarshal(payload, out)
	}
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

  --title T    the session's title (default: the prompt's first line)
  --ref R      the revision to check out (same as --repo owner/repo@R)
  --any-repo   start from the cwd even when it does not pin caos

<env> is an environment's name or its env_… id; <session> is cse_…, session_…,
or a claude.ai/code URL. --env-config and the --env-* writes need
$CLAUDE_SESSION_KEY; everything else uses the CLI's own login.
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
