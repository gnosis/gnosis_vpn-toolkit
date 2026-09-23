{
  description = "Gnosis VPN toolkit";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
    };
    crane = {
      url = "github:ipetkov/crane";
    };

    pre-commit.url = "github:cachix/git-hooks.nix";
    pre-commit.inputs.nixpkgs.follows = "nixpkgs";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };

    # HOPR Nix Library (provides reusable Rust build functions and treefmt config)
    nix-lib = {
      url = "github:hoprnet/nix-lib";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.crane.follows = "crane";
      inputs.rust-overlay.follows = "rust-overlay";
    };
  };

  outputs =
    inputs@{
      self,
      flake-parts,
      nixpkgs,
      rust-overlay,
      crane,
      advisory-db,
      pre-commit,
      nix-lib,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.nix-lib.flakeModules.default
      ];
      # The crate builds on all three: `check-update` and `version` behave the
      # same everywhere, while `update` has an install engine only on macOS and
      # refuses with the apt instructions elsewhere. Each system exposes the
      # binaries it can actually produce (see `packages` below).
      systems = [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      perSystem =
        {
          config,
          self',
          inputs',
          lib,
          system,
          ...
        }:
        let
          pkgs = import nixpkgs {
            localSystem = system;
            overlays = [ (import rust-overlay) ];
          };

          isDarwin = system == "aarch64-darwin";

          nixLib = nix-lib.lib.${system};

          craneLib = (crane.mkLib pkgs).overrideToolchain (
            p:
            (p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
              targets = [ ];
            }
          );

          toolkitPackages = import ./nix/toolkit.nix {
            inherit
              lib
              nixLib
              self
              pkgs
              craneLib
              advisory-db
              ;
          };

          pre-commit-check = pre-commit.lib.${system}.run {
            src = ./.;
            hooks = {
              # https://github.com/cachix/git-hooks.nix
              treefmt.enable = false;
              treefmt.package = config.treefmt.build.wrapper;
              check-executables-have-shebangs.enable = true;
              check-shebang-scripts-are-executable.enable = true;
              check-case-conflicts.enable = true;
              check-symlinks.enable = true;
              check-merge-conflicts.enable = true;
              check-added-large-files.enable = true;
              commitizen.enable = true;
            };
            tools = pkgs;
            excludes = [ ];
          };

        in
        {
          # nix-lib's flake module sets up treefmt and formatter automatically.
          # nix-lib already covers: rustfmt, nixfmt, taplo, yamlfmt, shfmt, prettier, ruff-format.
          nix-lib.treefmt = {
            projectRootFile = "LICENSE";
            extraFormatters = {
              programs.shellcheck.enable = true;
              programs.shfmt.indent_size = 4;
            };
          };

          checks = {
            inherit (toolkitPackages)
              toolkit-clippy
              toolkit-docs
              toolkit-test
              toolkit-audit
              toolkit-licenses
              ;
          };

          # Native builds everywhere; the release binaries are per-target, and
          # each system only exposes the ones it can build (the musl cross
          # pkg-sets are a Linux affair, the darwin pair needs a darwin host).
          packages = {
            inherit (toolkitPackages)
              binary-gnosis_vpn-update
              binary-gnosis_vpn-update-dev
              ;
            default = toolkitPackages.binary-gnosis_vpn-update;
          }
          // lib.optionalAttrs isDarwin {
            inherit (toolkitPackages)
              binary-gnosis_vpn-update-aarch64-darwin
              binary-gnosis_vpn-update-aarch64-darwin-dev
              ;
            # The pre-commit hooks still trip over `.envrc` (a shebang on a
            # non-executable file), so this stays off the Linux shells.
            inherit pre-commit-check;
          }
          // lib.optionalAttrs (!isDarwin) {
            inherit (toolkitPackages)
              binary-gnosis_vpn-update-x86_64-linux
              binary-gnosis_vpn-update-x86_64-linux-dev
              binary-gnosis_vpn-update-aarch64-linux
              binary-gnosis_vpn-update-aarch64-linux-dev
              ;
          };

          devShells.default =
            if isDarwin then
              craneLib.devShell {
                inherit pre-commit-check;
                checks = self.checks.${system};

                packages = [
                  pkgs.cargo-machete
                  pkgs.cargo-shear
                  pkgs.just
                  pkgs.rust-analyzer
                ];

                VERGEN_GIT_SHA = toString (self.shortRev or self.dirtyShortRev or "unknown");
              }
            else
              # Slim shell for non-darwin hosts: the full cargo toolchain for
              # building and testing the crate (and for CI's bump-version step,
              # which runs `nix develop --command cargo metadata … | jq … |
              # cargo update`), without the pre-commit check — its hooks still
              # fail on `.envrc`.
              pkgs.mkShell {
                packages = [
                  (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
                  pkgs.cargo-machete
                  pkgs.cargo-shear
                  pkgs.jq
                  pkgs.just
                ];
              };

        };
      flake = { };
    };
}
