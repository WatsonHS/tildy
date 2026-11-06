unset HISTFILE
PS1='%~ %# '
# TILDY_BIN_PATH and TILDY_DEBUG_LOG come from the environment (bench/run.py).
eval "$("$TILDY_BIN_PATH" init zsh)"
