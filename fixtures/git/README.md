# Git fixtures

`make.sh <dir>` builds one repository per scenario under `<dir>` (deterministic
author/dates, no network, global git config ignored). `expected/<scenario>.json`
holds the stable `GitData` truth: in_repo, branch, detached, unborn, ahead,
behind, staged, unstaged, untracked, conflicted, state, renamed, deleted,
stashed, operation, operation_progress. Paths and object ids are intentionally
omitted. `tests/providers_git.rs` builds the fixtures in a temp dir and
compares.

M1b scenarios: `deleted` (one staged and one unstaged deletion), `stash` (two
stash entries, clean worktree), `merge_in_progress` (non-conflicting
`merge --no-commit`, so `MERGE_HEAD` exists), `rebase_in_progress` (`git rebase`
stopped on a conflict at commit 1 of 2, `rebase-merge/` exists, progress
`[1, 2]`). `conflicted` is a conflicted merge, so its `operation` is `merge`.

Helper dirs also created: `linked_worktree_main`, `sub_origin`,
`ahead_behind_seed`, `ahead_behind_origin.git`.
