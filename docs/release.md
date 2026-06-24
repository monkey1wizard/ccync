# ccync release architecture

How ccync is built, signed, and published. For day-to-day internals see
[`devguide.md`](devguide.md); for the command surface see [`manual.md`](manual.md).

ccync ships as a **binary-only archive** — a single self-contained static binary (no runtime
dependencies, no source bundle, no install-time convergence step). The catalog is embedded at
compile time; the canonical root is rendered at runtime on `ccync init`.

## `release.yml` pipeline

`.github/workflows/release.yml` (triggered on `v*` tags):

1. Builds `ccync-cli` (`cargo build -p ccync-cli --release`) for each target triple.
2. Produces binary-only archives (`ccync-<version>-<os>-<arch>.{tar.gz,zip}`).
3. Calls `packaging/gen-release-artifacts.sh` to produce:
   - `checksums.txt` — SHA-256 for every archive
   - `Monkey1Wizard.ccync.*.yaml` — winget manifests (version / installer / locale)
   - `ccync.rb` — homebrew formula (binary-only, no source bottle)
   - `artifact-manifest.json` — machine-readable asset index (reserved for future updater)
4. Uploads all assets to the GitHub release.
5. Pushes `ccync.rb` to the homebrew tap.
6. Signs the binary with **cosign keyless** (Sigstore OIDC / GitHub Actions identity) — mandatory
   SHA-256 in `checksums.txt`; cosign verify is best-effort in the installer scripts.

## `packaging/gen-release-artifacts.sh`

Self-contained POSIX script that generates manifests from pre-built binary archives. It has no
dependency on the `ccync` binary and no shared tooling with any other product's release. All inputs
are file paths; all outputs land in `--output-dir`. Run `gen-release-artifacts.sh --help` for usage.

The regression guard (`packaging/test-release-artifacts.sh`) validates the generator with synthetic
assets on every CI run:

- greps packaging files + `release.yml` for GAL/golem residue (count must = 0)
- creates dummy archives, runs the generator, verifies checksums and manifest presence

## cosign keyless trust model

cosign is invoked in CI with the GitHub Actions OIDC token; no long-lived signing key is stored.
Verification:

```sh
cosign verify-blob ccync-<version>-<os>-<arch>.tar.gz \
  --certificate-identity "https://github.com/monkey1wizard/ccync/.github/workflows/release.yml@refs/tags/..." \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  --bundle ccync-<version>-<os>-<arch>.tar.gz.bundle
```

The installer scripts (`install.sh` / `install.ps1`) always enforce SHA-256 from `checksums.txt`
and attempt cosign verification as best-effort (warning if cosign is absent, never a hard block).

## Public submission gate

winget PR and homebrew public tap push are **secret-gated** in `release.yml` (controlled by the
`PUBLISH_PUBLIC` repository secret) and are off by default. They are triggered manually after the
first-run visibility gate is validated and a dogfood round completes.
