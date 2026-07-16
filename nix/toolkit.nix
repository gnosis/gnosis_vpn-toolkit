# toolkit.nix - gnosis_vpn-toolkit workspace package definitions
#
# Package definitions using HOPR nix-lib build tools (nixLib.mkRustPackage) for
# consistent, reproducible builds across platforms. Adapted from
# gnosis_vpn-client's nix/gnosisvpn.nix. The repo is a virtual Cargo workspace;
# each binary is selected with a `--bin` flag against the workspace root (the
# first is `--bin gnosis_vpn-update`). Notes vs the client:
#   * no libmnl / libnftnl / sqlite (no killswitch / routing / db) — reqwest's
#     openssl + cacert come from nix-lib's defaults, so no extra build inputs.
#   * no shell completions, no system-test derivation.
{
  lib,
  nixLib,
  self,
  craneLib,
  advisory-db,
}:

let
  fs = lib.fileset;
  # `unknown` fallback covers a freshly-initialized repo with no commits yet,
  # where neither shortRev nor dirtyShortRev exists.
  rev = toString (self.shortRev or self.dirtyShortRev or "unknown");

  builders = nixLib.mkRustBuilders {
    rustToolchainFile = ../rust-toolchain.toml;
  };

  sources = {
    main = nixLib.mkSrc {
      inherit fs;
      root = ../.;
      extraFiles = [
        ../gnosis_vpn-update/gnosisvpn-public-key.asc
      ];
    };
    test = nixLib.mkTestSrc {
      inherit fs;
      root = ../.;
      # The manifest fixture data files are read by the check tests at
      # CARGO_MANIFEST_DIR/tests/fixtures; include them so the sandboxed test
      # source has them (mkTestSrc only picks up `.rs`/`.toml` files by default).
      extraFiles = [
        ../gnosis_vpn-update/gnosisvpn-public-key.asc
        ../gnosis_vpn-update/tests/fixtures/macos-arm64.json
        ../gnosis_vpn-update/tests/fixtures/macos-arm64.json.asc
      ];
    };
    deps = nixLib.mkDepsSrc {
      inherit fs;
      root = ../.;
    };
    # Includes audit and license config files needed by crane-based checks
    checks = nixLib.mkSrc {
      inherit fs;
      root = ../.;
      extraFiles = [
        ../.cargo/audit.toml
        ../deny.toml
      ];
    };
  };

  # Darwin: set CARGO_BUILD_RUSTFLAGS with +crt-static and system libiconv flags,
  # then rewrite any Nix store libiconv references to /usr/lib so the binary
  # works outside of Nix.
  withDarwinStaticFlags =
    drv:
    drv.overrideAttrs (prev: {
      CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static -C link-arg=-L/usr/lib -C link-arg=-liconv";

      postInstall =
        lib.optionalString (prev ? postInstall && prev.postInstall != null) prev.postInstall
        + ''
          for bin in $(find "$out/bin" -type f); do
            linked_iconv=$(otool -L "$bin" | grep "/nix/store/.*libiconv.*dylib" | awk '{print $1}')

            if [ -n "$linked_iconv" ]; then
              echo "Rewriting $bin - found nix libiconv reference: $linked_iconv"
              install_name_tool -change "$linked_iconv" "/usr/lib/libiconv.2.dylib" "$bin"
              echo "Fixed libiconv path"
            else
              echo "Not rewriting $bin - no nix libiconv reference found"
            fi
          done
        '';
    });

  # Shared build args over the whole workspace. `prependPackageName = false`
  # skips the automatic `-p gnosis_vpn-toolkit` that nix-lib would derive from
  # [workspace.metadata.crane].name — it has no matching package in this virtual
  # workspace. Binary derivations select their target via `extraCargoArgs`
  # (`--bin gnosis_vpn-update`); the checks pass none, so tests/clippy/docs stay
  # workspace-wide (`cargoTestExtraArgs` defaults to `--workspace`). A `--bin`
  # on the checks would restrict `cargo test` and silently skip the lib tests.
  mkToolkitBuildArgs =
    {
      src,
      depsSrc,
      extraCargoArgs ? "",
    }:
    {
      inherit src depsSrc rev;
      prependPackageName = false;
      cargoExtraArgs = extraCargoArgs;
      cargoToml = ../Cargo.toml;
    };
in
{
  # Local builds
  binary-gnosis_vpn-update = builders.local.callPackage nixLib.mkRustPackage (mkToolkitBuildArgs {
    src = sources.main;
    depsSrc = sources.deps;
    extraCargoArgs = "--bin gnosis_vpn-update";
  });

  binary-gnosis_vpn-update-dev = builders.local.callPackage nixLib.mkRustPackage (
    (mkToolkitBuildArgs {
      src = sources.main;
      depsSrc = sources.deps;
      extraCargoArgs = "--bin gnosis_vpn-update";
    })
    // {
      CARGO_PROFILE = "dev";
    }
  );

  # Tests / QA
  toolkit-test = builders.local.callPackage nixLib.mkRustPackage (
    (mkToolkitBuildArgs {
      src = sources.test;
      depsSrc = sources.deps;
    })
    // {
      runTests = true;
    }
  );

  toolkit-clippy = builders.local.callPackage nixLib.mkRustPackage (
    (mkToolkitBuildArgs {
      src = sources.main;
      depsSrc = sources.deps;
    })
    // {
      runClippy = true;
    }
  );

  toolkit-docs = builders.localNightly.callPackage nixLib.mkRustPackage (
    (mkToolkitBuildArgs {
      src = sources.main;
      depsSrc = sources.deps;
    })
    // {
      buildDocs = true;
    }
  );

  # Audit dependencies
  toolkit-audit = craneLib.cargoAudit {
    src = sources.checks;
    inherit advisory-db;
  };

  # Audit licenses
  toolkit-licenses = craneLib.cargoDeny {
    src = sources.checks;
  };

  # macOS — aarch64
  #
  # Use `builders.local` rather than `builders.aarch64-darwin`. The flake
  # exposes package outputs only for the aarch64-darwin system (flake.nix
  # guards `packages`/`checks` with `optionalAttrs`; the linux system exists
  # solely for a devshell), so these outputs are always evaluated and built on
  # a native aarch64-darwin host — `builders.local` is therefore already the
  # aarch64-darwin builder, and a native darwin build is behaviorally identical
  # to nix-lib's `aarch64-darwin` builder (isCross collapses to false,
  # crossSystem to localSystem).
  #
  # This also avoids a bug in nix-lib's `mkAarch64DarwinBuilder`
  # (lib/rust-builders.nix): it computes `isNative` by passing an un-elaborated
  # `lib.systems.examples.aarch64-darwin` to `lib.systems.equals`, which reads
  # `._withoutFunctions` on both operands and throws `attribute
  # '_withoutFunctions' missing` on current nixpkgs. The pinned nix-lib commit
  # is the latest on its default branch, so there is no fixed release to bump
  # to yet.
  binary-gnosis_vpn-update-aarch64-darwin = withDarwinStaticFlags (
    builders.local.callPackage nixLib.mkRustPackage (mkToolkitBuildArgs {
      src = sources.main;
      depsSrc = sources.deps;
      extraCargoArgs = "--bin gnosis_vpn-update";
    })
  );

  binary-gnosis_vpn-update-aarch64-darwin-dev = withDarwinStaticFlags (
    builders.local.callPackage nixLib.mkRustPackage (
      (mkToolkitBuildArgs {
        src = sources.main;
        depsSrc = sources.deps;
        extraCargoArgs = "--bin gnosis_vpn-update";
      })
      // {
        CARGO_PROFILE = "dev";
      }
    )
  );
}
