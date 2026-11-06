# Tildy instant prompt. Goes at the very top of .zshrc, before plugin
# managers and anything slow:
#
#   eval "$(tildy init zsh --instant)"      # one tildy spawn per startup
#   # or, with no spawn: tildy init zsh --instant >| ~/.cache/tildy-instant.zsh
#   #                    source ~/.cache/tildy-instant.zsh
#
# It prints the prompt the worker cached in an earlier session for the same
# directory, terminal width, TERM_PROGRAM and tildy version, then lets the rest
# of .zshrc run with input echo off, stdin from /dev/null and stdout/stderr
# captured to a private temp file. The first precmd puts everything back,
# erases the instant prompt (zle draws the real one in the same place) and
# prints whatever init wrote. Decisions: docs/decisions/0006-instant-prompt.md.
#
# Environment:
#   TILDY_INSTANT_PROMPT   quiet (default) | verbose (one-line note when init
#                          wrote output) | off (this snippet does nothing)
#   TILDY_INSTANT_DISABLE  set to anything: same as off
#   TILDY_BASE_PROMPT      shown when nothing is cached (last line, default '%~ %# ')
#   TILDY_DEBUG_LOG        `instant` and `instant_done` events are appended here
#
# Interface with tildy.zsh (the main loader) and the worker:
#   _tildy_instant_key     function, REPLY = cache key; the only definition of
#                          the key (the shell sends it as TILDY_INSTANT_KEY in the
#                          REQ env, the worker just uses it)
#   _tildy_instant_dir     cache directory, exported to the worker as TILDY_INSTANT_DIR
#   _tildy_instant_finish  function, undoes everything (idempotent)
#   _tildy_instant_state   init (redirected, before the first precmd) | done
#   _tildy_instant_nl, _tildy_instant_left, _tildy_instant_right,
#   _tildy_instant_pwd, _tildy_instant_cols
#                          the cached prompt (L line count, L lines joined by
#                          newlines, R) and where it was shown; nl=0 on a miss
#   _tildy_instant_hint    on a miss: the L line count of the newest cache
#                          file for the same version and TERM_PROGRAM (another
#                          directory), 0 when there is none; the base prompt is
#                          padded to that height so the first result fits
#
# Parsed and run inside `() { emulate -L zsh -o no_aliases; eval ... }` by the
# init output, so options are local and aliases cannot touch it. Functions it
# defines run later under the user's options and start with `emulate -L zsh`.

if [[ ${TILDY_INSTANT_PROMPT:-quiet} != off && -z ${TILDY_INSTANT_DISABLE-} && -z ${TILDY_FONT_REVISION-} ]] &&
   (( ! ${+_tildy_instant_state} && ! ${+_tildy_loaded} )) &&
   [[ -o interactive && -o zle && -t 0 && -t 1 && -t 2 ]] &&
   [[ -z ${ZSH_EXECUTION_STRING-} && -z ${ZSH_SCRIPT-} && ${ZSH_SUBSHELL:-0} == 0 ]] &&
   [[ ${TERM:-dumb} != dumb ]] && zmodload zsh/system 2>/dev/null; then

typeset -g _tildy_instant_state=done
typeset -g _tildy_instant_dir= _tildy_instant_mode=${TILDY_INSTANT_PROMPT:-quiet}
typeset -g _tildy_instant_fd0= _tildy_instant_fd1= _tildy_instant_fd2= _tildy_instant_ofd=
typeset -g _tildy_instant_left= _tildy_instant_right= _tildy_instant_pwd=$PWD _tildy_instant_cols=${COLUMNS:-80}
typeset -gi _tildy_instant_nl=0 _tildy_instant_rows=0 _tildy_instant_lastw=0 _tildy_instant_echo=0 _tildy_instant_hint=0

# REPLY = cache key: tildy version, terminal width, TERM_PROGRAM (it picks the
# symbol tier) and a 32-bit FNV-1a hash of $PWD, reduced to [A-Za-z0-9._-]
# (the worker refuses any other byte). Called again by tildy.zsh per request.
_tildy_instant_key() {
  emulate -L zsh
  local p=$PWD c
  local -i h=2166136261 i
  for (( i = 1; i <= ${#p}; i++ )); do
    c=${p[i]}
    (( h = ((h ^ #c) * 16777619) & 0xffffffff ))
  done
  # No COLUMNS in the key (2026-10-20): every pane width would be a separate
  # cache and a miss; lines wider than the terminal simply wrap (rows are
  # counted from the measured width) and the right side is dropped when it
  # does not fit.
  REPLY="v${_tildy_instant_version:-0}_t${TERM_PROGRAM:-none}_${(l:8::0:)$(( [##16] h ))}"
  REPLY=${REPLY//[^A-Za-z0-9._-]/_}
}

# REPLY = display width of the prompt string $1, measured by zsh itself
# (%{...%}, wide characters), with the %N(l..) binary search p10k uses.
_tildy_instant_width() {
  emulate -L zsh
  local -i COLUMNS=1024 x=0 y=${#1} m
  if (( y )); then
    while (( ${${(%):-$1%$y(l.1.0)}[-1]} )); do
      x=y
      (( y *= 2 ))
    done
    while (( y > x + 1 )); do
      (( m = x + (y - x) / 2 ))
      (( ${${(%):-$1%$m(l.x.y)}[-1]} = m ))
    done
  fi
  REPLY=$x
}

_tildy_instant_log() {
  [[ -n ${TILDY_DEBUG_LOG-} ]] || return 0
  zmodload zsh/datetime 2>/dev/null || return 0
  local t=$EPOCHREALTIME f
  f=${t#*.}000
  print -r -- "${t%.*}${f[1,3]}"$'\t'"${(pj:\t:)@}" >>| $TILDY_DEBUG_LOG
} 2>/dev/null

# Undo the redirections and echo suppression, erase the instant prompt, show
# what init wrote. Runs once: from the first precmd (this snippet's hook, or
# tildy.zsh's) or, when .zshrc exits the shell, from the zshexit hook. Keeps $?.
_tildy_instant_finish() {
  local -i ret=$? sp=0 up
  # The user's PROMPT_SP/PROMPT_CR, before emulate hides them.
  [[ -o prompt_sp && -o prompt_cr ]] && sp=1
  emulate -L zsh
  [[ $_tildy_instant_state == init ]] || return ret
  _tildy_instant_state=done
  zshexit_functions=(${zshexit_functions:#_tildy_instant_exit})
  local out= chunk how=${1:-precmd}
  # No `{ ... } 2>/dev/null` around these: the group would put its own fd 2
  # back afterwards, i.e. leave stderr on the capture file.
  exec 0<&$_tildy_instant_fd0 1>&$_tildy_instant_fd1 2>&$_tildy_instant_fd2
  exec {_tildy_instant_fd0}<&- {_tildy_instant_fd1}>&- {_tildy_instant_fd2}>&-
  _tildy_instant_fd0= _tildy_instant_fd1= _tildy_instant_fd2=
  (( _tildy_instant_echo )) && command stty echo 2>/dev/null
  _tildy_instant_echo=0
  # The capture file was unlinked when it was opened; read it back through
  # the descriptor kept for that.
  if [[ -n $_tildy_instant_ofd ]]; then
    sysseek -u $_tildy_instant_ofd 0 2>/dev/null
    while sysread -i $_tildy_instant_ofd -s 65536 chunk 2>/dev/null; do
      out+=$chunk
    done
    exec {_tildy_instant_ofd}<&-
    _tildy_instant_ofd=
  fi
  # Back to the first column of the instant prompt's first row and clear to
  # the end of the screen; zle draws the real prompt from here. In precmd,
  # zsh has already done PROMPT_SP (before any precmd hook): the cursor was
  # not in column 0, so it is now one row further down.
  if (( _tildy_instant_rows )); then
    up=$(( _tildy_instant_rows - 1 ))
    [[ $how != exit ]] && (( sp && _tildy_instant_lastw > 0 )) && (( up++ ))
    if (( up )); then
      print -rn -- $'\r\e['$up$'A\e[J'
    else
      print -rn -- $'\r\e[J'
    fi
  fi
  _tildy_instant_log instant_done ${#out} $how $_tildy_instant_rows
  _tildy_instant_rows=0
  if [[ -n $out ]]; then
    # PROMPT_SP is over for this prompt: end a partial line ourselves.
    [[ $out == *$'\n' ]] || out+=$'\n'
    print -rn -- "$out"
    if [[ $_tildy_instant_mode == verbose ]]; then
      print -P -- "%F{yellow}tildy:%f output above came from .zshrc during the instant prompt"
    fi
  fi
  return ret
}

_tildy_instant_precmd() {
  _tildy_instant_finish precmd
  precmd_functions=(${precmd_functions:#_tildy_instant_precmd})
}

_tildy_instant_exit() {
  _tildy_instant_finish exit
}

() {
  emulate -L zsh
  local dir= key REPLY file content= line out ofd= base
  local -a lines left
  local right=
  local -i nl=0 rows=0 w lw cols=${COLUMNS:-80} i
  if [[ ${XDG_CACHE_HOME-} == /* ]]; then
    dir=$XDG_CACHE_HOME/tildy/instant
  elif [[ ${HOME-} == /* ]]; then
    dir=$HOME/.cache/tildy/instant
  fi
  (( cols > 0 )) || cols=80
  _tildy_instant_dir=$dir

  # Capture file first: without it nothing is shown or redirected (the key
  # function stays, so the worker still fills the cache for next time). It is
  # private, unlinked at once (nothing is left behind even if the shell is
  # killed during init) and read back through this descriptor.
  out=${TMPDIR:-/tmp}
  out=${out%/}/tildy-instant.$UID.$$.$RANDOM$RANDOM
  sysopen -rw -o creat,excl,cloexec -m 600 -u ofd $out 2>/dev/null || return 0
  zmodload -F zsh/files b:zf_rm 2>/dev/null
  zf_rm -f $out 2>/dev/null

  if [[ -n $dir ]]; then
    _tildy_instant_key
    key=$REPLY
    file=$dir/$key.prompt
    { IFS= read -rd '' content < $file } 2>/dev/null
  fi
  if [[ -n $content ]]; then
    lines=("${(@f)content}")
    if [[ $lines[1] == tildy-instant$'\t'1$'\t'<1-12> ]]; then
      nl=${lines[1]##*$'\t'}
      for line in "${(@)lines[2,-1]}"; do
        case $line in
          (L$'\t'*) left+=("${${line#L$'\t'}//[[:cntrl:]]/}") ;;
          (R$'\t'*) right=${${line#R$'\t'}//[[:cntrl:]]/} ;;
        esac
      done
      (( ${#left} == nl )) || nl=0
    fi
  fi
  if (( ! nl )); then
    # Nothing cached for this directory: the base prompt tildy.zsh shows
    # first anyway. Its height comes from the newest cache file of the same
    # version and TERM_PROGRAM (the prompt's line count depends on the
    # config, not the directory): the first result then has the same height
    # and is drawn at once instead of being held back as lines_mismatch
    # (ADR 0003) until the next Enter.
    local -a sib
    local -i hint=0
    if [[ -n $dir && -n $key ]]; then
      sib=($dir/${key%_*}_*.prompt(N.om[1]))
      if (( ${#sib} )); then
        line=
        { IFS= read -r line < $sib[1] } 2>/dev/null
        [[ $line == tildy-instant$'\t'1$'\t'<1-12> ]] && hint=${line##*$'\t'}
      fi
    fi
    base=${TILDY_BASE_PROMPT-'%~ %# '}
    left=()
    repeat $(( hint > 1 ? hint - 1 : 0 )) left+=("")
    left+=("${base##*$'\n'}")
    right=
    _tildy_instant_hint=$hint
  fi

  # Screen rows the prompt takes (lines wider than the terminal wrap).
  local text=
  for (( i = 1; i <= ${#left}; i++ )); do
    _tildy_instant_width "$left[i]"
    w=$REPLY
    (( rows += w > 0 ? (w + cols - 1) / cols : 1 ))
    (( i > 1 )) && text+=$'\n'
    text+=${(%)left[i]}
  done
  lw=$(( w % cols ))
  if [[ -n $right ]]; then
    # Like zle: right-aligned with one free column, only if it fits.
    _tildy_instant_width "$right"
    if (( REPLY > 0 && w < cols && w + REPLY + 1 < cols )); then
      text+=$'\e['$(( cols - REPLY ))G${(%)right}$'\e['$(( lw + 1 ))G
    fi
  fi
  print -rn -- "$text"$'\e[0m'
  _tildy_instant_rows=$rows
  _tildy_instant_lastw=$w
  if (( nl )); then
    _tildy_instant_nl=$nl
    _tildy_instant_left=${(pj:\n:)left}
    _tildy_instant_right=$right
  fi
  if (( nl )); then
    _tildy_instant_log instant $rows hit
  else
    _tildy_instant_log instant $rows miss $_tildy_instant_hint
  fi

  # Keys typed from now on stay in the tty queue for zle, unechoed.
  command stty -echo 2>/dev/null && _tildy_instant_echo=1
  exec {_tildy_instant_fd0}<&0 {_tildy_instant_fd1}>&1 {_tildy_instant_fd2}>&2 0</dev/null 1>&$ofd 2>&$ofd
  _tildy_instant_ofd=$ofd
  _tildy_instant_state=init

  # First precmd hook, ahead of the plugins' hooks; tildy.zsh's precmd calls
  # finish as well in case .zshrc resets precmd_functions. (Not `sched +0`:
  # a scheduled event at the first prompt makes zsh run precmd twice.)
  typeset -ga precmd_functions zshexit_functions
  precmd_functions=(_tildy_instant_precmd ${precmd_functions:#_tildy_instant_precmd})
  # .zshrc may exit the shell: restore echo and show the captured output. A
  # hook array, not an EXIT trap: a trap set in here would belong to this
  # function and fire when it returns.
  zshexit_functions+=(_tildy_instant_exit)
}

fi
