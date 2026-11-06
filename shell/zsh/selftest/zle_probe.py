#!/usr/bin/env python3
"""Reproduce the zsh 5.9 ZLE observations behind ADR 0002 (safe redraw).

Usage: python3 shell/zsh/selftest/zle_probe.py

Independent of tildy.zsh: each case starts `zsh -i` with a throw-away ZDOTDIR
whose .zshrc defines a timer fd (`probe <secs>`) and a handler that logs the
zle state and, optionally, calls `zle .reset-prompt` after setting PS1='NEW> '.
The screen is rendered with pyte. The PTY is drained continuously (zsh blocks
in tcsetattr(TCSADRAIN) otherwise).
"""
import os, sys, time, tempfile, shutil
import pexpect, pyte

RC = r"""
unsetopt global_rcs
PS1='P> '
zmodload zsh/system zsh/complist
autoload -Uz compinit add-zle-hook-widget && compinit -u -d $ZDOTDIR/zcd
zstyle ':completion:*' menu select
[[ -n $PROBE_MENU_COMPLETE ]] && setopt menu_complete
L() { print -r -- "$*" >>| $PROBE_LOG }
_dump() { local z=n; zle && z=y
  L "$1: CONTEXT=${CONTEXT-<unset>} WIDGET=${WIDGET-<unset>} LASTWIDGET=${LASTWIDGET-<unset>} KEYMAP=${KEYMAP-<unset>} ZLE_STATE=${ZLE_STATE-<unset>} ISEARCHMATCH_ACTIVE=${ISEARCHMATCH_ACTIVE-<unset>} zle_active=$z" }
_reset() { [[ -n $PROBE_RESET ]] || return 0; PS1='NEW> '; zle .reset-prompt; }
_w_check() { _dump nolast-widget; _reset }
zle -N _w_check
_h_plain() { local fd=$1 x; sysread -i $fd x; zle -F $fd; exec {fd}<&-; _dump plain-handler
  case $PROBE_KIND in (plain) zle && _reset ;; (nolast) zle _w_check -f nolast ;; esac }
_h_widget() { local fd=$1 x; sysread -i $fd x; zle -F $fd; exec {fd}<&-; _dump w-handler; _reset }
zle -N _h_widget
probe() { local fd; exec {fd}< <(sleep $1; print x)
  if [[ $PROBE_KIND == w ]]; then zle -F -w $fd _h_widget; else zle -F $fd _h_plain; fi }
_prd() { L "pre-redraw: LASTWIDGET=$LASTWIDGET" }
zle -N _prd
[[ -n $PROBE_PRD ]] && add-zle-hook-widget line-pre-redraw _prd
lw() { L "next-widget sees LASTWIDGET=$LASTWIDGET" }; zle -N lw; bindkey '^Xl' lw
TRAPWINCH() { local z=n; zle && z=y; L "TRAPWINCH zle_active=$z" }
"""


def run(title, steps, **env):
    tmp = tempfile.mkdtemp(prefix="zleprobe-", dir="/tmp")
    log = os.path.join(tmp, "log")
    open(log, "w").close()
    with open(os.path.join(tmp, ".zshrc"), "w") as f:
        f.write(RC)
    e = {k: v for k, v in os.environ.items() if not k.startswith(("ZSH", "TILDY_"))}
    e.update(ZDOTDIR=tmp, TERM="xterm-256color", PROBE_LOG=log)
    e.update({k: v for k, v in env.items()})
    scr = pyte.Screen(100, 30)
    st = pyte.Stream(scr)
    c = pexpect.spawn("zsh", ["-i"], env=e, encoding="utf-8", dimensions=(30, 100), cwd=tmp)

    def pump(s):
        end = time.time() + s
        while time.time() < end:
            try:
                st.feed(c.read_nonblocking(65536, timeout=0.03))
            except pexpect.TIMEOUT:
                pass
            except pexpect.EOF:
                return

    pump(1.0)
    print(f"=== {title}  [{' '.join(f'{k}={v}' for k, v in env.items())}]")
    for s in steps:
        if s[0] == "send":
            c.send(s[1])
        elif s[0] == "pump":
            pump(s[1])
        elif s[0] == "resize":
            c.setwinsize(*s[1])
            scr.resize(*s[1])
        elif s[0] == "show":
            lines = [l.rstrip() for l in scr.display]
            while lines and not lines[-1]:
                lines.pop()
            print(f"  screen [{s[1]}]")
            for l in lines[-5:]:
                print("   |" + l)
    c.terminate(force=True)
    for l in open(log).read().splitlines():
        print("  log: " + l)
    shutil.rmtree(tmp, ignore_errors=True)


S, P, SH = (lambda x: ("send", x)), (lambda x: ("pump", x)), (lambda x: ("show", x))

# 1. Which zle parameters a handler sees (plain / -w / plain + `zle w -f nolast`)
for kind in ("plain", "w", "nolast"):
    run(f"1 params while typing, handler kind={kind}",
        [S("probe 0.5\r"), P(0.2), S("echo abc"), P(0.6), S("\x18l"), P(0.3)], PROBE_KIND=kind)

# 2. Menu selection vs finished completion (nolast check widget)
run("2a menu selection active (menu_complete: first Tab enters the menu)",
    [S("probe 0.8\r"), P(0.2), S("ls /\t"), P(1.0), SH("handler fired inside menu")],
    PROBE_KIND="nolast", PROBE_MENU_COMPLETE="1", PROBE_PRD="1")
run("2b unique completion finished",
    [S("probe 0.8\r"), P(0.2), S("ls /us\t"), P(1.0)], PROBE_KIND="nolast", PROBE_PRD="1")
run("2c Tab Tab (second Tab enters the menu): WIDGET == LASTWIDGET collision",
    [S("probe 1.0\r"), P(0.2), S("ls /\t"), P(0.3), S("\t"), P(1.0)], PROBE_KIND="nolast")

# 3. Visual effect of reset-prompt in each state
R = dict(PROBE_KIND="nolast", PROBE_RESET="1")
run("3a reset-prompt while typing", [S("probe 0.5\r"), P(0.2), S("echo hello"), P(0.6), SH("after")], **R)
run("3b reset-prompt inside menu selection",
    [S("probe 0.8\r"), P(0.2), S("ls /\t"), P(0.3), SH("before"), P(0.8), SH("after")],
    PROBE_MENU_COMPLETE="1", **R)
run("3c reset-prompt with a plain completion list shown",
    [S("zstyle -d ':completion:*' menu; probe 0.8\r"), P(0.2), S("ls /\t"), P(0.3), SH("before"), P(0.8), SH("after")], **R)
run("3d reset-prompt during isearch",
    [S("echo foo1\r"), P(0.3), S("probe 0.6\r"), P(0.2), S("\x12fo"), P(0.3), SH("before"), P(0.6), SH("after")], **R)
run("3e reset-prompt in PS2 (CONTEXT=cont)",
    [S("probe 0.6\r"), P(0.2), S("for i in 1; do\r"), P(0.3), SH("before"), P(0.6), SH("after")], **R)
run("3f reset-prompt in vi command mode",
    [S("bindkey -v; probe 0.6\r"), P(0.2), S("echo vi\x1b"), P(0.8), SH("after")], **R)

run("3g reset-prompt in the execute-named-cmd minibuffer (M-x)",
    [S("probe 0.6\r"), P(0.2), S("echo abc\x1bx"), P(0.3), SH("before"), P(0.6), SH("after"),
     S("end-of-line\r"), P(0.3), S("\r"), P(0.3), SH("executed")], **R)
run("3h reset-prompt while quoted-insert waits for a key (^V)",
    [S("probe 0.6\r"), P(0.2), S("echo q\x16"), P(0.8), SH("after"), S("\x09Z\r"), P(0.3), SH("executed")], **R)

# 4. A -w handler overwrites LASTWIDGET; `zle w -f nolast` does not
run("4a -w handler then next widget", [S("probe 0.4\r"), P(0.2), S("a"), P(0.5), S("\x18l"), P(0.3)], PROBE_KIND="w")
run("4b nolast call then next widget", [S("probe 0.4\r"), P(0.2), S("a"), P(0.5), S("\x18l"), P(0.3)], PROBE_KIND="nolast")

# 5. Resize while idle: TRAPWINCH runs, line-pre-redraw does not
run("5 resize while idle", [S("true\r"), P(0.3), ("resize", (24, 80)), P(0.5)], PROBE_PRD="1")
