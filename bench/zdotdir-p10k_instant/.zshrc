# p10k instant prompt ON (the preamble p10k configure writes), then the same 150 ms slow init.
# XDG_CACHE_HOME points at a bench temp dir (bench/run.py), never the user's ~/.cache.
if [[ -r "${XDG_CACHE_HOME:-$HOME/.cache}/p10k-instant-prompt-${(%):-%n}.zsh" ]]; then
  source "${XDG_CACHE_HOME:-$HOME/.cache}/p10k-instant-prompt-${(%):-%n}.zsh"
fi
unset HISTFILE
sleep 0.15
typeset -g POWERLEVEL9K_DISABLE_CONFIGURATION_WIZARD=true
source ~/.oh-my-zsh/custom/themes/powerlevel10k/powerlevel10k.zsh-theme
source "${ZDOTDIR}/.p10k.zsh"
