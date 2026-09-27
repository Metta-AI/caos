// Mint the OAuth token that `drive.go` reads from /secret/claude-oauth-token.
//
//	go run integrations/claude-code/drive/authorize.go [--days N] [--scopes "..."]
//
// It runs on the HOST and nowhere else: it needs a browser and a paste, and a
// worker has neither. That is the whole reason it is a separate program rather
// than a mode of drive.go — drive.go is the worker, and a worker must not
// carry code that mints credentials.
//
// The session and environment routes need a token carrying
// `user:sessions:claude_code`. Three obvious routes are measured shut:
//
//   - `claude setup-token` consents at the CONSOLE, which grants the console
//     scopes; every route answers its token 401 oauth_scope_insufficient.
//   - a refresh_token exchange grants the right scopes, but the server ignores
//     `expires_in` there — 365 days asked, 8 hours given — and the refresh
//     token is SINGLE-USE: spending it rotates it, and the spent one is
//     answered `invalid_grant`. So only something that can persist the
//     replacement can do the exchanging, and a worker cannot persist.
//   - a claude.ai browser `sessionKey` is a cookie, not a bearer. It reaches
//     claude.ai's environment-definition service and nothing else; the session
//     routes answer it 401 on every host.
//
// What works is the authorization-code flow consented at CLAUDE.AI, with
// `expires_in` on the CODE exchange — which is where setup-token's long
// lifetime comes from, and it is the GRANT rather than the request that
// narrows the scopes. 30 days is the ceiling on these two scopes.
//
// Stdlib only, and self-contained: the few helpers it shares in spirit with
// drive.go are repeated here rather than shared, because drive.go is a LONE
// FILE by construction (std/go curries one `worker1`) and cannot import a
// sibling.
package main

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

const (
	// Consent at CLAUDE.AI, not the console: both authorize URLs request the
	// same scope set, and it is the grant that narrows. The console grants the
	// console scopes, which is why setup-token's token has no sessions scope.
	claudeAIAuthorizeURL = "https://claude.com/cai/oauth/authorize"
	oauthTokenURL        = "https://platform.claude.com/v1/oauth/token"
	// The MANUAL redirect, so no local port has to be opened — which is what
	// lets this run anywhere, including over ssh.
	oauthRedirectURL = "https://platform.claude.com/oauth/code/callback"
	// The PROD client. Claude Code carries three OAuth configs and the other
	// two are a localhost dev factory and a staging one; the prod block is the
	// one with an empty OAUTH_FILE_SUFFIX and mcp-proxy.anthropic.com. A dev
	// client id reaches the real token endpoint and is answered "Client with id
	// … not found", which reads like a revoked client rather than a wrong one.
	//
	// The consent page's name ("Claude Code") is this client's REGISTRATION,
	// held by Anthropic and not a parameter of the request. Nothing sent from
	// here can change it.
	oauthClientID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
	sessionsScope = "user:sessions:claude_code"
	// Where drive.go expects the result.
	secretName = "claude-oauth-token"
)

// defaultScopes is DELIBERATELY NARROW. A long lifetime is not available on
// every scope: the token endpoint answers
//
//	Custom expires_in not allowed for scope 'user:mcp_servers'
//
// so the client's own seven-scope set cannot carry one at all. These two are
// what the session AND environment routes need — measured, both answer 200 —
// and `user:profile` is in the console set, which setup-token already mints
// with a custom lifetime.
var defaultScopes = []string{"user:profile", sessionsScope}

func main() {
	days := flag.Int("days", 365, "lifetime to ask for; the server caps it and the ladder finds the ceiling")
	scopes := flag.String("scopes", strings.Join(defaultScopes, " "),
		"space-separated scopes to request")
	flag.Parse()
	if err := authorize(*days, strings.Fields(*scopes)); err != nil {
		fmt.Fprintln(os.Stderr, "authorize: "+err.Error())
		os.Exit(1)
	}
}

func authorize(days int, scopes []string) error {
	if days < 1 {
		return fmt.Errorf("--days %d is not a positive number of days", days)
	}
	if len(scopes) == 0 {
		return errors.New("--scopes is empty")
	}
	verifier, err := randomURLSafe(32)
	if err != nil {
		return err
	}
	// 32 bytes, not 16: the client mints both the verifier and the state as
	// base64url of 32 random bytes, and a 22-character state is refused as
	// "Authorization failed / Invalid request format" — which reads like a
	// wrong scope or a wrong redirect, and is neither.
	state, err := randomURLSafe(32)
	if err != nil {
		return err
	}
	sum := sha256.Sum256([]byte(verifier))
	challenge := base64.RawURLEncoding.EncodeToString(sum[:])

	// BUILT IN THE CLIENT'S OWN ORDER. url.Values.Encode sorts, and nothing
	// here needs to find out whether that matters.
	var q strings.Builder
	for i, kv := range [][2]string{
		{"code", "true"},
		{"client_id", oauthClientID},
		{"response_type", "code"},
		{"redirect_uri", oauthRedirectURL},
		{"scope", strings.Join(scopes, " ")},
		{"code_challenge", challenge},
		{"code_challenge_method", "S256"},
		{"state", state},
	} {
		if i > 0 {
			q.WriteByte('&')
		}
		q.WriteString(kv[0] + "=" + url.QueryEscape(kv[1]))
	}

	fmt.Fprintf(os.Stderr, "Open this, approve, and paste back what the page shows.\n"+
		"It will say \"Claude Code\" is asking: that is this OAuth client's registered\n"+
		"name, held by Anthropic, and not something the request can set.\n\n%s?%s\n\ncode: ",
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

	// ONE CONSENT, MANY EXCHANGES. The server caps how long a token may live
	// and says only "Invalid expiry for scope" — it does not name the ceiling.
	// Finding it by consenting again per guess would spend a browser round trip
	// each time, so the ladder is walked against the code already in hand. A
	// code survives a REFUSED exchange; only a granted one consumes it.
	var tok oauthToken
	var granted int
	for _, try := range ladder(days) {
		body := map[string]any{
			"grant_type":    "authorization_code",
			"code":          code,
			"redirect_uri":  oauthRedirectURL,
			"client_id":     oauthClientID,
			"code_verifier": verifier,
			"state":         state,
		}
		if try > 0 {
			body["expires_in"] = try * 24 * 60 * 60
		}
		var err error
		tok, err = postToken(body)
		if err == nil {
			granted = try
			break
		}
		if !strings.Contains(err.Error(), "xpiry") && !strings.Contains(err.Error(), "expires_in") {
			return err
		}
		if try == 0 {
			return err
		}
		fmt.Fprintf(os.Stderr, "  %d days: refused\n", try)
	}
	if granted > 0 {
		fmt.Fprintf(os.Stderr, "  %d days: accepted\n", granted)
	} else {
		fmt.Fprintln(os.Stderr, "  no custom lifetime accepted; took the default")
	}

	life := time.Duration(tok.ExpiresIn) * time.Second
	fmt.Fprintf(os.Stderr, "\ngranted: %s\nexpires: %s (%s from now)\n",
		tok.Scope, time.Now().Add(life).Format(time.RFC3339), life.Round(time.Minute))
	if !contains(strings.Fields(tok.Scope), sessionsScope) {
		return fmt.Errorf("the grant does not include %s, so this token is refused by every\n"+
			"  session and environment route.", sessionsScope)
	}
	if life < 24*time.Hour {
		fmt.Fprintf(os.Stderr, "\nNOTE: %s is not a lifetime a secret can hold. The ceiling above is\n"+
			"what this server allows on these scopes.\n", life.Round(time.Minute))
	}
	fmt.Fprintf(os.Stderr, "\nput this in .caos-secrets/%s as `value=`, keeping its\n"+
		"reader= and entropy= lines:\n\n", secretName)
	fmt.Println(tok.AccessToken)
	return nil
}

// ladder is the sequence of lifetimes to try, longest first, ending in 0 —
// no `expires_in` at all, which takes whatever the server defaults to. Values
// at or above the one asked for are skipped so an explicit --days is a ceiling
// rather than a suggestion.
func ladder(days int) []int {
	out := []int{days}
	for _, d := range []int{365, 180, 90, 60, 30, 14, 7, 1} {
		if d < days {
			out = append(out, d)
		}
	}
	return append(out, 0)
}

type oauthToken struct {
	AccessToken string `json:"access_token"`
	ExpiresIn   int64  `json:"expires_in"`
	Scope       string `json:"scope"`
}

// postToken sends one token request and returns the server's own message on a
// refusal — which is the thing that names the scope or the limit.
func postToken(body map[string]any) (oauthToken, error) {
	var tok oauthToken
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
		msg := string(answer)
		if len(msg) > 300 {
			msg = msg[:300]
		}
		return tok, fmt.Errorf("token endpoint: HTTP %d: %s", resp.StatusCode, msg)
	}
	if err := json.Unmarshal(answer, &tok); err != nil {
		return tok, err
	}
	if tok.AccessToken == "" {
		return tok, errors.New("the token endpoint returned no access_token")
	}
	return tok, nil
}

func randomURLSafe(n int) (string, error) {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(b), nil
}

func contains(haystack []string, needle string) bool {
	for _, s := range haystack {
		if s == needle {
			return true
		}
	}
	return false
}
