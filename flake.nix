{
  description = "WebAssembly shell";

  inputs.enarx.url = github:enarx/enarx;
  inputs.nixify.url = github:rvolosatovs/nixify;

  outputs = {
    enarx,
    nixify,
    ...
  }:
    with nixify.lib;
      rust.mkFlake {
        src = ./.;

        ignorePaths = [
          "/.github"
          "/.gitignore"
          "/flake.lock"
          "/flake.nix"
          "/rust-toolchain.toml"
        ];

        overlays = [
          enarx.overlays.rust
          enarx.overlays.default
        ];

        clippy.allFeatures = true;
        clippy.allTargets = true;
        clippy.deny = ["warnings"];

        withDevShells = {
          devShells,
          pkgs,
          ...
        }:
          extendDerivations {
            buildInputs = [
              pkgs.enarx
              pkgs.wasmtime
            ];
          }
          devShells;
      };
}
