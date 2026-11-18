#!/usr/bin/env bash
set -euo pipefail
REMOTE=github BRANCH=main EXECUTE=0 TAG= KEEP_SNAPSHOT= GATE=0 CHECKOUT_SHA= RECEIPT=
BACKUP=0 VERIFIED_SOURCE= VERIFIED_TAG_OBJECT= WINDOWS_RECEIPT= LINUX_RECEIPT= VERIFY_RECEIPTS=0
VERIFY_SIGNED_RECEIPTS=0 TRUST_LIST= WINDOWS_SIGNATURE= LINUX_SIGNATURE=
usage() { cat <<'EOF'
Usage: packaging/publish-public.sh [--remote NAME] [--branch main] [--tag TAG]
       packaging/publish-public.sh --execute [--tag TAG]
       packaging/publish-public.sh --gate [--checkout-sha SHA] [--receipt FILE]
Without --execute, build and inspect a temporary snapshot without pushing.
--keep-snapshot DIR retains that dry-run snapshot in a new or empty directory.
EOF
}
while (($#)); do
  case "$1" in
    --execute) EXECUTE=1 ;;
    --gate) GATE=1 ;;
    --backup-controller) BACKUP=1 ;;
    --verify-receipts) VERIFY_RECEIPTS=1 ;;
    --verify-signed-receipts) VERIFY_SIGNED_RECEIPTS=1; VERIFY_RECEIPTS=1 ;;
    --remote|--branch|--tag|--keep-snapshot|--checkout-sha|--receipt|--verified-source|--verified-tag-object|--windows-receipt|--linux-receipt|--trust-list|--windows-signature|--linux-signature)
      (($# >= 2)) || { echo "publish-public: $1 needs a value" >&2; exit 2; }
      case "$1" in --remote) REMOTE=$2 ;; --branch) BRANCH=$2 ;; --tag) TAG=$2 ;; --keep-snapshot) KEEP_SNAPSHOT=$2 ;; --checkout-sha) CHECKOUT_SHA=$2 ;; --receipt) RECEIPT=$2 ;; esac
      case "$1" in --verified-source) VERIFIED_SOURCE=$2 ;; --verified-tag-object) VERIFIED_TAG_OBJECT=$2 ;; --windows-receipt) WINDOWS_RECEIPT=$2 ;; --linux-receipt) LINUX_RECEIPT=$2 ;; esac
      case "$1" in --trust-list) TRUST_LIST=$2 ;; --windows-signature) WINDOWS_SIGNATURE=$2 ;; --linux-signature) LINUX_SIGNATURE=$2 ;; esac
      shift ;;
    -h|--help) usage; exit 0 ;;
    --force-tag|--include-project-md) echo "publish-public: removed option '$1'" >&2; exit 2 ;;
    *) echo "publish-public: unknown argument '$1'" >&2; exit 2 ;;
  esac
  shift
done
if [[ "$BACKUP" -eq 1 ]]; then
  [[ "${GITHUB_ACTIONS:-}" == true && "$EXECUTE" -eq 1 && -n "$VERIFIED_SOURCE" && -n "$VERIFIED_TAG_OBJECT" && -n "$WINDOWS_RECEIPT" && -n "$LINUX_RECEIPT" ]] || { echo 'publish-public: invalid backup controller arguments' >&2; exit 1; }
elif [[ -n "$VERIFIED_SOURCE$VERIFIED_TAG_OBJECT" ]]; then
  echo 'publish-public: verified values require backup controller' >&2; exit 1
fi
if [[ "$VERIFY_RECEIPTS" -eq 1 && ( "$EXECUTE" -eq 1 || "$GATE" -eq 1 || -z "$WINDOWS_RECEIPT" || -z "$LINUX_RECEIPT" ) ]]; then
  echo 'publish-public: invalid receipt verification arguments' >&2; exit 1
fi
if [[ "$VERIFY_SIGNED_RECEIPTS" -eq 1 ]]; then
  [[ -n "$TRUST_LIST" && -n "$WINDOWS_SIGNATURE" && -n "$LINUX_SIGNATURE" ]] || { echo 'publish-public: missing signed receipt arguments' >&2; exit 1; }
  for platform in windows linux; do
    if [[ "$platform" == windows ]]; then receipt_path=$WINDOWS_RECEIPT; signature=$WINDOWS_SIGNATURE; else receipt_path=$LINUX_RECEIPT; signature=$LINUX_SIGNATURE; fi
    [[ -s "$receipt_path" && -s "$signature" && -s "$TRUST_LIST" ]] || { echo "publish-public: missing $platform receipt, signature or trust list" >&2; exit 1; }
    principals=$(ssh-keygen -Y find-principals -f "$TRUST_LIST" -s "$signature") || { echo "publish-public: untrusted $platform signature" >&2; exit 1; }
    verified=0
    while IFS= read -r principal; do
      if ssh-keygen -Y verify -f "$TRUST_LIST" -I "$principal" -n ccync-release-receipt -s "$signature" < "$receipt_path"; then verified=1; break; fi
    done <<< "$principals"
    [[ "$verified" == 1 ]] || { echo "publish-public: invalid $platform signature" >&2; exit 1; }
  done
elif [[ -n "$TRUST_LIST$WINDOWS_SIGNATURE$LINUX_SIGNATURE" ]]; then
  echo 'publish-public: signature arguments require signed receipt verification' >&2; exit 1
fi
if [[ "$GATE" -eq 1 && ( "$EXECUTE" -eq 1 || -n "$TAG" || -n "$KEEP_SNAPSHOT" ) ]] || [[ "$GATE" -eq 0 && ( -n "$CHECKOUT_SHA" || -n "$RECEIPT" ) ]]; then
  echo 'publish-public: incompatible gate arguments' >&2; exit 2
fi
[[ "$BRANCH" == main ]] || { echo 'publish-public: only the main branch can be published' >&2; exit 1; }
if [[ -n "$KEEP_SNAPSHOT" && ( "$EXECUTE" -eq 1 || -L "$KEEP_SNAPSHOT" || ( -e "$KEEP_SNAPSHOT" && ( ! -d "$KEEP_SNAPSHOT" || -n "$(find "$KEEP_SNAPSHOT" -mindepth 1 -print -quit 2>/dev/null)" ) ) ) ]]; then
  echo 'publish-public: --keep-snapshot requires dry-run and a new or empty directory' >&2; exit 1
fi
ROOT=$(git rev-parse --show-toplevel); cd "$ROOT"
if [[ "$GATE" -eq 1 || "$EXECUTE" -eq 1 || "$VERIFY_RECEIPTS" -eq 1 ]]; then
  STATUS=$(git -c status.showUntrackedFiles=all status --porcelain --untracked-files=all) || { echo 'publish-public: git status failed' >&2; exit 1; }
  [[ -z "$STATUS" ]] || { printf 'publish-public: working tree is dirty:\n%s\n' "$STATUS" >&2; exit 1; }
else
  if ! git diff --quiet || ! git diff --cached --quiet; then echo 'publish-public: working tree is dirty — commit or stash first.' >&2; exit 1; fi
fi

if [[ "$GATE" -eq 0 && "$VERIFY_RECEIPTS" -eq 0 ]]; then
EXPECTED_HTTPS=https://github.com/monkey1wizard/ccync.git
EXPECTED_SSH=git@github.com:monkey1wizard/ccync.git
valid_url() {
  case "$1" in "$EXPECTED_HTTPS"|https://github.com/monkey1wizard/ccync|"$EXPECTED_SSH"|ssh://git@github.com/monkey1wizard/ccync.git|ssh://git@github.com/monkey1wizard/ccync) return 0 ;; *) return 1 ;; esac
}
CONFIGURED_URLS=$(git config --get-all "remote.$REMOTE.url" || true)
CONFIGURED_PUSHURLS=$(git config --get-all "remote.$REMOTE.pushurl" || true)
[[ -n "$CONFIGURED_URLS" ]] || { echo "publish-public: public remote '$REMOTE' not found." >&2; exit 1; }
while IFS= read -r url; do
  [[ -n "$url" ]] || continue
  valid_url "$url" || { echo "publish-public: configured fetch URL rejected for '$REMOTE'." >&2; exit 1; }
done <<< "$CONFIGURED_URLS"
if [[ -n "$CONFIGURED_PUSHURLS" ]]; then
  while IFS= read -r url; do
    [[ -n "$url" ]] || continue
    valid_url "$url" || { echo "publish-public: configured push URL rejected for '$REMOTE'." >&2; exit 1; }
  done <<< "$CONFIGURED_PUSHURLS"
fi
FETCH_URL=$(git remote get-url "$REMOTE" 2>/dev/null) || { echo 'publish-public: effective fetch URL lookup failed.' >&2; exit 1; }
PUSH_URL=$(git remote get-url --push "$REMOTE" 2>/dev/null) || { echo 'publish-public: effective push URL lookup failed.' >&2; exit 1; }
if ! valid_url "$FETCH_URL" || ! valid_url "$PUSH_URL" || [[ "$FETCH_URL" != "$PUSH_URL" ]]; then
  echo 'publish-public: effective fetch and push destinations must both be the canonical public repository.' >&2; exit 1
fi
if [[ -n "$TAG" ]]; then
  [[ "$EXECUTE" -eq 1 ]] || { echo 'publish-public: --tag requires --execute.' >&2; exit 1; }
  [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-rc[0-9]*)?$ ]] || { echo "publish-public: invalid release tag '$TAG'." >&2; exit 1; }
  TAG_VERSION=${TAG#v}; TAG_VERSION=${TAG_VERSION%%-rc*}
  CRATE_VERSION=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' crates/ccync-cli/Cargo.toml | head -n 1)
  [[ "$TAG_VERSION" == "$CRATE_VERSION" ]] || { echo 'publish-public: tag and CLI crate version differ.' >&2; exit 1; }
  bash packaging/release-body.sh "$TAG" --validate-only || { echo 'publish-public: release notes validation failed.' >&2; exit 1; }
  if [[ "$BACKUP" -eq 0 ]]; then
  PRIVATE_URLS=$(git config --get-all remote.origin.url) || { echo 'publish-public: private origin missing.' >&2; exit 1; }
  while IFS= read -r url; do
    case "$url" in https://github.com/monkey1wizard/ccync-private|https://github.com/monkey1wizard/ccync-private.git|git@github.com:monkey1wizard/ccync-private.git|ssh://git@github.com/monkey1wizard/ccync-private|ssh://git@github.com/monkey1wizard/ccync-private.git) ;; *) echo 'publish-public: private origin URL rejected.' >&2; exit 1 ;; esac
  done <<< "$PRIVATE_URLS"
  PRIVATE_URL=$(git remote get-url origin) || { echo 'publish-public: private origin lookup failed.' >&2; exit 1; }
  case "$PRIVATE_URL" in https://github.com/monkey1wizard/ccync-private|https://github.com/monkey1wizard/ccync-private.git|git@github.com:monkey1wizard/ccync-private.git|ssh://git@github.com/monkey1wizard/ccync-private|ssh://git@github.com/monkey1wizard/ccync-private.git) ;; *) echo 'publish-public: effective private origin URL rejected.' >&2; exit 1 ;; esac
  fi
  TAG_OBJECT=$(git rev-parse --verify "refs/tags/$TAG") || { echo 'publish-public: private tag missing.' >&2; exit 1; }
  [[ $(git cat-file -t "$TAG_OBJECT") == tag ]] || { echo 'publish-public: private tag must be annotated and SSH-signed.' >&2; exit 1; }
  TAG_HEADER=$(git cat-file -p "$TAG_OBJECT" | sed -n '1,2p')
  [[ "$TAG_HEADER" == "$(printf 'object %s\ntype commit' "$(git rev-parse HEAD)")" ]] || { echo 'publish-public: private tag must point directly at HEAD.' >&2; exit 1; }
  if [[ "$BACKUP" -eq 1 ]]; then
    [[ $(git rev-parse HEAD) == "$VERIFIED_SOURCE" && "$TAG_OBJECT" == "$VERIFIED_TAG_OBJECT" ]] || { echo 'publish-public: verified source or tag mismatch.' >&2; exit 1; }
  else
  REMOTE_TAG=$(git ls-remote --refs origin "refs/tags/$TAG") || { echo 'publish-public: private tag lookup failed.' >&2; exit 1; }
  [[ "$REMOTE_TAG" == "$TAG_OBJECT"$'\t'"refs/tags/$TAG" ]] || { echo 'publish-public: private tag object differs from origin.' >&2; exit 1; }
  git fetch --no-tags origin refs/heads/main || { echo 'publish-public: private main fetch failed.' >&2; exit 1; }
  TRUST_FILE=$(mktemp)
  if ! git show FETCH_HEAD:.github/release-trust/allowed_signers > "$TRUST_FILE"; then
    rm -f -- "$TRUST_FILE"; echo 'publish-public: current trust list unavailable.' >&2; exit 1
  fi
  if ! git cat-file -p "$TAG_OBJECT" | grep -q '^-----BEGIN SSH SIGNATURE-----$' || ! git -c gpg.format=ssh -c "gpg.ssh.allowedSignersFile=$TRUST_FILE" verify-tag "$TAG"; then
    rm -f -- "$TRUST_FILE"; echo 'publish-public: private tag SSH signature is invalid or untrusted.' >&2; exit 1
  fi
  rm -f -- "$TRUST_FILE"
  fi
  PUBLIC_REFS=$(git ls-remote --refs "$REMOTE" refs/heads/main "refs/tags/$TAG") || { echo 'publish-public: public refs lookup failed.' >&2; exit 1; }
  PUBLIC_MAIN=
  while IFS=$'\t' read -r oid ref; do
    [[ -n "$ref" ]] || continue
    case "$ref" in refs/heads/main) PUBLIC_MAIN=$oid ;; "refs/tags/$TAG") echo "publish-public: tag '$TAG' already exists at destination." >&2; exit 1 ;; *) echo 'publish-public: unexpected public ref.' >&2; exit 1 ;; esac
  done <<< "$PUBLIC_REFS"
fi
if [[ "$EXECUTE" -eq 1 && -z "$TAG" ]]; then
  echo 'publish-public: --execute requires --tag.' >&2; exit 1
fi
fi

SNAP=$(mktemp -d)
cleanup() {
  if [[ -n "$SNAP" && -d "$SNAP" ]]; then
    rm -rf -- "$SNAP"
  fi
}
trap cleanup EXIT
INDEX="$SNAP.index"
trap 'rm -f -- "$INDEX" "${SNAP:+$SNAP.blob}"; cleanup' EXIT
export GIT_INDEX_FILE="$INDEX"
git read-tree HEAD^{tree}
while IFS= read -r -d '' path; do
  case "$path" in
    docs/plans|docs/plans/*|.dev|.dev/*|CLAUDE.md|AGENTS.md|GEMINI.md|.agents|.agents/*|.claude|.claude/*|.gitlab-ci.yml|.github/copilot-instructions.md|.github/instructions|.github/instructions/*|.github/workflows/publish-public.yml|.github/workflows/release-rehearsal.yml|.github/release-trust|.github/release-trust/*|*.private.md|*.local.*|config.local.env|*/config.local.env|mcp.local.json|*/mcp.local.json)
      git update-index --force-remove -- "$path" ;;
  esac
done < <(git ls-files -z)
TREE=$(git write-tree)
unset GIT_INDEX_FILE
select_python() {
  local candidates=(python3 python) candidate found
  local -a words
  local probe='import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)'
  case $(uname -s) in MINGW*|MSYS*|CYGWIN*) candidates+=('py -3') ;; esac
  for candidate in "${candidates[@]}"; do
    read -r -a words <<< "$candidate"
    if "${words[@]}" -I -c "$probe" </dev/null >/dev/null 2>&1; then
      PYTHON_CMD=("${words[@]}"); return 0
    fi
  done
  if uv --version </dev/null >/dev/null 2>&1 && found=$(uv python find 3 </dev/null 2>/dev/null) && [[ -n "$found" ]] \
    && "$found" -I -c "$probe" </dev/null >/dev/null 2>&1; then
    PYTHON_CMD=("$found"); return 0
  fi
  candidates+=('uv python find')
  echo "publish-public: Python 3 was not found; tried: ${candidates[*]}. Install Python 3.8 or later on PATH." >&2
  return 1
}
if [[ "$EXECUTE" -eq 1 || "$VERIFY_RECEIPTS" -eq 1 ]]; then
  WINDOWS_RECEIPT=${WINDOWS_RECEIPT:-"$ROOT/.dev/pipeline/release-gate/$TREE/windows.receipt"}
  LINUX_RECEIPT=${LINUX_RECEIPT:-"$ROOT/.dev/pipeline/release-gate/$TREE/linux.receipt"}
  PYTHON_CMD=()
  select_python || exit 1
  for platform in windows linux; do
    if [[ "$platform" == windows ]]; then receipt_path=$WINDOWS_RECEIPT; else receipt_path=$LINUX_RECEIPT; fi
    case $(uname -s) in MINGW*|MSYS*|CYGWIN*) receipt_path=$(cygpath -w "$receipt_path") ;; esac
    "${PYTHON_CMD[@]}" -I - "$receipt_path" "$platform" "$(git rev-parse HEAD)" "$TREE" <<'PY' || { echo "publish-public: invalid $platform receipt" >&2; exit 1; }
import re
import sys
try:
    lines = open(sys.argv[1], encoding='utf-8').read().splitlines()
    assert lines.pop(0) == 'ccync-release-gate-receipt-v1'
    pairs = [line.split('=', 1) for line in lines]
    assert all(len(pair) == 2 for pair in pairs)
    data = dict(pairs)
    fields = {'mode', 'source', 'tree', 'os', 'rustc', 'uv', 's1', 's2', 'step1', 'step2', 'step3', 'step4', 'step5', 'result'}
    assert len(pairs) == len(data) and set(data) == fields
    assert all(data[k] for k in fields)
    assert re.fullmatch(r'rustc [^\s]+(?: .*)?', data['rustc']) and re.fullmatch(r'uv [^\s]+(?: .*)?', data['uv'])
    assert (data['mode'], data['source'], data['tree'], data['os'], data['result']) == ('source', sys.argv[3], sys.argv[4], sys.argv[2], 'pass')
    expected = {'s1': '0', 's2': '0', 'step1': '0', 'step2': '0', 'step3': '0', 'step4': '0', 'step5': '0'} if sys.argv[2] == 'linux' else {'s1': 'NotRun', 's2': 'NotRun', 'step1': '0', 'step2': '0', 'step3': '0', 'step4': 'NotRun', 'step5': 'NotRun'}
    assert all(data[k] == v for k, v in expected.items())
except (OSError, UnicodeError, IndexError, ValueError, AssertionError):
    sys.exit(1)
PY
  done
  [[ "$VERIFY_RECEIPTS" -eq 0 ]] || exit 0
fi
if [[ "$GATE" -eq 1 ]]; then
  SOURCE_SHA=$(git rev-parse HEAD)
  MODE=source
  if [[ -n "$CHECKOUT_SHA" ]]; then
    MODE=public
    [[ "$SOURCE_SHA" == "$CHECKOUT_SHA" ]] || { echo 'publish-public: checkout SHA does not match HEAD' >&2; exit 1; }
    TREE=$(git rev-parse 'HEAD^{tree}')
  fi
  case $(uname -s) in
    Linux*) PLATFORM=linux ;;
    MINGW*|MSYS*|CYGWIN*) PLATFORM=windows ;;
    *) echo 'publish-public: unsupported platform' >&2; exit 1 ;;
  esac
  RECEIPT=${RECEIPT:-"$ROOT/.dev/pipeline/release-gate/$TREE/$PLATFORM.receipt"}
  mkdir -p -- "$(dirname "$RECEIPT")"
  RECEIPT="$(cd "$(dirname "$RECEIPT")" && pwd -P)/$(basename "$RECEIPT")"
  if [[ -e "$RECEIPT" || -L "$RECEIPT" ]]; then
    [[ -f "$RECEIPT" && ! -L "$RECEIPT" ]] && IFS= read -r first_line < "$RECEIPT" && [[ "$first_line" == 'ccync-release-gate-receipt-v1' ]] || { echo 'publish-public: destination is not a receipt' >&2; exit 1; }
    rm -- "$RECEIPT"
  fi
  RUST_VERSION=$(rustc -V) || exit 1
  UV_VERSION=$(uv --version) || exit 1
  S1=NotRun S2=NotRun STEP1=NotRun STEP2=NotRun STEP3=NotRun STEP4=NotRun STEP5=NotRun RESULT=pass
  run_step() {
    local name=$1 status=0; shift
    if [[ "$RESULT" == pass ]]; then
      "$@" || status=$?
      printf -v "$name" '%s' "$status"
      [[ "$status" -eq 0 ]] || RESULT=fail
    fi
    return 0
  }
  write_receipt() {
    local tmp
    tmp=$(mktemp "${RECEIPT}.tmp.XXXXXXXX")
    printf 'ccync-release-gate-receipt-v1\nmode=%s\nsource=%s\ntree=%s\nos=%s\nrustc=%s\nuv=%s\ns1=%s\ns2=%s\nstep1=%s\nstep2=%s\nstep3=%s\nstep4=%s\nstep5=%s\nresult=%s\n' \
      "$MODE" "$SOURCE_SHA" "$TREE" "$PLATFORM" "$RUST_VERSION" "$UV_VERSION" "$S1" "$S2" "$STEP1" "$STEP2" "$STEP3" "$STEP4" "$STEP5" "$RESULT" > "$tmp"
    mv -f -- "$tmp" "$RECEIPT"
  }
  run_checks() {
    run_step STEP1 cargo test --workspace --locked --no-fail-fast
    run_step STEP2 cargo clippy --workspace --locked
    if [[ "$PLATFORM" == linux ]]; then
      run_step STEP3 bash packaging/test-release-notes.sh --public-snapshot
      run_step STEP4 bash packaging/test-release-artifacts.sh
      run_step STEP5 bash packaging/test-publish-public.sh
    else
      run_step STEP3 pwsh ./packaging/test-install.ps1
    fi
    write_receipt
    [[ "$RESULT" == pass ]]
  }
  if [[ "$PLATFORM" == linux && "$MODE" == source ]]; then
    run_step S1 bash packaging/test-release-notes.sh
    run_step S2 bash packaging/test-release-artifacts.sh
  fi
  if [[ "$RESULT" != pass || "$MODE" == public ]]; then
    run_checks; exit $?
  fi
fi
SOURCE_SHA=$(git rev-parse --short HEAD); STAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)
SNAP_SHA=$(printf 'Public snapshot from %s (%s)\n\nCurated export; planning and private controller stripped.\n' "$SOURCE_SHA" "$STAMP" | git -c user.name='ccync Publish' -c user.email=noreply@ccync.dev commit-tree "$TREE")
git -C "$SNAP" init -q -b main
printf '%s\n' "$SNAP_SHA" | git pack-objects --stdout --revs | git -C "$SNAP" unpack-objects -q
git -C "$SNAP" update-ref refs/heads/main "$SNAP_SHA"
git -C "$SNAP" -c core.autocrlf=false -c filter.ccync-config.smudge=cat reset -q --hard HEAD
LEAKS=0
PATTERN='BEGIN (RSA |OPENSSH |EC )?PRIVATE KEY|glpat-[A-Za-z0-9_-]{20}|gho_[A-Za-z0-9]{36}|xox[baprs]-[A-Za-z0-9-]+'
while IFS= read -r -d '' entry; do
  metadata=${entry%%$'\t'*}; path=${entry#*$'\t'}
  read -r mode type oid <<< "$metadata"
  [[ "$type" == blob ]] || { echo "publish-public: unsupported tree entry: $path" >&2; LEAKS=1; continue; }
  git -C "$SNAP" cat-file blob "$oid" > "$SNAP.blob"
  if grep -aEq "$PATTERN" "$SNAP.blob"; then
    echo "LEAK: heuristic scan found a possible secret token in $path; this scan is not exhaustive." >&2; LEAKS=1
  else
    status=$?
    [[ "$status" -eq 1 ]] || { echo 'publish-public: heuristic secret scan failed.' >&2; LEAKS=1; }
  fi
done < <(git -C "$SNAP" ls-tree -rz --full-tree HEAD)
rm -f -- "$SNAP.blob"
[[ "$LEAKS" -eq 0 ]] || { echo 'publish-public: ABORT — snapshot safety checks failed; nothing pushed.' >&2; exit 1; }

if [[ "$GATE" -eq 1 ]]; then
  SOURCE_SHA=$(git -C "$ROOT" rev-parse HEAD)
  cd "$SNAP"
  run_checks; exit $?
fi

FILE_COUNT=$(git -C "$SNAP" ls-tree -r --name-only HEAD | wc -l | tr -d ' ')
[[ -z "$TAG" ]] || git -C "$SNAP" tag "$TAG" HEAD
git -C "$SNAP" remote add "$REMOTE" "$PUSH_URL"
echo "publish-public: source=$SOURCE_SHA snapshot=$SNAP_SHA destination=$PUSH_URL branch=main files=$FILE_COUNT"
echo 'publish-public: heuristic scan found no matches for its configured token patterns; it does not detect every secret.'
if [[ "$EXECUTE" -ne 1 ]]; then
  if [[ -n "$KEEP_SNAPSHOT" ]]; then
    mkdir -p -- "$KEEP_SNAPSHOT"; cp -a "$SNAP/." "$KEEP_SNAPSHOT/"
    echo "publish-public: snapshot retained at $KEEP_SNAPSHOT"
  fi
  echo 'DRY RUN — nothing pushed.'; exit 0
fi
REFS=(refs/heads/main:refs/heads/main)
[[ -z "$TAG" ]] || REFS+=("refs/tags/$TAG:refs/tags/$TAG")
echo 'publish-public: pushing main and requested tag in one atomic transaction.'
git -C "$SNAP" push --atomic "--force-with-lease=refs/heads/main:$PUBLIC_MAIN" "$REMOTE" "${REFS[@]}"
echo "publish-public: published snapshot $SNAP_SHA to $REMOTE/main."
