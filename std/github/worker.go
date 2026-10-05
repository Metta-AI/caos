// The `github` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// One call to the GitHub API (design/agent-github.md, "PRs"). Args, under
// /cas/args:
//
//	method  GET, POST, PATCH, PUT or DELETE
//	path    the API path, with any query string
//	body    optional JSON request body
//	at      any value of the caller's; it only keys the result
//
// The github-token secret, when granted, is at /secret/github-token.
//
// The result is the status line and the body. A response of any status is a
// result: the model reads a 404 as an answer. Only a request that got no
// response fails the job.
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"time"

	"caos/w"
)

// The one host this tool reaches. A path is appended to it, never parsed into
// a URL of its own, and redirects are not followed, so the token is sent here
// and nowhere else.
const api = "https://api.github.com"

// Responses longer than this are cut, with a note saying where.
const maxBody = 100_000

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

func required(name string) string {
	value, ok := arg(name)
	w.True(ok && strings.TrimSpace(value) != "", "github: %s is required", name)
	return value
}

func token() string {
	bytes, err := os.ReadFile("/secret/github-token")
	if os.IsNotExist(err) {
		return ""
	}
	t := strings.TrimRight(string(w.Check(bytes, err)), "\r\n")
	// A line break inside a header value would end the header block early.
	w.True(!strings.ContainsAny(t, "\r\n"), "github: the github-token secret has a line break inside it")
	return t
}

// request checks the arguments and forms the call. Nothing is sent until they
// are all well-formed.
func request(token string) *http.Request {
	method := strings.ToUpper(strings.TrimSpace(required("method")))
	switch method {
	case "GET", "POST", "PATCH", "PUT", "DELETE":
	default:
		w.True(false, "github: method must be GET, POST, PATCH, PUT or DELETE, not %q", method)
	}
	raw := strings.TrimSpace(required("path"))
	// A model often copies a full `url` out of an earlier response.
	path := strings.TrimPrefix(raw, api)
	w.True(strings.HasPrefix(path, "/") && !strings.ContainsFunc(path, func(r rune) bool {
		return r <= ' ' || r == 0x7f
	}), "github: path must be an API path such as /repos/owner/repo/pulls, not %q", raw)
	required("at")
	var body io.Reader
	if text, ok := arg("body"); ok && strings.TrimSpace(text) != "" {
		w.True(method != "GET", "github: a GET takes no body; put its parameters in the query string")
		w.True(json.Valid([]byte(text)), "github: body is not JSON")
		body = strings.NewReader(text)
	}
	req := w.Check(http.NewRequest(method, api+path, body))
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("X-GitHub-Api-Version", "2022-11-28")
	req.Header.Set("User-Agent", "caos-github")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	return req
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

func main() {
	w.Main(func() {
		t := token()
		req := request(t)
		client := &http.Client{
			Timeout: 60 * time.Second,
			// A redirect would carry the request to wherever it points.
			CheckRedirect: func(*http.Request, []*http.Request) error {
				return http.ErrUseLastResponse
			},
		}
		resp, err := client.Do(req)
		unconfirmed := ""
		if req.Method != "GET" {
			unconfirmed = " The request may still have arrived: read the affected state with a GET before sending it again."
		}
		w.True(err == nil, "github: %s %s got no response: %v.%s", req.Method, req.URL.RequestURI(), err, unconfirmed)
		defer resp.Body.Close()
		w.Report(render(resp, t != ""))
	})
}
