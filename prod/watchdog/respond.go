package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"caos/w"
)

// The rules, all of them here so they can be read in one place:
//
//   - One incident at a time. An incident opens when the alerting set becomes
//     non-empty and closes when it is empty again. Everything that alerts
//     while it is open belongs to it: six monitors tripping together are one
//     problem with six symptoms, not six tickets.
//   - Slack hears first, from this loop, the moment the set is non-empty. The
//     alert path does not depend on Claude, the API key, or the model.
//   - Then wait `settle` so the storm finishes arriving, and run ONE session
//     over the whole set.
//   - A session runs again only if the set GREW (a monitor it has not seen).
//     The same set still alerting after a session means the session did not
//     fix it; running it again on the same facts buys nothing. Escalate once,
//     to a human, and wait.
//   - Hard caps regardless: maxRuns sessions per incident, maxDaily per day,
//     each bounded by sessionTimeout and maxTurns. The runaway failure mode
//     of this design is the responder, not the server.
const (
	poll           = 30 * time.Second
	settle         = 90 * time.Second
	maxRuns        = 3
	maxDaily       = 8
	sessionTimeout = 25 * time.Minute
	maxTurns       = 80
)

type run struct {
	StartedAt time.Time `json:"started_at"`
	EndedAt   time.Time `json:"ended_at"`
	Alerts    []string  `json:"alerts"`
	Report    string    `json:"report"`
	CostUSD   float64   `json:"cost_usd"`
	Turns     int       `json:"turns"`
	ExitCode  int       `json:"exit_code"`
}

type incident struct {
	ID        string           `json:"id"`
	OpenedAt  time.Time        `json:"opened_at"`
	Alerts    map[string]alert `json:"alerts"` // everything seen while open, by key
	Runs      []run            `json:"runs"`
	Escalated bool             `json:"escalated"`
	ClosedAt  *time.Time       `json:"closed_at,omitempty"`
}

// ledger is the on-disk state: the open incident (if any) and an append-only
// history. The next session reads the previous one's report from here, which
// is how "a restart was already tried" reaches it.
type ledger struct{ dir string }

func (l ledger) openPath() string    { return l.dir + "/incident.json" }
func (l ledger) historyPath() string { return l.dir + "/incidents.jsonl" }

func (l ledger) load() *incident {
	b, err := os.ReadFile(l.openPath())
	if os.IsNotExist(err) {
		return nil
	}
	w.Must(err)
	var inc incident
	w.Must(json.Unmarshal(b, &inc))
	return &inc
}

func (l ledger) save(inc *incident) {
	b := w.Check(json.MarshalIndent(inc, "", "  "))
	w.Must(os.WriteFile(l.openPath()+".tmp", b, 0o600))
	w.Must(os.Rename(l.openPath()+".tmp", l.openPath()))
}

func (l ledger) close(inc *incident) {
	now := time.Now().UTC()
	inc.ClosedAt = &now
	f := w.Check(os.OpenFile(l.historyPath(), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600))
	defer f.Close()
	w.Check(f.Write(append(w.Check(json.Marshal(inc)), '\n')))
	w.Must(os.Remove(l.openPath()))
}

// runsSince counts sessions in the history and the open incident after t, for
// the daily cap.
func (l ledger) runsSince(t time.Time, open *incident) int {
	n := 0
	if open != nil {
		for _, r := range open.Runs {
			if r.StartedAt.After(t) {
				n++
			}
		}
	}
	b, err := os.ReadFile(l.historyPath())
	if os.IsNotExist(err) {
		return n
	}
	w.Must(err)
	for _, line := range strings.Split(strings.TrimSpace(string(b)), "\n") {
		if line == "" {
			continue
		}
		var inc incident
		w.Must(json.Unmarshal([]byte(line), &inc))
		for _, r := range inc.Runs {
			if r.StartedAt.After(t) {
				n++
			}
		}
	}
	return n
}

func describe(alerts []alert) string {
	var b strings.Builder
	for _, a := range alerts {
		fmt.Fprintf(&b, "- %s [%s] group=%s since %s\n", a.Name, a.Status, a.Group, a.Since)
	}
	return b.String()
}

func respond(c conf) {
	l := ledger{c.stateDir}
	inc := l.load()
	var pendingSince time.Time // when the current unseen alerts first appeared
	for {
		current := alerting(c)
		now := time.Now().UTC()

		if len(current) == 0 {
			if inc != nil {
				fmt.Fprintf(os.Stderr, "%s incident %s resolved\n", now.Format(time.RFC3339), inc.ID)
				notify(c, fmt.Sprintf("*caos: resolved* incident %s after %s. Nothing is alerting.",
					inc.ID, now.Sub(inc.OpenedAt).Round(time.Minute)))
				l.close(inc)
				inc = nil
			}
			pendingSince = time.Time{}
			time.Sleep(poll)
			continue
		}

		if inc == nil {
			inc = &incident{ID: now.Format("20060102-1504"), OpenedAt: now, Alerts: map[string]alert{}}
			fmt.Fprintf(os.Stderr, "%s incident %s opened\n", now.Format(time.RFC3339), inc.ID)
			notify(c, fmt.Sprintf("*caos: alerting* incident %s\n%sResponder session starts in %s unless it clears.",
				inc.ID, describe(current), settle))
		}

		// Which of these has no session seen yet?
		var unseen []alert
		seen := map[string]bool{}
		for _, r := range inc.Runs {
			for _, k := range r.Alerts {
				seen[k] = true
			}
		}
		for _, a := range current {
			if _, known := inc.Alerts[a.key()]; !known {
				inc.Alerts[a.key()] = a
				if len(inc.Runs) > 0 {
					notify(c, fmt.Sprintf("*caos: also alerting* in incident %s\n%s", inc.ID, describe([]alert{a})))
				}
			}
			if !seen[a.key()] {
				unseen = append(unseen, a)
			}
		}
		l.save(inc)

		switch {
		case len(unseen) == 0:
			// Everything alerting has been through a session and is still
			// alerting. Say so once, then wait for a human or a change.
			if !inc.Escalated && len(inc.Runs) > 0 {
				inc.Escalated = true
				l.save(inc)
				notify(c, fmt.Sprintf("*caos: unresolved* incident %s is still alerting after %d responder session(s). "+
					"A human needs to look; the responder will not run again unless something new alerts.",
					inc.ID, len(inc.Runs)))
			}
			pendingSince = time.Time{}
		case pendingSince.IsZero():
			pendingSince = now
		case now.Sub(pendingSince) >= settle:
			if len(inc.Runs) >= maxRuns {
				if !inc.Escalated {
					inc.Escalated = true
					l.save(inc)
					notify(c, fmt.Sprintf("*caos: unresolved* incident %s has used its %d sessions. A human needs to look.",
						inc.ID, maxRuns))
				}
			} else if l.runsSince(now.Add(-24*time.Hour), inc) >= maxDaily {
				if !inc.Escalated {
					inc.Escalated = true
					l.save(inc)
					notify(c, fmt.Sprintf("*caos: unresolved* incident %s: the responder has run %d times in 24h and is pausing. "+
						"A human needs to look.", inc.ID, maxDaily))
				}
			} else {
				r := session(c, inc, current)
				inc.Runs = append(inc.Runs, r)
				inc.Escalated = false
				l.save(inc)
				notify(c, fmt.Sprintf("*caos: responder report* incident %s, session %d (%d turns, $%.2f, exit %d)\n%s",
					inc.ID, len(inc.Runs), r.Turns, r.CostUSD, r.ExitCode, r.Report))
			}
			pendingSince = time.Time{}
		}
		time.Sleep(poll)
	}
}

// session runs one Claude Code session over the alerting set and returns what
// it reported. The session's whole world is the workspace: CLAUDE.md (the
// runbook) and .claude/settings.json (which tools it may use) are copied in
// from the store on every run, so a deploy changes them; its only hands on
// the host are `caos-watchdog ssh`, and its only voice is `caos-watchdog
// notify` plus the report returned here.
func session(c conf, inc *incident, current []alert) run {
	r := run{StartedAt: time.Now().UTC()}
	for _, a := range current {
		r.Alerts = append(r.Alerts, a.key())
	}

	workspace := c.stateDir + "/workspace"
	w.Must(os.RemoveAll(workspace))
	w.Must(os.MkdirAll(workspace+"/.claude", 0o700))
	w.Must(os.WriteFile(workspace+"/CLAUDE.md", w.Check(os.ReadFile(c.runbookDir+"/CLAUDE.md")), 0o600))
	w.Must(os.WriteFile(workspace+"/.claude/settings.json", w.Check(os.ReadFile(c.runbookDir+"/settings.json")), 0o600))

	var prompt strings.Builder
	fmt.Fprintf(&prompt, "Incident %s on the caos production host %s, opened %s. Session %d of at most %d.\n\n",
		inc.ID, c.target, inc.OpenedAt.Format(time.RFC3339), len(inc.Runs)+1, maxRuns)
	fmt.Fprintf(&prompt, "Alerting Datadog monitors right now:\n")
	for _, a := range current {
		fmt.Fprintf(&prompt, "\n### %s\nstatus: %s  group: %s  since: %s\nquery: %s\nmonitor message:\n%s\n",
			a.Name, a.Status, a.Group, a.Since, a.Query, strings.TrimSpace(a.Message))
	}
	if len(inc.Runs) > 0 {
		fmt.Fprintf(&prompt, "\nEarlier sessions in this incident, oldest first. Do not repeat what they already tried.\n")
		for i, prev := range inc.Runs {
			fmt.Fprintf(&prompt, "\n--- session %d, %s ---\n%s\n", i+1, prev.StartedAt.Format(time.RFC3339), prev.Report)
		}
	}
	fmt.Fprintf(&prompt, "\nFollow CLAUDE.md: find the one cause behind these together, apply only the fixes it allows, "+
		"and end with the report in the format it gives.\n")

	cmd := exec.Command("claude", "-p", prompt.String(),
		"--output-format", "json",
		"--model", c.model,
		"--max-turns", fmt.Sprint(maxTurns),
		"--permission-mode", "dontAsk")
	cmd.Dir = workspace
	cmd.Env = append(os.Environ(),
		"HOME="+c.stateDir,
		"ANTHROPIC_API_KEY="+c.secret("anthropic-api-key"),
		"DISABLE_AUTOUPDATER=1",
		"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
	)
	var stdout, stderr strings.Builder
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	w.Must(cmd.Start())
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case err := <-done:
		if exit, ok := err.(*exec.ExitError); ok {
			r.ExitCode = exit.ExitCode()
		} else {
			w.Must(err)
		}
	case <-time.After(sessionTimeout):
		w.Must(cmd.Process.Kill())
		<-done
		r.ExitCode = -1
		r.EndedAt = time.Now().UTC()
		r.Report = fmt.Sprintf("session killed after %s with no report. stderr tail:\n%s", sessionTimeout, tail(stderr.String(), 1500))
		return r
	}
	r.EndedAt = time.Now().UTC()

	var result struct {
		Result       string  `json:"result"`
		IsError      bool    `json:"is_error"`
		NumTurns     int     `json:"num_turns"`
		TotalCostUSD float64 `json:"total_cost_usd"`
	}
	if err := json.Unmarshal([]byte(stdout.String()), &result); err != nil {
		r.Report = fmt.Sprintf("claude exited %d without a JSON result.\nstdout tail:\n%s\nstderr tail:\n%s",
			r.ExitCode, tail(stdout.String(), 1500), tail(stderr.String(), 1500))
		return r
	}
	r.Turns, r.CostUSD = result.NumTurns, result.TotalCostUSD
	r.Report = strings.TrimSpace(result.Result)
	if result.IsError && r.Report == "" {
		r.Report = "claude reported an error with no text. stderr tail:\n" + tail(stderr.String(), 1500)
	}
	// Keep the transcript for the operator; the store copy is the runbook,
	// this is what the session did with it.
	w.Must(os.MkdirAll(filepath.Join(c.stateDir, "sessions"), 0o700))
	w.Must(os.WriteFile(filepath.Join(c.stateDir, "sessions", inc.ID+"-"+fmt.Sprint(len(inc.Runs)+1)+".json"),
		[]byte(stdout.String()), 0o600))
	return r
}

func tail(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return "…" + s[len(s)-n:]
}
