{
  description = "A caos client repo: the tree an agent session starts from";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # THE PIN. This one line chooses the caos client the session installs, the
    # tools it offers and the tree it evaluates — `flake.lock` records the
    # commit, and `integrations/claude-code/cloud/caos-pin.sh` reads it back out.
    #
    # Move it with `nix flake update caos`, then update the two `rev=` values in
    # `.caos-expr` to match: `std/flake-input-loader` refuses to evaluate a tree
    # whose expression and lockfile disagree, naming both revisions, so this
    # cannot drift silently.
    caos.url = "github:Metta-AI/caos";
  };

  # Nothing is built from here. The flake exists so the pin has somewhere to
  # live that nix can resolve and lock; caos reads `flake.lock`, and
  # `std/flake-input-loader` checks `flake.nix` for presence only.
  outputs = _: { };
}
