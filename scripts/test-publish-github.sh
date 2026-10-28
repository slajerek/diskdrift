#!/bin/sh

set -eu

ROOT=$(git rev-parse --show-toplevel)
SCRIPT="$ROOT/claude/publish-github.sh"
mkdir -p "$ROOT/target"
FIXTURE=$(mktemp -d "$ROOT/target/publish-github-test.XXXXXX")
export TMPDIR="$FIXTURE"
cleanup() {
    rm -rf "$FIXTURE"
}
trap cleanup EXIT
trap 'exit 129' HUP INT TERM

PUBLIC_ROOT="$FIXTURE/github.com"
PUBLIC="$PUBLIC_ROOT/public.git"
SEED="$FIXTURE/seed"
WORK="$FIXTURE/work"

fail() {
    printf '%s\n' "FAIL: $*" >&2
    exit 1
}

assert_ref() {
    name=$1
    expected=$2
    actual=$(git -C "$WORK" rev-parse --verify "$name") || fail "missing $name"
    [ "$actual" = "$expected" ] || fail "$name is $actual, expected $expected"
}

assert_remote_ref() {
    ref=$1
    expected=$2
    if ! remote_line=$(git -C "$WORK" ls-remote github "$ref"); then
        fail "could not inspect remote ref $ref"
    fi
    actual=$(printf '%s\n' "$remote_line" | cut -f1)
    [ "$actual" = "$expected" ] || fail "$ref is $actual, expected $expected"
}

assert_remote_absent() {
    ref=$1
    if ! remote_line=$(git -C "$WORK" ls-remote github "$ref"); then
        fail "could not inspect remote ref $ref"
    fi
    [ -z "$remote_line" ] || fail "$ref unexpectedly exists"
}

assert_parent() {
    commit=$1
    expected=$2
    git -C "$WORK" cat-file -p "$commit" | grep -Fx "parent $expected" >/dev/null \
        || fail "$commit does not have parent $expected"
}

assert_no_parent() {
    commit=$1
    unexpected=$2
    if git -C "$WORK" cat-file -p "$commit" | grep -Fx "parent $unexpected" >/dev/null; then
        fail "$commit unexpectedly has parent $unexpected"
    fi
}

assert_no_private_paths() {
    commit=$1
    for path in specs ROADMAP.md docs/handover docs/superpowers experiments reports research_notes .claude claude CLAUDE.md AGENTS.md private privé; do
        tree_paths=$(git -C "$WORK" ls-tree -r --name-only "$commit" -- "$path")
        [ -z "$tree_paths" ] || fail "$commit contains private path $path"
    done
}

run_publisher() {
    (cd "$WORK" && "$SCRIPT" "$@")
}

push_fixture() {
    git -C "$WORK" -c core.hooksPath=/dev/null push -q github "$@"
}

run_with_git_wrapper() {
    wrapper_dir=$1
    shift
    (cd "$WORK" && PATH="$wrapper_dir:$PATH" "$@")
}

mkdir -p "$PUBLIC_ROOT" "$SEED"
git init -q --bare "$PUBLIC"
git init -q -b devel "$SEED"
git -C "$SEED" config user.name Test
git -C "$SEED" config user.email test@example.invalid
printf '%s\n' '# Public' > "$SEED/README.md"
printf '%s\n' 'public' > "$SEED/public.txt"
printf '%s\n' 'legacy public instructions' > "$SEED/CLAUDE.md"
printf '%s\n' 'legacy public instructions' > "$SEED/AGENTS.md"
git -C "$SEED" add .
git -C "$SEED" commit -q -m 'public devel'
git -C "$SEED" remote add github "file://$PUBLIC"
git -C "$SEED" push -q github devel

git clone -q -b devel "$PUBLIC" "$WORK"
git -C "$WORK" config user.name Test
git -C "$WORK" config user.email test@example.invalid
git -C "$WORK" remote add github "file://$PUBLIC" 2>/dev/null || true
git -C "$WORK" config core.hooksPath "$ROOT/claude/githooks"
git -C "$WORK" checkout -q -b main
mkdir -p "$WORK/specs" "$WORK/docs/handover" "$WORK/docs/superpowers" \
    "$WORK/experiments" "$WORK/reports" "$WORK/research_notes" "$WORK/.claude" \
    "$WORK/docs/adr" "$WORK/docs/benchmarks"
mkdir -p "$WORK/claude" "$WORK/private"
mkdir -p "$WORK/privé"
printf '%s\n' '# Private source' 'Specification: [private]' '## Build' 'build' > "$WORK/README.md"
printf '%s\n' 'public' > "$WORK/public.txt"
printf '%s\n' 'public literal path' > "$WORK/literal1.txt"
printf '%s\n' 'secret' > "$WORK/private/secret.txt"
printf '%s\n' 'secret' > "$WORK/privé/secret.txt"
printf '%s\n' 'secret' > "$WORK/specs/spec.md"
printf '%s\n' 'secret' > "$WORK/ROADMAP.md"
printf '%s\n' 'secret' > "$WORK/docs/handover/note.md"
printf '%s\n' 'secret' > "$WORK/docs/superpowers/plan.md"
printf '%s\n' 'secret' > "$WORK/experiments/experiment.txt"
printf '%s\n' 'secret' > "$WORK/reports/report.txt"
printf '%s\n' 'secret' > "$WORK/research_notes/note.txt"
printf '%s\n' 'secret' > "$WORK/.claude/settings.json"
printf '%s\n' 'public storage decision' > "$WORK/docs/adr/0001-storage-engine.md"
printf '%s\n' 'public benchmark notes' > "$WORK/docs/benchmarks/README.md"
printf '%s\n' 'private instructions' > "$WORK/CLAUDE.md"
printf '%s\n' 'private instructions' > "$WORK/AGENTS.md"
printf '%s\n' \
    'specs/' 'ROADMAP.md' 'docs/handover/' 'docs/superpowers/' \
    'experiments/' 'reports/' 'research_notes/' '.claude/' 'claude/' \
    'CLAUDE.md' 'AGENTS.md' 'private/' 'privé/' 'literal[1].txt' > "$WORK/claude/public-exclude"
printf '%s\n' 'agent script' > "$WORK/claude/private-agent.sh"
git -C "$WORK" add .
git -C "$WORK" commit -q -m 'private source'

good_source=$(git -C "$WORK" rev-parse HEAD)
git -C "$WORK" show "$good_source:claude/public-exclude" | grep -v '^AGENTS\.md$' > "$WORK/claude/public-exclude"
git -C "$WORK" add claude/public-exclude
git -C "$WORK" commit -q -m 'invalid public policy'
bad_policy_source=$(git -C "$WORK" rev-parse HEAD)
git -C "$WORK" show "$good_source:claude/public-exclude" > "$WORK/claude/public-exclude"
git -C "$WORK" add claude/public-exclude
git -C "$WORK" commit -q -m 'restore public policy'
git -C "$WORK" show "$bad_policy_source:claude/public-exclude" > "$WORK/claude/public-exclude"
if run_publisher -b gh/invalid-policy -r "$bad_policy_source" -m 'Portability CI snapshot' >/dev/null 2>&1; then
    fail 'source policy missing AGENTS.md was accepted'
fi
if git -C "$WORK" show-ref --verify --quiet refs/heads/gh/invalid-policy; then
    fail 'invalid source policy created a snapshot ref'
fi
git -C "$WORK" show "$good_source:claude/public-exclude" > "$WORK/claude/public-exclude"
clean_hook_policy="$FIXTURE/clean-hook-policy"
git -C "$WORK" show "$good_source:claude/public-exclude" > "$clean_hook_policy"

git -C "$WORK" rm -q README.md
git -C "$WORK" commit -q -m 'missing README source'
missing_readme_source=$(git -C "$WORK" rev-parse HEAD)
git -C "$WORK" show "$good_source:README.md" > "$WORK/README.md"
git -C "$WORK" add README.md
git -C "$WORK" commit -q -m 'restore README source'
if run_publisher -b gh/missing-readme -r "$missing_readme_source" -m 'Portability CI snapshot' >/dev/null 2>&1; then
    fail 'missing README source was accepted'
fi
if git -C "$WORK" show-ref --verify --quiet refs/heads/gh/missing-readme; then
    fail 'missing README source created a snapshot ref'
fi

filter_bin="$FIXTURE/filter-bin"
mkdir -p "$filter_bin"
filter_wrapper="$filter_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *" rm -r -q -f --cached "*) exit 0 ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$filter_wrapper"
chmod +x "$filter_wrapper"
filter_real=$(command -v git)
if run_with_git_wrapper "$filter_bin" env GIT_REAL="$filter_real" \
    "$SCRIPT" -b gh/filter-failure -r "$good_source" -m 'Portability CI snapshot' >/dev/null 2>&1; then
    fail 'candidate tree with failed filtering was accepted'
fi
if git -C "$WORK" show-ref --verify --quiet refs/heads/gh/filter-failure; then
    fail 'failed candidate filtering created a snapshot ref'
fi

unicode_filter_bin="$FIXTURE/unicode-filter-bin"
mkdir -p "$unicode_filter_bin"
unicode_filter_wrapper="$unicode_filter_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *" -- privé "*) exit 0 ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$unicode_filter_wrapper"
chmod +x "$unicode_filter_wrapper"
unicode_filter_real=$(command -v git)
if run_with_git_wrapper "$unicode_filter_bin" env GIT_REAL="$unicode_filter_real" \
    "$SCRIPT" -b gh/unicode-filter-failure -r "$good_source" -m 'Portability CI snapshot' >/dev/null 2>&1; then
    fail 'candidate tree with a non-ASCII private path was accepted'
fi
if git -C "$WORK" show-ref --verify --quiet refs/heads/gh/unicode-filter-failure; then
    fail 'failed Unicode filtering created a snapshot ref'
fi

inspect_bin="$FIXTURE/inspect-bin"
mkdir -p "$inspect_bin"
inspect_wrapper="$inspect_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *"ls-tree"*) exit 128 ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$inspect_wrapper"
chmod +x "$inspect_wrapper"
inspect_real=$(command -v git)

revlist_bin="$FIXTURE/revlist-bin"
mkdir -p "$revlist_bin"
revlist_wrapper="$revlist_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *"rev-list"*) exit 128 ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$revlist_wrapper"
chmod +x "$revlist_wrapper"
revlist_real=$(command -v git)

live_refs_bin="$FIXTURE/live-refs-bin"
mkdir -p "$live_refs_bin"
live_refs_wrapper="$live_refs_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *"ls-remote --heads"*) exit 128 ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$live_refs_wrapper"
chmod +x "$live_refs_wrapper"
live_refs_real=$(command -v git)

signal_bin="$FIXTURE/signal-bin"
mkdir -p "$signal_bin"
signal_mktemp_wrapper="$signal_bin/mktemp"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'path=$("$REAL_MKTEMP" "$@") || exit $?' \
    'printf "%s\\n" "$path" > "$TMP_RECORD" || exit 1' \
    'printf "%s\\n" "$path"' > "$signal_mktemp_wrapper"
chmod +x "$signal_mktemp_wrapper"
signal_git_wrapper="$signal_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case "$1" in fetch) sleep 10 ;; esac' \
    'exec "$REAL_GIT" "$@"' > "$signal_git_wrapper"
chmod +x "$signal_git_wrapper"
signal_record="$FIXTURE/signal-tmp-path"
signal_real_mktemp=$(command -v mktemp)
signal_real_git=$(command -v git)
(
    cd "$WORK"
    PATH="$signal_bin:$PATH" REAL_MKTEMP="$signal_real_mktemp" REAL_GIT="$signal_real_git" \
        TMP_RECORD="$signal_record" exec "$SCRIPT" -b gh/signal-cleanup -r "$good_source" \
        -m 'Portability CI snapshot'
) >/dev/null 2>&1 &
signal_pid=$!
signal_wait=0
while [ ! -s "$signal_record" ] && [ "$signal_wait" -lt 100 ]; do
    sleep 0.1
    signal_wait=$((signal_wait + 1))
done
[ -s "$signal_record" ] || fail 'publisher signal fixture did not start'
signal_tmp=
IFS= read -r signal_tmp < "$signal_record" || fail 'could not read publisher temporary path'
kill -TERM "$signal_pid" 2>/dev/null || fail 'could not signal publisher cleanup fixture'
if wait "$signal_pid"; then
    fail 'publisher signal cleanup unexpectedly succeeded'
fi
[ ! -e "$signal_tmp" ] || fail 'publisher temporary directory survived SIGTERM'

# The policy in the selected source commit wins over a dirty worktree policy.
printf '%s\n' 'public.txt' >> "$WORK/claude/public-exclude"
before_status=$(git -C "$WORK" status --porcelain)
dirty_branch=gh/dirty-policy
run_publisher -b "$dirty_branch" -r HEAD -m 'Portability CI snapshot' >/dev/null
dirty_commit=$(git -C "$WORK" rev-parse "refs/heads/$dirty_branch")
git -C "$WORK" ls-tree -r --name-only "$dirty_commit" | grep -Fx public.txt >/dev/null \
    || fail 'dirty exclusion file changed the filtered tree'
git -C "$WORK" ls-tree -r --name-only "$dirty_commit" | grep -Fx literal1.txt >/dev/null \
    || fail 'literal policy path was treated as a regular expression'
assert_no_private_paths "$dirty_commit"
[ "$(git -C "$WORK" status --porcelain)" = "$before_status" ] \
    || fail 'publisher changed the worktree'

# A real GitHub-shaped push must use the selected source policy, not the dirty
# worktree policy, in the pre-push hook.
dirty_push_branch=gh/dirty-push
baseline_warning="$FIXTURE/baseline-warning"
if ! run_publisher -b "$dirty_push_branch" -r HEAD -m 'Portability CI snapshot' --push \
    >/dev/null 2>"$baseline_warning"; then
    fail 'legacy private paths in github/devel blocked publication'
fi
grep -F 'pre-existing public baseline contamination' "$baseline_warning" >/dev/null \
    || fail 'baseline contamination warning was not reported'
grep -F 'CLAUDE.md' "$baseline_warning" >/dev/null \
    || fail 'baseline CLAUDE.md contamination was not reported'
grep -F 'AGENTS.md' "$baseline_warning" >/dev/null \
    || fail 'baseline AGENTS.md contamination was not reported'
dirty_push_commit=$(git -C "$WORK" rev-parse "refs/heads/$dirty_push_branch")
assert_remote_ref "refs/heads/$dirty_push_branch" "$dirty_push_commit"
assert_no_private_paths "$dirty_push_commit"
[ "$(git -C "$WORK" status --porcelain)" = "$before_status" ] \
    || fail 'dirty-policy push changed the worktree'

# Without the publisher override, the hook retains its normal worktree policy.
normal_hook_branch=gh/normal-hook
normal_hook_commit=$(printf '%s\n' 'normal hook check' | git -C "$WORK" commit-tree "$dirty_push_commit^{tree}" -p "$dirty_push_commit")
git -C "$WORK" update-ref "refs/heads/$normal_hook_branch" "$normal_hook_commit"
if git -C "$WORK" push github "$normal_hook_branch:$normal_hook_branch" >/dev/null 2>&1; then
    fail 'normal hook policy accepted a dirty exclusion override case'
fi
assert_remote_absent "refs/heads/$normal_hook_branch"

deletion_branch=gh/allowed-deletion
push_fixture "$dirty_push_commit:refs/heads/$deletion_branch"
if ! DISKDRIFT_PUBLIC_EXCLUDE="$clean_hook_policy" git -C "$WORK" push github \
    ":refs/heads/$deletion_branch" >/dev/null 2>&1; then
    fail 'allowed public branch deletion was rejected'
fi
assert_remote_absent "refs/heads/$deletion_branch"

remote_ref_branch=gh/remote-ref-check
remote_ref_commit=$(printf '%s\n' 'remote ref check' | git -C "$WORK" commit-tree "$dirty_push_commit^{tree}" -p "$dirty_push_commit")
git -C "$WORK" update-ref "refs/heads/$remote_ref_branch" "$remote_ref_commit"
if DISKDRIFT_PUBLIC_EXCLUDE="$clean_hook_policy" git -C "$WORK" push github \
    "refs/heads/$remote_ref_branch:refs/tags/public-boundary" >/dev/null 2>&1; then
    fail 'hook accepted a non-head remote ref'
fi
assert_remote_absent refs/tags/public-boundary

if DISKDRIFT_PUBLIC_EXCLUDE="$clean_hook_policy" git -C "$WORK" push github \
    refs/heads/main:refs/heads/gh/local-ref-check >/dev/null 2>&1; then
    fail 'hook accepted a non-gh local ref'
fi
assert_remote_absent refs/heads/gh/local-ref-check

for suffix in \
    'Co-Authored-By: someone' \
    'Claude-Session: session' \
    'Generated with [Claude Code]'; do
    hook_attribution_branch=gh/hook-attribution-$(printf '%s' "$suffix" | tr -cd 'A-Za-z' | tail -c 8)
    hook_attribution_commit=$(printf 'subject\n%s\n' "$suffix" | git -C "$WORK" commit-tree "$dirty_push_commit^{tree}" -p "$dirty_push_commit")
    git -C "$WORK" update-ref "refs/heads/$hook_attribution_branch" "$hook_attribution_commit"
    if DISKDRIFT_PUBLIC_EXCLUDE="$clean_hook_policy" git -C "$WORK" push github \
        "$hook_attribution_branch:$hook_attribution_branch" >/dev/null 2>&1; then
        fail "hook accepted prohibited attribution: $suffix"
    fi
    assert_remote_absent "refs/heads/$hook_attribution_branch"
done

hook_inspection_branch=gh/hook-inspection-failure
hook_inspection_commit=$(printf '%s\n' 'hook inspection failure' | git -C "$WORK" commit-tree "$dirty_push_commit^{tree}" -p "$dirty_push_commit")
git -C "$WORK" update-ref "refs/heads/$hook_inspection_branch" "$hook_inspection_commit"
if PATH="$inspect_bin:$PATH" git -C "$WORK" push github "$hook_inspection_branch:$hook_inspection_branch" >/dev/null 2>&1; then
    fail 'pre-push hook accepted a failed tree inspection'
fi
assert_remote_absent "refs/heads/$hook_inspection_branch"

hook_revlist_branch=gh/hook-revlist-failure
hook_revlist_commit=$(printf '%s\n' 'hook rev-list failure' | git -C "$WORK" commit-tree "$dirty_push_commit^{tree}" -p "$dirty_push_commit")
git -C "$WORK" update-ref "refs/heads/$hook_revlist_branch" "$hook_revlist_commit"
if PATH="$revlist_bin:$PATH" git -C "$WORK" push github "$hook_revlist_branch:$hook_revlist_branch" >/dev/null 2>&1; then
    fail 'pre-push hook accepted a failed commit-history inspection'
fi
assert_remote_absent "refs/heads/$hook_revlist_branch"

hook_live_refs_branch=gh/hook-live-refs-failure
hook_live_refs_commit=$(printf '%s\n' 'hook live refs failure' | git -C "$WORK" commit-tree "$dirty_push_commit^{tree}" -p "$dirty_push_commit")
git -C "$WORK" update-ref "refs/heads/$hook_live_refs_branch" "$hook_live_refs_commit"
if PATH="$live_refs_bin:$PATH" GIT_REAL="$live_refs_real" git -C "$WORK" push github \
    "$hook_live_refs_branch:$hook_live_refs_branch" >/dev/null 2>&1; then
    fail 'pre-push hook accepted a failed live ref inspection'
fi
assert_remote_absent "refs/heads/$hook_live_refs_branch"

bad_hook_policy="$FIXTURE/bad-hook-policy"
grep -v '^AGENTS\.md$' "$WORK/claude/public-exclude" > "$bad_hook_policy"
bad_hook_branch=gh/bad-hook-policy
bad_hook_commit=$(printf '%s\n' 'bad hook policy' | git -C "$WORK" commit-tree "$dirty_push_commit^{tree}" -p "$dirty_push_commit")
git -C "$WORK" update-ref "refs/heads/$bad_hook_branch" "$bad_hook_commit"
if DISKDRIFT_PUBLIC_EXCLUDE="$bad_hook_policy" git -C "$WORK" push github "$bad_hook_branch:$bad_hook_branch" >/dev/null 2>&1; then
    fail 'hook accepted an incomplete policy override'
fi
assert_remote_absent "refs/heads/$bad_hook_branch"

# Every prohibited attribution form is rejected before a snapshot ref exists.
for suffix in \
    'Co-Authored-By: someone' \
    'Claude-Session: session' \
    'Generated with [Claude Code]'; do
    message=$(printf 'subject\n%s\n' "$suffix")
    branch=gh/attribution-$(printf '%s' "$suffix" | tr -cd 'A-Za-z' | tail -c 8)
    if run_publisher -b "$branch" -r HEAD -m "$message" >/dev/null 2>&1; then
        fail "attribution message was accepted: $suffix"
    fi
    if git -C "$WORK" show-ref --verify --quiet "refs/heads/$branch"; then
        fail "attribution rejection created $branch"
    fi
done

# The initial public snapshot has the fetched devel commit as its first parent.
git -C "$WORK" show HEAD:claude/public-exclude > "$WORK/claude/public-exclude"
base_branch=gh/portability-ci
run_publisher -b "$base_branch" -r HEAD -m 'Portability CI snapshot' --push >/dev/null
base_commit=$(git -C "$WORK" rev-parse "refs/heads/$base_branch")
devel=$(git -C "$WORK" rev-parse github/devel)
assert_parent "$base_commit" "$devel"
assert_no_private_paths "$base_commit"
assert_remote_ref "refs/heads/$base_branch" "$base_commit"

# Distinct local and remote public histories are both retained as parents.
printf '%s\n' 'changed' >> "$WORK/public.txt"
git -C "$WORK" add public.txt
git -C "$WORK" commit -q -m 'source change'
source_commit=$(git -C "$WORK" rev-parse HEAD)

unicode_index="$FIXTURE/unicode-index"
GIT_INDEX_FILE="$unicode_index" git -C "$WORK" read-tree "$base_commit"
unicode_blob=$(git -C "$WORK" hash-object -w -- "privé/secret.txt")
GIT_INDEX_FILE="$unicode_index" git -C "$WORK" update-index --add --cacheinfo "100644,$unicode_blob,privé/secret.txt"
unicode_tree=$(GIT_INDEX_FILE="$unicode_index" git -C "$WORK" write-tree)
unicode_hook_commit=$(printf '%s\n' 'Unicode private path' | git -C "$WORK" commit-tree "$unicode_tree" -p "$base_commit")
unicode_hook_branch=gh/unicode-hook
git -C "$WORK" update-ref "refs/heads/$unicode_hook_branch" "$unicode_hook_commit"
if DISKDRIFT_PUBLIC_EXCLUDE="$clean_hook_policy" git -C "$WORK" push github \
    "$unicode_hook_branch:$unicode_hook_branch" >/dev/null 2>&1; then
    fail 'hook accepted a non-ASCII private path'
fi
assert_remote_absent "refs/heads/$unicode_hook_branch"

stale_branch=gh/stale-tracking
stale_commit=$(printf '%s\n' 'stale tracking private commit' | git -C "$WORK" commit-tree "$source_commit^{tree}" -p "$base_commit")
git -C "$WORK" update-ref "refs/heads/$stale_branch" "$stale_commit"
git -C "$WORK" update-ref "refs/remotes/github/$stale_branch" "$stale_commit"
assert_remote_absent "refs/heads/$stale_branch"
if DISKDRIFT_PUBLIC_EXCLUDE="$clean_hook_policy" git -C "$WORK" push github \
    "$stale_branch:$stale_branch" >/dev/null 2>&1; then
    fail 'hook trusted a stale remote-tracking ref'
fi
assert_remote_absent "refs/heads/$stale_branch"

remote_tree=$(git -C "$WORK" rev-parse "refs/remotes/github/$base_branch^{tree}")
remote_extra=$(printf '%s\n' 'remote history' | git -C "$WORK" commit-tree "$remote_tree" -p "refs/remotes/github/$base_branch")
push_fixture "$remote_extra:refs/heads/$base_branch"
local_old=$(git -C "$WORK" rev-parse "refs/heads/$base_branch")
run_publisher -b "$base_branch" -r "$source_commit" -m 'Portability CI snapshot' --push >/dev/null
combined=$(git -C "$WORK" rev-parse "refs/heads/$base_branch")
assert_parent "$combined" "$devel"
assert_parent "$combined" "$local_old"
assert_parent "$combined" "$remote_extra"
assert_remote_ref "refs/heads/$base_branch" "$combined"
assert_no_private_paths "$combined"

# A local-only public parent is rejected when any intermediate commit is private,
# even if the branch tip itself has a clean public tree.
private_parent_branch=gh/private-intermediate
private_intermediate=$(printf '%s\n' 'private intermediate' | git -C "$WORK" commit-tree "$source_commit^{tree}" -p "$devel")
private_parent_tip=$(printf '%s\n' 'clean public tip' | git -C "$WORK" commit-tree "$combined^{tree}" -p "$private_intermediate")
git -C "$WORK" update-ref "refs/heads/$private_parent_branch" "$private_parent_tip"
if run_publisher -b "$private_parent_branch" -r "$source_commit" -m 'Portability CI snapshot' >/dev/null 2>&1; then
    fail 'private intermediate local parent was accepted'
fi
assert_ref "refs/heads/$private_parent_branch" "$private_parent_tip"
if run_with_git_wrapper "$revlist_bin" env GIT_REAL="$revlist_real" \
    "$SCRIPT" -b "$private_parent_branch" -r "$source_commit" -m 'Portability CI snapshot' >/dev/null 2>&1; then
    fail 'publisher accepted a failed parent-history inspection'
fi

# A failed tree inspection must not be treated as an empty, clean tree.
if run_with_git_wrapper "$inspect_bin" env GIT_REAL="$inspect_real" \
    "$SCRIPT" -b gh/inspection-failure -r "$source_commit" -m 'Portability CI snapshot' >/dev/null 2>&1; then
    fail 'publisher accepted a failed tree inspection'
fi
if git -C "$WORK" show-ref --verify --quiet refs/heads/gh/inspection-failure; then
    fail 'failed publisher inspection created a snapshot ref'
fi

# Different public commits with the same tree remain a no-op with valid ancestry.
same_tree_remote=$(printf '%s\n' 'same tree, different history' | git -C "$WORK" commit-tree "$combined^{tree}" -p "$combined")
push_fixture "$same_tree_remote:refs/heads/$base_branch"
before_noop=$combined

noop_devel_tree=$(git -C "$WORK" rev-parse "github/devel^{tree}")
noop_devel_next=$(printf '%s\n' 'no-op devel race' | git -C "$WORK" commit-tree "$noop_devel_tree" -p github/devel)
push_fixture "$noop_devel_next:refs/heads/noop-devel-object"
noop_devel_bin="$FIXTURE/noop-devel-bin"
mkdir -p "$noop_devel_bin"
noop_devel_wrapper="$noop_devel_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *"refs/heads/devel:refs/remotes/github/devel"*)' \
    '        "$GIT_REAL" "$@"' \
    '        status=$?' \
    '        if [ "$status" -eq 0 ]; then' \
    '            if ! "$GIT_REAL" --git-dir "$PUBLIC_REPO" update-ref refs/heads/devel "$ADVANCE_SHA"; then exit 128; fi' \
    '        fi' \
    '        exit "$status"' \
    '        ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$noop_devel_wrapper"
chmod +x "$noop_devel_wrapper"
noop_devel_real=$(command -v git)
noop_devel_error="$FIXTURE/noop-devel.error"
if run_with_git_wrapper "$noop_devel_bin" env GIT_REAL="$noop_devel_real" PUBLIC_REPO="$PUBLIC" \
    ADVANCE_SHA="$noop_devel_next" "$SCRIPT" -b "$base_branch" -r "$source_commit" \
    -m 'Portability CI snapshot' --push >/dev/null 2>"$noop_devel_error"; then
    fail 'no-op accepted a devel race after discovery'
fi
grep -F 'github/devel changed during publication' "$noop_devel_error" >/dev/null \
    || fail 'no-op devel race did not fail its live SHA check'
assert_ref "refs/heads/$base_branch" "$before_noop"
assert_remote_ref "refs/heads/$base_branch" "$same_tree_remote"
if ! git --git-dir="$PUBLIC" update-ref refs/heads/devel "$devel"; then
    fail 'could not restore devel after no-op race'
fi
git -C "$WORK" update-ref refs/remotes/github/devel "$devel"

noop_branch_bin="$FIXTURE/noop-branch-bin"
mkdir -p "$noop_branch_bin"
noop_branch_wrapper="$noop_branch_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *"refs/heads/$NOOP_BRANCH:refs/remotes/github/$NOOP_BRANCH"*)' \
    '        "$GIT_REAL" "$@"' \
    '        status=$?' \
    '        if [ "$status" -eq 0 ]; then' \
    '            if ! "$GIT_REAL" --git-dir "$PUBLIC_REPO" update-ref -d "refs/heads/$NOOP_BRANCH"; then exit 128; fi' \
    '        fi' \
    '        exit "$status"' \
    '        ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$noop_branch_wrapper"
chmod +x "$noop_branch_wrapper"
noop_branch_real=$(command -v git)
noop_branch_error="$FIXTURE/noop-branch.error"
if run_with_git_wrapper "$noop_branch_bin" env GIT_REAL="$noop_branch_real" PUBLIC_REPO="$PUBLIC" \
    NOOP_BRANCH="$base_branch" "$SCRIPT" -b "$base_branch" -r "$source_commit" \
    -m 'Portability CI snapshot' --push >/dev/null 2>"$noop_branch_error"; then
    fail 'no-op accepted a remote branch deletion after discovery'
fi
if ! grep -F "github/$base_branch changed before push" "$noop_branch_error" >/dev/null; then
    fail 'no-op branch race did not fail its lease check'
fi
assert_ref "refs/heads/$base_branch" "$before_noop"
assert_remote_absent "refs/heads/$base_branch"
push_fixture "$same_tree_remote:refs/heads/$base_branch"
run_publisher -b "$base_branch" -r "$source_commit" -m 'Portability CI snapshot' --push >/dev/null
assert_ref "refs/heads/$base_branch" "$before_noop"
assert_remote_ref "refs/heads/$base_branch" "$same_tree_remote"

# An older remote public branch is rejected when it is not descended from the
# newly fetched devel head.
devel_tree=$(git -C "$WORK" rev-parse "github/devel^{tree}")
devel_next=$(printf '%s\n' 'public devel advance' | git -C "$WORK" commit-tree "$devel_tree" -p github/devel)
push_fixture "$devel_next:refs/heads/devel"
older_branch=gh/older-fork
git -C "$WORK" update-ref "refs/heads/$older_branch" "$before_noop"
push_fixture "$before_noop:refs/heads/$older_branch"
older_status=$(git -C "$WORK" status --porcelain)
if run_publisher -b "$older_branch" -r "$source_commit" -m 'Portability CI snapshot' --push >/dev/null 2>&1; then
    fail 'stale remote public parent was accepted'
fi
assert_remote_ref "refs/heads/$older_branch" "$before_noop"
[ "$(git -C "$WORK" status --porcelain)" = "$older_status" ] \
    || fail 'stale remote parent changed the worktree'

stale_local_branch=gh/stale-local-authority
stale_remote_head=$(printf '%s\n' 'remote authority' | git -C "$WORK" commit-tree "$before_noop^{tree}" -p "$devel_next")
git -C "$WORK" update-ref "refs/heads/$stale_local_branch" "$before_noop"
push_fixture "$stale_remote_head:refs/heads/$stale_local_branch"
if ! run_publisher -b "$stale_local_branch" -r "$source_commit" -m 'Portability CI snapshot' --push >/dev/null 2>&1; then
    fail 'clean stale local parent blocked the authoritative remote parent'
fi
stale_authority_commit=$(git -C "$WORK" rev-parse "refs/heads/$stale_local_branch")
assert_parent "$stale_authority_commit" "$devel_next"
assert_parent "$stale_authority_commit" "$stale_remote_head"
assert_no_parent "$stale_authority_commit" "$before_noop"
assert_remote_ref "refs/heads/$stale_local_branch" "$stale_authority_commit"
assert_no_private_paths "$stale_authority_commit"

legacy_remote_branch=gh/legacy-remote
legacy_remote_commit=$(printf '%s\n' 'legacy remote baseline' | git -C "$WORK" commit-tree "$devel_tree" -p "$before_noop")
push_fixture "$legacy_remote_commit:refs/heads/$legacy_remote_branch"
legacy_warning="$FIXTURE/legacy-remote-warning"
if run_publisher -b "$legacy_remote_branch" -r "$source_commit" -m 'Portability CI snapshot' --push \
    >/dev/null 2>"$legacy_warning"; then
    fail 'legacy files in a remote public baseline were accepted'
fi
assert_remote_ref "refs/heads/$legacy_remote_branch" "$legacy_remote_commit"

# An ls-remote transport failure is not the same as an absent branch.
discovery_branch=gh/discovery-failure
run_publisher -b "$discovery_branch" -r "$source_commit" -m 'Portability CI snapshot' >/dev/null
discovery_old=$(git -C "$WORK" rev-parse "refs/heads/$discovery_branch")
push_fixture "$discovery_old:refs/heads/$discovery_branch"
discovery_bin="$FIXTURE/discovery-bin"
mkdir -p "$discovery_bin"
discovery_wrapper="$discovery_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *"ls-remote"*"refs/heads/$FAIL_BRANCH"*) exit 128 ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$discovery_wrapper"
chmod +x "$discovery_wrapper"
discovery_real=$(command -v git)
if run_with_git_wrapper "$discovery_bin" env GIT_REAL="$discovery_real" FAIL_BRANCH="$discovery_branch" \
    "$SCRIPT" -b "$discovery_branch" -r "$source_commit" -m 'Portability CI snapshot' --push >/dev/null 2>&1; then
    fail 'remote discovery transport failure was treated as an absent branch'
fi
assert_ref "refs/heads/$discovery_branch" "$discovery_old"
assert_remote_ref "refs/heads/$discovery_branch" "$discovery_old"

# A discovered branch whose fetch fails is also a hard failure, not absence.
fetch_failure_branch=gh/fetch-failure
run_publisher -b "$fetch_failure_branch" -r "$source_commit" -m 'Portability CI snapshot' >/dev/null
fetch_failure_old=$(git -C "$WORK" rev-parse "refs/heads/$fetch_failure_branch")
push_fixture "$fetch_failure_old:refs/heads/$fetch_failure_branch"
fetch_bin="$FIXTURE/fetch-bin"
mkdir -p "$fetch_bin"
fetch_wrapper="$fetch_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'case " $* " in' \
    '    *"fetch"*"refs/heads/$FAIL_BRANCH:refs/remotes/github/$FAIL_BRANCH"*) exit 128 ;;' \
    'esac' \
    'exec "$GIT_REAL" "$@"' > "$fetch_wrapper"
chmod +x "$fetch_wrapper"
fetch_real=$(command -v git)
if run_with_git_wrapper "$fetch_bin" env GIT_REAL="$fetch_real" FAIL_BRANCH="$fetch_failure_branch" \
    "$SCRIPT" -b "$fetch_failure_branch" -r "$source_commit" -m 'Portability CI snapshot' --push >/dev/null 2>&1; then
    fail 'remote branch fetch failure was accepted'
fi
assert_ref "refs/heads/$fetch_failure_branch" "$fetch_failure_old"
assert_remote_ref "refs/heads/$fetch_failure_branch" "$fetch_failure_old"

# Set up a current branch whose second fetch can observe a devel race.
devel_race_branch=gh/devel-race
run_publisher -b "$devel_race_branch" -r "$source_commit" -m 'Portability CI snapshot' >/dev/null
devel_race_old=$(git -C "$WORK" rev-parse "refs/heads/$devel_race_branch")
push_fixture "$devel_race_old:refs/heads/$devel_race_branch"
printf '%s\n' 'devel race source' >> "$WORK/public.txt"
git -C "$WORK" add public.txt
git -C "$WORK" commit -q -m 'devel race source'
devel_race_source=$(git -C "$WORK" rev-parse HEAD)
devel_race_tree=$(git -C "$WORK" rev-parse "github/devel^{tree}")
devel_race_next=$(printf '%s\n' 'devel race' | git -C "$WORK" commit-tree "$devel_race_tree" -p github/devel)
push_fixture "$devel_race_next:refs/heads/devel-race-object"
race_bin="$FIXTURE/race-bin"
mkdir -p "$race_bin"
race_wrapper="$race_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'ADVANCE_AFTER=0' \
    'case " $* " in' \
    '    *"refs/heads/devel:refs/remotes/github/devel"*)' \
    '        count=0' \
    '        [ -f "$FETCH_COUNTER" ] && count=$(wc -l < "$FETCH_COUNTER")' \
    '        count=$((count + 1))' \
    '        printf "%s\\n" "$count" > "$FETCH_COUNTER"' \
    '        [ "$count" -eq 2 ] && ADVANCE_AFTER=1' \
    '        ;;' \
    'esac' \
    '"$GIT_REAL" "$@"' \
    'status=$?' \
    'if [ "$ADVANCE_AFTER" -eq 1 ]; then' \
    '    if ! "$GIT_REAL" --git-dir "$PUBLIC_REPO" update-ref refs/heads/devel "$ADVANCE_SHA"; then exit 128; fi' \
    'fi' \
    'exit "$status"' > "$race_wrapper"
chmod +x "$race_wrapper"
race_status=$(git -C "$WORK" status --porcelain)
race_real=$(command -v git)
race_counter="$FIXTURE/race-fetch-count"
if run_with_git_wrapper "$race_bin" env GIT_REAL="$race_real" PUBLIC_REPO="$PUBLIC" \
    ADVANCE_SHA="$devel_race_next" FETCH_COUNTER="$race_counter" \
    "$SCRIPT" -b "$devel_race_branch" -r "$devel_race_source" -m 'Portability CI snapshot' --push >/dev/null 2>&1; then
    fail 'devel race was not rejected'
fi
devel_race_local=$(git -C "$WORK" rev-parse "refs/heads/$devel_race_branch")
[ "$devel_race_local" != "$devel_race_old" ] || fail 'devel race did not retain the retry commit locally'
assert_remote_ref "refs/heads/$devel_race_branch" "$devel_race_old"
[ "$(git -C "$WORK" status --porcelain)" = "$race_status" ] \
    || fail 'devel race changed the worktree'

# A concurrent remote branch update must make the non-force push fail.
public_race_branch=gh/public-race
run_publisher -b "$public_race_branch" -r "$devel_race_source" -m 'Portability CI snapshot' >/dev/null
public_race_old=$(git -C "$WORK" rev-parse "refs/heads/$public_race_branch")
push_fixture "$public_race_old:refs/heads/$public_race_branch"
printf '%s\n' 'public branch race source' >> "$WORK/public.txt"
git -C "$WORK" add public.txt
git -C "$WORK" commit -q -m 'public branch race source'
public_race_source=$(git -C "$WORK" rev-parse HEAD)
public_race_tree=$(git -C "$WORK" rev-parse "$public_race_old^{tree}")
public_race_remote=$(printf '%s\n' 'remote branch race' | git -C "$WORK" commit-tree "$public_race_tree" -p "$public_race_old")
push_fixture "$public_race_remote:refs/heads/public-race-object"
receive_wrapper="$FIXTURE/receive-pack-race"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'if ! git --git-dir="$1" update-ref "refs/heads/$PUBLIC_RACE_BRANCH" "$RACE_SHA"; then exit 128; fi' \
    'exec git-receive-pack "$@"' > "$receive_wrapper"
chmod +x "$receive_wrapper"
git -C "$WORK" config remote.github.receivepack "$receive_wrapper"
public_race_status=$(git -C "$WORK" status --porcelain)
if PUBLIC_RACE_BRANCH="$public_race_branch" RACE_SHA="$public_race_remote" \
    run_publisher -b "$public_race_branch" -r "$public_race_source" -m 'Portability CI snapshot' --push >/dev/null 2>&1; then
    fail 'public branch race was not rejected'
fi
public_race_local=$(git -C "$WORK" rev-parse "refs/heads/$public_race_branch")
[ "$public_race_local" != "$public_race_old" ] || fail 'local public ref was not retained for retry'
assert_remote_ref "refs/heads/$public_race_branch" "$public_race_remote"
[ "$(git -C "$WORK" status --porcelain)" = "$public_race_status" ] \
    || fail 'public branch race changed the worktree'
git -C "$WORK" config --unset remote.github.receivepack

# A live branch deletion or rewind between discovery and push must fail the
# publisher's lease check rather than being silently recreated or advanced.
lease_bin="$FIXTURE/lease-bin"
mkdir -p "$lease_bin"
lease_wrapper="$lease_bin/git"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'LEASE_MUTATE=0' \
    'case " $* " in' \
    '    *"ls-remote"*"refs/heads/$LEASE_BRANCH"*)' \
    '        count=0' \
    '        [ -f "$LEASE_COUNTER" ] && count=$(wc -l < "$LEASE_COUNTER")' \
    '        count=$((count + 1))' \
    '        printf "%s\\n" "$count" > "$LEASE_COUNTER"' \
    '        [ "$count" -eq 2 ] && LEASE_MUTATE=1' \
    '        ;;' \
    'esac' \
    '"$GIT_REAL" "$@"' \
    'status=$?' \
    'if [ "$LEASE_MUTATE" -eq 1 ] && [ "$status" -eq 0 ]; then' \
    '    case "$LEASE_MODE" in' \
    '        delete) if ! "$GIT_REAL" --git-dir "$PUBLIC_REPO" update-ref -d "refs/heads/$LEASE_BRANCH"; then exit 128; fi ;;' \
    '        rewind) if ! "$GIT_REAL" --git-dir "$PUBLIC_REPO" update-ref "refs/heads/$LEASE_BRANCH" "$LEASE_SHA"; then exit 128; fi ;;' \
    '    esac' \
    'fi' \
    'exit "$status"' > "$lease_wrapper"
chmod +x "$lease_wrapper"
lease_real=$(command -v git)

lease_delete_branch=gh/lease-delete-race
run_publisher -b "$lease_delete_branch" -r "$source_commit" -m 'Portability CI snapshot' >/dev/null
lease_delete_old=$(git -C "$WORK" rev-parse "refs/heads/$lease_delete_branch")
push_fixture "$lease_delete_old:refs/heads/$lease_delete_branch"
lease_delete_counter="$FIXTURE/lease-delete-count"
lease_delete_status=$(git -C "$WORK" status --porcelain)
if run_with_git_wrapper "$lease_bin" env GIT_REAL="$lease_real" PUBLIC_REPO="$PUBLIC" \
    LEASE_BRANCH="$lease_delete_branch" LEASE_COUNTER="$lease_delete_counter" LEASE_MODE=delete \
    "$SCRIPT" -b "$lease_delete_branch" -r "$devel_race_source" -m 'Portability CI snapshot' --push \
    >/dev/null 2>&1; then
    fail 'remote branch deletion race was not rejected'
fi
assert_remote_absent "refs/heads/$lease_delete_branch"
[ "$(git -C "$WORK" status --porcelain)" = "$lease_delete_status" ] \
    || fail 'remote branch deletion race changed the worktree'

lease_rewind_branch=gh/lease-rewind-race
run_publisher -b "$lease_rewind_branch" -r "$source_commit" -m 'Portability CI snapshot' >/dev/null
lease_rewind_old=$(git -C "$WORK" rev-parse "refs/heads/$lease_rewind_branch")
push_fixture "$lease_rewind_old:refs/heads/$lease_rewind_branch"
lease_rewind_counter="$FIXTURE/lease-rewind-count"
lease_rewind_status=$(git -C "$WORK" status --porcelain)
if run_with_git_wrapper "$lease_bin" env GIT_REAL="$lease_real" PUBLIC_REPO="$PUBLIC" \
    LEASE_BRANCH="$lease_rewind_branch" LEASE_COUNTER="$lease_rewind_counter" \
    LEASE_MODE=rewind LEASE_SHA="$devel_next" \
    "$SCRIPT" -b "$lease_rewind_branch" -r "$devel_race_source" -m 'Portability CI snapshot' --push \
    >/dev/null 2>&1; then
    fail 'remote branch rewind race was not rejected'
fi
assert_remote_ref "refs/heads/$lease_rewind_branch" "$devel_next"
[ "$(git -C "$WORK" status --porcelain)" = "$lease_rewind_status" ] \
    || fail 'remote branch rewind race changed the worktree'

# A successful push is verified immediately afterward; a post-receive change is
# reported, but this check cannot make Git's push and verification atomic.
post_push_branch=gh/post-push-verification
run_publisher -b "$post_push_branch" -r "$source_commit" -m 'Portability CI snapshot' >/dev/null
post_push_local=$(git -C "$WORK" rev-parse "refs/heads/$post_push_branch")
post_push_tree=$(git -C "$WORK" rev-parse "$post_push_local^{tree}")
post_push_other=$(printf '%s\n' 'post-push interference' | git -C "$WORK" commit-tree "$post_push_tree" -p "$post_push_local")
push_fixture "$post_push_other:refs/heads/post-push-object"
post_receive_wrapper="$FIXTURE/receive-pack-post-push"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'git-receive-pack "$@"' \
    'status=$?' \
    'if [ "$status" -eq 0 ]; then' \
    '    if ! git --git-dir="$1" update-ref "refs/heads/$POST_PUSH_BRANCH" "$POST_PUSH_SHA"; then exit 128; fi' \
    'fi' \
    'exit "$status"' > "$post_receive_wrapper"
chmod +x "$post_receive_wrapper"
git -C "$WORK" config remote.github.receivepack "$post_receive_wrapper"
export POST_PUSH_BRANCH="$post_push_branch"
export POST_PUSH_SHA="$post_push_other"
if run_publisher -b "$post_push_branch" -r "$source_commit" -m 'Portability CI snapshot' --push >/dev/null 2>&1; then
    fail 'post-push remote mismatch was not reported'
fi
assert_remote_ref "refs/heads/$post_push_branch" "$post_push_other"
unset POST_PUSH_BRANCH POST_PUSH_SHA
git -C "$WORK" config --unset remote.github.receivepack

public_checkout="$FIXTURE/public-checkout"
git clone -q -b "$dirty_push_branch" "$PUBLIC" "$public_checkout"
printf '%s\n' '[untracked target](untracked-target.md)' >> "$public_checkout/README.md"
printf '%s\n' 'untracked target' > "$public_checkout/untracked-target.md"
if "$ROOT/scripts/check-public-tree.sh" "$public_checkout" >/dev/null 2>&1; then
    fail 'Git checkout accepted an untracked Markdown target'
fi
git -C "$public_checkout" checkout -- README.md
rm -f "$public_checkout/untracked-target.md"
rm -f "$public_checkout/docs/adr/0001-storage-engine.md"
"$ROOT/scripts/check-public-tree.sh" "$public_checkout"

archive="$FIXTURE/archive"
mkdir -p "$archive"
git -C "$WORK" archive "$dirty_push_commit" | tar -x -C "$archive"
printf '%s\n' 'private archive marker' > "$archive/AGENTS.md"
if "$ROOT/scripts/check-public-tree.sh" "$archive" >/dev/null 2>&1; then
    fail 'archive private-path check accepted AGENTS.md'
fi
rm -f "$archive/AGENTS.md"
ln -s missing-private-target "$archive/AGENTS.md"
if "$ROOT/scripts/check-public-tree.sh" "$archive" >/dev/null 2>&1; then
    fail 'archive private-path check accepted dangling AGENTS.md'
fi
rm -f "$archive/AGENTS.md"
printf '%s\n' '[archive target](archive-target.md)' >> "$archive/README.md"
printf '%s\n' 'archive target' > "$archive/archive-target.md"
"$ROOT/scripts/check-public-tree.sh" "$archive"

printf '%s\n' 'publish-github fixture tests passed'
