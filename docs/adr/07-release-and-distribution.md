---
type: adr
title: Release and distribution
description: Immutable release tags, six binary targets, checksums, signing, and platform gates.
tags: [adr, release, distribution]
status: stable
locale: en
translation_key: adr-07-release-and-distribution
---

# Release and distribution

## Background

ccync embeds its curated plugin catalog directly into the binary and compiles the canonical render tree at execution time. Installed binaries do not require repository source trees. Early testing revealed that a Windows binary built with dynamic MSVC CRT linkage crashed on a clean operating system installation missing the Visual C++ Redistributable, failing with `0xC0000135 STATUS_DLL_NOT_FOUND`. Furthermore, WinGet package submissions revealed manifest and hash compatibility constraints that prerelease-only checks failed to catch.

- Impact scope: version tags, six compilation targets, archive contents, SHA-256 checksums, artifact signing, Homebrew formulas, WinGet manifests, installer scripts, and post-release validation probes.
- Real alternatives: distribute source code alongside binaries, retain dynamic MSVC CRT linkage on Windows, overwrite or reuse release tags after failures, limit distribution testing to prereleases, or sign artifacts using long-lived private keys.
- Verifiable evidence: the release matrix targets six platform architectures; CI validates Windows binary imports using `dumpbin /imports`; release archives contain only the executable and `LICENSE`; stable and preview channels remain strictly segregated; cosign signs `checksums.txt` via GitHub Actions OIDC.

## Decision

ccync enforces immutable SemVer release tags, minimal binary-only archives, static CRT linkage on Windows, and keyless supply-chain artifact signing.

1. **Immutable tags:** each release tag uses `vX.Y.Z` for stable releases or `vX.Y.Z-rc` / `vX.Y.Z-rcN` for release candidates. The tag base version must match the workspace crate version. Stable tags must pass changelog verification, and release-candidate tags are exempt. Once published, a git tag must never be modified or reused after a failed release.
2. **Six compilation targets:** compile binaries for Windows x64 and arm64, macOS x64 and Apple Silicon arm64, and Linux musl x64 and arm64. Windows builds enforce `+crt-static`, with release gates verifying that binaries have no dynamic CRT dependencies. Release archives package exclusively the compiled executable and `LICENSE`.
3. **Supply-chain security and channels:** generate SHA-256 hashes in `checksums.txt` and sign the file with cosign using ephemeral GitHub Actions OIDC identity tokens. Only official stable tags update the Homebrew tap and submit WinGet community repository pull requests. Release candidates publish solely to GitHub Releases.

4. **Release sequence:** the local exporter publishes the curated snapshot and tag after Windows and Linux checks. The private manual workflow is the backup. Public builds and dual-platform checks must succeed before signing creates a pre-release. Stable releases become Latest only after asset checks, Homebrew publication and verification, and WinGet PR submission succeed. Release candidates stay pre-release and do not publish those channels.
5. **Four stages:** test locally and rehearse privately, publish the pre-release after maintainer authorization, report readiness, then promote a successful stable release automatically.

## Rejected alternatives

1. **Distributing repository source in release archives:** the compiled binary never inspects repository source files; bundling source inflates archive size and expands attack surface unnecessarily.
2. **Using dynamic MSVC CRT linkage on Windows:** clean-machine testing proved that dynamically linked binaries fail to start on machines lacking the external redistributable package.
3. **Overwriting or reusing published tags:** package managers and CDN mirrors cache archive downloads and hash signatures; mutating a tag causes hash verification failures across downstream package managers.
4. **Restricting validation to prerelease channels:** prereleases do not exercise Homebrew or WinGet publishing pipelines, making them incapable of validating external package manager contracts.
5. **Maintaining long-lived signing keys:** static private keys introduce key leakage, rotation, and revocation vulnerabilities; ephemeral OIDC tokens cryptographically tie signatures to specific GitHub Actions workflow executions.

## Consequences

- Users can execute the ccync binary on clean operating system installations without installing prerequisite support libraries; static CRT linkage slightly increases Windows binary size.
- Checksums and cryptographic signatures guarantee artifact authenticity and supply-chain provenance; stable and preview channels remain isolated.
- Each release runs public CI twice: once for the snapshot push and once through the release workflow.
- Supporting six compilation targets and code signing extends CI execution time; altering target architectures or naming conventions requires updating installer scripts, WinGet generators, and post-release test suites.
