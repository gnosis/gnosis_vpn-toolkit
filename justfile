# gnosis_vpn-toolkit tasks

# Build the release binary (native) via nix
build:
    nix build .#binary-gnosis_vpn-toolkit

# Build the static x86_64-linux binary via nix
build-x86_64:
    nix build .#binary-gnosis_vpn-toolkit-x86_64-linux

# Build the static aarch64-linux binary via nix
build-arm64:
    nix build .#binary-gnosis_vpn-toolkit-aarch64-linux

# Run the full flake check suite (clippy, tests, audit, licenses)
check:
    nix flake check -L

# Run the test suite
test:
    cargo test

# Format the tree
fmt:
    nix fmt
