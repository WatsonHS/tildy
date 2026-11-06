# Tildy Zsh integration, shell protocol v1.
#
# Normally loaded with `eval "$(tildy init zsh)"` at the end of .zshrc (after
# compinit and plugins); `tildy init zsh` prepends TILDY_BIN, TILDY_VERSION
# and TILDY_PROTOCOL_VERSION. Sourcing this file directly also works.
# Contract: docs/protocol.md ("Shell 协议 v1"). Decisions:
# docs/decisions/0001-shell-protocol.md, 0002-safe-redraw.md, 0003-zsh-v1.md.
#
# Environment:
#   TILDY_BIN                 tildy executable (default: `tildy` on PATH);
#                             exported once loaded
#   TILDY_VERSION             expected worker binary version (empty: no check);
#                             exported once loaded
#   TILDY_PROTOCOL_VERSION    expected protocol version (empty: no check)
#   TILDY_BACKEND             executable used instead of TILDY_BIN, invoked as
#                             `$TILDY_BACKEND worker --parent-pid PID` or
#                             `$TILDY_BACKEND render` (test harness)
#   TILDY_BACKEND_MODE        persistent (default: worker, oneshot fallback)
#                             | oneshot (never start a worker)
#   TILDY_BACKEND_TIMEOUT_MS  timeout for a oneshot result, for the first
#                             frame of a worker request and for HELLO;
#                             default 5000 (1 s granularity)
#   TILDY_PERSISTENT_MAX_RESTARTS  worker restarts allowed per window, default 3
#   TILDY_RESTART_WINDOW_MS   restart rate-limit window, default 60000
#   TILDY_DEBUG_LOG           append tab-separated debug events to this file
#   TILDY_BASE_PROMPT         immediate prompt, default '%~ %# ' (last line used)
#   TILDY_REFLOW              1: the terminal reflows soft-wrapped lines when
#                             its width changes (Ghostty, iTerm2, ...); 0: it
#                             does not; unset: guessed from TERM_PROGRAM and
#                             friends (read at load, see _tildy_reflow)
#   TILDY_RESIZE_DEBOUNCE_MS  quiet time after the last SIGWINCH before the
#                             resize request is sent, default 80
#   TILDY_SESSION_ID          exported: this shell's session id (agent status
#                             records published from here carry it)
#   TILDY_AGENT_WRAP          0: do not run agent programs through
#                             `tildy agent wrap` (read at load and per call)
#   TILDY_AGENT_PROGRAMS      replaces the agent program list (space or comma
#                             separated), read at load
#
# Instant prompt (shell/zsh/instant.zsh, `tildy init zsh --instant` at the top
# of .zshrc): when its snippet ran, the first precmd ends its redirections
# (_tildy_instant_finish), the first base prompt is the cached prompt it showed
# (same text, same place, so nothing flickers), and every request carries the
# snippet's cache key as TILDY_INSTANT_KEY in the env field so the worker can
# refresh the cache (docs/decisions/0006-instant-prompt.md).
#
# Requires zsh >= 5.8 (`zle widget -f nolast`, add-zle-hook-widget).
# Tested on zsh 5.9.

# Load guard around the whole file (closed by the `fi` at the end). There is no
# top-level `return`: under `eval "$(tildy init zsh)"` in .zshrc a return
# would leave .zshrc itself, e.g. when it is sourced again in a running shell.
# A second `source` (or eval of init) keeps this load: see the else branch.
if [[ -o interactive ]] && (( ! ${+_tildy_loaded} )) &&
   zmodload zsh/zle zsh/system zsh/datetime zsh/sched zsh/zleparameter zsh/parameter; then

# Only the zf_* names, so the user's mkdir/rm stay untouched.
zmodload -F zsh/files b:zf_mkdir b:zf_rm b:zf_rmdir 2>/dev/null
# zselect: the resize debounce timer sleeps without an external command.
zmodload zsh/zselect 2>/dev/null
autoload -Uz add-zsh-hook add-zle-hook-widget

typeset -g _tildy_loaded=1
# Exported: child processes (e.g. `tildy configure`) can tell tildy is active.
typeset -gx TILDY_BIN=${TILDY_BIN:-tildy}
typeset -gx TILDY_VERSION=${TILDY_VERSION-}
# `tildy …` on the command line (docs say `tildy configure`) when the binary
# is not on PATH: a function over TILDY_BIN. Never shadows a real command.
if [[ -x $TILDY_BIN ]] && ! (( ${+commands[tildy]} )) && ! (( ${+functions[tildy]} )); then
  tildy() { "$TILDY_BIN" "$@" }
fi
typeset -g TILDY_PROTOCOL_VERSION=${TILDY_PROTOCOL_VERSION-}
# What this load of the adapter was built against: a later `eval "$(tildy
# init zsh)"` in the same shell (the guard's else branch) keeps these.
typeset -g _tildy_loaded_bin=$TILDY_BIN _tildy_loaded_version=$TILDY_VERSION \
  _tildy_loaded_protocol=$TILDY_PROTOCOL_VERSION
typeset -g _tildy_session_id="$$.$RANDOM$RANDOM"
# Exported: agent status publishers started from this shell (`tildy agent
# wrap`, `tildy agent publish`) record it, so this shell's next prompt can
# drop their working/blocked records (docs/agent-status.md).
typeset -gx TILDY_SESSION_ID=$_tildy_session_id
typeset -g _tildy_shell_pid=$$

typeset -gi _tildy_gen=0             # current generation
typeset -gi _tildy_cycle=0           # prompt cycles (precmd count)
typeset -gi _tildy_last_exit=0 _tildy_last_duration_ms=0
typeset -gi _tildy_submission_id=0 _tildy_command_running=0
typeset -gi _tildy_blank_submission=0
typeset -g  _tildy_cmd_t0=            # preexec timestamp (ms)
typeset -gi _tildy_busy=0            # >0 while a completion widget runs
typeset -gi _tildy_isearch=0         # 1 between isearch-update and isearch-exit
typeset -gi _tildy_lines=1           # line count of the prompt currently shown
typeset -gi _tildy_next_lines=1      # L count of the last result: next base prompt
typeset -gi _tildy_wrapped=0         # completion widgets wrapped (once)
typeset -gi _tildy_timer_armed=0
typeset -g  _tildy_req_cols= _tildy_req_rows=   # terminal size of the last request
typeset -gi _tildy_nobackend_logged=0

# A validated result waiting to be drawn (current gen, unsafe state).
typeset -g  _tildy_def_gen= _tildy_def_left= _tildy_def_right=
typeset -gi _tildy_def_logged=0 _tildy_def_lines=1

# Transient form (T line) of the latest valid result, and the cycle it is for.
typeset -g  _tildy_t_text= _tildy_t_gen=
typeset -gi _tildy_t_cycle=-1 _tildy_t_has=0
typeset -g _tildy_t_key= _tildy_t_context= _tildy_t_suffix= _tildy_history_key=
typeset -gi _tildy_summary_enabled=0 _tildy_summary_threshold=3000 _tildy_summary_due=0
typeset -g _tildy_def_fd= _tildy_def_epoch= _tildy_def_seq=
typeset -g _tildy_def_t= _tildy_def_key= _tildy_def_context= _tildy_def_suffix= _tildy_def_summary=
typeset -gi _tildy_def_nt=0

# User prompt options, sampled before `emulate -L zsh` hides them.
typeset -g _tildy_opt_psubst=off _tildy_opt_pbang=off
typeset -g _tildy_ps_left= _tildy_ps_right=

# Per-channel state, keyed by the read fd. A channel is one oneshot process or
# one worker. state: active | done | cancelled (oneshot) ; worker | retired.
typeset -gA _tildy_ch_state _tildy_ch_mode _tildy_ch_gen _tildy_ch_pid \
  _tildy_ch_nonce _tildy_ch_t0 _tildy_ch_buf _tildy_ch_killpend _tildy_ch_epoch \
  _tildy_ch_fgen _tildy_ch_fstatus _tildy_ch_fepoch _tildy_ch_left _tildy_ch_nleft \
  _tildy_ch_right _tildy_ch_nright _tildy_ch_trans _tildy_ch_ntrans \
  _tildy_ch_seq _tildy_ch_key _tildy_ch_context _tildy_ch_ncontext _tildy_ch_suffix _tildy_ch_nsuffix _tildy_ch_summary
typeset -g _tildy_cur_fd=            # the active oneshot channel, if any

# Worker. state: off | starting (waiting for HELLO) | ready
typeset -g  _tildy_w_state=off _tildy_w_fd= _tildy_w_wfd= _tildy_w_pid= _tildy_w_t0=
typeset -g  _tildy_w_epoch= _tildy_w_version= _tildy_w_proto=
typeset -g  _tildy_w_req_gen= _tildy_w_req_t0= _tildy_w_got=   # last request sent
typeset -gi _tildy_w_disabled=0
typeset -ga _tildy_w_restarts        # restart timestamps (ms) inside the window

# Self-pipe: TRAPWINCH writes a byte, the zle -F handler does the work.
typeset -g _tildy_wake_fd=

# Resize (docs/decisions/0002-safe-redraw.md, "缩放与终端重排"). The full
# prompt tildy last put in PROMPT/RPROMPT (before any fitting), the display
# width of each of its lines (measured lazily, keyed by the text), whether
# PROMPT holds its width-relative fitted form, and the width zsh last drew
# the prompt at.
typeset -g  _tildy_shown_left= _tildy_shown_right= _tildy_shown_wkey=
typeset -ga _tildy_shown_w
typeset -gi _tildy_fitted=0 _tildy_drawn_cols=0 _tildy_rz_drew=0
# Debounce: a burst of SIGWINCH bumps the generation once (pending=1) and
# sends one request when it has been quiet for _tildy_rz_ms.
typeset -gi _tildy_rz_pending=0 _tildy_rz_ms=80
typeset -g  _tildy_rz_last= _tildy_rz_fd=
[[ ${TILDY_RESIZE_DEBOUNCE_MS-} == <0-2000> ]] && _tildy_rz_ms=$TILDY_RESIZE_DEBOUNCE_MS
# Does the terminal reflow soft-wrapped lines when its width changes? Then a
# narrower width moves the old prompt's rows in ways zsh does not know about.
typeset -gi _tildy_reflow=0
case ${TILDY_REFLOW-} in
  (1|yes|on) _tildy_reflow=1 ;;
  (0|no|off) _tildy_reflow=0 ;;
  (*)
    if [[ ${TERM_PROGRAM-} == (ghostty|iTerm.app|Apple_Terminal|WezTerm|vscode|tmux|WarpTerminal|rio|Tabby|Hyper) ||
          ${TERM-} == (xterm-ghostty|xterm-kitty|alacritty|wezterm) ||
          -n ${KITTY_WINDOW_ID-}${ALACRITTY_WINDOW_ID-}${KONSOLE_VERSION-}${VTE_VERSION-}${WT_SESSION-} ]]; then
      _tildy_reflow=1
    fi ;;
esac

# Instant prompt: cache key memo (context -> key) and whether the prompt shown
# is still the cached instant prompt of the first cycle.
typeset -g _tildy_ikey_ctx= _tildy_ikey=
typeset -gi _tildy_base_cached=0
(( ${_tildy_instant_nl:-0} > 0 )) && _tildy_next_lines=$_tildy_instant_nl
# Instant miss with a sibling cache: start at that height (the snippet printed
# the base prompt padded to it), reducing the first result's height adjustment.
(( ${_tildy_instant_nl:-0} == 0 && ${_tildy_instant_hint:-0} > 0 )) && _tildy_next_lines=$_tildy_instant_hint

# ---------------------------------------------------------------- helpers

_tildy_ms() {
  local t=$EPOCHREALTIME
  local f=${t#*.}000
  REPLY=${t%.*}${f[1,3]}
}

# Debug log: "<ms>\t<event>\t<fields...>", appended per event.
_tildy_log() {
  [[ -n ${TILDY_DEBUG_LOG-} ]] || return 0
  local REPLY
  _tildy_ms
  print -r -- "$REPLY"$'\t'"${(pj:\t:)@}" >>| $TILDY_DEBUG_LOG
} 2>/dev/null

# Escape for a tab-separated line: \ -> \\, TAB -> \t, LF -> \n.
_tildy_escape() {
  REPLY=${1//\\/\\\\}
  REPLY=${REPLY//$'\t'/\\t}
  REPLY=${REPLY//$'\n'/\\n}
}

# REQ env field: KEY=VALUE pairs joined by \x1f (missing/empty keys omitted),
# values stripped of \x1f, the whole field escaped like cwd.
_tildy_env_field() {
  local k v out= s=$'\x1f'
  for k in SSH_CONNECTION SSH_TTY USER HOST VIRTUAL_ENV CONDA_DEFAULT_ENV DOCKER_CONTEXT DOCKER_HOST PATH SHLVL IN_NIX_SHELL name DIRENV_FILE DIRENV_DIR; do
    v=${(P)k-}
    [[ -n $v ]] || continue
    v=${v//$s/}
    out+="${out:+$s}$k=$v"
  done
  # Instant prompt cache key, from the snippet's function (the only place the
  # key is defined); the worker removes it before using env as a revision.
  if (( ${+functions[_tildy_instant_key]} )); then
    local ctx="$PWD"$'\0'"${TERM_PROGRAM-}"
    if [[ $ctx != $_tildy_ikey_ctx ]]; then
      _tildy_instant_key
      _tildy_ikey=$REPLY _tildy_ikey_ctx=$ctx
    fi
    out+="${out:+$s}TILDY_INSTANT_KEY=$_tildy_ikey"
  fi
  # These identities are for the renderer only; providers remove them from
  # their environment revision. An explicit empty font label disables profiles.
  for k in TERM TERM_PROGRAM TERM_PROGRAM_VERSION LC_ALL LC_CTYPE LANG; do
    v=${(P)k-}; v=${v//$s/}
    [[ -n $v ]] && out+="${out:+$s}$k=$v"
  done
  v=${TILDY_FONT_REVISION-}; v=${v//$s/}
  out+="${out:+$s}TILDY_FONT_REVISION=$v"
  if (( _tildy_base_cached && _tildy_cycle == 1 )) && [[ -z ${TILDY_FONT_REVISION-} && $PWD == ${_tildy_instant_pwd-} ]]; then
    out+="${out:+$s}TILDY_INSTANT_SHOWN=1"
  fi
  out+="${out:+$s}TILDY_SUBMISSION_ID=$_tildy_submission_id"
  _tildy_escape "$out"
}

# Strip control characters (C0, DEL and, in UTF-8 locales, C1) line by line.
_tildy_sanitize() {
  local -a out
  local l
  for l in "${(@f)1}"; do
    out+=("${l//[[:cntrl:]]/}")
  done
  REPLY=${(pj:\n:)out}
}

# Decimal unsigned protocol counters are strings until validated; zsh's native
# integer arithmetic is signed, so do not evaluate unbounded wire numbers.
_tildy_u64() {
  [[ $1 == <-> && ${#1} -le 20 ]] || return 1
  (( ${#1} < 20 )) || [[ $1 < 18446744073709551615 || $1 == 18446744073709551615 ]]
}

_tildy_timeout_ms() {
  if [[ ${TILDY_BACKEND_TIMEOUT_MS-} == <1-> ]]; then
    REPLY=$TILDY_BACKEND_TIMEOUT_MS
  else
    REPLY=5000
  fi
}

# REPLY = backend executable (path), empty if none can be found.
_tildy_backend_exe() {
  local b=${TILDY_BACKEND:-$TILDY_BIN}
  REPLY=
  if [[ $b == */* ]]; then
    [[ -x $b ]] && REPLY=$b
  elif [[ -n $b ]]; then
    REPLY=${commands[$b]-}
  fi
}

_tildy_persistent() { [[ ${TILDY_BACKEND_MODE:-persistent} != oneshot ]] }

# REPLY = base prompt text: the last line of TILDY_BASE_PROMPT, padded on top
# with empty lines to $1 lines.
_tildy_base_text() {
  local base=${TILDY_BASE_PROMPT-'%~ %# '}
  local -i n=$1
  base=${base##*$'\n'}
  (( n < 1 )) && n=1
  repeat $(( n - 1 )) base=$'\n'$base
  REPLY=$base
}

# Base prompt with the line count of the previous cycle's result. In the first
# cycle after an instant prompt hit (same directory and width), the cached
# prompt the snippet printed is the base prompt: zle redraws the same text.
_tildy_set_base() {
  local REPLY
  local -i n=$_tildy_next_lines
  (( n < 1 )) && n=1
  _tildy_base_cached=0
  if (( _tildy_cycle == 1 && ${_tildy_instant_nl:-0} == n )) &&
     [[ -n ${_tildy_instant_left-} && $PWD == ${_tildy_instant_pwd-} ]]; then
    _tildy_sanitize "$_tildy_instant_left"
    _tildy_set_prompt "$REPLY" "${_tildy_instant_right-}" keep
    _tildy_shown "$REPLY" "${_tildy_instant_right-}"
    _tildy_lines=$n
    _tildy_base_cached=1
    return 0
  fi
  _tildy_base_text $n
  # The base is a user prompt template, not backend text: preserve its native
  # prompt_subst/prompt_bang semantics while retaining an earlier terminal
  # hook's marks (iTerm2 need not be installed after tildy).
  local -a reply
  local base=$REPLY
  _tildy_marks
  [[ -n $reply[2] ]] && base=${base//$'\n'/$'\n'$reply[2]}
  PROMPT=$reply[1]$base$reply[3]
  RPROMPT=
  _tildy_shown "$REPLY" ""
  _tildy_lines=$n
}

# Put backend content into PROMPT/RPROMPT without letting it execute.
# prompt_subst off: content goes in directly (only % escapes are expanded).
# prompt_subst on : PROMPT='${_tildy_ps_left}'; a parameter's value is not
#                   re-scanned for $(...) or `...`, only for % escapes.
# prompt_bang on  : '!' would become the history number, so double it.
# $3 = keep: carry over the terminal's semantic prompt marks (_tildy_marks).
_tildy_set_prompt() {
  local l=$1 r=$2 a= b= reply
  if [[ -n $3 ]]; then
    _tildy_marks
    a=$reply[1] b=$reply[3]
    [[ -n $reply[2] ]] && l=${l//$'\n'/$'\n'$reply[2]}
  fi
  if [[ $_tildy_opt_pbang == on ]]; then
    l=${l//\!/!!}
    r=${r//\!/!!}
  fi
  if [[ $_tildy_opt_psubst == on ]]; then
    _tildy_ps_left=$l
    _tildy_ps_right=$r
    PROMPT=$a'${_tildy_ps_left}'$b
    RPROMPT='${_tildy_ps_right}'
  else
    PROMPT=$a$l$b
    RPROMPT=$r
  fi
}

# reply = (start continuation end): the OSC 133 semantic prompt marks the
# terminal's shell integration wrapped around the current PROMPT, each a
# %{...%} group, or empty. Ghostty's zsh integration (loaded by cmux) puts
# %{ESC]133;A;cl=line BEL%} first, %{ESC]133;P;k=s BEL%} after each newline
# and %{ESC]133;B BEL%} last, in precmd. The terminal finds the prompt by
# them; Ghostty clears the prompt rows on resize so zsh's redraw starts on
# blank rows. A prompt replaced from a zle -F handler would lose them, so
# tildy carries them over (ADR 0002 "缩放与终端重排").
_tildy_marks() {
  emulate -L zsh -o extended_glob
  local p=$PROMPT q=$PROMPT
  local -a match mbegin mend
  reply=("" "" "")
  [[ $p == *$'\e]133;'* ]] || return 0
  # prompt_subst: the continuation marks are inside the parameter's value.
  [[ $p == *'${_tildy_ps_left}'* ]] && q+=$'\n'$_tildy_ps_left
  [[ $p == (#b)(%\{$'\e']133\;[^%]#%\})* ]] && reply[1]=$match[1]
  [[ $q == (#b)*$'\n'(%\{$'\e']133\;[^%]#%\})* ]] && reply[2]=$match[1]
  [[ $p == (#b)?*(%\{$'\e']133\;[^%]#%\}) ]] && reply[3]=$match[1]
  return 0
}

_tildy_sample_opts() {
  _tildy_opt_psubst=${options[promptsubst]}
  _tildy_opt_pbang=${options[promptbang]}
}

# ------------------------------------------------------ prompt geometry
# What the resize handling needs to know about the prompt on screen. Widths
# are display columns as zsh computes them (its wcwidth, %F/%K and other
# escapes take none).

# Record the full prompt tildy has just put in PROMPT/RPROMPT; zle draws it at
# the current width.
_tildy_shown() {   # left right
  _tildy_shown_left=$1 _tildy_shown_right=$2 _tildy_fitted=0
  _tildy_drawn_cols=${COLUMNS:-80}
}

# REPLY = display width of the prompt string $1: prompt-expanded like zle
# does it, CSI sequences removed, then zsh's own width of what is left.
# (Unlike the %N(l..) search in instant.zsh this never sets COLUMNS, which
# would be noticed by zle while it is active.)
_tildy_width() {
  emulate -L zsh -o extended_glob
  local s=$1 out= c digits
  local -i i=1 n=${#1} declared j
  if [[ $s == *'%{'* ]]; then
    # Fold declared-width groups to spaces before prompt expansion. Expanding
    # %G first would lose the declaration and fall back to libc wcwidth.
    while (( i <= n )); do
      if [[ ${s[i,i+1]} == '%%' ]]; then
        out+='%%'; (( i += 2 ))
      elif [[ ${s[i,i+1]} == '%{' ]]; then
        (( i += 2 )); declared=0
        while (( i <= n )); do
          if [[ ${s[i,i+1]} == '%}' ]]; then (( i += 2 )); break
          elif [[ ${s[i,i+1]} == '%%' ]]; then (( i += 2 ))
          elif [[ ${s[i,i+1]} == '%G' ]]; then (( ++declared, i += 2 ))
          elif [[ ${s[i]} == '%' ]]; then
            j=$(( i + 1 )); digits=
            while (( j <= n )) && [[ ${s[j]} == [0-9] ]]; do digits+=${s[j]}; (( ++j )); done
            if [[ -n $digits && ${s[j]} == G && ${#digits} -le 5 ]]; then
              (( declared += 10#$digits )); i=$(( j + 1 ))
            else (( ++i )); fi
          else (( ++i )); fi
          (( declared > 65535 )) && declared=65535
        done
        out+=${(l:$declared:: :)''}
      else
        out+=${s[i]}; (( ++i ))
      fi
    done
    s=$out
  fi
  s=${(%)s}
  s=${s//$'\e'\[[0-9;:?<=>]#[ -\/]#[@-~]/}
  s=${s//[[:cntrl:]]/}
  REPLY=${(m)#s}
}

# _tildy_shown_w = width of each line of _tildy_shown_left (cached).
_tildy_measure() {
  [[ $_tildy_shown_wkey == $_tildy_shown_left && ${#_tildy_shown_w} -gt 0 ]] && return 0
  local l REPLY
  _tildy_shown_w=()
  for l in "${(@ps:\n:)_tildy_shown_left}"; do
    _tildy_width "$l"
    _tildy_shown_w+=($REPLY)
  done
  (( ${#_tildy_shown_w} )) || _tildy_shown_w=(0)
  _tildy_shown_wkey=$_tildy_shown_left
}

# REPLY = how many rows below the first prompt row the cursor sits when the
# prompt, as zle drew it at width $2, followed by the edit buffer up to the
# cursor (LBUFFER), is laid out at width $1. A line of w columns takes
# ceil(w / width) rows (at least one); a fitted prompt line is drawn at most
# $2 - 1 wide. Equal widths give zle's own idea of the cursor row (it moves up
# exactly that many rows to redraw); a narrower $1 gives where a reflowing
# terminal has put the cursor relative to the old first row.
_tildy_cursor_row() {   # layout_cols drawn_cols
  emulate -L zsh
  local -i x=$1 cap=$(( $2 > 1 ? $2 - 1 : 1 )) n=${#_tildy_shown_w} i d r=0 w pos m j
  (( x > 0 )) || x=1
  for (( i = 1; i <= n; i++ )); do
    d=${_tildy_shown_w[i]}
    (( _tildy_fitted && d > cap )) && d=cap
    (( i < n )) && (( r += d > 0 ? (d + x - 1) / x : 1 ))
  done
  # d: the last prompt line, the one the buffer continues.
  local -a seg
  seg=("${(@ps:\n:)LBUFFER}")
  (( ${#seg} )) || seg=("")
  m=${#seg}
  if (( m == 1 )); then
    pos=$(( d + ${(m)#seg[1]} ))
  else
    w=$(( d + ${(m)#seg[1]} ))
    (( r += w > 0 ? (w + x - 1) / x : 1 ))
    for (( j = 2; j < m; j++ )); do
      w=${(m)#seg[j]}
      (( r += w > 0 ? (w + x - 1) / x : 1 ))
    done
    pos=${(m)#seg[m]}
  fi
  REPLY=$(( r + pos / x ))
}

# REPLY = _tildy_shown_left with every line cut to the terminal width by zsh
# itself at each expansion: %-1>…> keeps COLUMNS - 1 columns of a line from
# the left (upper lines), %-1<…< from the right (the last line, so the
# prompt character and the cursor stay). zle re-expands PROMPT on every
# SIGWINCH, so the fitted form follows any later width by itself; escapes in
# the cut part are kept, so colors stay right.
_tildy_fit_text() {
  local -a lines out
  local -i i n
  lines=("${(@ps:\n:)_tildy_shown_left}")
  n=${#lines}
  for (( i = 1; i <= n; i++ )); do
    if (( i < n )); then
      out+=("%-1>>${lines[i]}%>>")
    else
      out+=("%-1<<${lines[i]}%<<")
    fi
  done
  REPLY=${(pj:\n:)out}
}

# Create FIFOs in a private directory. REPLY = the directory. The caller
# opens them and removes them right away with _tildy_rmfifos.
_tildy_mkfifos() {
  local dir=${TMPDIR:-/tmp}
  dir=${dir%/}/tildy.$UID.$$.$RANDOM$RANDOM
  REPLY=
  zf_mkdir -m 700 $dir 2>/dev/null || return 1
  if ! command mkfifo -m 600 ${@/#/$dir/} 2>/dev/null; then
    _tildy_rmfifos $dir "$@"
    return 1
  fi
  REPLY=$dir
}

_tildy_rmfifos() {
  local dir=$1
  shift
  zf_rm -f ${@/#/$dir/} 2>/dev/null
  zf_rmdir $dir 2>/dev/null
}

# ---------------------------------------------------------- generations

_tildy_bump() {
  (( ++_tildy_gen ))
  [[ -n $_tildy_cur_fd ]] && _tildy_cancel $_tildy_cur_fd stale
  if [[ -n $_tildy_def_gen ]]; then
    _tildy_log discard $_tildy_def_gen stale
    _tildy_def_gen=
  fi
}

# Give up on an active oneshot channel: log, kill, keep draining for EXIT.
_tildy_cancel() {
  local fd=$1
  [[ ${_tildy_ch_state[$fd]-} == active ]] || return 0
  _tildy_ch_state[$fd]=cancelled
  [[ $_tildy_cur_fd == $fd ]] && _tildy_cur_fd=
  _tildy_log discard ${_tildy_ch_gen[$fd]} $2
  _tildy_kill $fd
}

_tildy_kill() {
  local pid=${_tildy_ch_pid[$1]-}
  if [[ $pid == <2-> ]]; then
    kill -TERM $pid 2>/dev/null
  else
    _tildy_ch_killpend[$1]=1   # PID line not read yet; kill when it arrives
  fi
}

# --------------------------------------------------------------- requests

_tildy_request() {
  local gen=$_tildy_gen REPLY cwd env req
  _tildy_escape "$PWD"
  cwd=$REPLY
  _tildy_env_field
  env=$REPLY
  _tildy_req_cols=${COLUMNS:-80}
  _tildy_req_rows=${LINES:-24}
  req="REQ"$'\t'"$gen"$'\t'"$_tildy_session_id"$'\t'"$cwd"$'\t'"$_tildy_req_cols"$'\t'"$_tildy_req_rows"$'\t'"$_tildy_last_exit"$'\t'"$_tildy_last_duration_ms"$'\t'"$env"$'\t'"${#jobstates}"
  if _tildy_w_usable; then
    _tildy_w_send "$gen" "$req" "$cwd" && return 0
  fi
  _tildy_os_spawn "$gen" "$req" "$cwd"
}

_tildy_chan_init() {   # fd state mode gen nonce
  local fd=$1 REPLY
  _tildy_ms
  _tildy_ch_state[$fd]=$2
  _tildy_ch_mode[$fd]=$3
  _tildy_ch_gen[$fd]=$4
  _tildy_ch_nonce[$fd]=$5
  _tildy_ch_t0[$fd]=$REPLY
  _tildy_ch_pid[$fd]=
  _tildy_ch_buf[$fd]=
  _tildy_ch_killpend[$fd]=0
  _tildy_ch_fgen[$fd]=
  _tildy_ch_epoch[$fd]=
}

# oneshot: one `render` process per request. The wrapper subshell reports the
# backend pid and exit code in-band (zsh 5.9 sets neither $! nor a waitable job
# for a process substitution). The nonce keeps a backend from forging these.
_tildy_os_spawn() {
  local gen=$1 req=$2 cwd=$3 fd= nonce=$RANDOM$RANDOM$RANDOM REPLY exe
  _tildy_backend_exe
  exe=$REPLY
  if [[ -z $exe ]]; then
    # Nothing to run: keep the base prompt, say so once.
    if (( ! _tildy_nobackend_logged )); then
      _tildy_nobackend_logged=1
      _tildy_log backend_degraded no_backend
    fi
    return 1
  fi
  _tildy_nobackend_logged=0
  exec {fd}< <(_tildy_os_child "$exe" "$req" "$nonce")
  [[ -n $fd ]] || return 1
  _tildy_chan_init $fd active oneshot $gen $nonce
  _tildy_cur_fd=$fd
  zle -F $fd _tildy_on_fd
  _tildy_log send $gen "$cwd" oneshot
  _tildy_arm_timer
}

_tildy_os_child() {
  local exe=$1 req=$2 nonce=$3 fd
  exec 2>/dev/null </dev/null
  if [[ -n $_tildy_w_wfd ]]; then
    local w=$_tildy_w_wfd
    { exec {w}>&- } 2>/dev/null
  fi
  for fd in ${_tildy_instant_fd0-} ${_tildy_instant_fd1-} ${_tildy_instant_fd2-}; do
    { exec {fd}<&- } 2>/dev/null
  done
  print -r -- "$req" | "$exe" render &
  print -r -- "PID"$'\t'"$nonce"$'\t'"$!"
  wait $!
  print -r -- "EXIT"$'\t'"$nonce"$'\t'"$?"
}

# ------------------------------------------------------------------ worker
# One worker per shell, connected through two FIFOs that are unlinked right
# after opening. Our write end is non-blocking, so a stuck worker can never
# block the shell. The forked child execs the worker directly (no resident
# wrapper); EOF on the response pipe is the death signal.

_tildy_w_start() {   # [dir with req/resp FIFOs already created]
  local dir=$1 t1= t2= bin= bout= wfd= rfd= ok=1 exe pid REPLY
  _tildy_backend_exe
  exe=$REPLY
  if [[ -z $exe ]]; then
    [[ -n $dir ]] && _tildy_rmfifos $dir req resp
    return 1
  fi
  if [[ -z $dir ]]; then
    _tildy_mkfifos req resp || return 1
    dir=$REPLY
  fi
  {
    sysopen -rw -o cloexec -u t1 $dir/req &&
    sysopen -r -o cloexec -u bin $dir/req &&
    sysopen -w -o cloexec,nonblock -u wfd $dir/req &&
    sysopen -rw -o cloexec -u t2 $dir/resp &&
    sysopen -w -o cloexec -u bout $dir/resp &&
    sysopen -r -o cloexec -u rfd $dir/resp
  } 2>/dev/null || ok=0
  _tildy_rmfifos $dir req resp
  # The read-write helpers only existed so the opens above don't block. Close
  # them before forking, or the child would hold a writer and never see EOF.
  [[ -n $t1 ]] && { exec {t1}>&- } 2>/dev/null
  [[ -n $t2 ]] && { exec {t2}>&- } 2>/dev/null
  if (( ! ok )); then
    [[ -n $bin ]] && { exec {bin}<&- } 2>/dev/null
    [[ -n $bout ]] && { exec {bout}>&- } 2>/dev/null
    [[ -n $wfd ]] && { exec {wfd}>&- } 2>/dev/null
    [[ -n $rfd ]] && { exec {rfd}<&- } 2>/dev/null
    return 1
  fi
  _tildy_w_child "$exe" $wfd $rfd <&$bin >&$bout 2>/dev/null &!
  pid=$!
  { exec {bin}<&- {bout}>&- } 2>/dev/null
  _tildy_chan_init $rfd worker worker $_tildy_gen ''
  _tildy_ch_pid[$rfd]=$pid
  _tildy_w_fd=$rfd
  _tildy_w_wfd=$wfd
  _tildy_w_pid=$pid
  _tildy_w_state=starting
  _tildy_w_epoch= _tildy_w_version= _tildy_w_proto=
  _tildy_w_req_gen= _tildy_w_got=
  _tildy_ms
  _tildy_w_t0=$REPLY
  zle -F $rfd _tildy_on_fd
  _tildy_log backend_start $_tildy_gen $pid persistent
  _tildy_arm_timer
  return 0
}

_tildy_w_child() {
  local exe=$1 fd
  # Don't leak our other pipes (or the instant prompt's saved tty fds, when
  # the worker starts during init) into a long-lived process.
  for fd in ${(k)_tildy_ch_state} $_tildy_wake_fd $2 $3 \
            ${_tildy_instant_fd0-} ${_tildy_instant_fd1-} ${_tildy_instant_fd2-}; do
    { exec {fd}<&- } 2>/dev/null
  done
  # The instant prompt snippet chose the cache directory; the worker writes there.
  [[ -n ${_tildy_instant_dir-} ]] && export TILDY_INSTANT_DIR=$_tildy_instant_dir
  exec "$exe" worker --parent-pid $_tildy_shell_pid
}

# Is the worker able to take this request? Starts it if it is due.
_tildy_w_usable() {
  if ! _tildy_persistent; then
    [[ -n $_tildy_w_fd ]] && _tildy_w_lost mode_oneshot bye norequest
    return 1
  fi
  (( _tildy_w_disabled )) && return 1
  case $_tildy_w_state in
    (off)
      _tildy_w_restart
      return 1 ;;
    (starting)
      # HELLO may already be waiting (worker started before the first prompt).
      _tildy_w_poll
      [[ $_tildy_w_state == ready ]] || return 1 ;;
  esac
  # TILDY_VERSION changes when init is evaluated again after an upgrade.
  local REPLY
  _tildy_w_mismatch $_tildy_w_proto $_tildy_w_version
  if [[ -n $REPLY ]]; then
    _tildy_w_lost $REPLY bye norequest
    _tildy_w_restart
    return 1
  fi
  return 0
}

# REPLY = protocol_mismatch | version_mismatch | empty.
_tildy_w_mismatch() {   # protocol version
  REPLY=
  if [[ -n ${TILDY_PROTOCOL_VERSION-} && $1 != $TILDY_PROTOCOL_VERSION ]]; then
    REPLY=protocol_mismatch
  elif [[ -n ${TILDY_VERSION-} && $2 != $TILDY_VERSION ]]; then
    REPLY=version_mismatch
  fi
}

# Restart, subject to the rate limit (not counting the very first start).
_tildy_w_restart() {
  [[ $_tildy_w_state == off ]] || return 0
  (( _tildy_w_disabled )) && return 1
  _tildy_persistent || return 1
  local REPLY t
  local -i now max=3 win=60000
  _tildy_backend_exe
  [[ -n $REPLY ]] || return 1   # nothing to run; the oneshot path logs it
  [[ ${TILDY_PERSISTENT_MAX_RESTARTS-} == <-> ]] && max=$TILDY_PERSISTENT_MAX_RESTARTS
  [[ ${TILDY_RESTART_WINDOW_MS-} == <1-> ]] && win=$TILDY_RESTART_WINDOW_MS
  _tildy_ms
  now=$REPLY
  local -a keep
  for t in $_tildy_w_restarts; do
    (( now - t < win )) && keep+=($t)
  done
  _tildy_w_restarts=($keep)
  if (( ${#keep} >= max )); then
    _tildy_w_disabled=1
    _tildy_log backend_degraded restart_limit
    return 1
  fi
  _tildy_w_restarts+=($now)
  if ! _tildy_w_start; then
    _tildy_log backend_degraded spawn_failed
    return 1
  fi
}

_tildy_w_send() {
  local gen=$1 req=$2 cwd=$3 REPLY n=0
  setopt local_options local_traps no_multibyte
  trap '' PIPE
  if ! syswrite -o $_tildy_w_wfd -c n -- "$req"$'\n' 2>/dev/null || (( n != ${#req} + 1 )); then
    # Dead or not reading: retire it, this request goes oneshot.
    _tildy_w_lost write_failed kill norequest
    return 1
  fi
  _tildy_w_req_gen=$gen
  _tildy_w_got=
  _tildy_ms
  _tildy_w_req_t0=$REPLY
  _tildy_ch_gen[$_tildy_w_fd]=$gen
  _tildy_log send $gen "$cwd" worker
  _tildy_arm_timer
  return 0
}

# Read whatever the starting worker already sent, without blocking.
_tildy_w_poll() {
  local fd=$_tildy_w_fd chunk= ret
  [[ -n $fd ]] || return 0
  sysread -t 0 -i $fd -s 8192 chunk
  ret=$?
  if (( ret == 0 )); then
    _tildy_feed $fd "$chunk"
  elif (( ret == 5 )); then
    _tildy_finish $fd -1
  fi
  return 0
}

# Stop using the worker: optionally BYE (graceful) or kill (misbehaving).
_tildy_w_retire() {   # bye|kill|none reason
  local fd=$_tildy_w_fd w=$_tildy_w_wfd how=$1 REPLY
  if [[ $how == bye && -n $w ]]; then
    setopt local_options local_traps
    trap '' PIPE
    syswrite -o $w -- "BYE"$'\n' 2>/dev/null
    _tildy_log bye ${_tildy_w_epoch:-${_tildy_ch_epoch[$fd]:-0}} $2
  fi
  # Closing our write end is an EOF for the worker either way.
  [[ -n $w ]] && { exec {w}>&- } 2>/dev/null
  [[ $how == kill && $_tildy_w_pid == <2-> ]] && kill -TERM $_tildy_w_pid 2>/dev/null
  if [[ -n $fd && ${_tildy_ch_state[$fd]-} == worker ]]; then
    _tildy_ch_state[$fd]=retired
    _tildy_ms
    _tildy_ch_t0[$fd]=$REPLY   # drain for EOF, closed by the timer otherwise
  fi
  _tildy_w_fd= _tildy_w_wfd= _tildy_w_pid= _tildy_w_state=off
  _tildy_w_epoch= _tildy_w_version= _tildy_w_proto=
}

# The worker can't be used any more: retire it, fall back to oneshot. The
# restart happens at the next request (rate limited), except after a version
# mismatch, where the contract asks for an immediate restart with the new
# binary. A request of the current prompt that has no fresh result yet is
# sent again as oneshot.
_tildy_w_lost() {   # reason bye|kill|none [norequest]
  local reason=$1 g=$_tildy_w_req_gen got=$_tildy_w_got
  [[ -n $_tildy_w_fd ]] || return 0
  _tildy_w_retire $2 $reason
  _tildy_w_req_gen= _tildy_w_got=
  _tildy_log backend_degraded $reason
  [[ $reason == (version|protocol)_mismatch && -z $3 ]] && _tildy_w_restart
  if [[ -z $3 && -n $g && $g == $_tildy_gen && $got != fresh ]]; then
    _tildy_request
  fi
}

# HELLO<TAB>protocol_version<TAB>binary_version<TAB>worker_epoch<TAB>pid
_tildy_w_hello() {
  local fd=$1 line=$2 REPLY
  local -a f
  f=("${(@ps:\t:)line}")
  if (( ${#f} != 5 )) || [[ $f[2] != <-> || -z $f[3] || $f[4] != <-> || $f[5] != <-> ]]; then
    _tildy_frame_bad $fd bad_frame
    return 0
  fi
  _tildy_ch_epoch[$fd]=$f[4]
  _tildy_log hello $f[4] $f[3] $f[2] $f[5]
  _tildy_w_mismatch $f[2] $f[3]
  if [[ -n $REPLY ]]; then
    _tildy_w_lost $REPLY bye
    return 0
  fi
  _tildy_w_state=ready
  _tildy_w_epoch=$f[4]
  _tildy_w_version=$f[3]
  _tildy_w_proto=$f[2]
}

# ------------------------------------------------------------------ timer
# zsh/sched runs inside zle's wait loop; granularity is one second, so a
# timeout fires between T and T+1 s.

_tildy_arm_timer() {
  (( _tildy_timer_armed )) && return 0
  _tildy_timer_armed=1
  sched +1 _tildy_on_timer
}

# A sched command's status becomes $? when it fires while zle is idle, i.e.
# between a command and the next command line: hand the old status back.
_tildy_on_timer() {
  local -i ret=$?
  emulate -L zsh
  _tildy_timer_armed=0
  local REPLY fd pending=0
  local -i now timeout t0
  _tildy_timeout_ms; timeout=$REPLY
  _tildy_ms; now=$REPLY
  case $_tildy_w_state in
    (starting)
      if (( now - _tildy_w_t0 >= timeout )); then
        _tildy_w_lost hello_timeout kill
      else
        pending=1
      fi ;;
    (ready)
      # Only the first frame is timed: after a partial result the worker owns
      # the rest (it times out its own providers).
      if [[ -n $_tildy_w_req_gen && -z $_tildy_w_got ]]; then
        if (( now - _tildy_w_req_t0 >= timeout )); then
          local reason=timeout
          [[ -n ${_tildy_ch_fgen[$_tildy_w_fd]} ]] && reason=half_frame
          _tildy_log discard $_tildy_w_req_gen $reason
          _tildy_w_lost $reason kill
        else
          pending=1
        fi
      fi ;;
  esac
  for fd in ${(k)_tildy_ch_state}; do
    [[ -n ${_tildy_ch_state[$fd]-} ]] || continue
    t0=${_tildy_ch_t0[$fd]:-$now}
    case ${_tildy_ch_state[$fd]} in
      (active)
        if (( now - t0 >= timeout )); then
          _tildy_cancel $fd timeout
        fi
        pending=1 ;;
      (worker) ;;
      (*)
        # done/cancelled/retired channels normally reach EOF within ms
        if (( now - t0 >= timeout + 10000 )); then
          _tildy_close $fd -1
        else
          pending=1
        fi ;;
    esac
  done
  (( pending )) && _tildy_arm_timer
  return ret
}

# ------------------------------------------------------------ frame input

_tildy_close() {   # fd [exit-code to log]
  local fd=$1
  [[ -n $2 ]] && _tildy_log backend_exit ${_tildy_ch_gen[$fd]-0} ${_tildy_ch_pid[$fd]:--1} $2
  zle -F $fd 2>/dev/null
  { exec {fd}<&- } 2>/dev/null
  [[ $_tildy_cur_fd == $fd ]] && _tildy_cur_fd=
  unset "_tildy_ch_state[$fd]" "_tildy_ch_mode[$fd]" "_tildy_ch_gen[$fd]" \
    "_tildy_ch_pid[$fd]" "_tildy_ch_nonce[$fd]" "_tildy_ch_t0[$fd]" \
    "_tildy_ch_buf[$fd]" "_tildy_ch_killpend[$fd]" "_tildy_ch_epoch[$fd]" \
    "_tildy_ch_fgen[$fd]" "_tildy_ch_fstatus[$fd]" "_tildy_ch_fepoch[$fd]" \
    "_tildy_ch_left[$fd]" "_tildy_ch_nleft[$fd]" "_tildy_ch_right[$fd]" \
    "_tildy_ch_nright[$fd]" "_tildy_ch_trans[$fd]" "_tildy_ch_ntrans[$fd]" \
    "_tildy_ch_seq[$fd]" "_tildy_ch_key[$fd]" "_tildy_ch_context[$fd]" \
    "_tildy_ch_ncontext[$fd]" "_tildy_ch_suffix[$fd]" "_tildy_ch_nsuffix[$fd]" "_tildy_ch_summary[$fd]"
}

# zle -F handler (plain function, not a widget: a -w handler would overwrite
# LASTWIDGET on every call and break widgets that depend on it).
_tildy_on_fd() {
  _tildy_sample_opts
  emulate -L zsh
  local fd=$1 chunk=
  if [[ -z ${_tildy_ch_state[$fd]-} ]]; then
    zle -F $fd 2>/dev/null
    { exec {fd}<&- } 2>/dev/null
    return 0
  fi
  if sysread -i $fd -s 8192 chunk; then
    _tildy_feed $fd "$chunk"
  else
    _tildy_finish $fd -1   # EOF (or read error) without an EXIT line
  fi
  return 0
}

_tildy_feed() {   # fd chunk
  local fd=$1 line buf=${_tildy_ch_buf[$1]}$2
  while [[ $buf == *$'\n'* ]]; do
    line=${buf%%$'\n'*}
    buf=${buf#*$'\n'}
    _tildy_ch_buf[$fd]=$buf
    _tildy_on_line $fd "$line"
    [[ -n ${_tildy_ch_state[$fd]-} ]] || return 0
  done
  _tildy_ch_buf[$fd]=$buf
  if (( ${#buf} > 65536 )); then
    _tildy_ch_buf[$fd]=
    _tildy_frame_bad $fd bad_frame
  fi
}

# Backend process ended (EXIT line or EOF): settle any open request, close.
_tildy_finish() {   # fd code
  local fd=$1 code=$2 g= reason=
  case ${_tildy_ch_state[$fd]-} in
    (active)
      g=${_tildy_ch_gen[$fd]}
      reason=eof
      [[ -n ${_tildy_ch_fgen[$fd]} ]] && reason=half_frame
      _tildy_ch_state[$fd]=done
      [[ $_tildy_cur_fd == $fd ]] && _tildy_cur_fd=
      _tildy_log discard $g $reason ;;
    (worker)
      reason=eof
      if [[ -n ${_tildy_ch_fgen[$fd]} ]]; then
        reason=half_frame
        _tildy_log discard ${_tildy_ch_fgen[$fd]} half_frame
      elif [[ -n $_tildy_w_req_gen && $_tildy_w_got != fresh ]]; then
        _tildy_log discard $_tildy_w_req_gen eof
      fi
      _tildy_close $fd $code
      [[ $_tildy_w_fd == $fd ]] && _tildy_w_lost $reason none
      return 0 ;;
  esac
  _tildy_close $fd $code
}

_tildy_on_line() {
  local fd=$1 line=$2 tag rest g st ep
  local mode=${_tildy_ch_mode[$fd]}
  if [[ $mode == oneshot ]]; then
    local nonce=${_tildy_ch_nonce[$fd]}
    if [[ $line == PID$'\t'$nonce$'\t'<-> ]]; then
      [[ -n ${_tildy_ch_pid[$fd]} ]] && return 0
      _tildy_ch_pid[$fd]=${line##*$'\t'}
      _tildy_log backend_start ${_tildy_ch_gen[$fd]} ${_tildy_ch_pid[$fd]} oneshot
      (( ${_tildy_ch_killpend[$fd]:-0} )) && kill -TERM ${_tildy_ch_pid[$fd]} 2>/dev/null
      return 0
    fi
    if [[ $line == EXIT$'\t'$nonce$'\t'* ]]; then
      _tildy_finish $fd "${line##*$'\t'}"
      return 0
    fi
  fi
  [[ ${_tildy_ch_state[$fd]} == (active|worker) ]] || return 0
  [[ -n $line ]] || return 0
  if [[ $line != *$'\t'* ]]; then
    _tildy_frame_bad $fd bad_frame
    return 0
  fi
  tag=${line%%$'\t'*}
  rest=${line#*$'\t'}
  # A worker must introduce itself before anything else, and only once.
  if [[ $mode == worker && -z ${_tildy_ch_epoch[$fd]} ]]; then
    if [[ $tag == HELLO ]]; then
      _tildy_w_hello $fd "$line"
    else
      _tildy_frame_bad $fd bad_frame
    fi
    return 0
  fi
  case $tag in
    (RES)
      # A new frame while one is open: the open one was a half frame.
      [[ -n ${_tildy_ch_fgen[$fd]} ]] && _tildy_frame_bad $fd half_frame
      [[ ${_tildy_ch_state[$fd]} == (active|worker) ]] || return 0
      local -a f
      f=("${(@ps:\t:)rest}")
      g=$f[1] st=$f[2] ep=$f[3]
      if (( ${#f} != 3 )) || [[ $g != <-> || $st != (fresh|partial) || $ep != <-> ]] ||
         [[ $mode == oneshot && $g != ${_tildy_ch_gen[$fd]} ]] ||
         [[ $mode == worker && $ep != ${_tildy_ch_epoch[$fd]} ]]; then
        _tildy_frame_bad $fd bad_frame
        return 0
      fi
      _tildy_ch_fgen[$fd]=$g
      _tildy_ch_fstatus[$fd]=$st
      _tildy_ch_fepoch[$fd]=$ep
      _tildy_ch_left[$fd]=
      _tildy_ch_nleft[$fd]=0
      _tildy_ch_right[$fd]=
      _tildy_ch_nright[$fd]=0
      _tildy_ch_trans[$fd]=
      _tildy_ch_ntrans[$fd]=0
      _tildy_ch_seq[$fd]= _tildy_ch_key[$fd]= _tildy_ch_context[$fd]=
      _tildy_ch_ncontext[$fd]=0 _tildy_ch_suffix[$fd]= _tildy_ch_nsuffix[$fd]=0 _tildy_ch_summary[$fd]= ;;
    (O|K|U|S|H)
      if [[ -z ${_tildy_ch_fgen[$fd]} ]]; then
        _tildy_frame_bad $fd bad_frame; return 0
      fi
      case $tag in
        (O)
          if [[ -n ${_tildy_ch_seq[$fd]} ]] || ! _tildy_u64 "$rest"; then _tildy_frame_bad $fd bad_frame; return 0; fi
          _tildy_ch_seq[$fd]=$rest ;;
        (K)
          if [[ -n ${_tildy_ch_key[$fd]} || -z $rest || $rest == *[^0-9a-f]* ]]; then _tildy_frame_bad $fd bad_frame; return 0; fi
          _tildy_ch_key[$fd]=$rest ;;
        (U)
          if (( ${_tildy_ch_nsuffix[$fd]} )); then _tildy_frame_bad $fd bad_frame; return 0; fi
          _tildy_ch_suffix[$fd]=$rest _tildy_ch_nsuffix[$fd]=1 ;;
        (S)
          if [[ -n ${_tildy_ch_summary[$fd]} ]] || ! _tildy_u64 "$rest"; then _tildy_frame_bad $fd bad_frame; return 0; fi
          _tildy_ch_summary[$fd]=$rest ;;
        (H)
          if (( ${_tildy_ch_ncontext[$fd]} >= 12 )); then _tildy_frame_bad $fd bad_frame; return 0; fi
          if (( ${_tildy_ch_ncontext[$fd]} )); then _tildy_ch_context[$fd]+=$'\n'$rest; else _tildy_ch_context[$fd]=$rest; fi
          (( _tildy_ch_ncontext[$fd]++ )) ;;
      esac ;;
    (L)
      if [[ -z ${_tildy_ch_fgen[$fd]} ]]; then
        _tildy_frame_bad $fd bad_frame
        return 0
      fi
      if (( ${_tildy_ch_nleft[$fd]} >= 12 )); then _tildy_frame_bad $fd bad_frame; return 0; fi
      if (( ${_tildy_ch_nleft[$fd]} )); then
        _tildy_ch_left[$fd]+=$'\n'$rest
      else
        _tildy_ch_left[$fd]=$rest
      fi
      (( _tildy_ch_nleft[$fd]++ )) ;;
    (R)
      if [[ -z ${_tildy_ch_fgen[$fd]} ]] || (( ${_tildy_ch_nright[$fd]} )); then
        _tildy_frame_bad $fd bad_frame
        return 0
      fi
      _tildy_ch_right[$fd]=$rest
      _tildy_ch_nright[$fd]=1 ;;
    (T)
      # 0..n lines (a blank line above the collapsed prompt with add_newline).
      if [[ -z ${_tildy_ch_fgen[$fd]} ]]; then
        _tildy_frame_bad $fd bad_frame
        return 0
      fi
      if (( ${_tildy_ch_ntrans[$fd]} >= 12 )); then _tildy_frame_bad $fd bad_frame; return 0; fi
      if (( ${_tildy_ch_ntrans[$fd]} )); then
        _tildy_ch_trans[$fd]+=$'\n'$rest
      else
        _tildy_ch_trans[$fd]=$rest
      fi
      (( _tildy_ch_ntrans[$fd]++ )) ;;
    (END)
      if [[ -z ${_tildy_ch_fgen[$fd]} || $rest != <-> ]] || (( ${_tildy_ch_nleft[$fd]} < 1 )); then
        _tildy_frame_bad $fd bad_frame
      elif [[ $rest != ${_tildy_ch_fgen[$fd]} ]]; then
        # END gen != RES gen counts as a half frame.
        _tildy_frame_bad $fd half_frame
      elif (( ${_tildy_ch_ncontext[$fd]:-0} > ${_tildy_ch_nleft[$fd]} )); then
        _tildy_frame_bad $fd bad_frame
      else
        _tildy_frame_done $fd
      fi ;;
    (*)
      _tildy_frame_bad $fd bad_frame ;;
  esac
}

# Invalid input. oneshot: the request is over. worker: a protocol error, so
# the worker is retired and the shell falls back to oneshot.
_tildy_frame_bad() {   # fd reason
  local fd=$1 g
  if [[ ${_tildy_ch_mode[$fd]} == oneshot ]]; then
    _tildy_ch_fgen[$fd]=
    [[ ${_tildy_ch_state[$fd]} == active ]] || return 0
    g=${_tildy_ch_gen[$fd]}
    _tildy_ch_state[$fd]=done
    [[ $_tildy_cur_fd == $fd ]] && _tildy_cur_fd=
    _tildy_log discard $g $2
    _tildy_kill $fd
  else
    [[ ${_tildy_ch_state[$fd]} == worker ]] || return 0
    g=${_tildy_ch_fgen[$fd]:-${_tildy_w_req_gen:-0}}
    _tildy_ch_fgen[$fd]=
    _tildy_log discard $g $2
    [[ $_tildy_w_fd == $fd ]] && _tildy_w_lost $2 kill
  fi
}

_tildy_frame_done() {
  local fd=$1 REPLY
  local g=${_tildy_ch_fgen[$fd]} st=${_tildy_ch_fstatus[$fd]} ep=${_tildy_ch_fepoch[$fd]}
  local left=${_tildy_ch_left[$fd]} right=${_tildy_ch_right[$fd]} trans=${_tildy_ch_trans[$fd]}
  local -i nl=${_tildy_ch_nleft[$fd]} nt=${_tildy_ch_ntrans[$fd]}
  _tildy_ch_fgen[$fd]=
  if [[ ${_tildy_ch_mode[$fd]} == oneshot ]]; then
    _tildy_ch_state[$fd]=done
    [[ $_tildy_cur_fd == $fd ]] && _tildy_cur_fd=
  elif [[ $g == $_tildy_w_req_gen && $_tildy_w_got != fresh ]]; then
    _tildy_w_got=$st
  fi
  _tildy_log recv $g $st $ep
  if [[ $g != $_tildy_gen ]] || (( _tildy_command_running )); then
    _tildy_log discard $g stale
    return 0
  fi
  # Remember the height for the next cycle too. An empty editor can redraw
  # at the new height now; the widget defers it while input is present.
  _tildy_next_lines=$nl
  _tildy_sanitize "$left"; left=$REPLY
  _tildy_sanitize "$right"; right=$REPLY
  _tildy_def_fd=$fd _tildy_def_epoch=$ep _tildy_def_seq=${_tildy_ch_seq[$fd]}
  _tildy_def_t=$trans _tildy_def_nt=$nt
  _tildy_def_key=${_tildy_ch_key[$fd]} _tildy_def_context=${_tildy_ch_context[$fd]}
  _tildy_def_suffix=${_tildy_ch_suffix[$fd]} _tildy_def_summary=${_tildy_ch_summary[$fd]}
  _tildy_def_gen=$g
  _tildy_def_left=$left
  _tildy_def_right=$right
  _tildy_def_lines=$nl
  _tildy_def_logged=0
  if zle; then
    # Run the state check as a widget so CONTEXT/WIDGET/... are visible;
    # -f nolast keeps LASTWIDGET untouched.
    zle _tildy-apply -f nolast
  else
    _tildy_log defer $g zle=inactive
    _tildy_def_logged=1
  fi
}

# ------------------------------------------------------------ safe redraw

# REPLY = reason the editor is not in a safe state, empty if safe.
_tildy_unsafe() {
  REPLY=
  if [[ $CONTEXT != start ]]; then
    REPLY="context=${CONTEXT:-none}"
  elif (( _tildy_busy > 0 )); then
    REPLY="widget=${WIDGET:-completion}"
  elif (( _tildy_isearch )) || [[ ${ISEARCHMATCH_ACTIVE:-0} == 1 ]]; then
    REPLY="state=isearch"
  fi
}

# Widget: draw the pending result if safe, else leave it pending.
# Log: drawn at once -> accept; drawn later -> defer ... apply_deferred;
# never drawn -> defer ... discard stale.
_tildy_apply_widget() {
  emulate -L zsh
  [[ -n $_tildy_def_gen ]] || return 0
  if [[ $_tildy_def_gen != $_tildy_gen ]] || (( _tildy_command_running )); then
    _tildy_log discard $_tildy_def_gen stale
    _tildy_def_gen=
    return 0
  fi
  local REPLY g=$_tildy_def_gen logged=$_tildy_def_logged
  _tildy_unsafe
  if [[ -z $REPLY && -n $BUFFER ]] && (( _tildy_def_lines != _tildy_lines )); then
    REPLY=height_with_input
  fi
  if [[ -n $REPLY ]]; then
    if (( ! logged )); then
      _tildy_log defer $g "$REPLY"
      _tildy_def_logged=1
    fi
    return 0
  fi
  _tildy_t_cycle=$_tildy_cycle _tildy_t_gen=$g _tildy_t_has=$_tildy_def_nt
  _tildy_sanitize "$_tildy_def_t"; _tildy_t_text=$REPLY
  _tildy_t_key=$_tildy_def_key
  _tildy_sanitize "$_tildy_def_context"; _tildy_t_context=$REPLY
  _tildy_sanitize "$_tildy_def_suffix"; _tildy_t_suffix=$REPLY
  _tildy_summary_enabled=0
  if [[ $_tildy_def_summary == <-> ]] && (( _tildy_t_has )); then
    local threshold=$_tildy_def_summary
    while [[ $threshold == 0?* ]]; do threshold=${threshold#0}; done
    if (( ${#threshold} > 19 )) || { (( ${#threshold} == 19 )) && [[ $threshold > 9223372036854775807 ]]; }; then
      threshold=9223372036854775807
    fi
    _tildy_summary_enabled=1 _tildy_summary_threshold=$threshold
  fi
  _tildy_set_prompt "$_tildy_def_left" "$_tildy_def_right" keep
  _tildy_shown "$_tildy_def_left" "$_tildy_def_right"
  if (( _tildy_lines != _tildy_def_lines )); then
    _tildy_log height_changed $g $_tildy_lines $_tildy_def_lines
    _tildy_lines=$_tildy_def_lines
  fi
  _tildy_def_gen= _tildy_base_cached=0
  zle .reset-prompt
  # Finish the redraw before ACK/accept and channel cleanup. reset-prompt
  # invalidates ZLE's display; an async callback must also flush that display.
  zle -R
  if [[ -n $_tildy_def_seq && $_tildy_def_fd == $_tildy_w_fd && $_tildy_def_epoch == $_tildy_w_epoch && $_tildy_w_req_gen == $g && -n $_tildy_w_wfd ]]; then
    syswrite -o $_tildy_w_wfd $'ACK\t'$g$'\t'$_tildy_def_epoch$'\t'$_tildy_def_seq$'\n' 2>/dev/null &&
    _tildy_log ack $g $_tildy_def_epoch $_tildy_def_seq
  fi
  if (( logged )); then
    _tildy_log apply_deferred $g
  else
    _tildy_log accept $g
  fi
  return 0
}

# zle-line-pre-redraw hook: the next safe moment for a deferred result
# (runs after every widget, including right after a completion widget or
# isearch returns).
_tildy_pre_redraw() {
  [[ -n $_tildy_def_gen ]] || return 0
  _tildy_sample_opts
  _tildy_apply_widget
}

_tildy_isearch_update() { _tildy_isearch=1 }
_tildy_isearch_exit() { _tildy_isearch=0 }

# Completion widgets run menu selection (and the "list all?" query) inside
# themselves, reading keys while zle -F handlers can still fire. A thin
# wrapper keeps a depth counter so those handlers know to defer.
_tildy_cwrap() {
  local orig=$1
  shift
  (( ++_tildy_busy ))
  {
    zle $orig -- "$@"
  } always {
    (( _tildy_busy > 0 )) && (( --_tildy_busy ))
  }
}

_tildy_wrap_completion_widgets() {
  _tildy_wrapped=1
  local w
  for w in ${(k)widgets}; do
    case $w in
      (_tildy_*|.*) continue ;;
    esac
    if [[ ${widgets[$w]} == completion:* ]] ||
       [[ $w == (complete-word|delete-char-or-list|expand-or-complete|expand-or-complete-prefix|list-choices|menu-complete|menu-expand-or-complete|reverse-menu-complete|accept-and-menu-complete|menu-select) ]]; then
      zle -A $w _tildy_orig_$w || continue
      functions[_tildy_cw_$w]="_tildy_cwrap ${(q)${:-_tildy_orig_$w}} \"\$@\""
      zle -N $w _tildy_cw_$w
    fi
  done
}

# -------------------------------------------------------------- transient
# The T line of the latest valid result of this prompt cycle is the collapsed
# form; its presence is how the shell learns history.mode is compact.

_tildy_transient_ready() {
  (( _tildy_t_has && _tildy_t_cycle == _tildy_cycle )) || return 1
  # Terminal.app creates native marks on Return, then loses them when the
  # accepted line is erased for a compact redraw. It cannot recreate those
  # marks from our preserved OSC 133 text. Keep its accepted prompt intact so
  # Cmd-Up/Down still navigates commands, including when compact is configured.
  if [[ ${TERM_PROGRAM-} == Apple_Terminal ]]; then
    _tildy_log transient_skipped $_tildy_t_gen terminal_native_marks
    return 1
  fi
  return 0
}

# zle-line-finish: the line was accepted (Enter, also from isearch). Only the
# first line of a command (CONTEXT=start); continuation lines keep PS2.
_tildy_line_finish() {
  _tildy_sample_opts
  emulate -L zsh
  [[ $CONTEXT == start ]] || return 0
  # Empty input dismisses the previous error display, without changing $?.
  # Track the accepted buffer, not preexec: syntax errors skip preexec too.
  _tildy_blank_submission=0
  [[ -z ${BUFFER//[[:space:]]/} ]] && _tildy_blank_submission=1
  _tildy_transient_ready || return 0
  local text=$_tildy_t_text
  if [[ -n $_tildy_history_key && -n $_tildy_t_key && $_tildy_history_key != $_tildy_t_key ]]; then
    if [[ -n $_tildy_t_context ]]; then
      text=$_tildy_t_context
    elif [[ -n $_tildy_t_suffix ]]; then
      # ZLE owns BUFFER and executes it exactly once; POSTDISPLAY is visual
      # text after BUFFER and participates in the final accepted-line redraw.
      local REPLY suffix=${(%)_tildy_t_suffix} last=${BUFFER##*$'\n'}
      local -i room=${COLUMNS:-80} used=0
      if [[ $BUFFER != *$'\n'* ]]; then _tildy_width "$text"; used=$REPLY; fi
      (( room -= used + ${(m)#last} + 1 ))
      if (( room > 4 )); then
        while (( ${(m)#suffix} > room )); do suffix=${suffix[1,-2]}; done
        POSTDISPLAY=$suffix
      fi
    fi
  fi
  [[ -n $_tildy_t_key ]] && _tildy_history_key=$_tildy_t_key
  _tildy_set_prompt "$text" "" keep
  zle .reset-prompt
  _tildy_log transient $_tildy_t_gen
  return 0
}

# Ctrl-C aborts the line without zle-line-finish and without a precmd, so
# collapse from the INT trap and put the full prompt back for the next line.
# Only when zle is idle at the top level: in isearch, a completion menu or a
# minibuffer, Ctrl-C ends that state and the line stays.
_tildy_int() {
  _tildy_sample_opts
  emulate -L zsh
  zle || return 0
  [[ $CONTEXT == start && $WIDGET == $LASTWIDGET ]] || return 0
  (( _tildy_busy == 0 && _tildy_isearch == 0 )) || return 0
  [[ ${ISEARCHMATCH_ACTIVE:-0} == 1 ]] && return 0
  _tildy_transient_ready || return 0
  local p=$PROMPT r=$RPROMPT pl=$_tildy_ps_left pr=$_tildy_ps_right
  _tildy_set_prompt "$_tildy_t_text" "" keep
  zle .reset-prompt
  zle -R
  PROMPT=$p RPROMPT=$r _tildy_ps_left=$pl _tildy_ps_right=$pr
  _tildy_log transient $_tildy_t_gen int
  return 0
}

# ------------------------------------------------------------- self-pipe

_tildy_wake_drain() {
  [[ -n $_tildy_wake_fd ]] || return 0
  local chunk
  while sysread -i $_tildy_wake_fd -s 4096 chunk 2>/dev/null; do :; done
}

_tildy_on_wake() {
  emulate -L zsh
  _tildy_wake_drain
  if (( _tildy_rz_pending )); then
    _tildy_rz_arm
  else
    _tildy_resize_refresh
  fi
  return 0
}

# New terminal size: new generation, new request (the shown prompt, fitted to
# the width if it had to be, stays until the result arrives). After a burst
# of SIGWINCH the generation is already bumped (_tildy_rz_pending): only the
# request is owed. Same size as the last request: nothing to do.
_tildy_resize_refresh() {
  (( _tildy_gen )) || return 0
  zle || return 0   # not editing: the next precmd asks anyway
  if (( _tildy_rz_pending )); then
    _tildy_rz_pending=0
    _tildy_request
    return 0
  fi
  [[ ${COLUMNS:-80} == $_tildy_req_cols && ${LINES:-24} == $_tildy_req_rows ]] && return 0
  _tildy_bump
  _tildy_request
}

# Debounce timer: a short-lived child that exits after $1 ms; its EOF wakes
# zle (zle -F), like the self-pipe. One at a time; it re-arms itself while
# SIGWINCH keeps coming.
_tildy_rz_arm() {
  [[ -n $_tildy_rz_fd ]] && return 0
  local REPLY fd=
  local -i wait
  _tildy_ms
  wait=$(( ${_tildy_rz_last:-$REPLY} + _tildy_rz_ms - REPLY ))
  (( wait < 1 )) && wait=1
  exec {fd}< <(_tildy_rz_sleep $wait)
  if [[ -z $fd ]]; then
    _tildy_resize_refresh   # no timer: ask at once
    return 0
  fi
  _tildy_rz_fd=$fd
  zle -F $fd _tildy_rz_fire
}

_tildy_rz_sleep() {   # ms
  local fd
  exec 2>/dev/null </dev/null
  for fd in ${_tildy_w_wfd-} ${_tildy_instant_fd0-} ${_tildy_instant_fd1-} ${_tildy_instant_fd2-}; do
    { exec {fd}<&- } 2>/dev/null
  done
  if (( ${+builtins[zselect]} )); then
    zselect -t $(( ($1 + 9) / 10 ))
  else
    command sleep $(( $1 / 1000.0 ))
  fi
  return 0
}

_tildy_rz_fire() {
  emulate -L zsh
  local fd=$1 REPLY
  zle -F $fd 2>/dev/null
  { exec {fd}<&- } 2>/dev/null
  [[ $_tildy_rz_fd == $fd ]] && _tildy_rz_fd=
  (( _tildy_rz_pending )) || return 0
  _tildy_ms
  if (( REPLY - ${_tildy_rz_last:-0} < _tildy_rz_ms - 5 )); then
    _tildy_rz_arm   # more SIGWINCH came meanwhile
    return 0
  fi
  _tildy_resize_refresh
  return 0
}

# Widget run from TRAPWINCH right after zle redrew the prompt at the new
# width (zsh: adjustwinsize, then the trap). ADR 0002 "缩放与终端重排":
#  - zle moved up by its idea of the prompt's height at the old width
#    (_tildy_cursor_row old old). A reflowing terminal had already re-wrapped
#    the old rows at the new width (_tildy_cursor_row new old), so the top
#    G rows of the old prompt stayed above zle's redraw. Moving the cursor up
#    G rows makes zle's next relative redraw start at the old first row, and
#    its clear-to-end-of-screen removes them.
#  - A line wider than the new width is replaced by the fitted form (zsh cuts
#    every line to the width, now and at every later SIGWINCH) until the
#    result for the new width arrives.
# Never in completion/isearch/other unsafe states (the safe-redraw rules).
_tildy_resize_widget() {
  emulate -L zsh
  local -i c=${COLUMNS:-80} cd=$_tildy_drawn_cols g=0 need=0 i n before
  local REPLY
  _tildy_drawn_cols=$c   # zle has just drawn the prompt at this width
  (( cd > 0 && c > 0 && cd != c )) || return 0
  [[ -n $_tildy_shown_left ]] || return 0
  _tildy_unsafe
  if [[ -n $REPLY ]]; then
    _tildy_log resize_skip $_tildy_gen "$REPLY"
    return 0
  fi
  _tildy_measure
  # Ghostty (also embedded by cmux) clears/repositions its marked prompt on
  # resize. Compensating for reflow again moves above that prompt and erases
  # command output. Its integration also marks the input in line-init when
  # another hook prevented PS1 decoration, so checking PS1 alone is too late.
  if (( _tildy_reflow )) && ! { [[ ${TERM_PROGRAM-} == ghostty || ${TERM-} == xterm-ghostty ]] &&
       (( ${+functions[_ghostty_precmd]} )); }; then
    _tildy_cursor_row $cd $cd
    before=$REPLY
    _tildy_cursor_row $c $cd
    (( g = REPLY - before ))
  fi
  if (( ! _tildy_fitted )); then
    n=${#_tildy_shown_w}
    for (( i = 1; i <= n; i++ )); do
      if (( _tildy_shown_w[i] > (i < n ? c : c - 1) )); then
        need=1
        break
      fi
    done
  fi
  (( g > 0 || need )) || return 0
  (( g > 0 )) && print -rn -- $'\e['$g'A'
  if (( need )); then
    _tildy_fit_text
    _tildy_set_prompt "$REPLY" "$_tildy_shown_right" keep
    _tildy_fitted=1
  fi
  zle .reset-prompt
  _tildy_rz_drew=1
  _tildy_log resize_fit $_tildy_gen $cd $c $g $_tildy_fitted
  return 0
}

# ------------------------------------------------------------------ hooks

_tildy_precmd() {
  local -i ret=$?
  # The first cycle may put backend text (the cached instant prompt) into PROMPT.
  _tildy_sample_opts
  # Normally the instant snippet's own precmd hook ran first. (Before emulate:
  # finish reads the user's PROMPT_SP.)
  [[ ${_tildy_instant_state-} == init ]] && _tildy_instant_finish tildy
  emulate -L zsh
  local REPLY
  _tildy_last_exit=$ret
  (( _tildy_blank_submission )) && _tildy_last_exit=0
  _tildy_blank_submission=0
  _tildy_ms
  if [[ -n $_tildy_cmd_t0 ]]; then
    _tildy_last_duration_ms=$(( REPLY - _tildy_cmd_t0 ))
  else
    _tildy_last_duration_ms=0
  fi
  _tildy_cmd_t0=
  _tildy_command_running=0
  POSTDISPLAY=
  _tildy_isearch=0
  _tildy_busy=0
  # First prompt: .zshrc has finished, so compinit and plugins are in place.
  (( _tildy_wrapped )) || _tildy_wrap_completion_widgets
  (( ++_tildy_cycle ))
  _tildy_wake_drain
  _tildy_rz_pending=0
  _tildy_bump
  _tildy_set_base
  _tildy_request
  # The base prompt is in place and nothing else is synchronous: zle can start.
  _tildy_log ready $_tildy_gen
}

# Print once before the next prompt. Do not compete for the last precmd slot:
# Ghostty needs it to wrap PROMPT so marks survive async redraws. Depending on
# integration order, the terminal's completion D can precede or follow this
# output; the terminal owns that boundary, while the summary stays before A.
_tildy_summary_precmd() {
  emulate -L zsh
  if (( _tildy_summary_due )); then
    _tildy_summary_due=0
    if (( _tildy_last_exit != 0 || _tildy_last_duration_ms >= _tildy_summary_threshold )); then
      local summary= part=
      (( _tildy_last_exit != 0 )) && summary="退出码 $_tildy_last_exit"
      if (( _tildy_last_duration_ms >= _tildy_summary_threshold )); then
        # Same shape as the elapsed module: 4.9s, 2m 3s, 1h 22m.
        local -i ms=$_tildy_last_duration_ms
        if (( ms < 60000 )); then part="$(( ms / 1000 )).$(( (ms / 100) % 10 ))s"
        elif (( ms < 3600000 )); then part="$(( ms / 60000 ))m $(( (ms / 1000) % 60 ))s"
        else part="$(( ms / 3600000 ))h $(( (ms / 60000) % 60 ))m"; fi
        summary+="${summary:+ · }$part"
      fi
      print -r -- "  ↳ $summary"
      _tildy_log summary $_tildy_gen $_tildy_last_exit $_tildy_last_duration_ms
    fi
  fi
  return 0
}

_tildy_preexec() {
  (( ++_tildy_submission_id ))
  _tildy_command_running=1
  _tildy_blank_submission=0
  _tildy_def_gen=
  _tildy_summary_due=$_tildy_summary_enabled
  local REPLY
  _tildy_ms
  _tildy_cmd_t0=$REPLY
}

_tildy_chpwd() {
  emulate -L zsh
  (( _tildy_gen )) || return 0
  _tildy_bump
  # cd from inside a widget: no precmd follows, so ask now.
  if zle; then
    _tildy_rz_pending=0
    _tildy_set_base
    _tildy_request
  fi
}

# SIGWINCH. zsh has already redrawn the prompt at the new width when this
# runs (and blocks SIGWINCH while zle is not waiting for input, so this never
# interrupts a widget or a zle -F handler). Here: bump the generation once per
# burst (results for the old width are stale), fix the screen up
# (_tildy-resize), and wake zle through the self-pipe; the request itself is
# debounced (_tildy_on_wake -> _tildy_rz_arm).
_tildy_winch() {
  emulate -L zsh
  (( _tildy_gen )) || return 0
  if ! zle; then
    # Not editing: the next precmd asks, and zle draws at the new width.
    _tildy_drawn_cols=${COLUMNS:-80}
    return 0
  fi
  local REPLY
  _tildy_ms
  _tildy_rz_last=$REPLY
  if (( ! _tildy_rz_pending )) &&
     [[ ${COLUMNS:-80} != $_tildy_req_cols || ${LINES:-24} != $_tildy_req_rows ]]; then
    _tildy_bump
    _tildy_rz_pending=1
  fi
  _tildy_rz_drew=0
  zle _tildy-resize -f nolast
  (( _tildy_rz_drew )) && zle -R
  if [[ -n $_tildy_wake_fd ]]; then
    syswrite -o $_tildy_wake_fd . 2>/dev/null
  else
    _tildy_resize_refresh   # no self-pipe: ask at once, no debounce
  fi
  return 0
}

# ----------------------------------------------------------- agent wrapping

# Known agent programs run through `tildy agent wrap` (a transparent pty
# proxy) so their OSC 7501 status reaches the prompts of other panes. Only on
# a terminal, with a usable TILDY_BIN and without TILDY_AGENT_WRAP=0; anything
# else runs the program directly. The exit status is the program's.
_tildy_agent_run() {   # name [args...]
  local name=$1
  shift
  if [[ ${TILDY_AGENT_WRAP-} != 0 && -t 0 && -t 1 ]] &&
     { [[ $TILDY_BIN == */* && -x $TILDY_BIN ]] || (( ${+commands[$TILDY_BIN]} )) }; then
    "$TILDY_BIN" agent wrap --app "$name" -- "$name" "$@"
  else
    command "$name" "$@"
  fi
}

# Define a wrapper function for each listed program found on PATH that is not
# already a function, alias, builtin or reserved word. The list is
# `_tildy_agent_programs` from `tildy init zsh` (config `agent.programs`;
# empty with `agent.wrap: false`), the built-in list when the script is
# sourced directly, or TILDY_AGENT_PROGRAMS. `unfunction NAME` removes one.
typeset -ga _tildy_agent_wrapped
_tildy_agent_define() {
  emulate -L zsh
  local p
  local -a list
  if [[ ${TILDY_AGENT_WRAP-} == 0 ]]; then
    return 0
  elif [[ -n ${TILDY_AGENT_PROGRAMS-} ]]; then
    list=(${(s:,:)${TILDY_AGENT_PROGRAMS//[[:space:]]/,}})
  elif (( ${+_tildy_agent_programs} )); then
    list=($_tildy_agent_programs)
  else
    list=(claude codex gemini aider opencode amp goose copilot cursor-agent)
  fi
  for p in $list; do
    [[ -n $p && $p != *[^A-Za-z0-9_.+-]* && $p != -* ]] || continue
    (( ${+functions[$p]} || ${+aliases[$p]} || ${+builtins[$p]} || ${reswords[(Ie)$p]} )) && continue
    (( ${+commands[$p]} )) || continue
    functions[$p]="'_tildy_agent_run' ${(qq)p} \"\$@\""
    _tildy_agent_wrapped+=($p)
  done
  (( ${#_tildy_agent_wrapped} )) && _tildy_log agent_wrap ${(j:,:)_tildy_agent_wrapped}
  return 0
}

# ------------------------------------------------------------------ setup

# A list-style trap (`trap '...' INT`) would be replaced by a TRAPxxx
# function, so only chain onto function traps. `trap` prints list traps as
# `trap -- '<cmd>' SIG` at column 0 (function bodies are indented).
_tildy_list_trap() {   # SIGNAL dir
  local f=$2/traps t= l
  { trap >| $f && IFS= read -rd '' t < $f } 2>/dev/null
  zf_rm -f $f 2>/dev/null
  for l in "${(@f)t}"; do
    [[ $l == "trap -- "*" $1" || $l == *"' $1" ]] && return 0
  done
  return 1
}

() {
  emulate -L zsh
  # emulate -L implies local_traps, which would drop TRAPWINCH/TRAPINT again
  # when this function returns.
  setopt no_local_traps
  local dir= fd= REPLY persistent=0
  _tildy_persistent && persistent=1
  # One mkfifo for the self-pipe and (in persistent mode) the worker pipes.
  if (( persistent )); then
    _tildy_mkfifos wake req resp && dir=$REPLY
  else
    _tildy_mkfifos wake && dir=$REPLY
  fi
  if [[ -n $dir ]]; then
    if sysopen -rw -o cloexec,nonblock -u fd $dir/wake 2>/dev/null; then
      _tildy_wake_fd=$fd
      zle -F $fd _tildy_on_wake
    fi
    zf_rm -f $dir/wake 2>/dev/null
  fi

  add-zsh-hook precmd _tildy_precmd && _tildy_log hook_installed precmd
  add-zsh-hook chpwd _tildy_chpwd && _tildy_log hook_installed chpwd
  add-zsh-hook preexec _tildy_preexec
  _tildy_agent_define

  # No hook arrays for signals: keep an existing TRAPxxx function and chain it.
  if [[ -n $dir ]] && _tildy_list_trap WINCH $dir; then
    _tildy_log hook_skipped winch list_trap
  else
    (( ${+functions[TRAPWINCH]} )) && functions -c TRAPWINCH _tildy_orig_TRAPWINCH
    TRAPWINCH() {
      _tildy_winch
      if (( ${+functions[_tildy_orig_TRAPWINCH]} )); then
        _tildy_orig_TRAPWINCH "$@"
        return
      fi
      return 0
    }
    _tildy_log hook_installed winch
  fi
  if [[ -n $dir ]] && _tildy_list_trap INT $dir; then
    _tildy_log hook_skipped int list_trap
  else
    (( ${+functions[TRAPINT]} )) && functions -c TRAPINT _tildy_orig_TRAPINT
    TRAPINT() {
      _tildy_int
      if (( ${+functions[_tildy_orig_TRAPINT]} )); then
        _tildy_orig_TRAPINT "$@"
        return
      fi
      # Non-zero: behave as if there were no trap (abort the line/command).
      return $(( 128 + $1 ))
    }
    _tildy_log hook_installed int
  fi

  zle -N _tildy-apply _tildy_apply_widget
  zle -N _tildy-resize _tildy_resize_widget
  zle -N _tildy-pre-redraw _tildy_pre_redraw
  zle -N _tildy-isearch-update _tildy_isearch_update
  zle -N _tildy-isearch-exit _tildy_isearch_exit
  zle -N _tildy-line-finish _tildy_line_finish
  add-zsh-hook precmd _tildy_summary_precmd
  add-zle-hook-widget line-pre-redraw _tildy-pre-redraw
  add-zle-hook-widget isearch-update _tildy-isearch-update
  add-zle-hook-widget isearch-exit _tildy-isearch-exit
  add-zle-hook-widget line-finish _tildy-line-finish

  # Start the worker now so it can be ready by the first prompt. It removes
  # its FIFOs and the directory.
  if (( persistent )); then
    if [[ -n $dir ]]; then
      _tildy_w_start $dir
    else
      _tildy_w_start
    fi
  elif [[ -n $dir ]]; then
    zf_rmdir $dir 2>/dev/null
  fi
  # Loading never fails the caller's `eval`/`source` (and so the first
  # prompt's $?); problems show up as backend_degraded in the debug log.
  return 0
}

elif [[ -o interactive ]] && (( ${+_tildy_loaded} )); then
  # Evaluated again in a running shell (`source ~/.zshrc` after a rebuild).
  # The functions above stay the loaded version, so TILDY_BIN/TILDY_VERSION,
  # which `tildy init zsh` just reset to the new binary, must match them
  # again: otherwise the next worker start would fail the version check, the
  # replacement binary's frames would be rejected by this parser, and the
  # prompt would stay at the base prompt. The new version is used by new
  # shells; say so once per version.
  if [[ ${TILDY_VERSION-} != ${_tildy_loaded_version-} ]]; then
    # Resolve at reload time, including when the loaded adapter predates i18n.
    typeset -g _tildy_notice_locale=${TILDY_LANG:-en}
    [[ ${(L)_tildy_notice_locale} == auto ]] && \
      _tildy_notice_locale=${LC_ALL:-${LC_MESSAGES:-${LANG:-en}}}
    case ${(L)_tildy_notice_locale} in
      (zh|zh_*|zh-*|zh.*)
        'builtin' 'print' -u2 -r -- \
          "tildy: 本 shell 仍使用已加载的版本（${_tildy_loaded_version:-未检查}）；新版本 ${TILDY_VERSION:-未检查} 在新开的 shell 中生效（可运行 exec zsh）" ;;
      (*)
        'builtin' 'print' -u2 -r -- \
          "tildy: This shell is still using version ${_tildy_loaded_version:-unchecked}. Version ${TILDY_VERSION:-unchecked} will take effect in a new shell (run exec zsh)." ;;
    esac
    unset _tildy_notice_locale
  fi
  typeset -gx TILDY_BIN=$_tildy_loaded_bin TILDY_VERSION=$_tildy_loaded_version
  typeset -g TILDY_PROTOCOL_VERSION=$_tildy_loaded_protocol
fi   # load guard
