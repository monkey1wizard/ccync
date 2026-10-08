#!/usr/bin/env bash
set -euo pipefail
REMOTE=github BRANCH=main EXECUTE=0 TAG= KEEP_SNAPSHOT=
usage() { cat <<'EOF'
Usage: packaging/publish-public.sh [--remote NAME] [--branch main] [--tag TAG]
       packaging/publish-public.sh --execute [--tag TAG]
Without --execute, build and inspect a temporary snapshot without pushing.
--keep-snapshot DIR retains that dry-run snapshot in a new or empty directory.
EOF
}
while (($#)); do
  case "$1" in
    --execute) EXECUTE=1 ;;
    --remote|--branch|--tag|--keep-snapshot)
      (($# >= 2)) || { echo "publish-public: $1 needs a value" >&2; exit 2; }
      case "$1" in --remote) REMOTE=$2 ;; --branch) BRANCH=$2 ;; --tag) TAG=$2 ;; --keep-snapshot) KEEP_SNAPSHOT=$2 ;; esac
      shift ;;
    -h|--help) usage; exit 0 ;;
    --force-tag|--include-project-md) echo "publish-public: removed option '$1'" >&2; exit 2 ;;
    *) echo "publish-public: unknown argument '$1'" >&2; exit 2 ;;
  esac
  shift
done
[[ "$BRANCH" == main ]] || { echo 'publish-public: only the main branch can be published' >&2; exit 1; }
if [[ -n "$KEEP_SNAPSHOT" && ( "$EXECUTE" -eq 1 || -L "$KEEP_SNAPSHOT" || ( -e "$KEEP_SNAPSHOT" && ( ! -d "$KEEP_SNAPSHOT" || -n "$(find "$KEEP_SNAPSHOT" -mindepth 1 -print -quit 2>/dev/null)" ) ) ) ]]; then
  echo 'publish-public: --keep-snapshot requires dry-run and a new or empty directory' >&2; exit 1
fi
ROOT=$(git rev-parse --show-toplevel); cd "$ROOT"
if ! git diff --quiet || ! git diff --cached --quiet; then echo 'publish-public: working tree is dirty — commit or stash first.' >&2; exit 1; fi

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
  if git ls-remote --exit-code --tags "$REMOTE" "refs/tags/$TAG" "refs/tags/$TAG^{}" >/dev/null 2>&1; then
    echo "publish-public: tag '$TAG' already exists at destination." >&2; exit 1
  else
    status=$?
    [[ "$status" -eq 2 ]] || { echo "publish-public: tag-state lookup failed for '$TAG' (git ls-remote exit $status)." >&2; exit 1; }
  fi
fi
if [[ "$EXECUTE" -eq 1 && -z "$TAG" ]]; then
  echo 'publish-public: --execute requires --tag.' >&2; exit 1
fi

EXCLUDE=(docs/plans .dev CLAUDE.md AGENTS.md GEMINI.md .agents .claude .gitlab-ci.yml .github/copilot-instructions.md .github/instructions .github/workflows/publish-public.yml)
SNAP=$(mktemp -d)
cleanup() {
  if [[ -n "$SNAP" && -d "$SNAP" ]]; then
    rm -rf -- "$SNAP"
  fi
}
trap cleanup EXIT
git -c filter.ccync-config.smudge=cat -c filter.ccync-config.clean=cat archive --format=tar HEAD | tar -x -C "$SNAP"
for path in "${EXCLUDE[@]}"; do rm -rf -- "$SNAP/$path"; done
find "$SNAP" -type f \( -name '*.private.md' -o -name '*.local.*' -o -name config.local.env -o -name mcp.local.json \) -delete
LEAKS=0
for path in docs/plans .dev/state.md .dev/plans CLAUDE.md AGENTS.md GEMINI.md .agents .claude .gitlab-ci.yml .github/copilot-instructions.md .github/instructions .github/workflows/publish-public.yml; do
  if [[ -e "$SNAP/$path" ]]; then echo "LEAK: excluded path present: $path" >&2; LEAKS=1; fi
done
if find "$SNAP" -type f -name '*.private.md' -print -quit | grep -q .; then echo 'LEAK: private note file remains.' >&2; LEAKS=1; fi
PATTERN='BEGIN (RSA |OPENSSH |EC )?PRIVATE KEY|glpat-[A-Za-z0-9_-]{20}|gho_[A-Za-z0-9]{36}|xox[baprs]-[A-Za-z0-9-]+'
set +e
SCAN_RESULTS=$(grep -rIlE "$PATTERN" "$SNAP" 2>&1)
SCAN_STATUS=$?
set -e
if [[ "$SCAN_STATUS" -eq 0 ]]; then
  echo 'LEAK: heuristic scan found a possible secret token; this scan is not exhaustive.' >&2
  printf '%s\n' "$SCAN_RESULTS" >&2; LEAKS=1
elif [[ "$SCAN_STATUS" -ne 1 ]]; then
  echo 'publish-public: heuristic secret scan failed; snapshot was not published.' >&2
  printf '%s\n' "$SCAN_RESULTS" >&2; LEAKS=1
fi
[[ "$LEAKS" -eq 0 ]] || { echo 'publish-public: ABORT — snapshot safety checks failed; nothing pushed.' >&2; exit 1; }

FILE_COUNT=$(find "$SNAP" -type f | wc -l | tr -d ' ')
git -C "$SNAP" init -q -b main
git -C "$SNAP" config filter.ccync-config.clean cat
git -C "$SNAP" config filter.ccync-config.smudge cat
git -C "$SNAP" -c user.name='ccync Publish' -c user.email=noreply@ccync.dev add -A
SOURCE_SHA=$(git rev-parse --short HEAD); STAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)
git -C "$SNAP" -c user.name='ccync Publish' -c user.email=noreply@ccync.dev commit -q -m "Public snapshot from $SOURCE_SHA ($STAMP)" -m 'Curated export; planning and private controller stripped.'
SNAP_SHA=$(git -C "$SNAP" rev-parse HEAD)
[[ -z "$TAG" ]] || git -C "$SNAP" tag "$TAG" HEAD
git -C "$SNAP" remote add "$REMOTE" "$PUSH_URL"
echo "publish-public: source=$SOURCE_SHA snapshot=$SNAP_SHA destination=$PUSH_URL branch=main files=$FILE_COUNT"
echo 'publish-public: heuristic scan found no matches for its configured token patterns; it does not detect every secret.'
if [[ "$EXECUTE" -ne 1 ]]; then
  if [[ -n "$KEEP_SNAPSHOT" ]]; then
    mkdir -p -- "$KEEP_SNAPSHOT"; cp -a "$SNAP/." "$KEEP_SNAPSHOT/"; SNAP=
    echo "publish-public: snapshot retained at $KEEP_SNAPSHOT"
  fi
  echo 'DRY RUN — nothing pushed.'; exit 0
fi
REFS=(+refs/heads/main:refs/heads/main)
[[ -z "$TAG" ]] || REFS+=("refs/tags/$TAG:refs/tags/$TAG")
echo 'publish-public: pushing main and requested tag in one atomic transaction.'
git -C "$SNAP" push --atomic "$REMOTE" "${REFS[@]}"
echo "publish-public: published snapshot $SNAP_SHA to $REMOTE/main."
