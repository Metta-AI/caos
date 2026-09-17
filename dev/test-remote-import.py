#!/usr/bin/env python3
"""Real HTTPS Git + running CAOS server regression test (no external service).
Requires Python 3, Git and openssl.
Usage: python3 dev/test-remote-import.py /path/to/server [/path/to/caos]
Ports default to 9093/5003; override CAOS_IMPORT_TEST_PORT/GIT_IMPORT_TEST_PORT.
"""
import base64
import concurrent.futures
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request


def run(*args, **kwargs):
    return subprocess.check_output(args, stderr=subprocess.PIPE, **kwargs).decode().strip()


def wait_for(predicate):
    for _ in range(300):
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("timed out")


def main():
    binary = str(Path(sys.argv[1]).resolve())
    port = int(os.environ.get("CAOS_IMPORT_TEST_PORT", "9093"))
    remote_port = int(os.environ.get("GIT_IMPORT_TEST_PORT", "5003"))
    token = "fixture-credential-never-in-import-state"
    with tempfile.TemporaryDirectory(prefix="caos-import-", dir=os.environ.get("TMPDIR")) as temp:
        root = Path(temp)
        origin = root / "origin.git"
        run("git", "init", "--bare", "-q", str(origin))
        run("git", "--git-dir", str(origin), "config", "uploadpack.allowAnySHA1InWant", "true")
        run("git", "--git-dir", str(origin), "symbolic-ref", "HEAD", "refs/heads/main")
        env = dict(os.environ, GIT_AUTHOR_NAME="test", GIT_AUTHOR_EMAIL="test@example.com", GIT_COMMITTER_NAME="test", GIT_COMMITTER_EMAIL="test@example.com")
        def advance(parent=None):
            data = ("unique blob " + str(time.time_ns()) + "\n") * 100
            blob = run("git", "--git-dir", str(origin), "hash-object", "-w", "--stdin", input=data.encode())
            tree = run("git", "--git-dir", str(origin), "mktree", input=f"100644 blob {blob}\tfile\n".encode())
            args = ["git", "--git-dir", str(origin), "commit-tree", tree, "-m", "fixture"]
            if parent:
                args += ["-p", parent]
            commit = run(*args, env=env)
            run("git", "--git-dir", str(origin), "update-ref", "refs/heads/main", commit)
            return commit, tree, blob
        first = advance()
        second = advance(first[0])
        cert, key = root / "cert.pem", root / "key.pem"
        run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost", "-keyout", str(key), "-out", str(cert))
        observations = []
        fault = {"fail": False, "pause": False}
        transfer = threading.Event()
        release = threading.Event()

        class Remote(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            def log_message(self, *_):
                pass
            def do_GET(self):
                self.serve()
            def do_POST(self):
                self.serve()
            def serve(self):
                path, _, query = self.path.partition("?")
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                private = path.startswith("/private.git/")
                expected = "Basic " + base64.b64encode(("x-access-token:" + token).encode()).decode()
                authorized = self.headers.get("Authorization") == expected
                if private and not authorized:
                    self.send_response(401)
                    self.send_header("WWW-Authenticate", 'Basic realm="fixture"')
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                assert private or self.headers.get("Authorization") is None, "credential escaped repository scope"
                fetching = b"want " in body
                if fetching:
                    transfer.set()
                    if fault["pause"]:
                        release.wait(30)
                    if fault["fail"]:
                        self.send_response(503)
                        # A hostile remote can echo credentials. They must never
                        # reach a tool error or the server's log.
                        response = token.encode()
                        self.send_header("Content-Length", str(len(response)))
                        self.end_headers()
                        self.wfile.write(response)
                        return
                git_env = dict(os.environ, GIT_PROJECT_ROOT=str(root), GIT_HTTP_EXPORT_ALL="1", REQUEST_METHOD=self.command, PATH_INFO=path.replace("/public.git/", "/origin.git/").replace("/private.git/", "/origin.git/"), QUERY_STRING=query, CONTENT_TYPE=self.headers.get("Content-Type", ""), CONTENT_LENGTH=str(len(body)))
                # Protocol v2 supplies explicit negotiation requests.
                if self.headers.get("Git-Protocol"):
                    git_env["HTTP_GIT_PROTOCOL"] = self.headers["Git-Protocol"]
                response = subprocess.check_output(["git", "http-backend"], input=body, env=git_env)
                headers, payload = response.split(b"\r\n\r\n", 1)
                observations.append((path, body, len(payload), authorized))
                self.send_response(200)
                for line in headers.split(b"\r\n"):
                    name, value = line.decode().split(":", 1)
                    if name.lower() not in ("status", "content-length"):
                        self.send_header(name, value.strip())
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                try:
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
                    pass
        remote = http.server.ThreadingHTTPServer(("127.0.0.1", remote_port), Remote)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(cert, key)
        remote.socket = tls.wrap_socket(remote.socket, server_side=True)
        thread = threading.Thread(target=remote.serve_forever, daemon=True)
        thread.start()
        server = None
        log = open(root / "server.log", "wb")
        odb = root / "server.git"
        def start():
            process = subprocess.Popen([binary], env=dict(os.environ, SERVER_ADDR=f"127.0.0.1:{port}", CAOS_GIT_DIR=str(odb), GIT_SSL_CAINFO=str(cert)), stdout=log, stderr=log, start_new_session=True)
            def ready():
                if process.poll() is not None:
                    raise AssertionError((root / "server.log").read_text())
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=.1):
                        return True
                except OSError:
                    return False
            wait_for(ready)
            return process
        def stop(process):
            if process:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=10)
        base = f"http://127.0.0.1:{port}"
        public = f"https://localhost:{remote_port}/public.git"
        private = f"https://localhost:{remote_port}/private.git"
        def payload(key, source=public, revision=None, scope=""):
            return {"source":source, "revision":revision, "invocation":hashlib.sha256(key.encode()).hexdigest(), "secret_scope":scope}
        def call(data, credential=None, expected=200):
            headers = {"Content-Type":"application/json"}
            if credential:
                headers["X-Caos-Git-Token"] = credential
            request = urllib.request.Request(base + "/git/import", json.dumps(data).encode(), headers)
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    status, body = response.status, response.read()
            except urllib.error.HTTPError as error:
                status, body = error.code, error.read()
            assert token.encode() not in body
            assert status == expected, (status, body)
            return json.loads(body) if status == 200 else body
        def visible(commit):
            closure = run("git", "--git-dir", str(origin), "rev-list", "--objects", "--no-object-names", commit).splitlines()
            for oid in closure:
                with urllib.request.urlopen(base + "/object/" + oid) as response:
                    raw = response.read()
                assert hashlib.sha1(raw).hexdigest() == oid
        try:
            server = start()
            # Warm the live ODB before fetching a newly packed closure.
            try:
                urllib.request.urlopen(base + "/object/" + second[0])
                raise AssertionError("object unexpectedly present")
            except urllib.error.HTTPError as error:
                assert error.code == 404
            run("git", "--git-dir", str(odb), "config", "fetch.unpackLimit", "1")
            initial = call(payload("initial"))
            assert initial["commit"] == second[0] and initial["default_branch"] == "main"
            visible(second[0])
            assert list((odb / "objects/pack").glob("*.pack")), "fixture must exercise packed objects"
            count = len(observations)
            assert call(payload("initial")) == initial
            assert len(observations) == count, "completed replay contacted remote"
            third = advance(second[0])
            fresh = call(payload("fresh", revision="main"))
            assert fresh["commit"] == third[0]
            visible(third[0])
            fetches = [body for _, body, _, _ in observations[count:] if b"want " in body]
            assert any(second[0].encode() in body for body in fetches), "complete prior import was not a negotiation tip"
            pack_count = len(list((odb / "objects/pack").glob("*.pack")))
            before_same = len(observations)
            assert call(payload("same-tip"))["commit"] == third[0]
            same_tip_requests = len(observations) - before_same
            assert len(list((odb / "objects/pack").glob("*.pack"))) == pack_count, "unchanged tip transferred another pack"
            assert call(payload("exact", revision=first[0]))["commit"] == first[0]
            run("git", "--git-dir", str(origin), "tag", "-a", "release", first[0], "-m", "annotated tag", env=env)
            assert call(payload("tag", revision="refs/tags/release"))["commit"] == first[0]
            assert call(payload("full-ref", revision="refs/heads/main"))["commit"] == third[0]
            count = len(observations)
            with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
                results = list(pool.map(lambda _: call(payload("duplicate")), range(6)))
            assert all(r == results[0] for r in results)
            # Only one invocation resolves the branch; duplicates replay it.
            duplicate_count = len(observations) - count
            assert duplicate_count == same_tip_requests, "duplicate invocations transferred or resolved more than once"
            assert call(payload("duplicate")) == results[0] and len(observations) - count == duplicate_count
            with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
                results = list(pool.map(lambda i: call(payload(f"independent-{i}")), range(3)))
            assert all(r["commit"] == third[0] for r in results)
            call(payload("private", private, scope="a" * 40), token)
            call(payload("private", private, scope="b" * 40), "wrong-token", expected=502)
            call(payload("private", private), expected=502)
            # Separate credentials do not inherit the first scope's completed result.
            call(payload("private", private, scope="b" * 40), token)
            # Failure after pinning, followed by remote advancement and a replay.
            fourth = advance(third[0])
            fault["fail"] = True
            call(payload("interrupted"), expected=502)
            records = [json.loads(p.read_text()) for p in (odb / "caos-imports").glob("*/*.json")]
            assert any(r["commit"] == fourth[0] and not r["complete"] for r in records)
            fifth = advance(fourth[0])
            fault["fail"] = False
            assert call(payload("interrupted"))["commit"] == fourth[0]
            visible(fourth[0])
            # Restart between pin and transfer: kill the server and its Git child.
            transfer.clear(); release.clear(); fault["pause"] = True
            pool = concurrent.futures.ThreadPoolExecutor(max_workers=1)
            crashed = payload("crashed", private, scope="a" * 40)
            future = pool.submit(call, crashed, token)
            assert transfer.wait(20)
            def check_arguments(pid):
                assert token.encode() not in Path(f"/proc/{pid}/cmdline").read_bytes(), "credential in process arguments"
                children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
                for child in children:
                    try:
                        check_arguments(int(child))
                    except FileNotFoundError:
                        pass
            check_arguments(server.pid)
            stop(server); server = None
            release.set(); fault["pause"] = False
            try:
                future.result()
                raise AssertionError("crashed call returned success")
            except (urllib.error.URLError, http.client.RemoteDisconnected, ConnectionResetError):
                pass
            pool.shutdown()
            sixth = advance(fifth[0])
            server = start()
            assert call(crashed, token)["commit"] == fifth[0]
            # A response lost after completion replays the same durable observation.
            raw = json.dumps(payload("lost-response")).encode()
            sock = socket.create_connection(("127.0.0.1", port))
            sock.sendall(f"POST /git/import HTTP/1.1\r\nHost: localhost\r\nContent-Length: {len(raw)}\r\nConnection: close\r\n\r\n".encode() + raw)
            sock.close()
            def completed():
                return any(json.loads(p.read_text())["commit"] == sixth[0] and json.loads(p.read_text())["complete"] for p in (odb / "caos-imports").glob("*/*.json"))
            wait_for(completed)
            advance(sixth[0])
            assert call(payload("lost-response"))["commit"] == sixth[0]
            # Commit presence alone cannot certify a partial import. Seed only
            # the commit object, leaving its tree/blob absent, then import it.
            partial = advance(sixth[0])
            commit_bytes = subprocess.check_output(["git", "--git-dir", str(origin), "cat-file", "commit", partial[0]])
            run("git", "--git-dir", str(odb), "hash-object", "-t", "commit", "-w", "--stdin", input=commit_bytes)
            replacement = "refs/replace/" + partial[0]
            run("git", "--git-dir", str(odb), "update-ref", replacement, first[0])
            assert call(payload("partial", revision=partial[0]))["commit"] == partial[0]
            visible(partial[0])
            run("git", "--git-dir", str(odb), "update-ref", "-d", replacement)
            # A shallow upstream must neither be certified complete nor mutate
            # the shared ODB's shallow bookkeeping. Replay recovers when complete.
            shallow_parent = advance()
            shallow_tip = advance(shallow_parent[0])
            (origin / "shallow").write_text(shallow_tip[0] + "\n")
            call(payload("shallow-origin"), expected=502)
            assert not (odb / "shallow").exists()
            (origin / "shallow").unlink()
            assert call(payload("shallow-origin"))["commit"] == shallow_tip[0]
            visible(shallow_tip[0])
            for source in ["/tmp/repo", "file:///tmp/repo", "http://localhost/x", "ssh://git@host/repo", "ext::helper", "https://user:password@host/repo", "https://host/repo?x", "https://host/repo#x", "https://host/../repo", "https://host:bad/repo"]:
                call(payload("invalid", source), expected=400)
            for revision in ["", "-main", "main:other", "../main", "refs/heads/*", "main\n"]:
                call(payload("invalid", revision=revision), expected=400)
            call(dict(payload("invalid"), extra="no"), expected=400)
            call(payload("no-scope", private), token, expected=400)
            assert not (odb / "FETCH_HEAD").exists()
            assert not (odb / "shallow").exists()
            assert not run("git", "--git-dir", str(odb), "for-each-ref"), "imports created refs"
            if len(sys.argv) > 2:
                # Exercise secret-file -> sensitive header -> credential helper.
                # This standalone fixture owns /cas only inside its test container.
                cas = Path("/cas")
                assert not cas.exists(), "run CLI fixture in an empty test container"
                cas.mkdir()
                try:
                    arguments_dir = root / "args"
                    arguments_dir.mkdir()
                    (arguments_dir / "secret-hash").write_text("c" * 40)
                    token_file = root / "token"
                    token_file.write_text(token + "\n")
                    cli_env = dict(os.environ, CAOS_SERVER_URL=base)
                    cli = str(Path(sys.argv[2]).resolve())
                    run(cli, "put", str(arguments_dir), str(cas / "args"), env=cli_env)
                    arguments = [cli, "import-git", private, first[0], "--invocation=" + "d" * 64, "--github-token-file=" + str(token_file)]
                    assert run(*arguments, env=cli_env) == first[0]
                    # Rotating only the value must not change an invocation key.
                    token_file.write_text("rotated-but-same-scope")
                    assert run(*arguments, env=cli_env) == first[0]
                    public_result = json.loads(run(cli, "import-git", public, "--json", env=cli_env))
                    assert public_result["complete"]
                    result = subprocess.run([cli, "import-git", "/tmp/repo"], env=cli_env, capture_output=True)
                    assert result.returncode and token.encode() not in result.stderr
                finally:
                    shutil.rmtree(cas)
            for name, value in [("gc.auto", "0"), ("receive.autogc", "false"), ("maintenance.geometric-repack.enabled", "false"), ("core.fsync", "objects,reference"), ("core.fsyncMethod", "batch")]:
                assert run("git", "--git-dir", str(odb), "config", name) == value
            for p in [odb / "config", root / "server.log", *list((odb / "caos-imports").glob("*/*.json"))]:
                assert token.encode() not in p.read_bytes(), f"credential leaked to {p.name}"
            for oid in run("git", "--git-dir", str(odb), "cat-file", "--batch-all-objects", "--batch-check=%(objectname)").splitlines():
                data = subprocess.check_output(["git", "--git-dir", str(odb), "cat-file", "-p", oid])
                assert token.encode() not in data, "credential leaked into Git object"
            print("remote-import: HTTPS, credentials/scopes, freshness, explicit commit, history, live packs, negotiation, concurrency, interrupted transfer, restart, lost response, and rejection PASS")
        finally:
            release.set()
            stop(server)
            remote.shutdown(); remote.server_close(); thread.join()
            log.close()


if __name__ == "__main__":
    main()
