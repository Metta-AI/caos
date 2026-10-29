package main

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"time"

	"caos/w"
)

// notify posts one message to #proj-caos through the incoming webhook. Every
// human-facing word this program or its Claude session emits goes through
// here: the poller's "something is alerting", the session's report, and its
// requests for a human to do what it may not.
func notify(c conf, text string) {
	body := w.Check(json.Marshal(map[string]string{"text": text}))
	req := w.Check(http.NewRequest(http.MethodPost, c.secret("slack-webhook"), bytes.NewReader(body)))
	req.Header.Set("Content-Type", "application/json")
	resp := w.Check((&http.Client{Timeout: 15 * time.Second}).Do(req))
	defer resp.Body.Close()
	reply := w.Check(io.ReadAll(resp.Body))
	w.True(resp.StatusCode == http.StatusOK, "slack webhook: %s: %s", resp.Status, strings.TrimSpace(string(reply)))
}
