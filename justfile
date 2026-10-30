# gnosis_vpn-toolkit tasks

# Build the gnosis_vpn-update release binary (native) via nix
build:
    nix build .#binary-gnosis_vpn-update

# Build the gnosis_vpn-migrate release binary (native) via nix
build-migrate:
    nix build .#binary-gnosis_vpn-migrate

# Build the static x86_64-linux updater via nix
build-x86_64:
    nix build .#binary-gnosis_vpn-update-x86_64-linux

# Build the static aarch64-linux updater via nix
build-arm64:
    nix build .#binary-gnosis_vpn-update-aarch64-linux

# Build the whole toolkit (every binary) for x86_64-linux, as CI publishes it
build-toolkit-x86_64:
    nix build .#binary-toolkit-x86_64-linux

# Build the whole toolkit (every binary) for aarch64-linux, as CI publishes it
build-toolkit-arm64:
    nix build .#binary-toolkit-aarch64-linux

# Build the whole toolkit (every binary) for aarch64-darwin, as CI publishes it (darwin host)
build-toolkit-darwin:
    nix build .#binary-toolkit-aarch64-darwin

# Run the full flake check suite (clippy, docs, tests, licenses)
check:
    nix flake check -L

# Run cargo-audit against the live RUSTSEC advisory DB
audit:
    nix run -L .#audit

# Run the test suite
test:
    cargo test

# Run the migrate tests as root, so chown to a foreign uid is exercised for real
test-root:
    sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME cargo test -p gnosis_vpn-migrate

# Format the tree
fmt:
    nix fmt
