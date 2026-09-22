# Install

QQ is a single static binary. Pick one route.

## Prebuilt binary (Linux, macOS, Windows)

Every [GitHub release](https://github.com/retsu-AI/qq/releases) attaches
archives for:

| Target | Archive |
| --- | --- |
| Linux x86_64 (static musl) | `qq-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` |
| Linux aarch64 (static musl) | `qq-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple silicon | `qq-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `qq-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `qq-vX.Y.Z-x86_64-pc-windows-msvc.zip` |

plus a `SHA256SUMS` file covering all of them.

Linux / macOS, replacing the version and target:

```sh
V=0.1.2 T=x86_64-unknown-linux-musl
curl -fsSLO "https://github.com/retsu-AI/qq/releases/download/v$V/qq-v$V-$T.tar.gz"
curl -fsSLO "https://github.com/retsu-AI/qq/releases/download/v$V/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS      # macOS: shasum -a 256 --ignore-missing -c SHA256SUMS
tar -xzf "qq-v$V-$T.tar.gz"
install -m 755 qq ~/.local/bin/qq             # or anywhere on your PATH
qq --version
```

Windows (PowerShell): download the `.zip` and `SHA256SUMS`, check the hash
with `Get-FileHash`, extract `qq.exe`, and put its folder on `Path`.

macOS may quarantine a downloaded binary. If `qq` is blocked, run
`xattr -d com.apple.quarantine qq` once.

An install script (`curl … | sh`), a Homebrew tap, and a Nix flake package
are tracked in [`../plans/onboarding-ux.md`](../plans/onboarding-ux.md)
(OB6) and will replace the manual steps above.

## From source

Requires the pinned stable Rust toolchain (`rust-toolchain.toml` selects it
automatically when you build inside the repository):

```sh
git clone https://github.com/retsu-AI/qq
cd qq
cargo build --release
install -m 755 target/release/qq ~/.local/bin/qq
```

or without cloning:

```sh
cargo install --git https://github.com/retsu-AI/qq --locked qq
```

`cargo build --release --no-default-features` builds the minimal profile
without the Amazon Bedrock family and its AWS SDK dependency closure; use it
when you only need the HTTP providers.

With Nix, `nix develop` inside the repository gives a shell with the exact
toolchain.

## Check the install

```sh
qq --version        # qq 0.1.2 (abc1234 2026-09-22)
qq version          # adds the protocol, capabilities, descriptor, and store schema versions
qq config paths     # where QQ will look for configuration and keep sessions
```

## Upgrade

Replace the binary. Sessions and configuration are forward-compatible within
a major version; `qq version` shows the store schema QQ will migrate to on
first open. Release notes are on the
[releases page](https://github.com/retsu-AI/qq/releases).

## Uninstall

Remove the binary, then optionally:

| Remove | Path (`qq config paths` prints yours) |
| --- | --- |
| configuration | the `global:` directory |
| sessions and trust state | the `data:` directory |
| credentials | `qq auth list`, then `qq auth logout <name>` for each |

Next: [Quickstart](quickstart.md).
