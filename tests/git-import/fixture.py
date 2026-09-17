#!/usr/bin/env python3
"""Real HTTPS Git + running CAOS server regression test (no external service).
Requires Python 3, Git and openssl.
Usage: python3 tests/git-import/fixture.py /path/to/server
Uses ephemeral loopback ports.
"""
import base64
import concurrent.futures
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import shlex
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
    try:
        return subprocess.check_output(args, stderr=subprocess.PIPE, **kwargs).decode().strip()
    except subprocess.CalledProcessError as error:
        raise AssertionError(f"{args}: {error.stderr.decode(errors='replace')}") from error


def wait_for(predicate):
    for _ in range(300):
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("timed out")


def main():
    binary = str(Path(sys.argv[1]).resolve())
    cli = str(Path(sys.argv[2]).resolve()) if len(sys.argv) > 2 else None
    port = int(os.environ.get("CAOS_IMPORT_TEST_PORT", "0"))
    remote_port = int(os.environ.get("GIT_IMPORT_TEST_PORT", "0"))
    if not port:
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
    token = "fixture-credential-never-in-import-state"
    with tempfile.TemporaryDirectory(prefix="caos-import-", dir=os.environ.get("TMPDIR")) as temp:
        root = Path(temp)
        # Record server-side Git operations without logging arguments or secrets.
        git_commands = root / "git-commands"
        git_bin = root / "git-bin"
        git_bin.mkdir()
        git_shim = git_bin / "git"
        git_shim.write_text("#!/bin/sh\nfor arg do\n"
            'case "$arg" in ls-remote|fetch|cat-file|rev-list|show-index)\n'
            "printf '%s\\n' \"$arg\" >> " + shlex.quote(str(git_commands)) + "; break;;\n"
            "esac\ndone\nexec " + shlex.quote(shutil.which("git")) + ' "$@"\n')
        git_shim.chmod(0o755)
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
        fault = {"fail": False}
        surplus = {}
        def packet(data):
            return f"{len(data) + 4:04x}".encode() + data

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
                if path.startswith("/surplus.git/"):
                    if self.command == "GET":
                        payload = (packet(b"# service=git-upload-pack\n") + b"0000"
                            + packet(surplus["tip"].encode() + b" refs/heads/main\0shallow\n")
                            + packet(b"shallow " + surplus["bad"].encode() + b"\n") + b"0000")
                        content_type = "application/x-git-upload-pack-advertisement"
                    else:
                        payload = packet(b"NAK\n") + surplus["pack"]
                        content_type = "application/x-git-upload-pack-result"
                    self.send_response(200)
                    self.send_header("Content-Type", content_type)
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
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
        remote_port = remote.server_address[1]
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(cert, key)
        remote.socket = tls.wrap_socket(remote.socket, server_side=True)
        thread = threading.Thread(target=remote.serve_forever, daemon=True)
        thread.start()
        server = None
        log = open(root / "server.log", "wb")
        odb = root / "server.git"
        def start():
            process = subprocess.Popen([binary], env=dict(os.environ, PATH=str(git_bin) + os.pathsep + os.environ["PATH"], SERVER_ADDR=f"127.0.0.1:{port}", CAOS_GIT_DIR=str(odb), GIT_SSL_CAINFO=str(cert)), stdout=log, stderr=log, start_new_session=True)
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
        def payload(commit, source=public):
            return {"source":source, "commit":commit}
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
        def object_request(oid, expected=200):
            try:
                with urllib.request.urlopen(base + "/object/" + oid) as response:
                    status, body = response.status, response.read()
            except urllib.error.HTTPError as error:
                status, body = error.code, error.read()
            assert status == expected, (oid, status, body)
            return body
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
            assert call(payload(second[0])) == {"commit": second[0]}
            visible(second[0])
            assert list((odb / "objects/pack").glob("*.pack"))
            before = len(observations)
            commands = git_commands.read_text()
            assert call(payload(second[0])) == {"commit": second[0]}
            assert len(observations) == before, "completed import contacted remote"
            assert git_commands.read_text() == commands, "completed history was verified again"
            third = advance(second[0])
            assert call(payload(third[0]))["commit"] == third[0]
            visible(third[0])
            fetches = [body for _, body, _, _ in observations[before:] if b"want " in body]
            assert any(second[0].encode() in body for body in fetches), "missing negotiation tip"
            assert "ls-remote" not in git_commands.read_text(), "endpoint resolved a ref"
            fourth = advance(third[0])
            before = len(observations)
            with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
                results = list(pool.map(lambda _: call(payload(fourth[0])), range(6)))
            assert all(r == {"commit": fourth[0]} for r in results)
            assert sum(b"want " in body for _, body, _, _ in observations[before:]) == 1
            private_tip = advance(fourth[0])
            call(payload(private_tip[0], private), "wrong-token", expected=502)
            call(payload(private_tip[0], private), expected=502)
            call(payload(private_tip[0], private), token)
            # Already imported content is available by hash, as with /object/H.
            before = len(observations)
            call(payload(private_tip[0], private))
            assert len(observations) == before
            fifth = advance(fourth[0])
            fault["fail"] = True
            call(payload(fifth[0]), expected=502)
            advance(fifth[0])
            fault["fail"] = False
            stop(server); server = None
            server = start()
            assert call(payload(fifth[0]))["commit"] == fifth[0]
            visible(fifth[0])
            commit_bytes = subprocess.check_output(["git", "--git-dir", str(origin), "cat-file", "commit", first[0]])
            malformed = commit_bytes.replace(b"\nauthor ", b"\nparent " + b"2" * 40 + b"\nauthor ", 1)
            malformed_oid = run("git", "--git-dir", str(origin), "hash-object", "-t", "commit", "-w", "--stdin", input=malformed)
            # A remote can declare an unrelated surplus commit shallow. Git may
            # accept its pack and omit that unused boundary from the final shallow
            # file. Checking every received root must still reject its missing parent.
            surplus_good = advance()
            object_ids = run("git", "--git-dir", str(origin), "rev-list",
                "--objects", "--no-object-names", surplus_good[0]).splitlines()
            surplus.update(tip=surplus_good[0], bad=malformed_oid,
                pack=subprocess.check_output(["git", "--git-dir", str(origin), "pack-objects", "--stdout"],
                    input=("\n".join(object_ids + [malformed_oid]) + "\n").encode()))
            commands_before = git_commands.read_text()
            call(payload(surplus_good[0], f"https://localhost:{remote_port}/surplus.git"), expected=502)
            assert "show-index" in git_commands.read_text()[len(commands_before):], "surplus fixture did not reach staged pack verification"
            object_request(surplus_good[0], expected=404)
            object_request(malformed_oid, expected=404)

            shallow_parent = advance()
            shallow_tip = advance(shallow_parent[0])
            (origin / "shallow").write_text(shallow_tip[0] + "\n")
            call(payload(shallow_tip[0]), expected=502)
            object_request(shallow_tip[0], expected=404)
            object_request(shallow_parent[0], expected=404)
            assert not list((odb / "caos-imports").glob("*/incoming"))
            assert not (odb / "shallow").exists()
            (origin / "shallow").unlink()
            call(payload(shallow_tip[0]))
            visible(shallow_tip[0])
            for source in ["/tmp/repo", "file:///tmp/repo", "http://localhost/x", "ssh://git@host/repo", "ext::helper", "https://user:password@host/repo", "https://host/repo?x", "https://host/repo#x", "https://host/../repo", "https://host:bad/repo"]:
                call(payload(first[0], source), expected=400)
            for commit in ["", "main", "HEAD", "-main", "../main", "a" * 39, "g" * 40]:
                call(payload(commit), expected=400)
            call({"source": public, "revision": "main"}, expected=400)
            call(dict(payload(first[0]), invocation="a" * 64), expected=400)
            noncommit = advance()
            call(payload(noncommit[1]), expected=502)
            object_request(noncommit[1], expected=404)
            assert not (odb / "FETCH_HEAD").exists()
            assert not (odb / "shallow").exists()
            assert not run("git", "--git-dir", str(odb), "for-each-ref")
            for p in [odb / "config", root / "server.log", *list((odb / "caos-imports").glob("*/*"))]:
                assert token.encode() not in p.read_bytes(), f"credential leaked to {p.name}"
            if cli:
                token_file = root / "token"
                token_file.write_text(token)
                cli_env = dict(os.environ, CAOS_SERVER_URL=base)
                cli_tip = advance()
                result = run(cli, "import-git", private, cli_tip[0], "--github-token-file=" + str(token_file), env=cli_env)
                assert result == cli_tip[0]
                visible(cli_tip[0])
                assert run(cli, "import-git", private, cli_tip[0], env=cli_env) == cli_tip[0]
                for args in [[public], [public, "main"], ["/tmp/repo", cli_tip[0]], [public, cli_tip[0], "--invocation=old"]]:
                    invalid = subprocess.run([cli, "import-git", *args], env=cli_env, capture_output=True)
                    assert invalid.returncode != 0
                    assert token.encode() not in invalid.stdout + invalid.stderr
            print("git-import: exact commits, HTTPS credentials, full history, quarantined imports, packs, reuse, concurrency and retry PASS")
        finally:
            stop(server)
            remote.shutdown(); remote.server_close(); thread.join()
            log.close()


if __name__ == "__main__":
    main()
