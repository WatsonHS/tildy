#!/bin/bash
# STUB backend for harness self-test only. Oneshot; honours TILDY_FAKE_* basics.
IFS=$'\t' read -r tag gen sid cwd cols lines le ld
base=${cwd##*/}
delay=${TILDY_FAKE_DELAY_MS:-0}
if [[ -n $TILDY_FAKE_DELAY_RULES ]]; then
  IFS=, read -ra rs <<<"$TILDY_FAKE_DELAY_RULES"
  for r in "${rs[@]}"; do [[ ${r%%=*} == "$base" ]] && delay=${r#*=}; done
fi
[[ $TILDY_FAKE_FAIL == 1 ]] && exit 3
if (( delay > 0 )); then
  trap 'kill $spid 2>/dev/null; exit 143' TERM
  sleep "$(printf '%d.%03d' $((delay/1000)) $((delay%1000)))" & spid=$!
  wait $spid
fi
extra=
[[ $TILDY_FAKE_PERCENT == 1 ]] && extra=' 100%%'
printf 'RES\t%s\tfresh\n' "$gen"
printf 'L\t[g%s %s]%s %%#\n' "$gen" "$base" "$extra"
[[ $TILDY_FAKE_HALF_FRAME == 1 ]] && exit 0
if [[ $TILDY_FAKE_BAD_GEN == 1 ]]; then printf 'END\t%s\n' "$((gen+1000))"; else printf 'END\t%s\n' "$gen"; fi
