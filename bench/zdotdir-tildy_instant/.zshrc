# Instant prompt first (documented form: one tildy spawn), then the same 150 ms slow init.
# TILDY_BIN_PATH, TILDY_DEBUG_LOG and XDG_CACHE_HOME (bench temp dir) come from bench/run.py.
eval "$("$TILDY_BIN_PATH" init zsh --instant)"
unset HISTFILE
PS1='%~ %# '
sleep 0.15
eval "$("$TILDY_BIN_PATH" init zsh)"
