package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"sort"
	"strings"
	"time"

	"caos/w"
)

// alert is one alerting (monitor, group) pair: the unit the responder reasons
// about. A monitor grouped by host and unit alerts once per group, and each
// group is a separate fact for the session.
type alert struct {
	MonitorID int64  `json:"monitor_id"`
	Name      string `json:"name"`
	Group     string `json:"group"`
	Status    string `json:"status"`
	Since     string `json:"since"`
	Message   string `json:"message"`
	Query     string `json:"query"`
}

func (a alert) key() string { return fmt.Sprintf("%d/%s", a.MonitorID, a.Group) }

// alerting asks Datadog which caos monitors are in Alert or No Data, with
// their alerting groups. Warn is deliberately not included: a warning is a
// human's heads-up, not a thing to fix at 03:00.
func alerting(c conf) []alert {
	q := url.Values{}
	q.Set("monitor_tags", c.monitorTag)
	q.Set("group_states", "alert,no data")
	q.Set("with_downtimes", "false")
	req := w.Check(http.NewRequest(http.MethodGet, "https://api.datadoghq.com/api/v1/monitor?"+q.Encode(), nil))
	req.Header.Set("DD-API-KEY", c.secret("datadog-api-key"))
	req.Header.Set("DD-APPLICATION-KEY", c.secret("datadog-app-key"))
	resp := w.Check((&http.Client{Timeout: 30 * time.Second}).Do(req))
	defer resp.Body.Close()
	body := w.Check(io.ReadAll(resp.Body))
	w.True(resp.StatusCode == http.StatusOK, "datadog monitors: %s: %s", resp.Status, strings.TrimSpace(string(body)))

	var monitors []struct {
		ID           int64  `json:"id"`
		Name         string `json:"name"`
		Message      string `json:"message"`
		Query        string `json:"query"`
		OverallState string `json:"overall_state"`
		State        struct {
			Groups map[string]struct {
				Status          string `json:"status"`
				LastTriggeredTS int64  `json:"last_triggered_ts"`
			} `json:"groups"`
		} `json:"state"`
	}
	w.Must(json.Unmarshal(body, &monitors))

	var out []alert
	for _, m := range monitors {
		if m.OverallState != "Alert" && m.OverallState != "No Data" {
			continue
		}
		for group, g := range m.State.Groups {
			if g.Status != "Alert" && g.Status != "No Data" {
				continue
			}
			out = append(out, alert{
				MonitorID: m.ID, Name: m.Name, Group: group, Status: g.Status,
				Since:   time.Unix(g.LastTriggeredTS, 0).UTC().Format(time.RFC3339),
				Message: m.Message, Query: m.Query,
			})
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].key() < out[j].key() })
	return out
}
