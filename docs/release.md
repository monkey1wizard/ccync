# ccync Release Architecture

This document details how ccync is built, digitally signed, and published. For day-to-day internal development mechanics, please refer to the [`devguide.md`](devguide.md); for details regarding the command-line interface, consult the [`manual.md`](manual.md).

ccync is distributed exclusively as a **binary-only archive**. It is packaged as a single, self-contained static binary that requires no runtime dependencies, lacks a source bundle, and involves no installation-time convergence steps. The curated catalog is embedded directly into the binary during compilation, while the canonical root is dynamically rendered at runtime when executing `ccync init`.

## The `release.yml` Pipeline

The `.github/workflows/release.yml` workflow triggers automatically on any tag matching `v*`. It performs the following steps:

1. **Compilation:** Builds the `ccync-cli` crate (`cargo build -p ccync-cli --release`) for each target architecture triplet.
2. **Archiving:** Packages the compiled output into binary-only archives (e.g., `ccync-<version>-<os>-<arch>.{tar.gz,zip}`).
3. **Manifest Generation:** Invokes `packaging/gen-release-artifacts.sh` to generate the following supplementary files:
   - `checksums.txt` — The SHA-256 hashes for all generated archives.
   - `Monkey1Wizard.ccync.*.yaml` — Winget manifests detailing the version, installer, and locale.
   - `ccync.rb` — The Homebrew formula (configured for binary-only distribution, omitting any source bottle).
   - `artifact-manifest.json` — A machine-readable asset index reserved for future automatic updater functionality.
4. **Upload:** Attaches all generated assets to the corresponding GitHub Release.
5. **Homebrew Publication:** Pushes the generated `ccync.rb` formula to the dedicated Homebrew tap.
6. **Signing:** Cryptographically signs the binary using **cosign keyless** infrastructure (leveraging Sigstore OIDC and the GitHub Actions identity). The SHA-256 validation via `checksums.txt` is mandatory, whereas the `cosign verify` step within the installation scripts operates on a best-effort basis.

## The `packaging/gen-release-artifacts.sh` Script

This is a self-contained POSIX-compliant script responsible for generating distribution manifests from the pre-built binary archives. It operates independently of the `ccync` binary itself and shares no tooling with any other product's release pipeline. All inputs are provided as file paths, and all generated outputs are written to the directory specified by `--output-dir`. Run `gen-release-artifacts.sh --help` for detailed usage instructions.

To prevent regressions, the `packaging/test-release-artifacts.sh` script continuously validates the generator using synthetic assets during every CI run:

- It uses `grep` to scan the packaging scripts and `release.yml` for any residual foreign-product terminology (the count must be zero).
- It creates dummy archive files, executes the generator, and strictly verifies both the resulting checksums and the presence of the expected manifests.

## The cosign Keyless Trust Model

We invoke `cosign` during the CI process using the short-lived GitHub Actions OIDC token; no long-lived cryptographic signing keys are stored. The signing identity is strictly pinned to this repository's `release.yml` workflow, specifically referencing a stable, semantic version (semver) tag.

To independently verify the release (after downloading `checksums.txt`, `checksums.txt.sig`, and `checksums.txt.pem` from the GitHub Release), run the following command:

```sh
cosign verify-blob checksums.txt \
  --signature checksums.txt.sig \
  --certificate checksums.txt.pem \
  --certificate-identity-regexp '^https://github\.com/monkey1wizard/ccync/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com'
```

A `Verified OK` output confirms that the checksums were genuinely produced by this repository's CI system acting on the exact specified tag reference.

The primary installer scripts (`install.sh` and `install.ps1`) stringently enforce SHA-256 verification against `checksums.txt`. They will also attempt a `cosign` verification on a best-effort basis; if `cosign` is unavailable on the host system, it simply issues a warning but never rigidly blocks the installation.

## Release Runbook

The following steps outline the process for cutting a new stable release (intended for maintainers only):

```sh
# 1. Tag the stable release on the private source-of-truth (GitLab origin).
git tag v<version>
git push origin v<version>

# 2. Export the curated public snapshot along with the tag to GitHub.
bash packaging/publish-public.sh --execute --tag v<version>

# 3. Monitor the release.yml workflow execution.
gh run list -R monkey1wizard/ccync --limit 5

# 4. Verify the published GitHub Release.
gh release view v<version> -R monkey1wizard/ccync

# 5. Submit the winget manifests (this step is manual for v1).
#    Run gen-release-artifacts.sh locally, and place the output under manifests/m/Monkey1Wizard/ccync/<version>/.
#    winget validate && gh pr create --repo microsoft/winget-pkgs
```

The Homebrew tap (`monkey1wizard/homebrew-tap`) is automatically updated by the `release.yml` workflow when stable tags are pushed (tags lacking a `-` pre-release suffix). Submission to winget remains a manual process for v1.

## Distribution Channels

| Channel | Method | Gate |
| --- | --- | --- |
| GitHub Releases | Handled by `release.yml` upon a `v*` tag push | Automatic |
| Homebrew Tap | `release.yml` pushes `ccync.rb` on stable tags | Secured via the `HOMEBREW_TAP_TOKEN` secret |
| Winget | Manual Pull Request to `microsoft/winget-pkgs` | Maintainer review required |
| `install.sh` / `install.ps1` | Direct downloads from GitHub Releases | SHA-256 verification (mandatory) + cosign (best-effort) |
