{
  # The fixture: one tiny input with no inputs of its own, and nothing to build.
  description = "tests/flake-update fixture";
  inputs.systems.url = "github:nix-systems/default";
  outputs = { self, systems }: { };
}
