unset HISTFILE
PS1='%~ %# '
# Same 150 ms slow init, no instant prompt. TILDY_BIN_PATH and TILDY_DEBUG_LOG come from bench/run.py.
sleep 0.15
eval "$("$TILDY_BIN_PATH" init zsh)"
