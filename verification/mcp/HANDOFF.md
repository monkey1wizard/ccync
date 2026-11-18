# MCP finalize input guide

This guide identifies inputs for two separate owning finalize reviews. Run `git rev-parse HEAD` to identify the current commit and compare changed sources before either review. A source hash is an identity record, not a review verdict.

## Original MCP control-plane plan

Both plans below were closed as absorbed on 2026-11-12. Their plan files are no longer in the working tree. Retrieve them from git history.

- Scope: its source (`.dev/plans/refactor-mcp-control-plane.md`) and execution prompt (`.dev/plans/refactor-mcp-control-plane.prompt.md`) contain the full requirements and task-to-test map. Requirements cover declarations and variables, ownership and routing, secret handling, recovery and diagnostics, Windows publication blocking, stop evidence, recovery inventory, and attribution. The original plan leaves native measurement to follow-up work and live application to P7.
- Source SHA-256: `5ac055ec3327e1984301012b9d770e42af685dc73e770fec98ce626a00ca0a60`. Prompt SHA-256: `9e2065c125732f7bc1032b0c50d0a9898918aff6d71f43d8f887f7a3fbaf7843`.
- The original 29 tasks are recorded complete. Finalize review was interrupted after two rejected outputs. The follow-up planning base is `1b05580e5bc864b75cffdb822d90c3a36dae19ec`. For the full landing diff, compare the current branch to its actual merge base with `main`. Review the original scope and subsequent fixes separately, using the task and test maps in the prompt.
- Retain the original source and prompt unchanged, original task evidence and receipts under `.dev/pipeline/refactor-mcp-control-plane/`, and the following original review artifacts under `.dev/pipeline/refactor-mcp-control-plane/finalize-review/`: `1793986772-580536600-000000-finalize-review-finalize-review-codex.log`, `1793989575-421834400-000000-finalize-review-finalize-review-codex.log`, `finalize-review-finalize-review.prompt.bak`, and `finalize-review.receipt.md`. The receipt is a rejected candidate, not a pass. Keep original bytes and any sanitized references needed for review.

## Completion follow-up plan

- Scope: its source (`.dev/plans/fix-mcp-control-plane-completion.md`) and execution prompt (`.dev/plans/fix-mcp-control-plane-completion.prompt.md`) contain the full requirements, source disposition map, task map and test plan. They cover isolation and capability evidence, event analysis and attribution, stop and window controls, qualification and publication, recovery replay, P5/P7 handoff, and the two-plan closing inputs. Fixture results do not qualify native hosts.
- Source SHA-256: `157c4b432546750482c981625187182901d6ca56f75c6689104639f8d9d94e4e`. Compute the working prompt SHA-256 before the follow-up review with `Get-FileHash -Algorithm SHA256 .dev/plans/fix-mcp-control-plane-completion.prompt.md`. The prompt is an in-flight file.
- Planning base: `1b05580e5bc864b75cffdb822d90c3a36dae19ec`. Preparation base: `7707c1e26d3397ea3e870ca6ddcf9251e6d2ac58`. Find the latest guide commit with `git log -1 --format=%H -- verification/mcp/HANDOFF.md`. Review `git diff 1b05580e5bc864b75cffdb822d90c3a36dae19ec..HEAD` alongside the current uncommitted diff. This range includes both scopes, so use the task map to distinguish follow-up changes.
- Retain the source and prompt, `.dev/pipeline/fix-mcp-control-plane-completion/source-manifest.json`, planning and baseline evidence, task receipts and sanitized evidence under `.dev/pipeline/fix-mcp-control-plane-completion/`, and the original rejected outputs listed above as historical inputs. Preserve the final source hashes, implementation commit, evidence locations and new closing receipts with their owning scope. Do not treat a moved historical receipt as a pass for a new scope.

## Replay and prerequisites

Use the isolated entry and evidence-root rules in [README.md](README.md). Run from the repository root with a distinct evidence root inside the applicable task directory. The following entries are review inputs, not claims that a run passed:

```powershell
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case isolation -Mode Fixture -SelfTest -EvidenceRoot <evidence-root>
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case replay -Mode Fixture -EvidenceRoot <evidence-root>
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case publication-status -Mode Native -EvidenceRoot <evidence-root>
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case cargo -Mode Fixture -CargoCommand 'cargo test --workspace --no-fail-fast' -EvidenceRoot <evidence-root>
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case cargo -Mode Fixture -CargoCommand 'cargo clippy --workspace' -EvidenceRoot <evidence-root>
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case cargo -Mode Fixture -CargoCommand 'cargo test -p ccync-engine --test docs_i18n' -EvidenceRoot <evidence-root>
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case cargo -Mode Fixture -CargoCommand 'cargo test -p ccync-engine --test projection_vocabulary' -EvidenceRoot <evidence-root>
```

Before cargo, verify the existing Python, uv and pip fixture prerequisites. If needed, point `CCYNC_RECORDED_PYTHON` to an existing qualified interpreter. This guide grants no authority to install packages or delete files. The first rejected review recorded workspace exit 101 with an unmet Python prerequisite and did not run clippy. The second review's workspace exit 0 is separate evidence, not a replacement for the first result. The review's RTK tee exceeded the receipt-only boundary. Inventory any later artifacts read-only and do not automatically delete them. The historical `cwk.mjs` check was unavailable. Preserve that degradation and use available writing checks without claiming the missing check passed.

The orchestrator owns the full workspace regression and source-disposition comparison at goal verification. At preparation they are NotRun by this documentation executor. Compare failures by test name with the recorded baseline, account for every target and Doc-tests, and retain the actual output. The independent documentation comparison and final goal verification remain for the orchestrator.

## Custody and remaining gates

1. Before the first teardown, retain both sources and existing sanitized evidence in committed repository state, and preserve planning evidence, historical receipts and new P3 closing receipts in the canonical repository archive that survives worktree removal. Record scope, hashes, implementation version and source location. Ignored worktree-only files do not satisfy retention.
2. Finish required fixes and verification on the branch. Before changing scope, confirm no unfinished attempt, lease or pending recovery write exists. Each plan needs its own valid entry, Owner Acceptance and fresh receipts. Do not reuse another scope's pin, handback or pass.
3. The original P3 coordinator obtains P3's independent review and doc-sync, then performs its single landing and teardown under its own gates. The follow-up's future review is not a prerequisite for that landing.
4. After P3 closes, the follow-up uses its own already-on-main entry, fresh independent review, doc-sync, receipt and close. It does not merge again or use P3's review as its own pass. Preserve each new receipt before its owning cleanup. If retention or a closing condition is unmet, preserve the unfinished output and stop that workflow.

Neither plan's future finalize review, doc-sync, landing, lifecycle close or deployment is established by this guide. The follow-up's goal verification does not require its own future review or close first. Unexecuted safety premises remain unproven. P7 retains separate per-operation authorization for real-machine work. Planning text and synthetic fixtures grant no native qualification or deployment authority.
