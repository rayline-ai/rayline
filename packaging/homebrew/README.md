# Homebrew Packaging

Rayline Local is not published to a Homebrew tap yet. The release workflow
creates platform archives that are suitable for a future tap formula:

- `rayline-<version>-macosx_11_0_arm64.tar.gz`
- `rayline-<version>-macosx_10_12_x86_64.tar.gz`
- `rayline-<version>-linux_x86_64.tar.gz`
- `rayline-<version>-linux_aarch64.tar.gz`

Each archive contains a single `rayline-<version>-<platform>/` directory with
`rayline`, `rld`, `README.md`, and `LICENSE`.

When the tap is created, generate the formula from the GitHub Release
`SHA256SUMS` file and use the matching archive for each OS/architecture. Do not
point Homebrew at the bare `rayline-<platform>` and `rld-<platform>` assets;
those are reserved for the install scripts and self-update mirror.

Homebrew installs are intentionally treated as externally managed by
`rayline update`; users should run:

```bash
brew upgrade rayline
```
