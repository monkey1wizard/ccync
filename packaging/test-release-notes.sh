#!/usr/bin/env bash
#
# test-release-notes.sh — regression guard for the release changelog gate and
# draft generator.
#
# Extractor probes (packaging/release-body.sh):
#   Failure paths: missing file, missing section (incl. a heading that
#   only exists inside a fenced code block), blank body, sentinel
#   residue. Each fails with a distinct diagnostic, a non-zero exit,
#   and leaves --output unwritten.
#   Success paths: bare/`v`-prefixed stable version, with/without a
#   date suffix, byte-identical extraction, default --changelog
#   resolution from a repo subdirectory, a valid RC skip (zero
#   writes), and invalid-form rejection (beta suffix, short version,
#   non-version text, option-shaped argument).
#
# Generator probes (packaging/gen-release-notes.sh), in temp repos:
#   Range resolution: default range picks the numerically highest
#   HEAD-reachable stable tag (not the most recently created one),
#   ignores prerelease/non-SemVer tags; explicit <from>..<to> validates
#   both endpoints; empty endpoints, three-dot ranges, extra/option-
#   shaped arguments, and unresolvable revisions are all rejected; an
#   induced git-subprocess failure and an induced malformed-framing
#   response each abort before any stdout is produced.
#   Merge exclusion: a real merge commit — the merged branch's own
#   commits are retained, the merge commit's own subject never appears,
#   and the footer reports exactly one excluded structural merge for
#   both the default range and an equivalent explicit range.
#
# Usage: bash packaging/test-release-notes.sh
# Exit: 0 = all probes pass; 1 = one or more probes fail.
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${REPO_ROOT}/packaging/release-body.sh"
GEN_SCRIPT="${REPO_ROOT}/packaging/gen-release-notes.sh"
FAILURES=0
PASS_COUNT=0
PUBLIC_SNAPSHOT=0

if [[ "${1:-}" == "--public-snapshot" && $# -eq 1 ]]; then
  PUBLIC_SNAPSHOT=1
elif (($#)); then
  echo "Usage: bash packaging/test-release-notes.sh [--public-snapshot]" >&2
  exit 2
fi

CONTROLLER="${REPO_ROOT}/.github/workflows/publish-public.yml"
if [[ "$PUBLIC_SNAPSHOT" -eq 1 ]]; then
  if [[ -e "$CONTROLLER" ]]; then
    echo "[FAIL] public snapshot must not contain the private publication controller" >&2
    exit 1
  fi
else
  if [[ ! -f "$CONTROLLER" ]]; then
    echo "[FAIL] source checkout requires .github/workflows/publish-public.yml" >&2
    exit 1
  fi
fi

ok()   { echo "[PASS] $*"; PASS_COUNT=$(( PASS_COUNT + 1 )); }
fail() { echo "[FAIL] $*" >&2; FAILURES=$(( FAILURES + 1 )); }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------------------
# Fixture changelog for the extractor failure and success probes below
# ---------------------------------------------------------------------------
FIXTURE="${TMP}/CHANGELOG.md"
cat > "$FIXTURE" <<'EOF'
# Changelog

## [0.9.0] - 2026-01-15
Body for 0.9.0.

Second paragraph for 0.9.0.

## [0.8.0]
Body for 0.8.0, no date suffix.

## [0.7.0]
Example fence:

```
## [0.7.0] - fake heading inside a fence
```

Real body for 0.7.0.

## [0.6.0]
```
## [0.6.5]
```
Only body for 0.6.0.

## [0.5.0]

## [0.4.0]
Body for 0.4.0.

## [0.3.0]
[[CCYNC-RELEASE-DRAFT:abc1234]] draft line for 0.3.0.

## [0.2.0]
Terminal body for 0.2.0, extends to EOF.
EOF

EXPECTED_090=$'Body for 0.9.0.\n\nSecond paragraph for 0.9.0.\n\n'

# ---------------------------------------------------------------------------
# Extractor failure paths
# ---------------------------------------------------------------------------

assert_fails_cleanly() {
  local desc="$1" expect_diag="$2"; shift 2
  local out_path="${TMP}/out-$$-${RANDOM}.md"
  local stderr_out status
  set +e
  stderr_out="$("$SCRIPT" "$@" --output "$out_path" 2>&1 1>/dev/null)"
  status=$?
  set -e
  if [[ $status -eq 0 ]]; then
    fail "${desc}: expected non-zero exit, got 0"
    return
  fi
  if [[ -e "$out_path" ]]; then
    fail "${desc}: output path exists after failure (${out_path})"
    return
  fi
  if [[ "$stderr_out" != *"$expect_diag"* ]]; then
    fail "${desc}: diagnostic missing expected substring '${expect_diag}' — got: ${stderr_out}"
    return
  fi
  ok "${desc}"
}

assert_fails_cleanly "missing changelog file" "not found" \
  "0.1.0" --changelog "${TMP}/does-not-exist.md"

assert_fails_cleanly "missing section" "no changelog section found" \
  "1.2.3" --changelog "$FIXTURE"

assert_fails_cleanly "fenced fake heading never matches" "no changelog section found" \
  "0.6.5" --changelog "$FIXTURE"

assert_fails_cleanly "blank body" "is blank" \
  "0.5.0" --changelog "$FIXTURE"

assert_fails_cleanly "sentinel residue" "unresolved draft sentinel" \
  "0.3.0" --changelog "$FIXTURE"

# Unbalanced fence, content-swallow shape: a lone unterminated fence inside
# the target section must not let a later section's body leak into it.
FIXTURE_FENCE_SWALLOW="${TMP}/CHANGELOG-fence-swallow.md"
cat > "$FIXTURE_FENCE_SWALLOW" <<'EOF'
## [0.9.0]
```
unterminated fence...

## [0.8.0]
Body for 0.8.0 that should NOT belong to 0.9.0's release.
EOF

assert_fails_cleanly "unbalanced fence (content-swallow shape)" "unbalanced code fence" \
  "0.9.0" --changelog "$FIXTURE_FENCE_SWALLOW"

# Unbalanced fence, blank-body-bypass shape: the target section is actually
# blank once its own lone fence is discounted, but a naive extractor would
# merge the next section's body in and pass the non-blank-body check.
FIXTURE_FENCE_BLANK_BYPASS="${TMP}/CHANGELOG-fence-blank-bypass.md"
cat > "$FIXTURE_FENCE_BLANK_BYPASS" <<'EOF'
## [0.9.0]
```

## [0.8.0]
Real unrelated content that should never appear in 0.9.0's release body.
EOF

assert_fails_cleanly "unbalanced fence (blank-body-bypass shape)" "unbalanced code fence" \
  "0.9.0" --changelog "$FIXTURE_FENCE_BLANK_BYPASS"

# ---------------------------------------------------------------------------
# Extractor success paths
# ---------------------------------------------------------------------------

# Bare and v-prefixed stable versions extract byte-identical output.
OUT_BARE="${TMP}/out-bare.md"
OUT_VPFX="${TMP}/out-vpfx.md"
"$SCRIPT" "0.9.0" --changelog "$FIXTURE" --output "$OUT_BARE" >/dev/null
"$SCRIPT" "v0.9.0" --changelog "$FIXTURE" --output "$OUT_VPFX" >/dev/null
if cmp -s "$OUT_BARE" "$OUT_VPFX"; then
  ok "stable 0.9.0 and v0.9.0 extract byte-identical output"
else
  fail "bare vs v-prefixed output differs"
fi
if printf '%s' "$EXPECTED_090" | cmp -s - "$OUT_BARE"; then
  ok "extracted body is byte-identical to the fixture section"
else
  fail "extracted body does not match the fixture section"
fi

# Section without a date suffix still extracts correctly.
OUT_NODATE="${TMP}/out-nodate.md"
"$SCRIPT" "0.8.0" --changelog "$FIXTURE" --output "$OUT_NODATE" >/dev/null
if [[ -s "$OUT_NODATE" ]] && grep -qF "Body for 0.8.0" "$OUT_NODATE"; then
  ok "heading without a date suffix extracts"
else
  fail "heading without a date suffix failed to extract"
fi

# A fenced fake heading inside the real section's body does not truncate it,
# and does not act as a section boundary.
OUT_FENCE="${TMP}/out-fence.md"
"$SCRIPT" "0.7.0" --changelog "$FIXTURE" --output "$OUT_FENCE" >/dev/null
if grep -qF "Real body for 0.7.0." "$OUT_FENCE" && grep -qF "fake heading inside a fence" "$OUT_FENCE"; then
  ok "fenced heading-shaped line is retained as body, not a boundary"
else
  fail "fenced heading-shaped line handling incorrect"
fi

# Terminal section (no following heading) extracts through EOF.
OUT_EOF="${TMP}/out-eof.md"
"$SCRIPT" "0.2.0" --changelog "$FIXTURE" --output "$OUT_EOF" >/dev/null
if grep -qF "Terminal body for 0.2.0, extends to EOF." "$OUT_EOF"; then
  ok "terminal section extracts through EOF"
else
  fail "terminal section extraction failed"
fi

# Default --changelog resolution from a repo subdirectory.
GITREPO="${TMP}/gitrepo"
mkdir -p "${GITREPO}/sub/dir"
git -C "$GITREPO" init -q
cat > "${GITREPO}/CHANGELOG.md" <<'EOF'
## [0.1.0]
Body from repo-root default resolution.
EOF
DEFAULT_OUT="$(cd "${GITREPO}/sub/dir" && "$SCRIPT" "0.1.0")"
if [[ "$DEFAULT_OUT" == *"Body from repo-root default resolution."* ]]; then
  ok "default --changelog resolves repo root from a subdirectory"
else
  fail "default --changelog resolution from a subdirectory failed"
fi

# Bare and numbered RC forms skip the changelog gate and write nothing.
assert_rc_skip() {
  local version="$1" out_path="${TMP}/rc-${RANDOM}.md" stdout status
  set +e
  stdout="$("$SCRIPT" "$version" --changelog "${TMP}/missing-rc-changelog.md" --output "$out_path" 2>&1)"
  status=$?
  set -e
  if [[ $status -eq 0 && ! -e "$out_path" && "$stdout" == *"release candidate"* ]]; then
    ok "${version} RC skips changelog validation and writes no output"
  else
    fail "${version} RC handling incorrect (status=$status, output exists=$( [[ -e "$out_path" ]] && echo yes || echo no ))"
  fi
}

assert_rc_skip "0.1.0-rc"
assert_rc_skip "v0.1.0-rc1"

# A stable release still requires a matching non-blank changelog section.
assert_fails_cleanly "stable release retains missing-section gate" "no changelog section found" \
  "1.2.3" --changelog "$FIXTURE"

# Invalid version forms: each fails with the invalid-version diagnostic and
# zero writes.
assert_invalid_version() {
  local desc="$1" version="$2"
  local out_path="${TMP}/invalid-out-$$-${RANDOM}.md"
  local stderr_out status
  set +e
  stderr_out="$("$SCRIPT" "$version" --changelog "$FIXTURE" --output "$out_path" 2>&1 1>/dev/null)"
  status=$?
  set -e
  if [[ $status -ne 0 && ! -e "$out_path" && "$stderr_out" == *"invalid version"* ]]; then
    ok "invalid version rejected: ${desc}"
  else
    fail "invalid version not rejected as expected: ${desc} (status=$status, stderr=${stderr_out})"
  fi
}

assert_invalid_version "prerelease suffix not -rcN (v1.2.3-beta)" "v1.2.3-beta"
assert_invalid_version "malformed RC suffix (v1.2.3-rcx)" "v1.2.3-rcx"
assert_invalid_version "malformed numeric suffix (v1.2.3-rc1x)" "v1.2.3-rc1x"
assert_invalid_version "short version (1.2)" "1.2"
assert_invalid_version "non-version text (foo)" "foo"
assert_invalid_version "option-shaped argument (--bogus)" "--bogus"

# ---------------------------------------------------------------------------
# Generator probes (packaging/gen-release-notes.sh) — range resolution
# ---------------------------------------------------------------------------

commit() {
  # commit(repo, filename, content, subject)
  local repo="$1" file="$2" content="$3" subject="$4"
  printf '%s\n' "$content" > "${repo}/${file}"
  git -C "$repo" add "$file"
  git -C "$repo" commit -q -m "$subject"
}

GENREPO="${TMP}/genrepo"
mkdir -p "$GENREPO"
git -C "$GENREPO" init -q
git -C "$GENREPO" config user.email "test@example.com"
git -C "$GENREPO" config user.name "Test"

commit "$GENREPO" "a.txt" "1" "chore: initial commit"
git -C "$GENREPO" tag v0.3.0
commit "$GENREPO" "b.txt" "2" "chore: second commit"
git -C "$GENREPO" tag v0.2.0
commit "$GENREPO" "c.txt" "3" "chore: third commit"
git -C "$GENREPO" tag v0.4.0-rc1
git -C "$GENREPO" tag latest
commit "$GENREPO" "d.txt" "4" "feat: after all tags"

# Default range: numerically highest reachable stable tag (v0.3.0), not the
# more-recently-created v0.2.0 and not the higher-looking but non-stable
# v0.4.0-rc1/latest tags.
GEN_DEFAULT_OUT="$(cd "$GENREPO" && "$GEN_SCRIPT")"
if [[ "$GEN_DEFAULT_OUT" == *"second commit"* \
   && "$GEN_DEFAULT_OUT" == *"third commit"* \
   && "$GEN_DEFAULT_OUT" == *"after all tags"* \
   && "$GEN_DEFAULT_OUT" != *"initial commit"* \
   && "$GEN_DEFAULT_OUT" == *"Merges excluded: 0"* ]]; then
  ok "default range selects the numerically highest reachable stable tag, ignoring newer-dated/prerelease/non-SemVer tags"
else
  fail "default range selection incorrect: ${GEN_DEFAULT_OUT}"
fi

# Explicit range: both endpoints validated and resolved.
GEN_EXPLICIT_OUT="$(cd "$GENREPO" && "$GEN_SCRIPT" "v0.3.0..v0.2.0")"
if [[ "$GEN_EXPLICIT_OUT" == *"second commit"* \
   && "$GEN_EXPLICIT_OUT" != *"initial commit"* \
   && "$GEN_EXPLICIT_OUT" != *"third commit"* ]]; then
  ok "explicit <from>..<to> range resolves both endpoints"
else
  fail "explicit range resolution incorrect: ${GEN_EXPLICIT_OUT}"
fi

assert_gen_fails_cleanly() {
  local desc="$1" expect_diag="$2"; shift 2
  local stderr_out status
  set +e
  stderr_out="$(cd "$GENREPO" && "$GEN_SCRIPT" "$@" 2>&1 1>/dev/null)"
  status=$?
  set -e
  if [[ $status -eq 0 ]]; then
    fail "${desc}: expected non-zero exit, got 0"
    return
  fi
  if [[ "$stderr_out" != *"$expect_diag"* ]]; then
    fail "${desc}: diagnostic missing expected substring '${expect_diag}' — got: ${stderr_out}"
    return
  fi
  ok "${desc}"
}

assert_gen_fails_cleanly "empty from-endpoint" "must be non-empty" "..v0.3.0"
assert_gen_fails_cleanly "empty to-endpoint" "must be non-empty" "v0.3.0.."
assert_gen_fails_cleanly "three-dot range" "three-dot ranges are not supported" "v0.2.0...v0.3.0"
assert_gen_fails_cleanly "extra positional argument" "unexpected extra arguments" "v0.2.0..v0.3.0" "v0.1.0..v0.2.0"
assert_gen_fails_cleanly "option-shaped argument" "option-shaped argument" "--bogus"
assert_gen_fails_cleanly "unresolvable from-endpoint" "does not resolve to a commit" "bogus-ref..v0.3.0"
assert_gen_fails_cleanly "unresolvable to-endpoint" "does not resolve to a commit" "v0.3.0..bogus-ref"

# Induced git-subprocess failure: `git log` fails outright, and the abort
# happens before any stdout is produced.
REAL_GIT="$(command -v git)"
FAKEBIN_FAILURE="${TMP}/fakebin-failure"
mkdir -p "$FAKEBIN_FAILURE"
cat > "${FAKEBIN_FAILURE}/git" <<EOF
#!/usr/bin/env bash
if [[ "\$1" == "log" ]]; then
  echo "simulated git log failure" >&2
  exit 7
fi
exec "$REAL_GIT" "\$@"
EOF
chmod +x "${FAKEBIN_FAILURE}/git"

set +e
GEN_FAIL_STDOUT="$(cd "$GENREPO" && PATH="${FAKEBIN_FAILURE}:${PATH}" "$GEN_SCRIPT" 2>/dev/null)"
GEN_FAIL_STATUS=$?
set -e
if [[ $GEN_FAIL_STATUS -ne 0 && -z "$GEN_FAIL_STDOUT" ]]; then
  ok "induced git log subprocess failure aborts before stdout"
else
  fail "induced git log subprocess failure did not abort cleanly (status=$GEN_FAIL_STATUS, stdout=${GEN_FAIL_STDOUT})"
fi

# Induced malformed framing: `git log` returns output whose NUL-framed
# record count is not a multiple of 3.
FAKEBIN_FRAMING="${TMP}/fakebin-framing"
mkdir -p "$FAKEBIN_FRAMING"
cat > "${FAKEBIN_FRAMING}/git" <<EOF
#!/usr/bin/env bash
if [[ "\$1" == "log" ]]; then
  printf 'onlyonefield'
  exit 0
fi
exec "$REAL_GIT" "\$@"
EOF
chmod +x "${FAKEBIN_FRAMING}/git"

GEN_FRAME_STDERR_FILE="${TMP}/gen-frame-stderr"
set +e
GEN_FRAME_STDOUT="$(cd "$GENREPO" && PATH="${FAKEBIN_FRAMING}:${PATH}" "$GEN_SCRIPT" 2>"$GEN_FRAME_STDERR_FILE")"
GEN_FRAME_STATUS=$?
set -e
GEN_FRAME_STDERR="$(cat "$GEN_FRAME_STDERR_FILE")"
if [[ $GEN_FRAME_STATUS -ne 0 && -z "$GEN_FRAME_STDOUT" && "$GEN_FRAME_STDERR" == *"malformed git log output"* ]]; then
  ok "malformed git log framing aborts before stdout with a framing diagnostic"
else
  fail "malformed git log framing not handled as expected (status=$GEN_FRAME_STATUS, stdout=${GEN_FRAME_STDOUT}, stderr=${GEN_FRAME_STDERR})"
fi

# ---------------------------------------------------------------------------
# Generator probes (packaging/gen-release-notes.sh) — merge exclusion
# ---------------------------------------------------------------------------

MERGEREPO="${TMP}/mergerepo"
mkdir -p "$MERGEREPO"
git -C "$MERGEREPO" init -q
git -C "$MERGEREPO" config user.email "test@example.com"
git -C "$MERGEREPO" config user.name "Test"

commit "$MERGEREPO" "base.txt" "1" "chore: base commit"
git -C "$MERGEREPO" tag v0.1.0
MERGE_DEFAULT_BRANCH="$(git -C "$MERGEREPO" symbolic-ref --short HEAD)"

git -C "$MERGEREPO" checkout -q -b feature
commit "$MERGEREPO" "feature.txt" "1" "feat: add feature branch work"
git -C "$MERGEREPO" checkout -q "$MERGE_DEFAULT_BRANCH"
commit "$MERGEREPO" "main.txt" "1" "chore: main branch work"
git -C "$MERGEREPO" merge --no-ff -q -m "Merge branch 'feature'" feature

MERGE_DEFAULT_OUT="$(cd "$MERGEREPO" && "$GEN_SCRIPT")"
if [[ "$MERGE_DEFAULT_OUT" == *"add feature branch work"* \
   && "$MERGE_DEFAULT_OUT" == *"main branch work"* \
   && "$MERGE_DEFAULT_OUT" != *"Merge branch"* \
   && "$MERGE_DEFAULT_OUT" == *"Merges excluded: 1"* ]]; then
  ok "default range: merged branch commits retained, merge title absent, exactly one merge excluded"
else
  fail "default range merge handling incorrect: ${MERGE_DEFAULT_OUT}"
fi

MERGE_EXPLICIT_OUT="$(cd "$MERGEREPO" && "$GEN_SCRIPT" "v0.1.0..HEAD")"
if [[ "$MERGE_EXPLICIT_OUT" == *"add feature branch work"* \
   && "$MERGE_EXPLICIT_OUT" == *"main branch work"* \
   && "$MERGE_EXPLICIT_OUT" != *"Merge branch"* \
   && "$MERGE_EXPLICIT_OUT" == *"Merges excluded: 1"* ]]; then
  ok "explicit range: merged branch commits retained, merge title absent, exactly one merge excluded"
else
  fail "explicit range merge handling incorrect: ${MERGE_EXPLICIT_OUT}"
fi

# ---------------------------------------------------------------------------
# Generator probes (packaging/gen-release-notes.sh) — noise allowlist and
# grouping
# ---------------------------------------------------------------------------

extract_section() {
  # extract_section <content> <title> — the lines between "### <title>" and
  # the next "### " header (or EOF), exclusive of both header lines.
  local content="$1" title="$2"
  awk -v t="### ${title}" '
    $0 == t { flag=1; next }
    /^### / { flag=0 }
    flag { print }
  ' <<< "$content"
}

GROUPREPO="${TMP}/grouprepo"
mkdir -p "$GROUPREPO"
git -C "$GROUPREPO" init -q
git -C "$GROUPREPO" config user.email "test@example.com"
git -C "$GROUPREPO" config user.name "Test"

commit "$GROUPREPO" "base.txt" "1" "chore: base commit"
git -C "$GROUPREPO" tag v0.1.0

# Exact allowlist positives: one commit per fixed (type, scope, subject-prefix)
# triple. All three must be dropped entirely from the draft.
commit "$GROUPREPO" "f1.txt" "1" "docs(plan): converge T-hotfix excl-docs-plan"
commit "$GROUPREPO" "f2.txt" "1" "chore(plans): absorb the excl-chore-plans notes"
commit "$GROUPREPO" "f3.txt" "1" "chore(release): bump ccync-cli to excl-chore-release"

# Near misses: each changes exactly one of type, scope, or subject-prefix
# relative to the first triple, so none may be dropped.
commit "$GROUPREPO" "f4.txt" "1" "fix(plan): converge T-hotfix near-type"
commit "$GROUPREPO" "f5.txt" "1" "docs(plans): converge T-hotfix near-scope"
commit "$GROUPREPO" "f6.txt" "1" "docs(plan): diverge T-hotfix near-prefix"

# docs(state) must always be retained regardless of subject content.
commit "$GROUPREPO" "f7.txt" "1" "docs(state): always-state release progress"

# One commit per group to assert Added -> Changed -> Fixed -> Other ordering.
commit "$GROUPREPO" "f8.txt" "1" "feat: grp-added sync watcher"
commit "$GROUPREPO" "f9.txt" "1" "fix: grp-fixed race condition"
commit "$GROUPREPO" "f10.txt" "1" "feat(api)!: grp-changed redesign interface"
commit "$GROUPREPO" "f11.txt" "1" "chore: grp-other tidy formatting"

GEN_GROUP_OUT="$(cd "$GROUPREPO" && "$GEN_SCRIPT")"

if [[ "$GEN_GROUP_OUT" != *"excl-docs-plan"* \
   && "$GEN_GROUP_OUT" != *"excl-chore-plans"* \
   && "$GEN_GROUP_OUT" != *"excl-chore-release"* ]]; then
  ok "exact allowlist triples (docs/plan, chore/plans, chore/release) are dropped"
else
  fail "an exact allowlist triple was not dropped: ${GEN_GROUP_OUT}"
fi

if [[ "$GEN_GROUP_OUT" == *"near-type"* \
   && "$GEN_GROUP_OUT" == *"near-scope"* \
   && "$GEN_GROUP_OUT" == *"near-prefix"* \
   && "$GEN_GROUP_OUT" == *"always-state"* ]]; then
  ok "type/scope/subject-prefix near misses and docs(state) are retained"
else
  fail "a near-miss or docs(state) commit was incorrectly dropped: ${GEN_GROUP_OUT}"
fi

if [[ "$GEN_GROUP_OUT" == *"Allowlist excluded: 3"* ]]; then
  ok "footer reports exactly 3 allowlist exclusions"
else
  fail "allowlist exclusion count incorrect: ${GEN_GROUP_OUT}"
fi

if [[ "$GEN_GROUP_OUT" == *"Merges excluded: 0"* ]]; then
  ok "footer reports the merge count separately from the allowlist count"
else
  fail "merge exclusion count incorrect: ${GEN_GROUP_OUT}"
fi

HEADER_ORDER="$(grep -o '^### [A-Za-z]*' <<< "$GEN_GROUP_OUT" | sed 's/^### //' | tr '\n' ',' )"
if [[ "$HEADER_ORDER" == "Added,Changed,Fixed,Other," ]]; then
  ok "groups print in Added -> Changed -> Fixed -> Other order"
else
  fail "group order incorrect: ${HEADER_ORDER}"
fi

ADDED_SECTION="$(extract_section "$GEN_GROUP_OUT" "Added")"
CHANGED_SECTION="$(extract_section "$GEN_GROUP_OUT" "Changed")"
FIXED_SECTION="$(extract_section "$GEN_GROUP_OUT" "Fixed")"
OTHER_SECTION="$(extract_section "$GEN_GROUP_OUT" "Other")"

if [[ "$ADDED_SECTION" == *"grp-added"* && "$ADDED_SECTION" != *"grp-changed"* \
   && "$ADDED_SECTION" != *"grp-fixed"* && "$ADDED_SECTION" != *"grp-other"* ]]; then
  ok "feat commit is grouped under Added"
else
  fail "Added section content incorrect: ${ADDED_SECTION}"
fi

if [[ "$CHANGED_SECTION" == *"grp-changed"* ]]; then
  ok "breaking-change commit (feat(api)!:) is grouped under Changed ahead of its feat type"
else
  fail "Changed section content incorrect: ${CHANGED_SECTION}"
fi

if [[ "$FIXED_SECTION" == *"grp-fixed"* && "$FIXED_SECTION" == *"near-type"* ]]; then
  ok "fix commits (including the type-near-miss) are grouped under Fixed"
else
  fail "Fixed section content incorrect: ${FIXED_SECTION}"
fi

if [[ "$OTHER_SECTION" == *"grp-other"* && "$OTHER_SECTION" == *"near-scope"* \
   && "$OTHER_SECTION" == *"near-prefix"* && "$OTHER_SECTION" == *"always-state"* ]]; then
  ok "docs/chore commits (including scope/prefix near misses and docs(state)) are grouped under Other"
else
  fail "Other section content incorrect: ${OTHER_SECTION}"
fi

OTHER_GRP_LINE="$(grep -F "grp-other" <<< "$OTHER_SECTION")"
if [[ "$OTHER_GRP_LINE" == *"[REVIEW]"* ]]; then
  ok "Other items are prefixed with [REVIEW]"
else
  fail "Other item missing [REVIEW] prefix: ${OTHER_GRP_LINE}"
fi

ADDED_GRP_LINE="$(grep -F "grp-added" <<< "$ADDED_SECTION")"
if [[ "$ADDED_GRP_LINE" != *"[REVIEW]"* ]]; then
  ok "Added items are not tagged [REVIEW]"
else
  fail "Added item incorrectly tagged [REVIEW]: ${ADDED_GRP_LINE}"
fi

# Preserve log order: within a group, retained items keep git log's order
# (newest first, matching the underlying `git log` walk) rather than being
# resorted.
FIXED_ORDER="$(grep -o 'grp-fixed\|near-type' <<< "$FIXED_SECTION" | tr '\n' ',')"
if [[ "$FIXED_ORDER" == "grp-fixed,near-type," ]]; then
  ok "log order is preserved within a group"
else
  fail "log order not preserved within Fixed group: ${FIXED_ORDER}"
fi

# ---------------------------------------------------------------------------
# CHANGELOG.md snapshot/history probes — this repo's own committed
# CHANGELOG.md and history are the fixture; no synthetic repo needed.
# ---------------------------------------------------------------------------

REAL_CHANGELOG="${REPO_ROOT}/CHANGELOG.md"

# Archive membership: the seeded CHANGELOG.md ships in the curated snapshot.
ARCHIVE_LIST="$(git -C "$REPO_ROOT" archive HEAD --format=tar 2>/dev/null | tar -tf -)"
if grep -qxF "CHANGELOG.md" <<< "$ARCHIVE_LIST"; then
  ok "CHANGELOG.md is included in the git archive HEAD snapshot"
else
  fail "CHANGELOG.md is missing from the git archive HEAD snapshot"
fi

# eol attribute: .gitattributes pins CHANGELOG.md to LF.
CHANGELOG_EOL="$(git -C "$REPO_ROOT" check-attr eol -- CHANGELOG.md)"
if [[ "$CHANGELOG_EOL" == *": lf" ]]; then
  ok "CHANGELOG.md eol attribute is lf"
else
  fail "CHANGELOG.md eol attribute incorrect: ${CHANGELOG_EOL}"
fi

# Byte-identical extraction: release-body.sh's 0.1.5 output matches an
# independently-derived expected section (awk, not the script under test).
GEN_015_OUT="${TMP}/out-015.md"
"$SCRIPT" "0.1.5" --changelog "$REAL_CHANGELOG" --output "$GEN_015_OUT" >/dev/null
EXPECTED_015="${TMP}/expected-015.md"
awk '/^## \[0\.1\.5\]/ { flag=1; next } /^## \[/ { flag=0 } flag' "$REAL_CHANGELOG" > "$EXPECTED_015"
if [[ -s "$GEN_015_OUT" ]] && cmp -s "$GEN_015_OUT" "$EXPECTED_015"; then
  ok "0.1.5 extraction from CHANGELOG.md is non-blank and byte-identical to its own section"
else
  fail "0.1.5 extraction from CHANGELOG.md is not byte-identical to its own section"
fi

# Explicit source ranges: each 0.1.0-0.1.5 section names its locked range
# (0.1.0 has no predecessor, so it carries no "Source range:" line).
assert_source_range() {
  local version="$1" expect_range="$2"
  local section
  section="$("$SCRIPT" "$version" --changelog "$REAL_CHANGELOG")"
  if [[ "$section" == *"Source range: \`${expect_range}\`"* ]]; then
    ok "CHANGELOG.md ${version} section names source range ${expect_range}"
  else
    fail "CHANGELOG.md ${version} section missing/incorrect source range (expected ${expect_range}): ${section}"
  fi
}

assert_source_range "0.1.1" "v0.1.0..378c672b"
assert_source_range "0.1.2" "378c672b..v0.1.2"
assert_source_range "0.1.3" "v0.1.2..v0.1.3"
assert_source_range "0.1.4" "v0.1.3..v0.1.4"
assert_source_range "0.1.5" "v0.1.4..v0.1.5"

# Representative-commit ancestry: each locked evidence commit for 0.1.1-0.1.5
# resolves as a real commit in this repo's history (existence, not a literal
# citation inside the changelog prose).
if [[ "$PUBLIC_SNAPSHOT" -eq 0 ]]; then
  for rep_commit in bf230641 2bbc0894 ace74e93 0f3f86d0 1bb74f53 db2920e5; do
    if git -C "$REPO_ROOT" cat-file -e "${rep_commit}^{commit}" 2>/dev/null; then
      ok "representative commit ${rep_commit} resolves as a real commit"
    else
      fail "representative commit ${rep_commit} does not resolve as a commit"
    fi
  done
else
  ok "source-history representative commits are not required in the exported snapshot"
fi

# ---------------------------------------------------------------------------
# GitHub release contract probes — exact public repository guard and validated
# stable/bare-RC/numbered-RC version handling.
# ---------------------------------------------------------------------------

GH_RELEASE_YML="${REPO_ROOT}/.github/workflows/release.yml"
GH_SIGN_BLOCK="$(sed -n '/^  sign:/,/^  submit-winget:/p' "$GH_RELEASE_YML")"

GUARDED_JOB_COUNT="$(grep -cE "^    if: .*github\.repository == 'monkey1wizard/ccync'" "$GH_RELEASE_YML" || true)"
if [[ "$GUARDED_JOB_COUNT" -eq 11 ]]; then
  ok "all eleven release jobs contain the exact public repository guard"
else
  fail "release workflow expected 11 exact public repository job guards, found ${GUARDED_JOB_COUNT}"
fi

if grep -qF "needs: [build, test]" <<< "$GH_SIGN_BLOCK" \
    && grep -qF "needs.build.result == 'success'" <<< "$GH_SIGN_BLOCK" \
    && grep -qF "needs.test.result == 'success'" <<< "$GH_SIGN_BLOCK"; then
  ok "sign job requires the guarded build to succeed"
else
  fail "sign job does not require a successful guarded build"
fi

if grep -qF '(-rc[0-9]*)?' <<< "$GH_SIGN_BLOCK" \
    && grep -qF 'TAG_VERSION="${TAG_VERSION%%-rc*}"' <<< "$GH_SIGN_BLOCK"; then
  ok "release preflight accepts stable, bare RC, and numbered RC tags with validated base extraction"
else
  fail "release preflight does not implement the stable/bare-RC/numbered-RC contract"
fi

# ---------------------------------------------------------------------------
# GitHub workflow wiring probes — .github/workflows/release.yml gates the
# `sign` job immediately after the tag/crate-version preflight, writes the
# curated body to release-out/release-body.md, and routes stable vs.
# prerelease tags to two mutually exclusive Create GitHub Release steps.
# ---------------------------------------------------------------------------

GH_RELEASE_YML="${REPO_ROOT}/.github/workflows/release.yml"
GH_SIGN_BLOCK="$(sed -n '/^  sign:/,/^  submit-winget:/p' "$GH_RELEASE_YML")"

# Exact gate invocation: the tag and the release-out output path, nothing else.
if grep -qF 'bash packaging/release-body.sh "${{ github.ref_name }}" --output release-out/release-body.md' <<< "$GH_SIGN_BLOCK"; then
  ok "GitHub sign job invokes the gate with the tag and --output release-out/release-body.md"
else
  fail "GitHub sign job does not invoke release-body.sh with the expected tag and --output arguments"
fi

# Ordering: crate-version preflight < gate < artifact download.
gh_line() {
  grep -nF -- "$1" "$GH_RELEASE_YML" | head -n1 | cut -d: -f1
}

CRATE_PREFLIGHT_LINE="$(gh_line 'CRATE_VERSION="$(grep -m1')"
GH_GATE_LINE="$(gh_line 'bash packaging/release-body.sh')"
DOWNLOAD_ARTIFACTS_LINE="$(gh_line 'Download all build artifacts')"

if [[ -n "$CRATE_PREFLIGHT_LINE" && -n "$GH_GATE_LINE" && -n "$DOWNLOAD_ARTIFACTS_LINE" \
      && "$CRATE_PREFLIGHT_LINE" -lt "$GH_GATE_LINE" \
      && "$GH_GATE_LINE" -lt "$DOWNLOAD_ARTIFACTS_LINE" ]]; then
  ok "GitHub gate runs after crate-version preflight and before artifact download"
else
  fail "GitHub gate ordering incorrect (preflight=${CRATE_PREFLIGHT_LINE} gate=${GH_GATE_LINE} download=${DOWNLOAD_ARTIFACTS_LINE})"
fi

# Stable release step: body_path set, auto notes disabled, no unconditional
# generate_release_notes: true remains.
STABLE_STEP_BLOCK="$(sed -n '/Create GitHub Release (stable/,/Create GitHub Release (prerelease/p' <<< "$GH_SIGN_BLOCK")"

if grep -qF 'body_path: release-out/release-body.md' <<< "$STABLE_STEP_BLOCK"; then
  ok "Stable Create GitHub Release step sets body_path to the gate output"
else
  fail "Stable Create GitHub Release step is missing body_path: release-out/release-body.md"
fi

if grep -qF 'generate_release_notes: false' <<< "$STABLE_STEP_BLOCK"; then
  ok "Stable Create GitHub Release step disables auto notes"
else
  fail "Stable Create GitHub Release step does not disable generate_release_notes"
fi

# Prerelease release step: auto notes enabled, no body_path reference.
RC_STEP_BLOCK="$(sed -n '/Create GitHub Release (prerelease/,/^      - name:/p' <<< "$GH_SIGN_BLOCK")"
if grep -qF 'generate_release_notes: true' <<< "$RC_STEP_BLOCK"; then
  ok "Prerelease Create GitHub Release step keeps auto notes enabled"
else
  fail "Prerelease Create GitHub Release step does not enable generate_release_notes"
fi

if grep -qF 'body_path' <<< "$RC_STEP_BLOCK"; then
  fail "Prerelease Create GitHub Release step must not reference body_path"
else
  ok "Prerelease Create GitHub Release step has no body_path reference"
fi

# The two Create GitHub Release steps are mutually exclusive on the same
# ref-name predicate (stable = no '-', prerelease = has '-').
if grep -qF "if: \"!contains(github.ref_name, '-')\"" <<< "$GH_SIGN_BLOCK" \
    && grep -qF "if: \"contains(github.ref_name, '-')\"" <<< "$GH_SIGN_BLOCK"; then
  ok "Stable and prerelease Create GitHub Release steps use mutually exclusive ref-name conditions"
else
  fail "Create GitHub Release steps are missing the expected mutually exclusive ref-name conditions"
fi

# Trust-surface byte checks: permissions and cosign lines are byte-unchanged.
for expect_line in \
  'permissions:' \
  'contents: read' \
  'id-token: none' \
  'contents: write' \
  'id-token: write' \
  'uses: sigstore/cosign-installer@v3' \
  'cosign sign-blob checksums.txt' \
  '--output-signature checksums.txt.sig' \
  '--output-certificate checksums.txt.pem' \
  "--certificate-identity-regexp '^https://github\\.com/monkey1wizard/ccync/\\.github/workflows/release\\.yml@refs/tags/v[0-9]+\\.[0-9]+\\.[0-9]+'" \
  '--certificate-oidc-issuer https://token.actions.githubusercontent.com'
do
  if grep -qF -- "$expect_line" "$GH_RELEASE_YML"; then
    ok "GitHub workflow trust-surface line unchanged: ${expect_line}"
  else
    fail "GitHub workflow trust-surface line missing/changed: ${expect_line}"
  fi
done

# ---------------------------------------------------------------------------
# Private publication controller boundary and source/public tree modes.
# ---------------------------------------------------------------------------

if [[ "$PUBLIC_SNAPSHOT" -eq 0 ]]; then
  if command -v ruby >/dev/null 2>&1; then
    if ruby -e 'require "yaml"; YAML.parse_file(ARGV.fetch(0))' "$CONTROLLER" >/dev/null 2>&1; then
      ok "private controller parses as YAML"
    else
      fail "private controller does not parse as YAML"
    fi
  elif command -v python >/dev/null 2>&1 && python -I -c 'import yaml' >/dev/null 2>&1; then
    if python -I -c 'import sys, yaml; yaml.safe_load(open(sys.argv[1], encoding="utf-8"))' "$CONTROLLER" >/dev/null 2>&1; then
      ok "private controller parses as YAML"
    else
      fail "private controller does not parse as YAML"
    fi
  elif grep -q '^\\' "$CONTROLLER"; then
    fail "private controller has an invalid top-level backslash"
  else
    ok "private controller has no invalid top-level backslash"
  fi

  CONTROLLER_TEXT="$(cat "$CONTROLLER")"
  for required in \
    "github.repository == 'monkey1wizard/ccync-private'" \
    "github.event_name == 'workflow_dispatch'" \
    'ref: ${{ github.sha }}' \
    'persist-credentials: false' \
    'needs: validate' \
    'github.actor_id == github.repository_owner_id' \
    'github.ref == '\''refs/heads/main'\''' \
    '--verify-signed-receipts' \
    '--trust-list "$TRUST"' \
    '--windows-signature' \
    '--linux-signature' \
    '--backup-controller --verified-source' \
    'concurrency:' \
    'group: publish-public-main' \
    'queue: max' \
    'cancel-in-progress: false' \
    'PUBLIC_REPO_PUSH_TOKEN: ${{ secrets.PUBLIC_REPO_PUSH_TOKEN }}' \
    'GIT_ASKPASS=' \
    'needs.validate.outputs.tag_object'
  do
    if grep -qF -- "$required" <<< "$CONTROLLER_TEXT"; then
      ok "private controller contains contract: ${required}"
    else
      fail "private controller is missing contract: ${required}"
    fi
  done

  if grep -qF 'contents: write' <<< "$CONTROLLER_TEXT" || grep -qF 'id-token: write' <<< "$CONTROLLER_TEXT"; then
    fail "private controller must keep default and publish job permissions read-only"
  else
    ok "private controller grants no repository write or OIDC permission"
  fi

  PUBLISH_BLOCK="$(sed -n '/^  publish:/,$p' "$CONTROLLER")"
  SECRET_COUNT="$(grep -cF 'PUBLIC_REPO_PUSH_TOKEN: ${{ secrets.PUBLIC_REPO_PUSH_TOKEN }}' "$CONTROLLER" || true)"
  if [[ "$SECRET_COUNT" -eq 1 ]] && ! grep -qF 'PUBLIC_REPO_PUSH_TOKEN' <<< "${CONTROLLER_TEXT%%  publish:*}"; then
    ok "public push secret appears only in the final publish job"
  else
    fail "public push secret is present outside the final publish job or appears more than once"
  fi

  CHECKOUT_COUNT="$(grep -cF 'ref: ${{ github.sha }}' "$CONTROLLER" || true)"
  if [[ "$CHECKOUT_COUNT" -eq 1 ]] && [[ "$(grep -cF 'persist-credentials: false' "$CONTROLLER" || true)" -eq 2 ]]; then
    ok "controller jobs use immutable source without persisted credentials"
  else
    fail "controller jobs do not all use immutable credential-free checkouts"
  fi

  TAG_VALIDATION_LINE="$(grep -nF 'Verify source, tag and receipts' "$CONTROLLER" | cut -d: -f1)"
  PUBLISH_LINE="$(grep -n '^  publish:' "$CONTROLLER" | cut -d: -f1 || true)"
  if [[ -n "$TAG_VALIDATION_LINE" && -n "$PUBLISH_LINE" && "$TAG_VALIDATION_LINE" -lt "$PUBLISH_LINE" ]]; then
    ok "source and receipt verification precedes publication"
  else
    fail "controller validation, tests, and publication are not ordered safely"
  fi

  if grep -qF '  push:' <<< "$CONTROLLER_TEXT"; then
    fail "private tag push must not trigger publication"
  else
    ok "private tag push does not trigger publication"
  fi
else
  ok "explicit public snapshot mode confirms the private controller is absent"
fi

# ---------------------------------------------------------------------------
# Ordinary CI contract — main pushes, pull requests, and manual dispatch, with blocking
# Ubuntu/Windows gates and explicit source/public release-note probe modes.
# ---------------------------------------------------------------------------

CI_WORKFLOW="${REPO_ROOT}/.github/workflows/ci.yml"
if [[ -f "$CI_WORKFLOW" ]]; then
  CI_TEXT="$(cat "$CI_WORKFLOW")"
  PUSH_BLOCK="$(sed -n '/^  push:/,/^  pull_request:/p' "$CI_WORKFLOW")"

  TRIGGERS_BLOCK="$(sed -n '/^on:/,/^permissions:/p' "$CI_WORKFLOW")"
  EVENT_COUNT="$(grep -cE '^  [a-z_]+:' <<< "$TRIGGERS_BLOCK" || true)"
  if grep -qF '  push:' <<< "$TRIGGERS_BLOCK" \
      && [[ "$EVENT_COUNT" -eq 4 ]] \
      && grep -qF '    branches:' <<< "$PUSH_BLOCK" \
      && [[ "$(grep -cE '^      - ' <<< "$PUSH_BLOCK" || true)" -eq 1 ]] \
      && grep -qF '      - main' <<< "$PUSH_BLOCK" \
      && grep -qF '  pull_request:' <<< "$TRIGGERS_BLOCK" \
      && grep -qF '  workflow_dispatch:' <<< "$TRIGGERS_BLOCK" \
      && grep -qF '  workflow_call:' <<< "$TRIGGERS_BLOCK"; then
    ok "ordinary CI enables main pushes, pull requests, and manual dispatch"
  else
    fail "ordinary CI must enable main pushes, pull requests, and manual dispatch"
  fi

  if grep -qE '^    tags(-ignore)?:' <<< "$PUSH_BLOCK" \
      || grep -qE '^      - v\*' <<< "$PUSH_BLOCK" \
      || grep -qE '^  (schedule|release):' <<< "$TRIGGERS_BLOCK"; then
    fail "ordinary push CI must not add tag-only push triggers"
  else
    ok "ordinary push CI excludes tag-only triggers"
  fi

  for job in test-linux test-windows test-public-snapshot-linux test-public-snapshot-windows; do
    JOB_BLOCK="$(awk -v job="$job" '
      $0 == "  " job ":" { found=1; next }
      found && /^  [a-z][a-z-]*:$/ { exit }
      found { print }
    ' "$CI_WORKFLOW")"
    if [[ "$job" == test-public-snapshot-* ]]; then
      EXPECTED_IF="    if: github.repository == 'monkey1wizard/ccync-private' && github.event_name == 'workflow_dispatch'"
    else
      EXPECTED_IF="    if: github.repository != 'monkey1wizard/ccync-private' || github.event_name == 'workflow_dispatch'"
    fi
    if [[ "$(grep -cE "^  ${job}:$" "$CI_WORKFLOW" || true)" -eq 1 ]] \
        && [[ "$(grep -cE '^    if:' <<< "$JOB_BLOCK" || true)" -eq 1 ]] \
        && grep -qxF "$EXPECTED_IF" <<< "$JOB_BLOCK"; then
      ok "${job} has the required repository and event condition"
    else
      fail "${job} is missing or has an incorrect repository and event condition"
    fi
  done

  if [[ $(grep -cF -- '--gate --checkout-sha "$GITHUB_SHA"' <<< "$CI_TEXT") -eq 2 ]] && [[ $(grep -cF 'uses: actions/upload-artifact@v4' <<< "$CI_TEXT") -eq 2 ]]; then
    ok "ordinary CI runs the shared gate and retains both receipts"
  else
    fail "ordinary CI must run the shared gate and retain both receipts"
  fi

  IDENTITY_COUNT="$(grep -cF "git config --global user.name 'GitHub Actions'" <<< "$CI_TEXT" || true)"
  EMAIL_COUNT="$(grep -cF "git config --global user.email 'github-actions[bot]@users.noreply.github.com'" <<< "$CI_TEXT" || true)"
  CHECKOUTS_CREDENTIALLESS="$(awk '
    { sub(/\r$/, "") }
    /^        uses: actions\/checkout@/ {
      if (checkout && !credentialless) failed=1
      checkout=1
      credentialless=0
      count++
      next
    }
    checkout && /^      - / {
      if (!credentialless) failed=1
      checkout=0
    }
    checkout && /persist-credentials: false/ { credentialless=1 }
    END {
      if (checkout && !credentialless) failed=1
      if (count == 0 || failed) exit 1
      print count
    }
  ' <<< "$CI_TEXT" 2>/dev/null || true)"
  READ_ONLY_JOBS="$(awk '
    { sub(/\r$/, "") }
    /^jobs:$/ { in_jobs=1; next }
    in_jobs && /^  [a-z][a-z-]*:$/ {
      if (job && (!permissions || !contents_read || writable)) failed=1
      job=1
      permissions=0
      contents_read=0
      writable=0
      next
    }
    job && /^    permissions:$/ { permissions=1; next }
    permissions && /^      contents: read$/ { contents_read=1; next }
    permissions && /^      [a-z-]+: write$/ { writable=1 }
    END {
      if (job && (!permissions || !contents_read || writable)) failed=1
      if (failed || !job) exit 1
      print "ok"
    }
  ' "$CI_WORKFLOW" 2>/dev/null || true)"
  if [[ "$IDENTITY_COUNT" -eq 2 && "$EMAIL_COUNT" -eq 2 ]]; then
    ok "both disposable CI runners configure neutral Git identity"
  else
    fail "both CI runners must configure neutral Git name and email (found ${IDENTITY_COUNT}/${EMAIL_COUNT})"
  fi
  if grep -qF 'permissions:' <<< "$CI_TEXT" \
      && grep -qF 'contents: read' <<< "$CI_TEXT" \
      && [[ -n "$CHECKOUTS_CREDENTIALLESS" && "$READ_ONLY_JOBS" == "ok" ]]; then
    ok "every ordinary CI job is read-only and every checkout disables persisted credentials (${CHECKOUTS_CREDENTIALLESS} checkouts)"
  else
    fail "ordinary CI jobs or checkouts are not restricted safely (credential-free checkouts='${CHECKOUTS_CREDENTIALLESS}', read-only jobs='${READ_ONLY_JOBS}')"
  fi
  for job in test-linux test-windows; do
    JOB_BLOCK="$(sed -n "/^  ${job}:/,/^  [a-z-][a-z-]*:/p" "$CI_WORKFLOW")"
    if grep -qF "git config --global user.name 'GitHub Actions'" <<< "$JOB_BLOCK" \
        && grep -qF "git config --global user.email 'github-actions[bot]@users.noreply.github.com'" <<< "$JOB_BLOCK"; then
      ok "${job} configures neutral Git identity"
    else
      fail "${job} is missing neutral Git identity"
    fi
  done
  if grep -qF 'continue-on-error:' <<< "$CI_TEXT"; then
    fail "ordinary CI required checks must remain blocking"
  else
    ok "ordinary CI has no continue-on-error tolerance"
  fi
else
  fail "ordinary CI workflow is missing"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""

REHEARSAL="${REPO_ROOT}/.github/workflows/release-rehearsal.yml"
BUILD="${REPO_ROOT}/.github/workflows/release-build.yml"
if [[ "$PUBLIC_SNAPSHOT" -eq 0 ]]; then
  condition="github.repository == 'monkey1wizard/ccync-private' && github.ref == 'refs/heads/main' && github.actor_id == github.repository_owner_id"
  rehearsal_ok=1
  for value in '  workflow_dispatch:' '        required: true' '        type: string' '  contents: read' \
    'uses: ./.github/workflows/release-build.yml' 'version: ${{ inputs.version }}' \
    'os: [ubuntu-latest, ubuntu-24.04-arm, macos-15-intel, macos-latest, windows-latest, windows-11-arm]' \
    'needs: [build, stage-assets, install-script, install-cargo, install-homebrew, install-winget]'; do
    grep -qF "$value" "$REHEARSAL" || rehearsal_ok=0
  done
  [[ $(grep -cF "$condition" "$REHEARSAL") -eq 7 ]] || rehearsal_ok=0
  [[ $(grep -cE '^  [a-z-]+:$' "$REHEARSAL") -eq 7 ]] || rehearsal_ok=0
  [[ $(grep -cE '^  (workflow_dispatch|push|pull_request|schedule|workflow_call):' "$REHEARSAL") -eq 1 ]] || rehearsal_ok=0
  [[ $(awk '/^permissions:$/ {p=1; next} p && /^[^ ]/ {p=0} p && NF {n++} END {print n+0}' "$REHEARSAL") -eq 1 ]] || rehearsal_ok=0
  ! grep -Eq 'secrets\.|gh release|git tag|git push|softprops/action-gh-release|^[[:space:]]+(write-all|[a-z-]+: write)' "$REHEARSAL" || rehearsal_ok=0
  for job in install-script install-cargo install-homebrew install-winget; do
    section=$(awk -v job="$job" '$0 == "  " job ":" {p=1; next} /^  [a-z-]+:$/ {p=0} p' "$REHEARSAL")
    [[ "$section" == *'--version'* && "$section" == *'refresh'* && ( "$section" == *'grep -F'* || "$section" == *'.Contains('* ) ]] || rehearsal_ok=0
  done
  for pair in 'x86_64-pc-windows-msvc windows-latest' 'aarch64-pc-windows-msvc windows-latest' 'x86_64-apple-darwin macos-15-intel' 'aarch64-apple-darwin macos-latest' 'x86_64-unknown-linux-musl ubuntu-latest' 'aarch64-unknown-linux-musl ubuntu-latest'; do
    read -r target runner <<< "$pair"
    grep -A1 -F -- "- target: $target" "$BUILD" | grep -qF "os: $runner" || rehearsal_ok=0
  done
  [[ $(grep -cF -- '- target:' "$BUILD") -eq 6 ]] || rehearsal_ok=0
  grep -qF '  workflow_call:' "$BUILD" || rehearsal_ok=0
  ! grep -Eq 'github\.ref_name|github\.repository' "$BUILD" || rehearsal_ok=0
  grep -qF 'uses: ./.github/workflows/release-build.yml' "$GH_RELEASE_YML" || rehearsal_ok=0
  grep -qF 'version: ${{ github.ref_name }}' "$GH_RELEASE_YML" || rehearsal_ok=0
  if [[ $rehearsal_ok -eq 1 ]]; then ok 'rehearsal and shared build workflow shape'; else fail 'rehearsal or shared build workflow shape'; fi
fi
if [[ "$PUBLIC_SNAPSHOT" -eq 1 ]]; then
  [[ ! -f "$REHEARSAL" ]] && ok "private rehearsal excluded from snapshot" || fail "private rehearsal included in snapshot"
elif [[ -f "$REHEARSAL" ]] && grep -qF 'github.actor_id == github.repository_owner_id' "$REHEARSAL" && grep -qF 'workflow_call:' "$BUILD" && grep -qF 'uses: ./.github/workflows/release-build.yml' "$REHEARSAL"; then
  ok "private rehearsal calls the shared build with an owner guard"
else
  fail "private rehearsal workflow contract is missing"
fi


if [[ "$PUBLIC_SNAPSHOT" -eq 0 ]]; then
  for expected in 'stage-assets:' 'install-script:' 'install-cargo:' 'install-homebrew:' 'install-winget:' 'ready:' 'ubuntu-24.04-arm' 'macos-15-intel' 'windows-11-arm' 'CCYNC_DOWNLOAD_BASE:' 'cargo install --locked --path crates/ccync-cli' 'brew test ccync' 'winget validate' 'ccync refresh' 'github.ref == '\''refs/heads/main'\'''; do
    if grep -qF "$expected" "$REHEARSAL"; then
      ok "rehearsal contains $expected"
    else
      fail "rehearsal missing $expected"
    fi
  done
  if grep -qE 'secrets\.|contents: write|id-token: write' "$REHEARSAL"; then
    fail "rehearsal must remain read-only without secrets"
  else
    ok "rehearsal remains read-only without secrets"
  fi
fi


job_block() { awk -v job="$1" '$0 == "  " job ":" {p=1; next} /^  [a-z-]+:$/ {p=0} p' "$GH_RELEASE_YML"; }
check_release() { if eval "$2"; then ok "$1"; else fail "$1"; fi; }
CI_YML="${REPO_ROOT}/.github/workflows/ci.yml"
check_release 'shared dual-platform gate' 'grep -qF "  workflow_call:" "$CI_YML" && [[ $(grep -cF -- "--gate --checkout-sha" "$CI_YML") -eq 2 ]] && grep -qF "uses: ./.github/workflows/ci.yml" "$GH_RELEASE_YML"'
check_release 'both releases start pre-release' '[[ $(grep -cF "          prerelease: true" <<< "$GH_SIGN_BLOCK") -eq 2 ]]'
check_release 'both releases disable Latest' '[[ $(grep -cF "          make_latest: false" <<< "$GH_SIGN_BLOCK") -eq 2 ]]'
check_release 'sign does not push Homebrew' '! grep -qF "Push Homebrew formula" <<< "$GH_SIGN_BLOCK"'
asset_needs='needs: [smoke-artifacts, smoke-cargo, smoke-install-unix, smoke-install-windows]'
stable_condition="!contains(github.ref_name, '-')"
for job in publish-homebrew submit-winget; do
  job_text="$(job_block "$job")"
  check_release "$job requires four asset smokes" 'grep -qF "$asset_needs" <<< "$job_text"'
  check_release "$job excludes prerelease tags" 'grep -qF "$stable_condition" <<< "$job_text"'
  credential_block="$(sed -n '/is missing/,+1p' <<< "$job_text")"
  check_release "$job fails on missing credential" 'grep -qF "is missing" <<< "$credential_block" && grep -qF "exit 1" <<< "$credential_block"'
done
job_text="$(job_block smoke-homebrew)"
check_release 'Homebrew smoke requires publication' 'grep -qF "needs: publish-homebrew" <<< "$job_text"'
job_text="$(job_block promote)"
check_release 'promote requires all seven jobs' 'grep -qF "needs: [smoke-artifacts, smoke-cargo, smoke-install-unix, smoke-install-windows, publish-homebrew, smoke-homebrew, submit-winget]" <<< "$job_text"'
check_release 'promote excludes prerelease tags' 'grep -qF "$stable_condition" <<< "$job_text"'
check_release 'only signing and promotion have contents write' '[[ $(grep -cE "^      contents: write$" "$GH_RELEASE_YML") -eq 2 ]] && grep -qF "contents: write" <<< "$job_text" && grep -qF "contents: write" <<< "$GH_SIGN_BLOCK"'
check_release 'promote edits release and checks Latest' 'grep -qF "gh release edit" <<< "$job_text" && grep -qF "repos/monkey1wizard/ccync/releases/latest" <<< "$job_text" && grep -qF ".tag_name" <<< "$job_text"'
check_release 'promote does not build or push' '! grep -Eq "cargo build|git tag|git push" <<< "$job_text"'

echo "test-release-notes: $PASS_COUNT probe(s) passed, $FAILURES failed."
[[ $FAILURES -eq 0 ]]
