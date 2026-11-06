# Disable, uninstall and recover

Remove the two blocks bounded by `# >>> tildy instant >>>` / `# <<< tildy instant <<<` and `# >>> tildy prompt >>>` / `# <<< tildy prompt <<<` from `.zshrc` (or `$ZDOTDIR/.zshrc`). If initialization was added manually, remove its `eval "$(tildy init zsh ...)"` lines instead. Re-enable the previous prompt if desired, then open a new terminal.

`init` saves a `.zshrc.tildy-backup-*` file beside the file it edits. Restore a backup only if it will not overwrite later shell edits; otherwise remove just the Tildy blocks. Symbolic links are handled by editing and backing up their target.

Once initialization is removed:

```sh
brew uninstall tildy
```

Configuration and history are retained. Optional cleanup locations are `$XDG_CONFIG_HOME/tildy` (default `~/.config/tildy`), `$XDG_STATE_HOME/tildy` (default `~/.local/state/tildy`) and `$XDG_CACHE_HOME/tildy` (default `~/.cache/tildy`). `TILDY_CONFIG` may point elsewhere. Preserve anything needed for rollback before removing it. Do not delete another process's configuration lock to force a write.

Already-open shells keep their loaded adapter until they exit. Workers exit with their parent shell; opening a fresh shell after removing integration verifies that no Tildy startup remains.
