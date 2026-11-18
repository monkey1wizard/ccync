#!/usr/bin/env bash
set -euo pipefail
trap 'status=$?; echo "test-publish-public: failed at line $LINENO (exit $status)" >&2' ERR

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
PUBLISH="$SCRIPT_DIR/publish-public.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
REAL_GIT=$(command -v git)
PY_PROBE='import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)'
resolve_test_python() {
  local candidate found
  local -a words
  for candidate in python3 python 'py -3'; do
    read -r -a words <<< "$candidate"
    if "${words[@]}" -I -c "$PY_PROBE" </dev/null >/dev/null 2>&1; then words[0]=$(command -v "${words[0]}"); TEST_PYTHON=("${words[@]}"); return 0; fi
  done
  if command -v uv >/dev/null 2>&1 && found=$(uv python find 2>/dev/null) && [[ -n "$found" ]]; then
    case $(uname -s) in MINGW*|MSYS*|CYGWIN*) found=$(cygpath -u "$found") ;; esac
    if "$found" -I -c "$PY_PROBE" </dev/null >/dev/null 2>&1; then TEST_PYTHON=("$found"); return 0; fi
  fi
  echo 'test-publish-public: no working Python 3 interpreter for the receipt probes' >&2
  return 1
}
TEST_PYTHON=()
resolve_test_python
mkdir -p "$TMP/pybin" "$TMP/stub-python" "$TMP/stub-python3" "$TMP/no-python" "$TMP/uv-python"
for name in python3 python; do
  printf '#!/usr/bin/env bash\nexec %s "$@"\n' "$(printf '%q ' "${TEST_PYTHON[@]}")" > "$TMP/pybin/$name"
  chmod +x "$TMP/pybin/$name"
done
write_failing_stub() {
  printf '#!/usr/bin/env bash\necho "Python was not found; run without arguments to install from the Microsoft Store." >&2\nexit 9009\n' > "$1"
  chmod +x "$1"
}
write_failing_stub "$TMP/stub-python/python"
write_failing_stub "$TMP/stub-python3/python3"
for name in python3 python py uv; do write_failing_stub "$TMP/no-python/$name"; done
for name in python3 python py; do write_failing_stub "$TMP/uv-python/$name"; done
if [[ ${#TEST_PYTHON[@]} -eq 1 ]]; then UV_FOUND_PYTHON=${TEST_PYTHON[0]}; else UV_FOUND_PYTHON="$TMP/pybin/python3"; fi
printf '#!/usr/bin/env bash\ncase "$*" in --version) echo "uv fixture" ;; "python find 3") printf "%%s\\n" %q ;; *) exit 2 ;; esac\n' "$UV_FOUND_PYTHON" > "$TMP/uv-python/uv"
chmod +x "$TMP/uv-python/uv"
mkdir -p "$TMP/home"
export HOME="$TMP/home" XDG_CONFIG_HOME="$TMP/home/.config"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
mkdir -p "$TMP/bin"
cat >"$TMP/bin/git" <<'WRAPPER'
#!/usr/bin/env bash
args=("$@")
command_name=
for arg in "${args[@]}"; do
  case "$arg" in push|ls-remote|get-url|fetch) command_name=$arg; break ;; esac
done
if [[ "$command_name" == push || "$command_name" == ls-remote || "$command_name" == fetch ]]; then
  for ((i=0; i<${#args[@]}; i++)); do
    if [[ "${args[$i]}" == github ]]; then args[$i]="$PUBLISH_TEST_RECEIVER"; fi
    if [[ "${args[$i]}" == origin ]]; then args[$i]="$PUBLISH_TEST_PRIVATE"; fi
  done
fi
if [[ "$command_name" == push ]]; then
  [[ " ${args[*]} " == *" --atomic "* && " ${args[*]} " == *" --force-with-lease=refs/heads/main:"* && " ${args[*]} " == *" refs/heads/main:refs/heads/main "* && " ${args[*]} " == *" refs/tags/v1.2.3:refs/tags/v1.2.3 "* ]] || {
    echo 'push did not contain the required atomic main and non-force tag refspecs' >&2; exit 98;
  }
fi
if [[ "$command_name" == push && "${PUBLISH_TEST_TAG_RACE:-0}" == 1 ]]; then
  "$PUBLISH_TEST_REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" update-ref refs/tags/v1.2.3 "$PUBLISH_TEST_RACE_WINNER"
  unset PUBLISH_TEST_TAG_RACE
  exit 1
fi
if [[ "$command_name" == push && "${PUBLISH_TEST_MAIN_RACE:-0}" == 1 ]]; then
  "$PUBLISH_TEST_REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" update-ref refs/heads/main "$PUBLISH_TEST_RACE_WINNER"
fi
if [[ "$command_name" == ls-remote && "${PUBLISH_TEST_EXISTING_TAG:-0}" == 1 && " ${args[*]} " == *" $PUBLISH_TEST_RECEIVER "* ]]; then printf '%s\trefs/tags/v1.2.3\n' "${PUBLISH_TEST_RACE_WINNER:-0000000000000000000000000000000000000000}"; exit 0; fi
if [[ "$command_name" == push && "${PUBLISH_TEST_ATOMIC_REJECT:-0}" == 1 ]]; then
  [[ " ${args[*]} " == *" --atomic "* ]] || { echo 'push omitted --atomic' >&2; exit 98; }
  exit 1
fi
if [[ "$command_name" == ls-remote && "${PUBLISH_TEST_LOOKUP_FAIL:-0}" == 1 ]]; then exit 128; fi
if [[ "${PUBLISH_TEST_STATUS_FAIL:-0}" == 1 ]]; then
  for arg in "${args[@]}"; do [[ "$arg" == status ]] && exit 128; done
fi
if [[ "$command_name" == get-url && "${PUBLISH_TEST_EFFECTIVE_LOOKUP_FAIL:-0}" == 1 ]]; then exit 128; fi
exec "$PUBLISH_TEST_REAL_GIT" "${args[@]}"
WRAPPER
chmod +x "$TMP/bin/git"
export PUBLISH_TEST_REAL_GIT=$REAL_GIT PATH="$TMP/bin:$TMP/pybin:$PATH"

init_repo() {
  local name=$1
  mkdir -p "$TMP/$name/source" "$TMP/$name/receiver"
  "$REAL_GIT" init -q -b main "$TMP/$name/source"
  "$REAL_GIT" -C "$TMP/$name/source" config user.name Fixture
  "$REAL_GIT" -C "$TMP/$name/source" config user.email fixture@example.invalid
  "$REAL_GIT" -C "$TMP/$name/source" config commit.gpgsign false
  "$REAL_GIT" -C "$TMP/$name/source" config core.hooksPath /dev/null
  "$REAL_GIT" -C "$TMP/$name/source" config filter.ccync-config.clean 'sed s/personal/canonical/'
  "$REAL_GIT" -C "$TMP/$name/source" config filter.ccync-config.smudge 'sed s/canonical/personal/'
  mkdir -p "$TMP/$name/source/.github/workflows" "$TMP/$name/source/.github/release-trust" "$TMP/$name/source/docs/plans" "$TMP/$name/source/.dev/plans" "$TMP/$name/source/crates/ccync-cli" "$TMP/$name/source/packaging"
  ssh-keygen -q -t ed25519 -N '' -f "$TMP/$name/key"
  printf 'fixture ' >"$TMP/$name/source/.github/release-trust/allowed_signers"
  cat "$TMP/$name/key.pub" >>"$TMP/$name/source/.github/release-trust/allowed_signers"
  printf 'version = "1.2.3"\n' >"$TMP/$name/source/crates/ccync-cli/Cargo.toml"
  printf '#!/bin/sh\nexit 0\n' >"$TMP/$name/source/packaging/release-body.sh"
  printf 'private controller\n' >"$TMP/$name/source/.gitlab-ci.yml"
  printf 'canonical\n' >"$TMP/$name/source/config.toml"
  printf 'config.toml filter=ccync-config\n' >"$TMP/$name/source/.gitattributes"
  printf 'private\n' >"$TMP/$name/source/.dev/plans/secret.md"
  printf 'private\n' >"$TMP/$name/source/docs/plans/secret.md"
  printf '#!/bin/sh\n' >"$TMP/$name/source/executable.sh"
  chmod +x "$TMP/$name/source/executable.sh"
  printf 'controller\n' >"$TMP/$name/source/.github/workflows/publish-public.yml"
  printf 'release\n' >"$TMP/$name/source/.github/workflows/release.yml"
  printf 'build\n' >"$TMP/$name/source/.github/workflows/release-build.yml"
  printf 'rehearsal\n' >"$TMP/$name/source/.github/workflows/release-rehearsal.yml"
  "$REAL_GIT" -C "$TMP/$name/source" add -A
  "$REAL_GIT" -C "$TMP/$name/source" update-index --chmod=+x executable.sh
  "$REAL_GIT" -C "$TMP/$name/source" commit -qm source
  "$REAL_GIT" -C "$TMP/$name/source" -c gpg.format=ssh -c user.signingkey="$TMP/$name/key" tag -sm fixture v1.2.3
  "$REAL_GIT" init -q --bare -b main "$TMP/$name/private"
  "$REAL_GIT" -C "$TMP/$name/source" push -q "$TMP/$name/private" main refs/tags/v1.2.3
  "$REAL_GIT" init -q --bare -b main "$TMP/$name/receiver"
  "$REAL_GIT" -C "$TMP/$name/source" remote add github https://github.com/monkey1wizard/ccync.git
  "$REAL_GIT" -C "$TMP/$name/source" remote add origin https://github.com/monkey1wizard/ccync-private.git
  PUBLISH_TEST_RECEIVER="$TMP/$name/receiver"
  PUBLISH_TEST_PRIVATE="$TMP/$name/private"
  export PUBLISH_TEST_RECEIVER PUBLISH_TEST_PRIVATE
  make_receipts "$name"
}
make_receipts() {
  local name=$1 source="$TMP/$1/source" tree sha platform s1 s2 step4 step5
  GIT_INDEX_FILE="$TMP/$name/index" "$REAL_GIT" -C "$source" read-tree HEAD
  GIT_INDEX_FILE="$TMP/$name/index" "$REAL_GIT" -C "$source" update-index --force-remove -- .gitlab-ci.yml .github/workflows/publish-public.yml .github/workflows/release-rehearsal.yml .github/release-trust/allowed_signers docs/plans/secret.md .dev/plans/secret.md
  tree=$(GIT_INDEX_FILE="$TMP/$name/index" "$REAL_GIT" -C "$source" write-tree)
  sha=$("$REAL_GIT" -C "$source" rev-parse HEAD)
  printf '\n.dev/pipeline/\n' >> "$source/.git/info/exclude"
  mkdir -p "$source/.dev/pipeline/release-gate/$tree"
  for platform in windows linux; do
    s1=0 s2=0 step4=0 step5=0
    if [[ "$platform" == windows ]]; then s1=NotRun s2=NotRun step4=NotRun step5=NotRun; fi
    printf 'ccync-release-gate-receipt-v1\nmode=source\nsource=%s\ntree=%s\nos=%s\nrustc=rustc fixture\nuv=uv fixture\ns1=%s\ns2=%s\nstep1=0\nstep2=0\nstep3=0\nstep4=%s\nstep5=%s\nresult=pass\n' "$sha" "$tree" "$platform" "$s1" "$s2" "$step4" "$step5" > "$source/.dev/pipeline/release-gate/$tree/$platform.receipt"
  done
}
ref() { "$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" rev-parse --verify "$1" 2>/dev/null || true; }
run_rejected() {
  local before_main before_tag after_main after_tag
  before_main=$(ref refs/heads/main); before_tag=$(ref refs/tags/v1.2.3)
  if (cd "$1" && "$PUBLISH" --execute --tag v1.2.3) >"$TMP/out" 2>&1; then
    echo 'expected exporter rejection' >&2; cat "$TMP/out" >&2; exit 1
  fi
  after_main=$(ref refs/heads/main); after_tag=$(ref refs/tags/v1.2.3)
  [[ "$after_main" == "$before_main" && "$after_tag" == "$before_tag" ]] || { echo "rejected publication changed receiver refs: main $before_main -> $after_main; tag $before_tag -> $after_tag" >&2; exit 1; }
}

# Invalid destinations, removed flags, and non-main targets reject without writes.
init_repo rejected
before=$(ref refs/heads/main)
"$REAL_GIT" -C "$TMP/rejected/source" remote set-url github https://example.invalid/other.git
if (cd "$TMP/rejected/source" && "$PUBLISH" --execute) >/dev/null 2>&1; then exit 1; fi
[[ "$(ref refs/heads/main)" == "$before" ]]
"$REAL_GIT" -C "$TMP/rejected/source" remote set-url github https://github.com/monkey1wizard/ccync.git
PUBLISH_TEST_EFFECTIVE_LOOKUP_FAIL=1; export PUBLISH_TEST_EFFECTIVE_LOOKUP_FAIL
run_rejected "$TMP/rejected/source"
unset PUBLISH_TEST_EFFECTIVE_LOOKUP_FAIL
"$REAL_GIT" -C "$TMP/rejected/source" config --add remote.github.url https://example.invalid/extra.git
run_rejected "$TMP/rejected/source"
"$REAL_GIT" -C "$TMP/rejected/source" config --unset-all remote.github.url
"$REAL_GIT" -C "$TMP/rejected/source" config --add remote.github.url https://github.com/monkey1wizard/ccync.git
"$REAL_GIT" -C "$TMP/rejected/source" remote set-url github https://github.com/monkey1wizard/ccync.git
"$REAL_GIT" -C "$TMP/rejected/source" config remote.github.pushurl https://github.com/monkey1wizard/other.git
if (cd "$TMP/rejected/source" && "$PUBLISH" --execute) >/dev/null 2>&1; then exit 1; fi
[[ "$(ref refs/heads/main)" == "$before" ]]
"$REAL_GIT" -C "$TMP/rejected/source" config --unset remote.github.pushurl
for option in --force-tag --include-project-md; do
  if (cd "$TMP/rejected/source" && "$PUBLISH" "$option") >/dev/null 2>&1; then exit 1; fi
done
for option in --checkout-sha --receipt; do
  if (cd "$TMP/rejected/source" && "$PUBLISH" "$option" value) >/dev/null 2>&1; then exit 1; fi
done
if (cd "$TMP/rejected/source" && "$PUBLISH" --branch release) >/dev/null 2>&1; then exit 1; fi

# Success publishes an orphan snapshot with matching refs, canonical content, and no excluded paths.
init_repo backup
BACKUP_SHA=$("$REAL_GIT" -C "$TMP/backup/source" rev-parse HEAD)
BACKUP_TAG=$("$REAL_GIT" -C "$TMP/backup/source" rev-parse refs/tags/v1.2.3)
BACKUP_DIR=$(find "$TMP/backup/source/.dev/pipeline/release-gate" -mindepth 1 -maxdepth 1 -type d)
BACKUP_ARGS=(--execute --tag v1.2.3 --backup-controller --verified-source "$BACKUP_SHA" --verified-tag-object "$BACKUP_TAG" --windows-receipt "$BACKUP_DIR/windows.receipt" --linux-receipt "$BACKUP_DIR/linux.receipt")
TRUST="$TMP/backup/source/.github/release-trust/allowed_signers"
SIGNED_ARGS=(--verify-signed-receipts --trust-list "$TRUST" --windows-receipt "$BACKUP_DIR/windows.receipt" --linux-receipt "$BACKUP_DIR/linux.receipt" --windows-signature "$BACKUP_DIR/windows.receipt.sig" --linux-signature "$BACKUP_DIR/linux.receipt.sig")
sign_receipt() {
  rm -f "$1.sig"
  ssh-keygen -Y sign -f "$TMP/backup/key" -n "${2:-ccync-release-receipt}" "$1" >/dev/null 2>&1
}
sign_receipt "$BACKUP_DIR/windows.receipt"
sign_receipt "$BACKUP_DIR/linux.receipt"
cp "$BACKUP_DIR/linux.receipt" "$TMP/valid.receipt"
cp "$BACKUP_DIR/linux.receipt.sig" "$TMP/valid.sig"
signed_rejected() {
  local before_main before_tag
  before_main=$(ref refs/heads/main); before_tag=$(ref refs/tags/v1.2.3)
  if (cd "$TMP/backup/source" && "$PUBLISH" "${SIGNED_ARGS[@]}") >"$TMP/out" 2>&1; then
    echo "signed receipt verification unexpectedly accepted: $1" >&2; exit 1
  fi
  [[ "$(ref refs/heads/main)" == "$before_main" && "$(ref refs/tags/v1.2.3)" == "$before_tag" ]] || { echo "signed receipt rejection changed public refs: $1" >&2; exit 1; }
}
SAVED_RECEIPT=${SIGNED_ARGS[4]}
SIGNED_ARGS[4]=
signed_rejected 'missing receipt input'
SIGNED_ARGS[4]=$SAVED_RECEIPT
mv "$BACKUP_DIR/linux.receipt" "$TMP/missing.receipt"
signed_rejected 'missing receipt'
mv "$TMP/missing.receipt" "$BACKUP_DIR/linux.receipt"
rm "$BACKUP_DIR/linux.receipt.sig"
signed_rejected 'missing signature'
printf 'invalid signature\n' > "$BACKUP_DIR/linux.receipt.sig"
signed_rejected 'bad signature'
ssh-keygen -q -t ed25519 -N '' -f "$TMP/backup/untrusted-key"
rm "$BACKUP_DIR/linux.receipt.sig"
ssh-keygen -Y sign -f "$TMP/backup/untrusted-key" -n ccync-release-receipt "$BACKUP_DIR/linux.receipt" >/dev/null 2>&1
signed_rejected 'untrusted signer'
sign_receipt "$BACKUP_DIR/linux.receipt" wrong-namespace
signed_rejected 'wrong namespace'
for entry in result=fail mode=public source=invalid tree=invalid; do
  field=${entry%%=*}
  sed "s/^$field=.*/$entry/" "$TMP/valid.receipt" > "$BACKUP_DIR/linux.receipt"
  sign_receipt "$BACKUP_DIR/linux.receipt"
  signed_rejected "$entry"
done
cp "$TMP/valid.receipt" "$BACKUP_DIR/linux.receipt"
cp "$TMP/valid.sig" "$BACKUP_DIR/linux.receipt.sig"
if ! (cd "$TMP/backup/source" && "$PUBLISH" "${SIGNED_ARGS[@]}") >"$TMP/out" 2>&1; then cat "$TMP/out" >&2; exit 1; fi

# Receipt validation runs the first interpreter that executes, not the first name on PATH.
for stub_dir in "$TMP/stub-python" "$TMP/stub-python3"; do
  if ! (cd "$TMP/backup/source" && PATH="$stub_dir:$PATH" "$PUBLISH" "${SIGNED_ARGS[@]}") >"$TMP/out" 2>&1; then
    echo "receipt validation failed behind failing stub in $stub_dir" >&2; cat "$TMP/out" >&2; exit 1
  fi
done
if ! (cd "$TMP/backup/source" && PATH="$TMP/uv-python:$PATH" "$PUBLISH" "${SIGNED_ARGS[@]}") >"$TMP/out" 2>&1; then
  echo 'receipt validation failed with only the uv-located interpreter' >&2; cat "$TMP/out" >&2; exit 1
fi
before_main=$(ref refs/heads/main); before_tag=$(ref refs/tags/v1.2.3)
if (cd "$TMP/backup/source" && PATH="$TMP/no-python:$PATH" "$PUBLISH" "${SIGNED_ARGS[@]}") >"$TMP/out" 2>&1; then
  echo 'receipt validation accepted without a working Python 3' >&2; exit 1
fi
grep -q '^publish-public: Python 3 was not found' "$TMP/out" || { echo 'missing Python 3 not-found error' >&2; cat "$TMP/out" >&2; exit 1; }
grep -q 'uv python find' "$TMP/out" || { echo 'not-found error omits uv python find' >&2; cat "$TMP/out" >&2; exit 1; }
if grep -q 'invalid .* receipt' "$TMP/out"; then echo 'missing interpreter reported as an invalid receipt' >&2; exit 1; fi
if (cd "$TMP/backup/source" && GITHUB_ACTIONS=true PATH="$TMP/no-python:$PATH" "$PUBLISH" "${BACKUP_ARGS[@]}") >"$TMP/out" 2>&1; then
  echo 'backup publication accepted without a working Python 3' >&2; exit 1
fi
grep -q '^publish-public: Python 3 was not found' "$TMP/out" || { echo 'missing Python 3 not-found error on publication' >&2; cat "$TMP/out" >&2; exit 1; }
[[ "$(ref refs/heads/main)" == "$before_main" && "$(ref refs/tags/v1.2.3)" == "$before_tag" ]] || { echo 'missing interpreter changed public refs' >&2; exit 1; }

if (cd "$TMP/backup/source" && env -u GITHUB_ACTIONS "$PUBLISH" "${BACKUP_ARGS[@]}") >"$TMP/out" 2>&1; then echo 'backup accepted outside Actions' >&2; exit 1; fi

backup_rejected() {
  local label=$1 expected=$2 before_main before_tag
  shift 2
  before_main=$(ref refs/heads/main); before_tag=$(ref refs/tags/v1.2.3)
  if (cd "$TMP/backup/source" && GITHUB_ACTIONS=true "$PUBLISH" "$@") >"$TMP/out" 2>&1; then
    echo "backup accepted $label" >&2; cat "$TMP/out" >&2; exit 1
  fi
  grep -q "$expected" "$TMP/out" || { echo "backup rejected $label for another reason" >&2; cat "$TMP/out" >&2; exit 1; }
  [[ "$(ref refs/heads/main)" == "$before_main" && "$(ref refs/tags/v1.2.3)" == "$before_tag" ]] || { echo "backup rejection changed public refs: $label" >&2; exit 1; }
}
BACKUP_RECEIPT_ARGS=(--windows-receipt "$BACKUP_DIR/windows.receipt" --linux-receipt "$BACKUP_DIR/linux.receipt")
OTHER_OBJECT=$("$REAL_GIT" -C "$TMP/backup/source" rev-parse 'HEAD^{tree}')
backup_rejected 'without --verified-source' 'invalid backup controller arguments' \
  --execute --tag v1.2.3 --backup-controller --verified-tag-object "$BACKUP_TAG" "${BACKUP_RECEIPT_ARGS[@]}"
backup_rejected 'without --verified-tag-object' 'invalid backup controller arguments' \
  --execute --tag v1.2.3 --backup-controller --verified-source "$BACKUP_SHA" "${BACKUP_RECEIPT_ARGS[@]}"
backup_rejected 'a verified source other than HEAD' 'verified source or tag mismatch' \
  --execute --tag v1.2.3 --backup-controller --verified-source "$OTHER_OBJECT" --verified-tag-object "$BACKUP_TAG" "${BACKUP_RECEIPT_ARGS[@]}"
backup_rejected 'a verified tag object other than the local tag' 'verified source or tag mismatch' \
  --execute --tag v1.2.3 --backup-controller --verified-source "$BACKUP_SHA" --verified-tag-object "$BACKUP_SHA" "${BACKUP_RECEIPT_ARGS[@]}"
for field in mode source tree result; do
  cp "$BACKUP_DIR/linux.receipt" "$TMP/receipt"
  sed "s/^$field=.*/$field=invalid/" "$TMP/receipt" > "$BACKUP_DIR/linux.receipt"
  if (cd "$TMP/backup/source" && GITHUB_ACTIONS=true "$PUBLISH" "${BACKUP_ARGS[@]}") >"$TMP/out" 2>&1; then echo "backup accepted invalid $field" >&2; exit 1; fi
  [[ -z "$(ref refs/heads/main)" && -z "$(ref refs/tags/v1.2.3)" ]]
  cp "$TMP/receipt" "$BACKUP_DIR/linux.receipt"
done
if ! (cd "$TMP/backup/source" && GITHUB_ACTIONS=true "$PUBLISH" "${BACKUP_ARGS[@]}") >"$TMP/out" 2>&1; then cat "$TMP/out" >&2; exit 1; fi
[[ -n "$(ref refs/heads/main)" && "$(ref refs/heads/main)" == "$(ref refs/tags/v1.2.3)" ]]

init_repo success
if ! (cd "$TMP/success/source" && "$PUBLISH" --execute --tag v1.2.3) >"$TMP/success.log" 2>&1; then
  cat "$TMP/success.log" >&2; exit 1
fi
P=$(ref refs/heads/main)
[[ -n "$P" && "$P" == "$(ref refs/tags/v1.2.3)" ]]
printf 'receiver evidence: main=%s tag=%s\n' "$P" "$(ref refs/tags/v1.2.3)"
echo 'export tree evidence:'
"$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" ls-tree -r --name-only "$P"
[[ "$("$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" rev-list --parents -n 1 "$P" | wc -w | tr -d ' ')" == 1 ]]
[[ "$("$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" show "$P:config.toml")" == canonical ]]
for path in .dev/plans/secret.md docs/plans/secret.md .gitlab-ci.yml .github/workflows/publish-public.yml .github/workflows/release-rehearsal.yml .github/release-trust/allowed_signers; do
  if "$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" cat-file -e "$P:$path" 2>/dev/null; then echo "excluded path leaked: $path" >&2; exit 1; fi
done
"$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" cat-file -e "$P:.github/workflows/release.yml"
"$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" cat-file -e "$P:.github/workflows/release-build.yml"

# Existing tags and lookup failures preserve observed refs.
PUBLISH_TEST_EXISTING_TAG=1; export PUBLISH_TEST_EXISTING_TAG
run_rejected "$TMP/success/source"
unset PUBLISH_TEST_EXISTING_TAG
PUBLISH_TEST_LOOKUP_FAIL=1; export PUBLISH_TEST_LOOKUP_FAIL
run_rejected "$TMP/success/source"
unset PUBLISH_TEST_LOOKUP_FAIL

# Scan committed blob bytes even when they contain binary data.
init_repo secret
key_prefix='BEGIN OPENSSH'
key_suffix='PRIVATE KEY'
printf '\0%s %s\n' "$key_prefix" "$key_suffix" >"$TMP/secret/source/binary.dat"
"$REAL_GIT" -C "$TMP/secret/source" add binary.dat
"$REAL_GIT" -C "$TMP/secret/source" commit -qm secret
"$REAL_GIT" -C "$TMP/secret/source" tag -d v1.2.3 >/dev/null
"$REAL_GIT" -C "$TMP/secret/source" -c gpg.format=ssh -c user.signingkey="$TMP/secret/key" tag -sm fixture v1.2.3
"$REAL_GIT" -C "$TMP/secret/source" push -q "$TMP/secret/private" main +refs/tags/v1.2.3
make_receipts secret
run_rejected "$TMP/secret/source"

# An atomic receive rejection and a competing tag retain old refs and the winning tag object.
init_repo atomic
PUBLISH_TEST_RECEIVER="$TMP/atomic/receiver"; export PUBLISH_TEST_RECEIVER
PUBLISH_TEST_ATOMIC_REJECT=1; export PUBLISH_TEST_ATOMIC_REJECT
run_rejected "$TMP/atomic/source"
unset PUBLISH_TEST_ATOMIC_REJECT

init_repo race
PUBLISH_TEST_RECEIVER="$TMP/race/receiver"; export PUBLISH_TEST_RECEIVER
RACE_TAG=$("$REAL_GIT" -C "$TMP/race/source" rev-parse HEAD)
"$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" fetch -q "$TMP/race/source" HEAD
export PUBLISH_TEST_RACE_WINNER="$RACE_TAG"
export PUBLISH_TEST_TAG_RACE=1
if (cd "$TMP/race/source" && "$PUBLISH" --execute --tag v1.2.3) >"$TMP/race.log" 2>&1; then echo 'expected tag-race rejection' >&2; exit 1; fi
unset PUBLISH_TEST_TAG_RACE
[[ -z "$(ref refs/heads/main)" ]]
[[ "$(ref refs/tags/v1.2.3)" == "$RACE_TAG" ]]

init_repo lease
PUBLISH_TEST_RACE_WINNER=$("$REAL_GIT" -C "$TMP/lease/source" rev-parse HEAD)
"$REAL_GIT" --git-dir="$PUBLISH_TEST_RECEIVER" fetch -q "$TMP/lease/source" HEAD
export PUBLISH_TEST_RACE_WINNER PUBLISH_TEST_MAIN_RACE=1
if (cd "$TMP/lease/source" && "$PUBLISH" --execute --tag v1.2.3) >"$TMP/lease.log" 2>&1; then echo 'expected lease rejection' >&2; exit 1; fi
unset PUBLISH_TEST_MAIN_RACE
[[ "$(ref refs/heads/main)" == "$PUBLISH_TEST_RACE_WINNER" && -z "$(ref refs/tags/v1.2.3)" ]]

init_repo revoked
"$REAL_GIT" -C "$TMP/revoked/source" rm -q .github/release-trust/allowed_signers
"$REAL_GIT" -C "$TMP/revoked/source" commit -qm revoke
"$REAL_GIT" -C "$TMP/revoked/source" push -q "$TMP/revoked/private" main
"$REAL_GIT" -C "$TMP/revoked/source" checkout -q HEAD^
run_rejected "$TMP/revoked/source"
grep -q 'trust list unavailable' "$TMP/out"

init_repo version
"$REAL_GIT" -C "$TMP/version/source" tag -d v1.2.3 >/dev/null
"$REAL_GIT" -C "$TMP/version/source" -c gpg.format=ssh -c user.signingkey="$TMP/version/key" tag -sm fixture v1.2.4
if (cd "$TMP/version/source" && "$PUBLISH" --execute --tag v1.2.4) >"$TMP/out" 2>&1; then echo 'expected version rejection' >&2; exit 1; fi
grep -q 'version differ' "$TMP/out"

# Unsafe retained directories and dry-run tags reject before any push.
init_repo keep
PUBLISH_TEST_RECEIVER="$TMP/keep/receiver"; export PUBLISH_TEST_RECEIVER
mkdir -p "$TMP/occupied"; touch "$TMP/occupied/file"
if (cd "$TMP/keep/source" && "$PUBLISH" --keep-snapshot "$TMP/occupied") >/dev/null 2>&1; then exit 1; fi
ln -s "$TMP/occupied" "$TMP/symlink-keep"
if (cd "$TMP/keep/source" && "$PUBLISH" --keep-snapshot "$TMP/symlink-keep") >/dev/null 2>&1; then exit 1; fi
if (cd "$TMP/keep/source" && "$PUBLISH" --tag v1.2.3) >/dev/null 2>&1; then exit 1; fi
if (cd "$TMP/keep/source" && "$PUBLISH" --execute) >/dev/null 2>&1; then exit 1; fi
EMPTY_KEEP="$TMP/retained"
(cd "$TMP/keep/source" && "$PUBLISH" --keep-snapshot "$EMPTY_KEEP") >"$TMP/keep.log" 2>&1 || { cat "$TMP/keep.log" >&2; exit 1; }
echo 'retained snapshot tree evidence:'
ls -la "$EMPTY_KEEP"
[[ "$(cat "$EMPTY_KEEP/config.toml")" == canonical ]]
[[ ! -e "$EMPTY_KEEP/.gitlab-ci.yml" && ! -e "$EMPTY_KEEP/.dev/plans/secret.md" ]]
[[ -d "$EMPTY_KEEP/.git" ]]
[[ "$("$REAL_GIT" -C "$EMPTY_KEEP" ls-tree HEAD executable.sh | cut -d ' ' -f 1)" == 100755 ]]
"$REAL_GIT" -C "$TMP/keep/source" config core.filemode false
"$REAL_GIT" -C "$TMP/keep/source" config core.autocrlf true
(cd "$TMP/keep/source" && "$PUBLISH" --keep-snapshot "$TMP/retained-again") >"$TMP/keep-again.log" 2>&1 || { cat "$TMP/keep-again.log" >&2; exit 1; }
[[ "$("$REAL_GIT" -C "$EMPTY_KEEP" rev-parse 'HEAD^{tree}')" == "$("$REAL_GIT" -C "$TMP/retained-again" rev-parse 'HEAD^{tree}')" ]]
excluded_blob=$("$REAL_GIT" -C "$TMP/keep/source" rev-parse HEAD:.github/release-trust/allowed_signers)
if "$REAL_GIT" -C "$EMPTY_KEEP" cat-file -e "$excluded_blob" 2>/dev/null; then echo 'excluded blob present in snapshot object database' >&2; exit 1; fi
"$REAL_GIT" -C "$EMPTY_KEEP" fsck --full --no-reflogs --unreachable >"$TMP/unreachable"
[[ ! -s "$TMP/unreachable" ]]
[[ -z "$(ref refs/heads/main)" ]]

init_repo gate
GATE_ROOT="$TMP/gate/source"
for script in test-release-notes.sh:release-notes test-release-artifacts.sh:release-artifacts test-publish-public.sh:publish-public; do
  printf '#!/usr/bin/env bash\nexec gate-step %s "$@"\n' "${script#*:}" > "$GATE_ROOT/packaging/${script%%:*}"
done
printf 'fixture\n' > "$GATE_ROOT/packaging/test-install.ps1"
"$REAL_GIT" -C "$GATE_ROOT" add packaging
"$REAL_GIT" -C "$GATE_ROOT" commit -qm 'gate fixture'
"$REAL_GIT" -C "$GATE_ROOT" remote remove github
"$REAL_GIT" -C "$GATE_ROOT" remote remove origin
[[ -z "$("$REAL_GIT" -C "$GATE_ROOT" remote)" ]]
GATE_SHA=$("$REAL_GIT" -C "$GATE_ROOT" rev-parse HEAD)
GATE_HEAD_TREE=$("$REAL_GIT" -C "$GATE_ROOT" rev-parse 'HEAD^{tree}')
GIT_INDEX_FILE="$TMP/gate/export-index" "$REAL_GIT" -C "$GATE_ROOT" read-tree HEAD
GIT_INDEX_FILE="$TMP/gate/export-index" "$REAL_GIT" -C "$GATE_ROOT" update-index --force-remove -- .gitlab-ci.yml .github/workflows/publish-public.yml .github/workflows/release-rehearsal.yml .github/release-trust/allowed_signers docs/plans/secret.md .dev/plans/secret.md
GATE_EXPORT_TREE=$(GIT_INDEX_FILE="$TMP/gate/export-index" "$REAL_GIT" -C "$GATE_ROOT" write-tree)
[[ "$GATE_EXPORT_TREE" != "$GATE_HEAD_TREE" ]]
GATE_ROOT_PHYSICAL=$(cd "$GATE_ROOT" && pwd -P)
[[ -z "$("$REAL_GIT" -C "$GATE_ROOT" status --porcelain --untracked-files=all)" ]]

mkdir -p "$TMP/gate-bin" "$TMP/gate-tmp" "$TMP/gate-receipts"
cat > "$TMP/gate-bin/gate-step" <<'STEP'
#!/usr/bin/env bash
name=$1; shift
tree=$(git rev-parse 'HEAD^{tree}' 2>/dev/null || echo none)
private=absent; [[ -e .gitlab-ci.yml ]] && private=present
printf '%s|%s|%s|%s|%s\n' "$name" "$*" "$(pwd -P)" "$tree" "$private" >> "$GATE_TEST_LOG"
if [[ "${GATE_TEST_KILL:-}" == "$name" ]]; then kill -9 "$PPID"; exit 1; fi
if [[ "${GATE_TEST_FAIL:-}" == "$name:"* ]]; then exit "${GATE_TEST_FAIL#*:}"; fi
exit 0
STEP
printf '#!/usr/bin/env bash\nexec gate-step "cargo-$1" "$@"\n' > "$TMP/gate-bin/cargo"
printf '#!/usr/bin/env bash\nexec gate-step install "$@"\n' > "$TMP/gate-bin/pwsh"
printf '#!/usr/bin/env bash\n[[ "$1" == -V ]] && echo "rustc 1.0.0-fixture (gate fixture)"\n' > "$TMP/gate-bin/rustc"
printf '#!/usr/bin/env bash\n[[ "$1" == --version ]] && echo "uv 0.0.0-fixture"\n' > "$TMP/gate-bin/uv"
chmod +x "$TMP/gate-bin/"*
case $(uname -s) in
  Linux*) GATE_OS=linux; GATE_SOURCE_STEPS=(release-notes release-artifacts); GATE_CHECK_STEPS=(cargo-test cargo-clippy release-notes release-artifacts publish-public) ;;
  MINGW*|MSYS*|CYGWIN*) GATE_OS=windows; GATE_SOURCE_STEPS=(); GATE_CHECK_STEPS=(cargo-test cargo-clippy install) ;;
  *) echo 'test-publish-public: unsupported platform for gate probes' >&2; exit 1 ;;
esac
GATE_LOG="$TMP/gate.log"
GATE_FAIL= GATE_KILL= GATE_STATUS=0
run_gate() {
  : > "$GATE_LOG"
  GATE_STATUS=0
  (
    cd "$GATE_ROOT"
    export PATH="$TMP/gate-bin:$PATH" TMPDIR="$TMP/gate-tmp" GATE_TEST_LOG="$GATE_LOG" GATE_TEST_FAIL="$GATE_FAIL" GATE_TEST_KILL="$GATE_KILL"
    "$PUBLISH" --gate "$@"
  ) >"$TMP/out" 2>&1 || GATE_STATUS=$?
}
# expected_receipt MODE TREE RESULT then the step values s1 s2 step1 step2 step3 step4 step5.
expected_receipt() {
  printf 'ccync-release-gate-receipt-v1\nmode=%s\nsource=%s\ntree=%s\nos=%s\nrustc=rustc 1.0.0-fixture (gate fixture)\nuv=uv 0.0.0-fixture\ns1=%s\ns2=%s\nstep1=%s\nstep2=%s\nstep3=%s\nstep4=%s\nstep5=%s\nresult=%s\n' \
    "$1" "$GATE_SHA" "$2" "$GATE_OS" "$4" "$5" "$6" "$7" "$8" "$9" "${10}" "$3"
}
# step_values S1_S2_VALUE STATUS... prints the seven step fields for this platform.
step_values() {
  local s=$1 values=() i
  shift
  if [[ "$GATE_OS" == windows ]]; then
    values=(NotRun NotRun "$@" NotRun NotRun)
  else
    values=("$s" "$s" "$@")
  fi
  for i in "${values[@]}"; do printf '%s\n' "$i"; done
}
assert_receipt() {
  local file=$1 label=$2; shift 2
  expected_receipt "$@" > "$TMP/expected.receipt"
  diff -u "$TMP/expected.receipt" "$file" >&2 || { echo "gate receipt mismatch: $label" >&2; cat "$TMP/out" >&2; exit 1; }
}
gate_rejected() {
  local label=$1 expected=$2
  [[ "$GATE_STATUS" -ne 0 ]] || { echo "gate accepted $label" >&2; cat "$TMP/out" >&2; exit 1; }
  grep -q "$expected" "$TMP/out" || { echo "gate rejected $label for another reason" >&2; cat "$TMP/out" >&2; exit 1; }
  [[ ! -s "$GATE_LOG" ]] || { echo "gate ran checks despite $label" >&2; cat "$GATE_LOG" >&2; exit 1; }
}
repeat_value() { local i; for ((i = 0; i < $2; i++)); do printf '%s\n' "$1"; done; }
GATE_STEP_COUNT=${#GATE_CHECK_STEPS[@]}
mapfile -t GATE_ZEROS < <(repeat_value 0 "$GATE_STEP_COUNT")
mapfile -t GATE_LATER_ZEROS < <(repeat_value 0 $((GATE_STEP_COUNT - 1)))
mapfile -t GATE_LATER_NOT_RUN < <(repeat_value NotRun $((GATE_STEP_COUNT - 1)))
pass_values() { mapfile -t STEPS_OUT < <(step_values 0 "${GATE_ZEROS[@]}"); }

RECEIPT_FILE="$TMP/gate-receipts/gate.receipt"
run_gate --receipt "$RECEIPT_FILE"
[[ "$GATE_STATUS" -eq 0 ]] || { echo 'source-mode gate failed' >&2; cat "$TMP/out" >&2; exit 1; }
pass_values
assert_receipt "$RECEIPT_FILE" 'source mode pass' source "$GATE_EXPORT_TREE" pass "${STEPS_OUT[@]}"
echo 'gate receipt evidence:'
cat "$RECEIPT_FILE"
[[ "$(cut -d '|' -f 1 "$GATE_LOG" | tr '\n' ' ')" == "$(printf '%s ' "${GATE_SOURCE_STEPS[@]}" "${GATE_CHECK_STEPS[@]}")" ]] || { echo 'source-mode gate ran unexpected steps' >&2; cat "$GATE_LOG" >&2; exit 1; }
line=0
while IFS='|' read -r name args cwd tree private; do
  if ((line < ${#GATE_SOURCE_STEPS[@]})); then
    [[ "$cwd" == "$GATE_ROOT_PHYSICAL" && "$tree" == "$GATE_HEAD_TREE" && -z "$args" ]] || { echo "source check $name did not read the checkout" >&2; cat "$GATE_LOG" >&2; exit 1; }
  else
    [[ "$cwd" != "$GATE_ROOT_PHYSICAL" && "$tree" == "$GATE_EXPORT_TREE" && "$private" == absent ]] || { echo "check $name did not read the exported snapshot" >&2; cat "$GATE_LOG" >&2; exit 1; }
  fi
  line=$((line + 1))
done < "$GATE_LOG"
if [[ "$GATE_OS" == linux ]]; then grep -q '^release-notes|--public-snapshot|' "$GATE_LOG"; fi

# Public mode checks the original checkout at the given SHA, whose tree differs from the export.
run_gate --checkout-sha "$GATE_SHA" --receipt "$TMP/gate-receipts/public.receipt"
[[ "$GATE_STATUS" -eq 0 ]] || { echo 'public-mode gate failed' >&2; cat "$TMP/out" >&2; exit 1; }
mapfile -t STEPS_OUT < <(step_values NotRun "${GATE_ZEROS[@]}")
assert_receipt "$TMP/gate-receipts/public.receipt" 'public mode pass' public "$GATE_HEAD_TREE" pass "${STEPS_OUT[@]}"
[[ "$(cut -d '|' -f 1 "$GATE_LOG" | tr '\n' ' ')" == "$(printf '%s ' "${GATE_CHECK_STEPS[@]}")" ]] || { echo 'public-mode gate ran unexpected steps' >&2; cat "$GATE_LOG" >&2; exit 1; }
while IFS='|' read -r name args cwd tree private; do
  [[ "$cwd" == "$GATE_ROOT_PHYSICAL" && "$tree" == "$GATE_HEAD_TREE" && "$private" == present ]] || { echo "public check $name did not read the original checkout" >&2; cat "$GATE_LOG" >&2; exit 1; }
done < "$GATE_LOG"
run_gate --checkout-sha "$GATE_EXPORT_TREE" --receipt "$TMP/gate-receipts/mismatch.receipt"
gate_rejected 'a checkout SHA other than HEAD' 'checkout SHA does not match HEAD'
[[ ! -e "$TMP/gate-receipts/mismatch.receipt" ]]

run_gate
[[ "$GATE_STATUS" -eq 0 ]] || { echo 'default-destination gate failed' >&2; cat "$TMP/out" >&2; exit 1; }
pass_values
assert_receipt "$GATE_ROOT/.dev/pipeline/release-gate/$GATE_EXPORT_TREE/$GATE_OS.receipt" 'default destination' source "$GATE_EXPORT_TREE" pass "${STEPS_OUT[@]}"

for args in '--execute' '--tag v1.2.3' "--keep-snapshot $TMP/gate-keep"; do
  read -r -a words <<< "$args"
  run_gate "${words[@]}" --receipt "$TMP/gate-receipts/incompatible.receipt"
  gate_rejected "$args" 'incompatible gate arguments'
  [[ ! -e "$TMP/gate-receipts/incompatible.receipt" && ! -e "$TMP/gate-keep" ]]
done
printf 'changed\n' > "$GATE_ROOT/executable.sh"
run_gate --receipt "$TMP/gate-receipts/dirty.receipt"
gate_rejected 'a modified tracked file' 'working tree is dirty'
"$REAL_GIT" -C "$GATE_ROOT" checkout -q -- executable.sh
"$REAL_GIT" -C "$GATE_ROOT" config status.showUntrackedFiles no
printf 'untracked\n' > "$GATE_ROOT/untracked.txt"
[[ -z "$("$REAL_GIT" -C "$GATE_ROOT" status --porcelain)" ]]
run_gate --receipt "$TMP/gate-receipts/dirty.receipt"
gate_rejected 'an untracked file hidden by status.showUntrackedFiles=no' 'working tree is dirty'
grep -q 'untracked.txt' "$TMP/out"
rm "$GATE_ROOT/untracked.txt"
"$REAL_GIT" -C "$GATE_ROOT" config --unset status.showUntrackedFiles
PUBLISH_TEST_STATUS_FAIL=1; export PUBLISH_TEST_STATUS_FAIL
run_gate --receipt "$TMP/gate-receipts/dirty.receipt"
unset PUBLISH_TEST_STATUS_FAIL
gate_rejected 'a failed git status' 'git status failed'
[[ ! -e "$TMP/gate-receipts/dirty.receipt" ]]

[[ "$(sed -n 's/^result=//p' "$RECEIPT_FILE")" == pass ]]
GATE_FAIL=cargo-test:3
run_gate --receipt "$RECEIPT_FILE"
GATE_FAIL=
[[ "$GATE_STATUS" -ne 0 ]] || { echo 'gate passed with a failing first step' >&2; exit 1; }
mapfile -t STEPS_OUT < <(step_values 0 3 "${GATE_LATER_NOT_RUN[@]}")
assert_receipt "$RECEIPT_FILE" 'first step failure' source "$GATE_EXPORT_TREE" fail "${STEPS_OUT[@]}"
[[ "$(cut -d '|' -f 1 "$GATE_LOG" | tr '\n' ' ')" == "$(printf '%s ' "${GATE_SOURCE_STEPS[@]}" cargo-test)" ]] || { echo 'gate ran steps after the first failure' >&2; cat "$GATE_LOG" >&2; exit 1; }
GATE_FAIL="${GATE_CHECK_STEPS[GATE_STEP_COUNT - 1]}:4"
run_gate --receipt "$RECEIPT_FILE"
GATE_FAIL=
[[ "$GATE_STATUS" -ne 0 ]] || { echo 'gate passed with a failing last step' >&2; exit 1; }
mapfile -t STEPS_OUT < <(step_values 0 "${GATE_LATER_ZEROS[@]}" 4)
assert_receipt "$RECEIPT_FILE" 'last step failure' source "$GATE_EXPORT_TREE" fail "${STEPS_OUT[@]}"

run_gate --receipt "$RECEIPT_FILE"
[[ "$GATE_STATUS" -eq 0 && "$(sed -n 's/^result=//p' "$RECEIPT_FILE")" == pass ]]
GATE_KILL=cargo-clippy
run_gate --receipt "$RECEIPT_FILE"
GATE_KILL=
[[ "$GATE_STATUS" -ne 0 ]] || { echo 'killed gate reported success' >&2; exit 1; }
[[ "$(cut -d '|' -f 1 "$GATE_LOG" | tail -n 1)" == cargo-clippy ]]
[[ ! -e "$RECEIPT_FILE" ]] || { echo 'killed gate left a receipt at the destination' >&2; cat "$RECEIPT_FILE" >&2; exit 1; }
for leftover in "$RECEIPT_FILE".tmp.*; do
  [[ -e "$leftover" ]] || continue
  if grep -q '^result=pass$' "$leftover"; then echo "killed gate left a passing receipt: $leftover" >&2; exit 1; fi
done

printf 'user data\n' > "$TMP/gate-receipts/user.file"
cp "$TMP/gate-receipts/user.file" "$TMP/user.file.orig"
run_gate --receipt "$TMP/gate-receipts/user.file"
gate_rejected 'a non-receipt destination' 'destination is not a receipt'
cmp -s "$TMP/gate-receipts/user.file" "$TMP/user.file.orig" || { echo 'gate changed a non-receipt destination' >&2; exit 1; }
[[ -z "$(find "$TMP/gate-receipts" -name 'user.file.tmp.*')" ]]
echo 'publish-public probes passed'
