// tests/expression-http — a WORKER test: no client, no repo.
//
// Exercises the server's HTTP surface for ONE expression line with nothing but
// net/http: `/eval/<verb>` blocks for the answer, `/start/<verb>` returns a
// ticket, `/result/<ticket>` reports on it. The arguments are `caos-cli run`'s,
// with the type in the parameter's name (`base:hash=…`). The ticket IS the
// request's ArgTree hash, so it is also what `/run`, `/start?req=` and
// `/status` take."}
//
// It blocks on the server from inside a worker, like every client test does
// (`caos-cli run` in dev/cli-test blocks the same way), so under a contended
// suite a cold run waits for a runner slot: the polling bounds below are
// queues, not latencies.
package main

import (
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"strings"
	"time"

	"caos/w"
)

var (
	server string
	client = &http.Client{Timeout: 10 * time.Minute}
)

// escape percent-encodes a query component the way the server decodes it
// (`+` is NOT a space there, so url.QueryEscape's spelling would corrupt one).
func escape(s string) string {
	return strings.ReplaceAll(url.QueryEscape(s), "+", "%20")
}

// query joins key, value pairs into `k=v&k=v`, in the order given.
func query(pairs ...string) string {
	w.True(len(pairs)%2 == 0, "query wants key, value pairs")
	parts := make([]string, 0, len(pairs)/2)
	for i := 0; i < len(pairs); i += 2 {
		parts = append(parts, escape(pairs[i])+"="+escape(pairs[i+1]))
	}
	return strings.Join(parts, "&")
}

// call makes one request and returns its status and trimmed body. A refusal is
// a result here, not a reason to stop: several claims below are that the server
// says no.
func call(method, path, body string) (int, string) {
	var reader io.Reader
	if body != "" {
		reader = strings.NewReader(body)
	}
	req := w.Check(http.NewRequest(method, server+path, reader))
	if body != "" {
		req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	}
	resp, err := client.Do(req)
	w.True(err == nil, "%s %s did not complete: %v", method, path, err)
	defer resp.Body.Close()
	text := string(w.Check(io.ReadAll(resp.Body)))
	return resp.StatusCode, strings.TrimSpace(text)
}

func run(name string, args ...string) string {
	out, err := exec.Command(name, args...).CombinedOutput()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, out)
	return strings.TrimSpace(string(out))
}

// blobText reads a blob the server holds. `GET /object/<hash>` is git's native
// `<type> <size>\0<content>`.
func blobText(hash string) string {
	code, body := call("GET", "/object/"+hash, "")
	w.True(code == 200, "reading %s: HTTP %d: %s", hash, code, body)
	_, content, found := strings.Cut(body, "\x00")
	w.True(found, "object %s is not in git's native form: %q", hash, body)
	return content
}

// await polls /result/<ticket> until it stops saying `pending`. A queue, not a
// latency (CLAUDE.md, "A salted suite queues"): the loop leaves on success, so
// a large bound costs nothing on a healthy run.
func await(ticket string, tries int) string {
	state := ""
	for i := 0; i < tries; i++ {
		code, body := call("GET", "/result/"+ticket, "")
		w.True(code == 200, "result %s: HTTP %d: %s", ticket, code, body)
		state = body
		if state != "pending" {
			return state
		}
		time.Sleep(500 * time.Millisecond)
	}
	w.True(false, "ticket %s was still pending after %d polls", ticket, tries)
	return ""
}

func main() {
	w.Main(func() {
		server = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(server != "", "this test needs CAOS_SERVER_URL from the runner")

		// OWN SALT, per invocation. The stack is shared and long-lived, and a
		// ticket whose evaluation already succeeded is answered without running
		// anything, so a later suite run would be served by this one's memo and
		// would test nothing about starting work (tests/tracing says the same
		// of its record).
		salt := fmt.Sprintf("expression-http-%d-%d", time.Now().UnixNano(), os.Getpid())

		w.Step("the image every run below executes")
		hello := run("caos", "hash", "/cas/args/hello")
		w.True(len(hello) == 40, "expected an object hash for hello, got %q", hello)

		w.Step("POST /eval/run answers what /run does for the same request")
		code, viaExpr := call("POST", "/eval/run?"+query("base:hash", hello, "msg", "hi", "salt", salt), "")
		w.True(code == 200, "eval/run: HTTP %d: %s", code, viaExpr)
		w.True(strings.HasPrefix(viaExpr, "blob "), "expected '<kind> <hash>', got %q", viaExpr)
		// The reference answer, by the existing route: a request formed by
		// `caos prepare-request` and run with GET /run?req=<ArgTree>. The two
		// ArgTrees differ only in their salt, which hello does not report, so
		// the result is the same blob.
		req := run("caos", "prepare-request", "--base:hash="+hello, "--msg=hi")
		code, viaRun := call("GET", "/run?req="+req, "")
		w.True(code == 200, "/run?req=%s: HTTP %d: %s", req, code, viaRun)
		w.True(viaExpr == viaRun, "/eval/run said %q but /run said %q", viaExpr, viaRun)
		text := blobText(strings.TrimPrefix(viaExpr, "blob "))
		w.True(strings.Contains(text, "msg = hi"), "hello did not report the argument: %q", text)
		fmt.Fprintf(os.Stderr, "  ok: %s\n", viaExpr)

		w.Step("argument order does not matter, and a literal may start with $")
		code, body := call("POST", "/eval/run?"+query("msg", "$5", "base:hash", hello, "salt", salt), "")
		w.True(code == 200, "a literal $5 was refused: HTTP %d: %s", code, body)
		w.True(strings.Contains(blobText(strings.TrimPrefix(body, "blob ")), "msg = $5"),
			"hello did not report the literal $5")

		w.Step("a POST body carries parameters too")
		code, body = call("POST", "/eval/run?"+query("base:hash", hello, "salt", salt), query("msg", "hi"))
		w.True(code == 200, "POST eval/run with a body: HTTP %d: %s", code, body)
		w.True(body == viaExpr, "a body parameter gave %q, the query gave %q", body, viaExpr)

		w.Step("GET /eval/curry forms the ArgTree and runs nothing")
		code, body = call("GET", "/eval/curry?"+query("base:hash", hello, "msg", "hi", "salt", salt), "")
		w.True(code == 200, "eval/curry: HTTP %d: %s", code, body)
		w.True(strings.HasPrefix(body, "tree "), "curry should yield a tree, got %q", body)

		w.Step("there is no local tree to name with :@=")
		code, body = call("POST", "/eval/run?"+query("base:@", "DEEP-DEPS/hello"), "")
		w.True(code >= 400, "a :@= base was accepted: HTTP %d: %s", code, body)
		w.True(strings.Contains(body, "local tree"), "the refusal should say why, got %q", body)

		w.Step("an unknown verb and a duplicate argument are the caller's mistake")
		code, body = call("POST", "/eval/walk?"+query("base:hash", hello), "")
		w.True(code == 400, "unknown verb: expected 400, got %d: %s", code, body)
		code, body = call("POST", "/eval/run?"+query("base:hash", hello, "msg", "a", "msg", "b"), "")
		w.True(code == 400, "duplicate argument: expected 400, got %d: %s", code, body)

		w.Step("POST /start returns a ticket, and the same call is the same ticket")
		code, ticket := call("POST", "/start/run?"+query("base:hash", hello, "msg", "started", "salt", salt), "")
		w.True(code == 200, "start: HTTP %d: %s", code, ticket)
		w.True(len(ticket) == 40, "expected a 40-character ticket, got %q", ticket)
		code, again := call("POST", "/start/run?"+query("salt", salt, "msg", "started", "base:hash", hello), "")
		w.True(code == 200 && again == ticket, "reordered parameters gave ticket %q, not %q", again, ticket)

		w.Step("GET /result/<ticket> reports pending, then the answer")
		state := await(ticket, 600)
		w.True(strings.HasPrefix(state, "ok blob "), "expected 'ok blob <hash>', got %q", state)
		w.True(strings.Contains(blobText(strings.TrimPrefix(state, "ok blob ")), "msg = started"),
			"the ticket's answer is not hello's report of its argument")
		fmt.Fprintf(os.Stderr, "  ok: %s\n", state)

		w.Step("a finished ticket stays answered, and starting it again does not re-run")
		code, again = call("POST", "/start/run?"+query("base:hash", hello, "msg", "started", "salt", salt), "")
		w.True(code == 200 && again == ticket, "restarting a finished call minted %q", again)
		_, now := call("GET", "/result/"+ticket, "")
		w.True(now == state, "the answer changed from %q to %q", state, now)

		w.Step("a failed ticket reports the failure, and asking never re-dispatches")
		code, doomed := call("POST", "/start/run?"+query("base:hash", "0000000000000000000000000000000000000001", "salt", salt), "")
		w.True(code == 200, "start of a doomed call: HTTP %d: %s", code, doomed)
		failed := await(doomed, 240)
		w.True(strings.HasPrefix(failed, "failed "), "expected 'failed …', got %q", failed)
		_, repeat := call("GET", "/result/"+doomed, "")
		w.True(repeat == failed, "asking again changed the answer: %q then %q", failed, repeat)

		w.Step("a ticket nobody started is unknown, and a malformed one is refused")
		code, body = call("GET", "/result/0000000000000000000000000000000000000002", "")
		w.True(code == 404, "unknown ticket: expected 404, got %d: %s", code, body)
		code, body = call("GET", "/result/not-a-ticket", "")
		w.True(code == 400, "malformed ticket: expected 400, got %d: %s", code, body)
	})
}
