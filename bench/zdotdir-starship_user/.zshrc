unset HISTFILE
export STARSHIP_CONFIG="${ZDOTDIR}/starship.toml"
eval "$(starship init zsh)"
# Sentinel appended after starship has installed its prompt (no extra process cost).
PROMPT="${PROMPT}SKRDY "
