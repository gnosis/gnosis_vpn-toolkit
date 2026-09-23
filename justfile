# gnosis_vpn-toolkit tasks

# Build the gnosis_vpn-update release binary (native) via nix
build:
    nix build .#binary-gnosis_vpn-update

# Build the static x86_64-linux binary via nix
build-x86_64:
    nix build .#binary-gnosis_vpn-update-x86_64-linux

# Build the static aarch64-linux binary via nix
build-arm64:
    nix build .#binary-gnosis_vpn-update-aarch64-linux

# Run the full flake check suite (clippy, docs, tests, licenses)
check:
    nix flake check -L

# Run cargo-audit against the live RUSTSEC advisory DB
audit:
    nix run -L .#audit

# Run the test suite
test:
    cargo test

# Format the tree
fmt:
    nix fmt
