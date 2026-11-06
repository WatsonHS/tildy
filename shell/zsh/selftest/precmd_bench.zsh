# Micro-benchmark of _tildy_precmd outside zle (ADR 0003 "证据").
#
#   zsh -f -i -c 'source shell/zsh/selftest/precmd_bench.zsh'               # fake worker
#   MODE=oneshot zsh -f -i -c 'source shell/zsh/selftest/precmd_bench.zsh'  # fake oneshot
#   BACKEND=target/release/tildy zsh -f -i -c 'source shell/zsh/selftest/precmd_bench.zsh'
#
# Calls _tildy_precmd N times (default 300) back to back and prints p50/p95/max
# in microseconds. Replies are read between calls (untimed) without zle, so the
# numbers are the synchronous cost only (no prompt drawing, no debug log).
() {
  local root=${${(%):-%x}:A:h:h:h:h}
  local backend=${BACKEND:-$root/bench/fake-backend.sh}
  [[ $backend == /* ]] || backend=$root/$backend
  typeset -g TILDY_BACKEND=$backend TILDY_BACKEND_MODE=${MODE:-persistent}
  unset TILDY_DEBUG_LOG
  source $root/shell/zsh/tildy.zsh
  zmodload zsh/datetime
  local -i i n=${N:-300}
  if [[ $TILDY_BACKEND_MODE != oneshot ]]; then
    for i in {1..200}; do
      _tildy_w_poll
      [[ $_tildy_w_state == ready ]] && break
      sleep 0.01
    done
  fi
  local -F t0 t1
  local -i us
  local -a xs
  local fd chunk
  for i in {1..$n}; do
    t0=$EPOCHREALTIME
    _tildy_precmd
    t1=$EPOCHREALTIME
    (( us = (t1 - t0) * 1e6 ))
    xs+=$us
    # Read replies as zle would (untimed): worker without waiting; a oneshot
    # channel to its EXIT line, so processes and pipes don't pile up.
    [[ -n $_tildy_w_fd ]] && _tildy_w_poll
    for fd in ${(k)_tildy_ch_state}; do
      [[ $fd == $_tildy_w_fd ]] && continue
      while [[ -n ${_tildy_ch_state[$fd]-} ]] && sysread -t 2 -i $fd -s 8192 chunk; do
        _tildy_feed $fd "$chunk"
      done
      [[ -n ${_tildy_ch_state[$fd]-} ]] && _tildy_finish $fd -1
    done
  done
  xs=(${(on)xs})
  printf 'backend=%s mode=%s worker=%s n=%d p50=%d p95=%d max=%d (us)\n' \
    ${backend:t} $TILDY_BACKEND_MODE $_tildy_w_state $n \
    $xs[$(( (n + 1) / 2 ))] $xs[$(( (n * 95 + 99) / 100 ))] $xs[-1]
}
