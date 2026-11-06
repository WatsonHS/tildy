# Tildy PTY harness .zshrc. Never sources the user's ~/.zshrc.
# Instant prompt scenarios: the snippet goes first (as users are told to put
# it), then a scenario-specific slow or noisy init.
if [[ -n ${TILDY_TEST_INSTANT:-} && -n ${TILDY_TEST_INIT_BIN:-} ]]; then
  zmodload zsh/datetime && print -r -- "$EPOCHREALTIME" >>| "${TILDY_TEST_TMP}/zshrc-start"
  eval "$("$TILDY_TEST_INIT_BIN" init zsh --instant)"
fi
[[ -n ${TILDY_TEST_PRE_INIT:-} ]] && eval "$TILDY_TEST_PRE_INIT"
setopt no_beep
unset RPROMPT RPS1 PROMPT_COMMAND
PS1='%~ %# '
# History: private file, seeded by the harness.
HISTFILE=${TILDY_TEST_HISTFILE:-/dev/null}
HISTSIZE=1000
SAVEHIST=1000
# Completion with a persistent selectable menu (used by completion scenarios).
autoload -Uz compinit
compinit -u -d "${TILDY_TEST_TMP:-${TMPDIR:-/tmp}}/zcompdump"
zstyle ':completion:*' menu select
setopt menu_complete

# Optional plugins, only when requested.
_plugdir=${TILDY_TEST_PLUGIN_DIR:-$HOME/.oh-my-zsh/custom/plugins}
case ",${TILDY_TEST_PLUGINS}," in
  *,autosuggestions,*)
    source "$_plugdir/zsh-autosuggestions/zsh-autosuggestions.zsh" ;;
esac
case ",${TILDY_TEST_PLUGINS}," in
  *,syntax-highlighting,*)
    source "$_plugdir/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh" ;;
esac
case ",${TILDY_TEST_PLUGINS}," in
  *,fzf,*)
    source "$ZDOTDIR/../../../scripts/fzf.zsh" || print -u2 'tildy harness: fzf integration failed' ;;
esac
unset _plugdir

if [[ -n ${TILDY_TEST_INIT_BIN:-} ]]; then
  # Real-binary mode: exactly what a user puts in .zshrc.
  eval "$("$TILDY_TEST_INIT_BIN" init zsh)"
elif [[ -r ${TILDY_SCRIPT:-} ]]; then
  source "$TILDY_SCRIPT"
else
  print -u2 "tildy harness: TILDY_SCRIPT not readable: ${TILDY_SCRIPT:-<unset>}"
fi
# Scenario-specific tweak after loading (e.g. a bogus TILDY_VERSION).
[[ -n ${TILDY_TEST_POST_INIT:-} ]] && eval "$TILDY_TEST_POST_INIT"
