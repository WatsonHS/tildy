# Instant prompt from a pre-generated file (no tildy spawn before the instant prompt):
# bench/run.py writes `tildy init zsh --instant` to $TILDY_INSTANT_FILE once per run.
source "$TILDY_INSTANT_FILE"
unset HISTFILE
PS1='%~ %# '
sleep 0.15
eval "$("$TILDY_BIN_PATH" init zsh)"
