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
      systems = [
        "aarch64-darwin"
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

          packages = {
            inherit (toolkitPackages)
              binary-gnosis_vpn-update
              binary-gnosis_vpn-update-dev
              binary-gnosis_vpn-update-aarch64-darwin
              binary-gnosis_vpn-update-aarch64-darwin-dev
              ;
            inherit pre-commit-check;
            default = toolkitPackages.binary-gnosis_vpn-update;
          };

          devShells.default = craneLib.devShell {
            inherit pre-commit-check;
            checks = self.checks.${system};

            packages = [
              pkgs.cargo-machete
              pkgs.cargo-shear
              pkgs.just
              pkgs.rust-analyzer
            ];

            VERGEN_GIT_SHA = toString (self.shortRev or self.dirtyShortRev or "unknown");
          };

        };
      flake = { };
    };
}
