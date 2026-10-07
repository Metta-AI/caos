// The `remove-dev-envs` worker: list the cloud environments, and delete the ones
// named "z Caos Dev …" one at a time.
//
// STAGES, selected by `stage` (SPEC, "Worker scripts"):
//
//	list     (default) ask `drive` for the environments and hand the dev ones on.
//	delete   the `then` of that, and of itself: delete the first of `ids`, and
//	         continue with the rest. A worker may not wait for a run, so a loop is
//	         a stage that tail-calls itself, and what it has done so far rides in
//	         `done`.
//
// BY ID, not by name: an id is unique and a name is not, and what is listed is
// what is deleted.
package main

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"time"

	"caos/w"
)

const prefix = "z Caos Dev "

func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	var stderr strings.Builder
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	return strings.TrimSpace(string(out))
}

func caos(args ...string) string { return run("caos", args...) }

func has(name string) bool {
	_, err := os.Lstat("/cas/args/" + name)
	return err == nil
}

func arg(name string) string {
	caos("get", "/cas/args/"+name)
	return string(w.Check(os.ReadFile("/cas/args/" + name)))
}

func put(text string) {
	w.Must(os.WriteFile("/tmp/out", []byte(text), 0o644))
	caos("put", "/tmp/out", "/cas/out")
}

func next(stage string, extra ...string) string {
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage=" + stage, "--drive:@=/cas/args/drive"}
	return caos(append(args, extra...)...)
}

// drive is a call to `drive`, as an image. `at` is the time, because a worker's
// result is memoized on its ArgTree and this is not a pure function of it.
func drive(verb string, extra ...string) string {
	args := []string{"curry", "--base:@=/cas/args/drive", "--verb=" + verb,
		fmt.Sprintf("--at=%d", time.Now().UnixNano())}
	return caos(append(args, extra...)...)
}

// seed is a blob for `run-then` to run over: neither call reads its `in`.
func seed() string {
	w.Must(os.WriteFile("/tmp/seed", []byte("x"), 0o644))
	caos("put", "/tmp/seed", "/cas/seed")
	return "/cas/seed"
}

func main() {
	w.Main(func() {
		stage := "list"
		if has("stage") {
			stage = strings.TrimSpace(arg("stage"))
		}
		switch stage {
		case "list":
			caos("run-then", seed(), "--run:hash="+drive("env-list"), "--then:hash="+next("delete"))
		case "delete":
			remove()
		default:
			w.True(false, "unknown --stage: %s", stage)
		}
	})
}

// devEnvs picks the dev environments out of `drive env-list`'s output, whose lines
// are `<id>   <name>   [<state>]   <kind>`.
func devEnvs(listing string) (ids, names []string) {
	for _, line := range strings.Split(listing, "\n") {
		id, rest, ok := strings.Cut(strings.TrimSpace(line), " ")
		if !ok || !strings.HasPrefix(id, "env_") {
			continue
		}
		name, _, _ := strings.Cut(strings.TrimSpace(rest), "   [")
		if strings.HasPrefix(name, prefix) {
			ids, names = append(ids, id), append(names, name)
		}
	}
	return ids, names
}

// remove is the `delete` stage. Its first run has `--result` (the listing) and
// no `ids`; every later one has `ids` (what is left) and `done` (what is gone).
func remove() {
	var ids, names []string
	done := ""
	if has("ids") {
		ids = strings.Fields(arg("ids"))
		names = strings.Split(strings.TrimRight(arg("names"), "\n"), "\n")
		done = arg("done")
	} else {
		ids, names = devEnvs(arg("result"))
	}
	if len(ids) == 0 {
		if done == "" {
			put("no cloud environments named \"" + prefix + "…\"\n")
		} else {
			put(done)
		}
		return
	}
	// The one being deleted is only reported once the call has been made: the
	// `then` that arrives says so by `result`, so the line for it is added when
	// this stage next runs. Here it is added up front, and a failure fails the job
	// before anything claims it was done.
	id, name := ids[0], names[0]
	w.Must(os.WriteFile("/tmp/ids", []byte(strings.Join(ids[1:], "\n")+"\n"), 0o644))
	w.Must(os.WriteFile("/tmp/names", []byte(strings.Join(names[1:], "\n")+"\n"), 0o644))
	w.Must(os.WriteFile("/tmp/done", []byte(done+"deleted "+id+" ("+name+")\n"), 0o644))
	caos("put", "/tmp/ids", "/cas/next-ids")
	caos("put", "/tmp/names", "/cas/next-names")
	caos("put", "/tmp/done", "/cas/next-done")
	caos("run-then", seed(), "--run:hash="+drive("env-delete", "--env="+id),
		"--then:hash="+next("delete", "--ids:@=/cas/next-ids", "--names:@=/cas/next-names", "--done:@=/cas/next-done"))
}
