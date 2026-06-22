# Release Process

Rayline Local releases are binary-first. The Rust workspace is not published to
crates.io yet; all crates are marked `publish = false` until the package split
is intentionally reviewed.

## Release Tags

Create releases from annotated or signed tags named `vX.Y.Z`:

```bash
git tag -a v0.2.0 -m "Rayline 0.2.0"
git push origin v0.2.0
```

Pushing a `v*` tag runs `.github/workflows/release.yml`. The workflow builds
with Rust 1.88 and embeds:

- `RAYLINE_VERSION=<tag without v>`
- `RAYLINE_CHANNEL=prod`

## GitHub Release Assets

The release workflow uploads:

- `rayline-<platform>` and `rld-<platform>` bare binaries;
- `rayline-<version>-<platform>.tar.gz` archives for macOS/Linux;
- `rayline-<version>-windows_x86_64.zip` for Windows;
- `SHA256SUMS` covering every uploaded binary and archive.

Supported platform tags match the self-updater contract in
`crates/rayline-cli/src/update.rs`:

- `linux_x86_64`
- `linux_aarch64`
- `macosx_10_12_x86_64`
- `macosx_11_0_arm64`
- `windows_x86_64`

The public install scripts download from GitHub Releases and verify the bare
binary assets against `SHA256SUMS`.

## Self-Update Mirror

The built-in `rayline update` command reads from `https://get.rayline.ai` by
default. After the GitHub Release is created, mirror the same bare binary assets
and `SHA256SUMS` to:

```text
https://get.rayline.ai/cli/v<version>/rayline-<platform>
https://get.rayline.ai/cli/v<version>/rld-<platform>
https://get.rayline.ai/cli/v<version>/SHA256SUMS
```

Then update the channel pointer:

```text
https://get.rayline.ai/cli/latest.txt
```

The pointer file contains only the public version string, for example:

```text
0.2.0
```

Use `latest-dev.txt` or `latest-main.txt` only for non-production channels.

## Local Validation

Before tagging:

```bash
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 test --workspace --locked -- --test-threads=1
cargo +1.88.0 clippy --workspace --all-targets --locked -- -D warnings
git diff --check
```

After the release workflow completes, validate at least one installer path:

```bash
scripts/install-rayline.sh --version 0.2.0 --install-dir /tmp/rayline-bin
/tmp/rayline-bin/rayline --version
/tmp/rayline-bin/rayline update --check
```

For installer validation against a staged asset directory:

```bash
RAYLINE_RELEASE_BASE_URL=file:///tmp/rayline-release/cli/v0.2.0 \
  scripts/install-rayline.sh --install-dir /tmp/rayline-bin
```

For self-update validation without replacing the active binary, point the update
base at a local or staged mirror:

```bash
RAYLINE_UPDATE_BASE_URL=file:///tmp/rayline-release \
  /tmp/rayline-bin/rayline update --dry-run --version 0.2.0
```
