{
  description = "Floresta triage: what is blocked on you, then what is worth your evening";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
  };

  outputs = inputs @ {
    flake-parts,
    nixpkgs,
    rust-overlay,
    crane,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      # The unit it runs in is NixOS-only, like the desktop it runs on.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      perSystem = {
        system,
        pkgs,
        ...
      }: let
        craneLib =
          (crane.mkLib pkgs).overrideToolchain (p:
            p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml);

        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          strictDeps = true;
        };

        # Build only the dependencies, so the gates can share that work.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        oss-board = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;

            pname = "oss-board";

            # The test gate runs the tests. Do not run them two times.
            doCheck = false;

            meta = {
              description = "Floresta triage: what is blocked on you, then what is worth your evening";
              homepage = "https://github.com/lorenzolfm/oss-board";
              mainProgram = "oss-board";
              platforms = pkgs.lib.platforms.linux;
            };
          });

        # One derivation for each gate; `nix flake check` runs all of them.
        gates = {
          inherit oss-board;

          oss-board-clippy = craneLib.cargoClippy (commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            });

          oss-board-test = craneLib.cargoNextest (commonArgs
            // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
              # The unshipped tests build real repositories.
              nativeCheckInputs = [pkgs.git];
            });

          oss-board-fmt = craneLib.cargoFmt {inherit src;};
        };
      in {
        _module.args.pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };

        checks = gates;

        packages =
          gates
          // {
            default = oss-board;
          };

        apps.default = {
          type = "app";
          program = "${pkgs.lib.getExe oss-board}";
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [
            cargo-nextest
            git
          ];
        };

        formatter = pkgs.alejandra;
      };
    };
}
