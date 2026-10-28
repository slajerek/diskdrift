#!/usr/bin/env bash
set -euo pipefail

root=${1:-.}
cd "$root"
root=$PWD

git_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
git_mode=0
if [[ -n "$git_root" && "$git_root" == "$root" ]]; then
    git_mode=1
fi

# The public GitHub snapshot is produced without these private paths. Keep this
# check independent of the private publishing machinery so it also works on
# Windows runners under bash.
private_paths=(specs ROADMAP.md docs/handover docs/superpowers experiments reports research_notes .claude claude CLAUDE.md AGENTS.md)
for path in "${private_paths[@]}"; do
    if [[ -e "$path" || -L "$path" ]]; then
        printf 'private path present in public tree: %s\n' "$path" >&2
        exit 1
    fi
    if (( git_mode )); then
        tree_paths=$(git ls-tree -r --name-only HEAD -- "$path")
        if [[ -n "$tree_paths" ]]; then
            printf 'private path present in public Git tree: %s\n' "$path" >&2
            exit 1
        fi
    fi
done

# GitHub's Windows checkout can hide tracked Markdown targets from both the
# filesystem and Git path queries; Unix runners perform the complete check.
if [[ "${OS:-}" == "Windows_NT" ]]; then
    exit 0
fi

# Validate repository-local Markdown links that point at files or directories.
python3 - "$root" "$git_mode" <<'PY'
import pathlib, re, sys
import subprocess

root = pathlib.Path(sys.argv[1]).resolve()
git_mode = sys.argv[2] == "1"
tracked = set()
tree = set()
if git_mode:
    tree = set(subprocess.check_output(["git", "ls-tree", "-r", "--name-only", "HEAD"]).decode().splitlines())

def exists_in_public_tree(target):
    if git_mode:
        relative = target.relative_to(root).as_posix()
        return relative in tree or any(path.startswith(relative + "/") for path in tree)
    return target.exists() or target.is_symlink()

for path in root.rglob('*.md'):
    if any(part in {'target', '.git'} for part in path.relative_to(root).parts):
        continue
    text = path.read_text(encoding='utf-8')
    for match in re.finditer(r'\[[^]]+\]\(([^)]+)\)', text):
        value = match.group(1).strip().strip('<>')
        if value.startswith(('/', '#', 'mailto:')) or '://' in value:
            continue
        relative_value = value.split('#', 1)[0]
        if not relative_value:
            continue
        target = (path.parent / relative_value).resolve()
        try:
            target.relative_to(root)
        except ValueError:
            print(f"missing referenced public-tree path: {value}", file=sys.stderr)
            raise SystemExit(1)
        if not exists_in_public_tree(target):
            print(f"missing referenced public-tree path: {value}", file=sys.stderr)
            raise SystemExit(1)
PY
