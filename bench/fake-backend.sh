#!/bin/sh
# Tildy fake render backend, shell protocol v1 (docs/protocol.md); a stand-in
# for the `tildy` binary's `worker` and `render` subcommands.
#
#   fake-backend.sh worker [--parent-pid PID]   persistent: HELLO, then answer
#                                               REQ lines until BYE or EOF
#   fake-backend.sh render                      oneshot: answer one REQ, exit
#   fake-backend.sh                             legacy M0 (protocol v0) frames:
#                                               no HELLO, RES without epoch
#
# Left prompt is "[g<gen> <basename of cwd>] %# " so a screen scraper can find
# it (the prompt character is configurable, see TILDY_FAKE_CHAR).
#
# Environment knobs (M0 contract):
#   TILDY_FAKE_DELAY_MS=ms           fixed delay before answering
#   TILDY_FAKE_DELAY_RULES=a=ms,b=ms per-basename delay, wins over DELAY_MS
#   TILDY_FAKE_FAIL=1                exit 1 without output
#   TILDY_FAKE_HALF_FRAME=1          emit RES and L only (no END)
#   TILDY_FAKE_BAD_GEN=1             END carries gen+1
#   TILDY_FAKE_CRASH_AFTER=n         exit 3 after answering n requests
#   TILDY_FAKE_PERCENT=1             add R content built from a raw "%" and a
#                                    raw BEL, run through the encoder below
#                                    (-> "pct=100%% bel=^G")
#   TILDY_FAKE_RAW_CTRL=1            append raw ESC/BEL bytes to R, unencoded
#                                    (tests the Zsh-side sanitizer)
# Protocol v1 knobs (M1):
#   TILDY_FAKE_VERSION=s             HELLO binary version (default 0.1.0-m1)
#   TILDY_FAKE_PROTOCOL=n            HELLO protocol version (default 1)
#   TILDY_FAKE_EPOCH=n               worker epoch (default: own pid)
#   TILDY_FAKE_PARTIAL=1             worker only: send a "partial" frame at
#                                    once (R "pending"), then the "fresh" one
#                                    (R "ready") after the delay
#   TILDY_FAKE_SECOND_FRESH_MS=ms    worker only: ms after the fresh frame,
#                                    send another "fresh" one for the same
#                                    gen (R "late"), like a provider result
#                                    that arrives after the worker's timeout
#   TILDY_FAKE_RES_EPOCH=n           worker only: epoch written into RES
#                                    (HELLO keeps the real one)
#   TILDY_FAKE_LINES=n               n L lines; lines 1..n-1 are
#                                    "-- g<gen> line <i> --"
#   TILDY_FAKE_TRANSIENT=1           add the T line "❯ "
#   TILDY_FAKE_CHAR=s                prompt character instead of "%#"
#   TILDY_FAKE_POWERLINE=1           realistic two-line powerline result (Nerd
#                                    glyphs); L line 2 is "%F{green}❯%f ",
#                                    T line "❯ ". Overrides TILDY_FAKE_LINES.
#   TILDY_FAKE_POWERLINE=2           same, but a single L line: the powerline
#                                    segments end the line, "<sep> " is the
#                                    last thing before the cursor
#   TILDY_FAKE_WIDE=n                with TILDY_FAKE_POWERLINE=1: L line 1 is
#                                    "[g<gen> <dir>]-----WIDEEND" on a
#                                    background, min(n, cols) columns wide
#                                    (cols from the REQ), like a renderer
#                                    that shrinks rows to the terminal width
#   TILDY_FAKE_ECHO_REQ=1            append "jobs=N env=<fields joined by |>"
#                                    from the REQ to the R line (9 or 10 field
#                                    REQ both accepted; jobs absent -> 0)
#   TILDY_FAKE_TRACE=file            append "hello|req|bye|exit ..." lines
#
# Kept to POSIX sh builtins on the hot path: no command substitution, no
# external commands except `sleep` when a delay is requested.

set -f
TAB='	'
handled=0
mode=${1:-legacy}

trace() {
  [ -n "${TILDY_FAKE_TRACE:-}" ] && printf '%s\n' "$*" >>"$TILDY_FAKE_TRACE"
  return 0
}

# encode TEXT -> $enc : prompt-safe text. "%" -> "%%", control chars -> ^X
# caret form (or "?"), "\t" "\n" "\\" escapes from the request are decoded
# first and then shown in caret form.
encode() {
  _in=$1
  enc=
  case $_in in
    *[\\%[:cntrl:]]*) ;;
    *) enc=$_in; return ;;
  esac
  [ -n "${BEL:-}" ] || BEL=$(printf '\007')
  while [ -n "$_in" ]; do
    _rest=${_in#?}
    _ch=${_in%"$_rest"}
    _in=$_rest
    case $_ch in
      \\)
        _rest=${_in#?}
        _nx=${_in%"$_rest"}
        _in=$_rest
        case $_nx in
          t) enc="$enc^I" ;;
          n) enc="$enc^J" ;;
          \\) enc="$enc\\" ;;
          *) enc="$enc\\$_nx" ;;
        esac
        ;;
      %) enc="$enc%%" ;;
      "$BEL") enc="$enc^G" ;;
      [[:cntrl:]]) enc="$enc?" ;;
      *) enc="$enc$_ch" ;;
    esac
  done
}

# frame STATUS EXTRA_R -> $out : one RES ... END frame for $gen/$enc
frame() {
  if [ "$mode" = legacy ]; then
    out="RES${TAB}${gen}${TAB}$1"
  else
    out="RES${TAB}${gen}${TAB}$1${TAB}${res_epoch}"
  fi
  i=1
  while [ "$i" -lt "$nlines" ]; do
    out="$out
L${TAB}-- g${gen} line ${i} --"
    i=$((i + 1))
  done
  if [ -n "${TILDY_FAKE_POWERLINE:-}" ]; then
    # powerline segments with Nerd glyphs (U+F007 user, U+E0B0 separator, U+F07C folder)
    pl="%K{#9A348E}%F{white}  u %k%F{#9A348E}%K{#DA627D}%F{white}  ${enc} %k%F{#DA627D}%f"
    if [ -n "${TILDY_FAKE_WIDE:-}" ]; then
      _w=$TILDY_FAKE_WIDE
      case $req_cols in '' | *[!0-9]*) ;; *) [ "$req_cols" -lt "$_w" ] && _w=$req_cols ;; esac
      _head="[g${gen} ${enc}]"
      _pad=
      _n=$((_w - ${#_head} - 7))
      while [ "$_n" -gt 0 ]; do _pad="$_pad-"; _n=$((_n - 1)); done
      pl="%K{#9A348E}%F{white}${_head}${_pad}WIDEEND%k%f"
    fi
    if [ "$TILDY_FAKE_POWERLINE" = 2 ]; then
      out="$out
L${TAB}$pl "
    else
      out="$out
L${TAB}$pl
L${TAB}%F{green}❯%f "
    fi
  else
    out="$out
L${TAB}[g${gen} ${enc}] ${char} "
  fi

  r=$2
  if [ "${TILDY_FAKE_ECHO_REQ:-}" = 1 ]; then
    # Full REQ fields remain in the trace for protocol assertions. The visual
    # echo includes user/module data only: PATH and presentation identities can
    # exceed the test terminal width and correctly suppress the whole RPROMPT,
    # hiding jobs=N even though the field was transmitted correctly.
    _env_short=$(printf '%s' "$req_env" | tr '\037|' '\n\n' | grep -Ev '^(PATH|TERM|TERM_PROGRAM|TERM_PROGRAM_VERSION|LC_ALL|LC_CTYPE|LANG|TILDY_[A-Z0-9_]+)=' | tr '\n' '|')
    r="${r:+$r }jobs=${req_jobs} env=${_env_short%|}"
  fi
  if [ "${TILDY_FAKE_PERCENT:-}" = 1 ]; then
    encode "pct=100% bel=$(printf '\007')"
    r="${r:+$r }$enc"
    encode "$base"
  fi
  if [ "${TILDY_FAKE_RAW_CTRL:-}" = 1 ]; then
    r="$r$(printf '\033[31m')raw$(printf '\007')"
  fi
  [ -n "$r" ] && out="$out
R${TAB}${r}"
  { [ "${TILDY_FAKE_TRANSIENT:-}" = 1 ] || [ -n "${TILDY_FAKE_POWERLINE:-}" ]; } && out="$out
T${TAB}❯ "

  if [ -n "${TILDY_FAKE_OBSERVATION:-}" ]; then
    out="$out
O${TAB}${TILDY_FAKE_OBSERVATION}"
    if [ "${TILDY_FAKE_DUPLICATE_OBSERVATION:-}" = 1 ]; then out="$out
O${TAB}${TILDY_FAKE_OBSERVATION}"; fi
  fi
  if [ "${TILDY_FAKE_HALF_FRAME:-}" != 1 ]; then
    endgen=$gen
    [ "${TILDY_FAKE_BAD_GEN:-}" = 1 ] && endgen=$((gen + 1))
    out="$out
END${TAB}${endgen}"
  fi
}

# sleep_ms MS : sleep MS milliseconds (no-op for 0 or garbage)
sleep_ms() {
  _ms=$1
  case $_ms in
    '' | *[!0-9]*) return 0 ;;
  esac
  [ "$_ms" -gt 0 ] || return 0
  _s=$((_ms / 1000))
  _f=$((_ms % 1000))
  case $_f in
    ?) _f=00$_f ;;
    ??) _f=0$_f ;;
  esac
  sleep "$_s.$_f"
}

case $mode in
  worker) epoch=${TILDY_FAKE_EPOCH:-$$}; res_epoch=${TILDY_FAKE_RES_EPOCH:-$epoch} ;;
  *) epoch=0; res_epoch=0 ;;
esac
nlines=${TILDY_FAKE_LINES:-1}
case $nlines in
  '' | *[!0-9]* | 0) nlines=1 ;;
esac
[ -n "${TILDY_FAKE_POWERLINE:-}" ] && nlines=1
char=${TILDY_FAKE_CHAR:-%#}

if [ "$mode" = worker ]; then
  ver=${TILDY_FAKE_VERSION:-0.1.0-m1}
  proto=${TILDY_FAKE_PROTOCOL:-1}
  printf 'HELLO\t%s\t%s\t%s\t%s\n' "$proto" "$ver" "$epoch" "$$"
  trace "hello $proto $ver $epoch $$"
fi

while IFS= read -r line || [ -n "$line" ]; do
  case $line in
    REQ"$TAB"*) ;;
    ACK"$TAB"*) trace "ack $line"; continue ;;
    BYE)
      trace "bye $epoch"
      exit 0 ;;
    *) continue ;;
  esac
  trace "req $line"
  rest=${line#REQ"$TAB"}
  gen=${rest%%"$TAB"*}
  rest=${rest#*"$TAB"}
  # session_id
  rest=${rest#*"$TAB"}
  cwd=${rest%%"$TAB"*}
  case $gen in
    '' | *[!0-9]*) continue ;;
  esac
  req_cols=${rest#*"$TAB"}
  req_cols=${req_cols%%"$TAB"*}
  # fields after cwd: cols lines last_exit duration env [jobs]
  req_jobs=0
  req_env=
  if [ "${TILDY_FAKE_ECHO_REQ:-}" = 1 ]; then
    _t=${rest#*"$TAB"}   # now: cwd cols ...
    _t=${_t#*"$TAB"}; _t=${_t#*"$TAB"}; _t=${_t#*"$TAB"}; _t=${_t#*"$TAB"}
    case $_t in
      *"$TAB"*) req_env=${_t%%"$TAB"*}; req_jobs=${_t#*"$TAB"} ;;
      *) req_env=$_t ;;
    esac
    case $req_jobs in '' | *[!0-9]*) req_jobs=0 ;; esac
    US=$(printf '\037')
    _e=
    while :; do
      case $req_env in
        *"$US"*) _e="$_e${req_env%%"$US"*}|"; req_env=${req_env#*"$US"} ;;
        *) break ;;
      esac
    done
    req_env="$_e$req_env"
    encode "$req_env"; req_env=$enc
  fi

  base=${cwd##*/}
  [ -n "$base" ] || base=/
  encode "$base"

  if [ "$mode" = worker ] && [ "${TILDY_FAKE_PARTIAL:-}" = 1 ]; then
    frame partial pending
    printf '%s\n' "$out"
  fi

  delay=${TILDY_FAKE_DELAY_MS:-0}
  if [ -n "${TILDY_FAKE_DELAY_RULES:-}" ]; then
    _ifs=$IFS
    IFS=,
    for rule in $TILDY_FAKE_DELAY_RULES; do
      case $rule in
        "$base="*) delay=${rule#*=} ;;
      esac
    done
    IFS=$_ifs
  fi
  sleep_ms "$delay"

  if [ "${TILDY_FAKE_FAIL:-}" = 1 ]; then
    trace "exit 1"
    exit 1
  fi

  if [ "$mode" = worker ] && [ "${TILDY_FAKE_PARTIAL:-}" = 1 ]; then
    frame fresh ready
  else
    frame fresh ""
  fi
  printf '%s\n' "$out"
  # A deliberately truncated result models a producer interrupted mid-frame.
  if [ "${TILDY_FAKE_HALF_FRAME:-}" = 1 ]; then trace "exit half_frame"; exit 0; fi

  if [ "$mode" = worker ] && [ -n "${TILDY_FAKE_SECOND_FRESH_MS:-}" ]; then
    sleep_ms "$TILDY_FAKE_SECOND_FRESH_MS"
    frame fresh late
    printf '%s\n' "$out"
  fi

  handled=$((handled + 1))
  if [ -n "${TILDY_FAKE_CRASH_AFTER:-}" ] && [ "$handled" -ge "$TILDY_FAKE_CRASH_AFTER" ]; then
    trace "exit 3"
    exit 3
  fi
done
trace "exit 0"
exit 0
