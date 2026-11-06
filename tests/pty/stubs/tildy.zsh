# STUB tildy.zsh for harness self-test only (NOT the real implementation).
# Minimal async oneshot implementation of docs/architecture.md "Shell 协议 v0".
zmodload zsh/datetime zsh/system 2>/dev/null
autoload -Uz add-zsh-hook add-zle-hook-widget
typeset -g _SK_GEN=${_SK_GEN:-0} _SK_SID=${_SK_SID:-$$.$RANDOM}
typeset -g _SK_PEND_GEN= _SK_PEND_L= _SK_PEND_R= _SK_LASTEXIT=0
typeset -gA _SK_FDGEN _SK_FDPID _SK_FDT0

_sk_log() {
  [[ -n $TILDY_DEBUG_LOG ]] || return 0
  local ts IFS=$'\t'
  printf -v ts '%.0f' $(( EPOCHREALTIME * 1000 ))
  print -r -- "${ts}${IFS}$*" >> "$TILDY_DEBUG_LOG"
}

# $CONTEXT/$KEYMAP/$WIDGET are empty inside zle -F handlers, so track long-running widgets ourselves.
typeset -g _SK_BUSY=
_sk_safe() { [[ -z $_SK_BUSY ]] }
if (( ! $+widgets[_sk_orig_expand-or-complete] )); then
  for _w in expand-or-complete complete-word menu-complete menu-expand-or-complete \
            history-incremental-search-backward history-incremental-search-forward; do
    zle -A $_w _sk_orig_$_w || continue
    eval "_sk_w_${_w//-/_}() { local prev=\$_SK_BUSY; _SK_BUSY=$_w; zle _sk_orig_$_w -- \"\$@\"; local rc=\$?; _SK_BUSY=\$prev; return \$rc }"
    zle -N $_w _sk_w_${_w//-/_}
  done
  unset _w
fi

_sk_request() {
  local gen=$_SK_GEN fd req cwd=${PWD//\\/\\\\} timeout pl
  cwd=${cwd//$'\t'/\\t}; cwd=${cwd//$'\n'/\\n}
  req="REQ"$'\t'"$gen"$'\t'"$_SK_SID"$'\t'"$cwd"$'\t'"$COLUMNS"$'\t'"$LINES"$'\t'"$_SK_LASTEXIT"$'\t'"0"
  timeout=$(( ${TILDY_BACKEND_TIMEOUT_MS:-5000} / 1000.0 ))
  _sk_log send $gen $PWD
  exec {fd}< <(
    p=$sysparams[pid]
    print -r -- "PID $p"
    { sleep $timeout; kill $p } >/dev/null 2>&1 &!
    exec ${TILDY_BACKEND:-bench/fake-backend.sh} <<< "$req"
  )
  read -r -t 2 -u $fd pl
  _SK_FDGEN[$fd]=$gen; _SK_FDPID[$fd]=${pl#PID }; _SK_FDT0[$fd]=$EPOCHREALTIME
  _sk_log backend_start $gen ${pl#PID } oneshot
  zle -F $fd _sk_on_fd
}

_sk_close_fd() {
  local fd=$1
  zle -F $fd 2>/dev/null
  exec {fd}<&-
  unset "_SK_FDGEN[$fd]" "_SK_FDPID[$fd]" "_SK_FDT0[$fd]"
}

_sk_apply() {  # $1 left $2 right
  PROMPT=$1; RPROMPT=$2
  zle && zle reset-prompt
}

_sk_on_fd() {
  local fd=$1 l
  local fgen=${_SK_FDGEN[$fd]} fpid=${_SK_FDPID[$fd]} t0=${_SK_FDT0[$fd]}
  local -a lines f
  while IFS= read -r -t 0.3 -u $fd l; do lines+=("$l"); done
  _sk_close_fd $fd
  local elapsed=$(( (EPOCHREALTIME - t0) * 1000 )) tmo=${TILDY_BACKEND_TIMEOUT_MS:-5000}
  local resgen= rstat= endgen= left= right=
  for l in "${lines[@]}"; do
    f=("${(@ps:\t:)l}")
    case $f[1] in
      RES) resgen=$f[2]; rstat=$f[3] ;;
      L) left=${l#L$'\t'} ;;
      R) right=${l#R$'\t'} ;;
      END) endgen=$f[2] ;;
    esac
  done
  _sk_log backend_exit $fgen $fpid 0
  if [[ -z $resgen ]]; then
    if (( elapsed >= tmo * 0.9 )); then _sk_log discard $fgen timeout; else _sk_log discard $fgen eof; fi
    return 0
  fi
  _sk_log recv $resgen $rstat
  if [[ -z $endgen ]]; then _sk_log discard $resgen half_frame; return 0; fi
  if [[ $endgen != $resgen ]]; then _sk_log discard $resgen bad_frame; return 0; fi
  if (( resgen != _SK_GEN )); then _sk_log discard $resgen stale; return 0; fi
  if _sk_safe; then
    _sk_apply "$left" "$right"; _sk_log accept $resgen
  else
    _SK_PEND_GEN=$resgen _SK_PEND_L=$left _SK_PEND_R=$right
    _sk_log defer $resgen "widget=$_SK_BUSY"
  fi
}

_sk_pre_redraw() {
  [[ -n $_SK_PEND_GEN ]] || return 0
  _sk_safe || return 0
  local g=$_SK_PEND_GEN l=$_SK_PEND_L r=$_SK_PEND_R
  _SK_PEND_GEN= _SK_PEND_L= _SK_PEND_R=
  if (( g != _SK_GEN )); then _sk_log discard $g stale; return 0; fi
  PROMPT=$l; RPROMPT=$r
  _sk_log apply_deferred $g; _sk_log accept $g
  zle reset-prompt
}

_sk_kill_inflight() {
  local fd
  for fd in ${(k)_SK_FDGEN}; do
    kill ${_SK_FDPID[$fd]} 2>/dev/null
    _sk_log backend_exit ${_SK_FDGEN[$fd]} ${_SK_FDPID[$fd]} 143
    _sk_close_fd $fd
  done
}

_sk_precmd() {
  _SK_LASTEXIT=$?
  PROMPT='%~ %# '; RPROMPT=
  [[ -n $_SK_PEND_GEN ]] && { _sk_log discard $_SK_PEND_GEN stale; _SK_PEND_GEN= }
  (( _SK_GEN++ ))
  _sk_request
}

_sk_chpwd() { (( _SK_GEN++ )); _sk_kill_inflight }

if (( ! ${precmd_functions[(Ie)_sk_precmd]:-0} )); then
  add-zsh-hook precmd _sk_precmd; _sk_log hook_installed precmd
fi
if (( ! ${chpwd_functions[(Ie)_sk_chpwd]:-0} )); then
  add-zsh-hook chpwd _sk_chpwd; _sk_log hook_installed chpwd
fi
add-zle-hook-widget line-pre-redraw _sk_pre_redraw 2>/dev/null
if (( ! $+functions[TRAPWINCH] )); then
  TRAPWINCH() { (( _SK_GEN++ )); _sk_request; return 0 }
  _sk_log hook_installed winch
fi
