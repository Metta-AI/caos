{
  # std/go (design/flake-images.md): the script worker for Go — std/bash with a
  # different interpreter. /worker is the script runner (./worker, checked in
  # right here): it fetches the `worker1` arg — the script, the next executable
  # in the chain — and runs it with `go run`. Curry a script on
  # (`--worker1:@=…`) and run it like any image.
  #
  # THE BUILD CACHE IS LOAD-BEARING, NOT BLOAT. It is most of this image's
  # ~500MB against std/bash's 67MB, so it looks like the first thing to cut. A
  # worker container is fresh, so cutting it means `go run` compiling the
  # standard library on EVERY job: 4.4s for a small script, 7.2s for one
  # importing net/http, against ~205ms with it.
  #
  # DO NOT COPY IT SOMEWHERE WRITABLE. Go tolerates a GOCACHE it cannot write —
  # writes fail silently and the run still hits every primed entry — and a
  # writable copy measures the same, so a 135MB copy per job buys nothing.
  #
  # THE PRELUDE (./prelude) IS WHY A SCRIPT CAN IMPORT ANYTHING. A lone `go
  # run` file has no module and may import only the standard library, so
  # /worker copies this module beside the script and runs it from there.
  #
  # ITS DEPS COME FROM NIX, NOT FROM GIT — `vendorHash` below, the same shape
  # as std/cargo's `vendorCargoDeps`. No third-party source is checked in here
  # for the same reason no crate source is.
  #
  # The contract (std/flake-builder/worker): a flake defines everything about
  # the image except the caos additions. /worker included.
  #
  # This directory IS the published tree (literal trees, part 2): flake.nix,
  # worker (the lock is DEPped from the repo root and placed by the
  # flake-builder) — and tests/lint verifies the checked-in redundancies.
  description = "caos std/go — the Go script worker: /worker `go run`s `worker1` against a primed build cache";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };

          # The prelude's dependencies, fetched and pinned by nix. Only
          # `goModules` is ever built — the package itself is never needed,
          # this is `vendorCargoDeps` with a different spelling.
          #
          # `vendorHash` MUST BE UPDATED WHENEVER prelude/go.sum CHANGES. nix
          # prints the correct one on mismatch; there is no way to derive it
          # from go.sum, which is why it is a second hash over the same facts.
          vendorDir =
            (pkgs.buildGoModule {
              pname = "caos-go-prelude-deps";
              version = "0";
              src = ./prelude;
              vendorHash = "sha256-VCQFQiZLnz7QJhFWiXzolP+HkrIz7/qDYfoHNJ0x5No=";
            }).goModules;

          # The module as a worker sees it: the checked-in source with the
          # fetched deps dropped in as `vendor/`, so the runtime `-mod=vendor`
          # resolves without a module cache.
          prelude = pkgs.runCommand "go-prelude" { } ''
            mkdir -p $out
            cp -RL ${./prelude}/. $out/
            chmod -R u+w $out
            cp -RL ${vendorDir} $out/vendor
          '';

          # The stdlib a worker script gets for free; anything outside it
          # still compiles, just slower on first use. A FILE, not a heredoc in
          # the builder below: a heredoc terminator cannot be indented, and an
          # unindented one inside an indented nix string does not survive
          # reformatting.
          # What the prime compiles, and therefore what a worker script gets
          # for free: the prelude, script, and the stdlib both pull in.
          # Anything outside this still compiles, just slower on first use.
          # A FILE, not a heredoc in the builder below: a heredoc terminator
          # cannot be indented, and an unindented one inside an indented nix
          # string does not survive reformatting.
          primeSample = pkgs.writeText "prime-sample.go" ''
            package main

            import (
            	"os"

            	"caos/w"
            	"github.com/bitfield/script"
            )

            func main() {
            	w.Main(func() {
            		w.Step("prime")
            		_ = w.Out(script.Echo("x"))
            		_, _ = w.Try(script.Echo("x"))
            		_ = w.Check(os.Getwd())
            		w.Must(nil)
            		w.True(true, "unreachable")
            	})
            }
          '';

          # TWO STEPS, AND THE SECOND IS NOT REDUNDANT: `go build std` caches
          # every stdlib PACKAGE but not the LINK, and a first link against a
          # cold link-cache costs ~2.5s. Linking the sample pays that here,
          # and compiles the prelude and script while it is at it.
          goCache =
            pkgs.runCommand "go-primed-cache"
              {
                nativeBuildInputs = [ pkgs.go ];
              }
              ''
                export HOME=$TMPDIR
                export GOCACHE=$out
                export GOPATH=$TMPDIR/gopath
                export GOTOOLCHAIN=local
                export GOPROXY=off
                # EVERY ONE OF THESE IS PART OF THE BUILD CACHE KEY, so each
                # must match the image's env below or the worker never hits
                # what this primes. `-trimpath` is the load-bearing one: file
                # paths are in the key too, and the prime happens in a nix
                # build dir while the worker runs in /tmp/run — without it a
                # job pays 884ms re-compiling the prelude instead of 278ms.
                export CGO_ENABLED=0
                export GOFLAGS="-mod=vendor -trimpath"
                mkdir -p $out
                cp -RL ${prelude} $TMPDIR/mod
                chmod -R u+w $TMPDIR/mod
                cp ${primeSample} $TMPDIR/mod/worker1.go
                cd $TMPDIR/mod
                go build std
                go build -o $TMPDIR/sample .
              '';

          # /worker and /gocache. The cache lands at a FIXED PATH the image's
          # GOCACHE env names, rather than at its store path, so the env below
          # is a constant rather than something that moves with every rebuild.
          workerRoot = pkgs.runCommand "go-worker-root" { } ''
            mkdir -p $out/gocache $out/gomod
            install -m 755 ${./worker} $out/worker
            cp -R ${goCache}/. $out/gocache/
            cp -RL ${prelude}/. $out/gomod/
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "go";
          tag = "latest";
          # bash provides /bin/sh too, and /worker is a bash script — three
          # lines of it, the one shell script this image keeps. No Entrypoint:
          # runnerd forces `/bin/caos runner`, which execs /worker.
          contents = [
            workerRoot
            pkgs.go
            pkgs.bash
            pkgs.coreutils
          ];
          config = {
            Env = [
              "PATH=/bin"
              "GOCACHE=/gocache"
              # `go run` builds into a temp dir; /tmp exists in the image
              # (build-builtins.sh keeps it non-empty for deep-deps' sake).
              "TMPDIR=/tmp"
              # WITHOUT THIS, GO REACHES THE NETWORK. A `go` directive newer
              # than the shipped toolchain makes go DOWNLOAD a toolchain
              # ("go: downloading go1.99.0"); `local` turns that into a plain
              # error naming the version instead.
              "GOTOOLCHAIN=local"
              # No module cache is shipped — the prelude's deps are vendored —
              # so an import outside it must fail loudly rather than fetch.
              # `-trimpath` is not cosmetic: see goCache above, it is what
              # makes the primed cache hit from /tmp/run.
              "GOPROXY=off"
              "GOFLAGS=-mod=vendor -trimpath"
              "GOPATH=/tmp/gopath"
              # Must match the prime (see goCache): CGO_ENABLED is part of the
              # build cache key, and there is no C compiler here anyway.
              "CGO_ENABLED=0"
            ];
          };
        };
    in
    {
      packages = builtins.listToAttrs (
        map
          (system: {
            name = system;
            value = {
              caosImage = forSystem system;
            };
          })
          [
            "x86_64-linux"
            "aarch64-linux"
          ]
      );
    };
}
