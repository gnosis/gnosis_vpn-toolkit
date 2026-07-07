# toolkit.nix - gnosis_vpn-toolkit package definitions
#
# Package definitions using HOPR nix-lib build tools (nixLib.mkRustPackage) for
# consistent, reproducible builds across platforms. Adapted from
# gnosis_vpn-client's nix/gnosisvpn.nix, simplified for a single-crate binary:
#   * no libmnl / libnftnl / sqlite (no killswitch / routing / db) — reqwest's
#     openssl + cacert come from nix-lib's defaults, so no extra build inputs.
#   * builds a single `--bin gnosis_vpn-toolkit`.
#   * no shell completions, no system-test derivation.
{
  lib,
  nixLib,
  self,
  pkgs,
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
        ../gnosisvpn-public-key.asc
      ];
    };
    test = nixLib.mkTestSrc {
      inherit fs;
      root = ../.;
      # The manifest fixture data files are read by the check tests at
      # CARGO_MANIFEST_DIR/tests/fixtures; include them so the sandboxed test
      # source has them (mkTestSrc only picks up `.rs` test files by default).
      extraFiles = [
        ../gnosisvpn-public-key.asc
        ../tests/fixtures/linux-amd64.json
        ../tests/fixtures/linux-amd64.json.asc
        ../tests/fixtures/linux-arm64.json
        ../tests/fixtures/linux-arm64.json.asc
        ../tests/fixtures/macos-arm64.json
        ../tests/fixtures/macos-arm64.json.asc
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

  # Target-specific package sets for cross-compiled Linux builds.
  x86_64LinuxStaticPkgs = pkgs.pkgsCross.musl64.pkgsStatic;
  aarch64LinuxStaticPkgs = pkgs.pkgsCross.aarch64-multiplatform-musl.pkgsStatic;

  # Parameters required for musl static builds that nix-lib does not cover.
  # nix-lib handles CARGO_BUILD_TARGET, CARGO_TARGET_*_LINKER, +crt-static, and
  # openssl paths. We only need to disable the fortify hardening flag (musl
  # incompatible) and, per arch, point cc-rs at the right cross compiler for the
  # C in jemalloc's build script.
  mkLinuxStaticEnv = _staticPkgs: {
    hardeningDisable = [ "fortify" ];
  };

  mkWithStaticEnv =
    env: drv:
    drv.overrideAttrs (
      prev:
      env
      // {
        cargoArtifacts =
          if prev.cargoArtifacts != null then prev.cargoArtifacts.overrideAttrs (_: env) else null;
      }
    );

  withX86_64LinuxStaticEnv = mkWithStaticEnv (
    mkLinuxStaticEnv x86_64LinuxStaticPkgs
    // {
      CC_x86_64_unknown_linux_musl = "${x86_64LinuxStaticPkgs.stdenv.cc}/bin/x86_64-unknown-linux-musl-gcc";
      CXX_x86_64_unknown_linux_musl = "${x86_64LinuxStaticPkgs.stdenv.cc}/bin/x86_64-unknown-linux-musl-g++";
    }
  );

  withAarch64LinuxStaticEnv = mkWithStaticEnv (
    mkLinuxStaticEnv aarch64LinuxStaticPkgs
    // {
      CC_aarch64_unknown_linux_musl = "${aarch64LinuxStaticPkgs.stdenv.cc}/bin/aarch64-unknown-linux-musl-gcc";
      CXX_aarch64_unknown_linux_musl = "${aarch64LinuxStaticPkgs.stdenv.cc}/bin/aarch64-unknown-linux-musl-g++";
    }
  );

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

  # Single package with one lib + one bin target: no `--bin`/`-p` filter, so
  # the test derivation runs the lib unit tests (a `--bin` selector would limit
  # `cargo test` to the binary target and silently skip them).
  mkToolkitBuildArgs =
    {
      src,
      depsSrc,
    }:
    {
      inherit src depsSrc rev;
      prependPackageName = false;
      cargoToml = ../Cargo.toml;
    };
in
{
  # Local builds
  binary-gnosis_vpn-toolkit = builders.local.callPackage nixLib.mkRustPackage (mkToolkitBuildArgs {
    src = sources.main;
    depsSrc = sources.deps;
  });

  binary-gnosis_vpn-toolkit-dev = builders.local.callPackage nixLib.mkRustPackage (
    (mkToolkitBuildArgs {
      src = sources.main;
      depsSrc = sources.deps;
    })
    // {
      CARGO_PROFILE = "dev";
    }
  );

  # Cross-compiled — x86_64 Linux
  binary-gnosis_vpn-toolkit-x86_64-linux = withX86_64LinuxStaticEnv (
    builders.x86_64-linux.callPackage nixLib.mkRustPackage (mkToolkitBuildArgs {
      src = sources.main;
      depsSrc = sources.deps;
    })
  );

  binary-gnosis_vpn-toolkit-x86_64-linux-dev = withX86_64LinuxStaticEnv (
    builders.x86_64-linux.callPackage nixLib.mkRustPackage (
      (mkToolkitBuildArgs {
        src = sources.main;
        depsSrc = sources.deps;
      })
      // {
        CARGO_PROFILE = "dev";
      }
    )
  );

  # Cross-compiled — aarch64 Linux
  binary-gnosis_vpn-toolkit-aarch64-linux = withAarch64LinuxStaticEnv (
    builders.aarch64-linux.callPackage nixLib.mkRustPackage (mkToolkitBuildArgs {
      src = sources.main;
      depsSrc = sources.deps;
    })
  );

  binary-gnosis_vpn-toolkit-aarch64-linux-dev = withAarch64LinuxStaticEnv (
    builders.aarch64-linux.callPackage nixLib.mkRustPackage (
      (mkToolkitBuildArgs {
        src = sources.main;
        depsSrc = sources.deps;
      })
      // {
        CARGO_PROFILE = "dev";
      }
    )
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
}
// lib.optionalAttrs pkgs.stdenv.isDarwin {
  # macOS — aarch64 (only available on Darwin hosts)
  binary-gnosis_vpn-toolkit-aarch64-darwin = withDarwinStaticFlags (
    builders.aarch64-darwin.callPackage nixLib.mkRustPackage (mkToolkitBuildArgs {
      src = sources.main;
      depsSrc = sources.deps;
    })
  );

  binary-gnosis_vpn-toolkit-aarch64-darwin-dev = withDarwinStaticFlags (
    builders.aarch64-darwin.callPackage nixLib.mkRustPackage (
      (mkToolkitBuildArgs {
        src = sources.main;
        depsSrc = sources.deps;
      })
      // {
        CARGO_PROFILE = "dev";
      }
    )
  );
}
