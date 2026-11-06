#!/bin/sh
# Build deterministic git fixture repositories under directory $1.
# Usage: make.sh <empty-or-new-dir>
# No network. Global/system git config is ignored.
set -eu
[ $# -eq 1 ] || { echo "usage: $0 <dir>" >&2; exit 2; }
mkdir -p "$1"
ROOT=$(cd "$1" && pwd -P)

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=Fixture GIT_AUTHOR_EMAIL=fixture@example.invalid
export GIT_COMMITTER_NAME=Fixture GIT_COMMITTER_EMAIL=fixture@example.invalid
export GIT_AUTHOR_DATE="2024-01-01T00:00:00+0000" GIT_COMMITTER_DATE="2024-01-01T00:00:00+0000"
export GIT_TERMINAL_PROMPT=0

g() { git -c init.defaultBranch=main -c commit.gpgsign=false -c protocol.file.allow=always "$@"; }

# new_repo <name>: init with one commit containing a.txt and b.txt
new_repo() {
  mkdir -p "$ROOT/$1"
  cd "$ROOT/$1"
  g init -q .
  echo a > a.txt
  echo b > b.txt
  g add a.txt b.txt
  g commit -q -m init
}

mkdir -p "$ROOT/not_a_repo"

new_repo clean

new_repo dirty_unstaged
echo changed >> a.txt

new_repo staged
echo changed >> a.txt
echo new > n.txt
g add a.txt n.txt

new_repo untracked
echo u1 > u1.txt
echo u2 > u2.txt

new_repo mixed
echo changed >> a.txt
g add a.txt
echo changed >> b.txt
echo u > u.txt

new_repo conflicted
g checkout -q -b side
echo side > a.txt
g commit -q -a -m side
g checkout -q main
echo main > a.txt
g commit -q -a -m main
g merge side >/dev/null 2>&1 || true

new_repo detached
g checkout -q --detach

mkdir -p "$ROOT/unborn"
cd "$ROOT/unborn"
g init -q .

new_repo linked_worktree_main
g worktree add -q -b wt "$ROOT/linked_worktree"

# submodule: superproject with a committed sub repo
mkdir -p "$ROOT/sub_origin"
cd "$ROOT/sub_origin"
g init -q .
echo s > s.txt
g add s.txt
g commit -q -m sub
new_repo submodule
g submodule add -q "$ROOT/sub_origin" sub >/dev/null 2>&1
g commit -q -m "add sub"

new_repo renamed
g mv a.txt c.txt

# deleted: one staged deletion, one unstaged deletion
new_repo deleted
g rm -q a.txt
rm b.txt

# stash: two stash entries, clean worktree
new_repo stash
echo s1 >> a.txt
g stash -q
echo s2 >> b.txt
g stash -q

# merge_in_progress: non-conflicting merge stopped before its commit (MERGE_HEAD)
new_repo merge_in_progress
g checkout -q -b side
echo side > s.txt
g add s.txt
g commit -q -m side
g checkout -q main
echo main > m.txt
g add m.txt
g commit -q -m main
g merge -q --no-ff --no-commit side >/dev/null 2>&1

# rebase_in_progress: `git rebase` stopped on a conflict at commit 1 of 2
# (merge backend, so rebase-merge/ exists; the sequence editor is never opened)
new_repo rebase_in_progress
g checkout -q -b side
echo side > a.txt
g commit -q -a -m "side 1"
echo s2 > s2.txt
g add s2.txt
g commit -q -m "side 2"
g checkout -q main
echo main > a.txt
g commit -q -a -m main
g checkout -q side
GIT_SEQUENCE_EDITOR=true GIT_EDITOR=true g rebase main >/dev/null 2>&1 || true

# ahead_behind: clone of a bare origin, diverged by one commit each way
g init -q --bare "$ROOT/ahead_behind_origin.git"
new_repo ahead_behind_seed
g remote add origin "$ROOT/ahead_behind_origin.git"
g push -q origin main 2>/dev/null
g clone -q "$ROOT/ahead_behind_origin.git" "$ROOT/ahead_behind"
cd "$ROOT/ahead_behind"
echo local > local.txt
g add local.txt
g commit -q -m local
cd "$ROOT/ahead_behind_seed"
echo remote > remote.txt
g add remote.txt
g commit -q -m remote
g push -q origin main 2>/dev/null
cd "$ROOT/ahead_behind"
g fetch -q origin
