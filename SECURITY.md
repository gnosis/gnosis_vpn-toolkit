# Security Policy

## Binary Verification

All `gnosis_vpn-update` release binaries ship with a SHA256 checksum.
Additionally:

- **Linux binaries** (`x86_64-linux`, `aarch64-linux`) are signed with GPG
- **macOS binaries** (`aarch64-darwin`) use Apple's code signing mechanism and
  are signed with an Apple Developer certificate

We strongly recommend verifying binaries before installation.

### GPG Public Key

This is the same key used across the GnosisVPN projects.

**Key ID:** `84F73FEA46D10972`

**Fingerprint:** `9A30 8031 FD3B FE8E DBF5  076D 84F7 3FEA 46D1 0972`

**Email:** tech@hoprnet.org

### Importing the Public Key

You can import the GnosisVPN public key using any of these methods:

**From keyserver:**

```bash
gpg --keyserver hkps://keyserver.ubuntu.com --recv-keys 9A308031FD3BFE8EDBF5076D84F73FEA46D10972
echo "9A308031FD3BFE8EDBF5076D84F73FEA46D10972:6:" | gpg --import-ownertrust
```

**From this repository:**

```bash
curl -fsSLO https://raw.githubusercontent.com/gnosis/gnosis_vpn-toolkit/main/gnosis_vpn-update/gnosisvpn-public-key.asc
gpg --import gnosisvpn-public-key.asc
```

## Linux Binary Verification

Each Linux release includes three files per architecture:

1. **Binary file** (e.g., `gnosis_vpn-update-x86_64-linux`)
2. **SHA256 checksum** (e.g., `gnosis_vpn-update-x86_64-linux.sha256`)
3. **GPG signature** (e.g., `gnosis_vpn-update-x86_64-linux.asc`)

### Verify SHA256 Checksum

```bash
sha256sum -c gnosis_vpn-update-x86_64-linux.sha256
```

Expected output:

```
gnosis_vpn-update-x86_64-linux: OK
```

### Verify GPG Signature

```bash
gpg --verify gnosis_vpn-update-x86_64-linux.asc gnosis_vpn-update-x86_64-linux
```

Expected output:

```
gpg: Signature made Mon May  4 12:25:22 2026 CEST
gpg:                using EDDSA key 9A308031FD3BFE8EDBF5076D84F73FEA46D10972
gpg: Good signature from "GnosisVPN (Gnosis VPN) <tech@hoprnet.org>" [ultimate]
```

## macOS Binary Verification

macOS binaries are signed with an Apple Developer certificate, using the
hardened runtime and a secure timestamp. They are **not** notarized by Apple, so
Gatekeeper blocks a binary carrying the quarantine attribute (e.g. one
downloaded through a browser). Verify the binary before clearing that attribute.

### Verify SHA256 Checksum (macOS)

Download the binary and checksum from the release page
https://github.com/gnosis/gnosis_vpn-toolkit/releases

```bash
shasum -a 256 -c gnosis_vpn-update-aarch64-darwin.sha256
```

Expected output:

```
gnosis_vpn-update-aarch64-darwin: OK
```

### Verify Code Signature

```bash
codesign --verify --strict --verbose=2 gnosis_vpn-update-aarch64-darwin
```

Expected output:

```
gnosis_vpn-update-aarch64-darwin: valid on disk
gnosis_vpn-update-aarch64-darwin: satisfies its Designated Requirement
```

Inspect the signing identity with `codesign --display --verbose=4 <binary>`.

### Clearing the Quarantine Attribute

Only if a binary whose checksum and signature both verified is still blocked:

```bash
xattr -d com.apple.quarantine gnosis_vpn-update-aarch64-darwin
```

## Reporting Security Vulnerabilities

If you discover a security vulnerability in GnosisVPN, please report it
privately to:

**Email:** tech@hoprnet.org

Please include:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)
