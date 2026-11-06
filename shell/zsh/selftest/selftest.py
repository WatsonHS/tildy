#!/usr/bin/env python3
"""Task A self-test for shell/zsh/tildy.zsh (pexpect + pyte, isolated ZDOTDIR).

Usage: python3 shell/zsh/selftest/selftest.py [-v] [scenario ...]

Each scenario starts `zsh -i` with a throw-away ZDOTDIR whose .zshrc only sets
PS1, compinit (menu select + menu_complete) and sources tildy.zsh. The PTY is
drained continuously: zsh blocks in tcsetattr(TCSADRAIN) if nobody reads it.
"""
import os, re, sys, time, shutil, tempfile, subprocess
import pexpect, pyte

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
SCRIPT = os.path.join(ROOT, "shell", "zsh", "tildy.zsh")
BACKEND = os.path.join(ROOT, "bench", "fake-backend.sh")
FLAKY = os.path.join(ROOT, "shell", "zsh", "selftest", "flaky-backend.sh")
VERBOSE = "-v" in sys.argv
FAKE_RE = re.compile(r"\[g(\d+) ([^\]]+)\] %")
BASE_RE = re.compile(r"^[~/]\S* % ?$")


class Fail(Exception):
    pass


class Skip(Exception):
    pass


class Shell:
    def __init__(self, tmp, env=None, rc_extra="", rows=30, cols=100, mode="oneshot", backend=BACKEND, rc_post=""):
        self.tmp = tmp
        self.log = os.path.join(tmp, "debug.log")
        open(self.log, "w").close()
        zd = os.path.join(tmp, "zdotdir")
        os.makedirs(zd, exist_ok=True)
        with open(os.path.join(zd, ".zshenv"), "w") as f:
            f.write("unsetopt global_rcs\n")
        with open(os.path.join(zd, ".zshrc"), "w") as f:
            f.write(
                "setopt no_beep\nPS1='%~ %# '\nHISTFILE=$ZDOTDIR/hist; HISTSIZE=100; SAVEHIST=100\n"
                "autoload -Uz compinit; compinit -u -d $ZDOTDIR/zcompdump\n"
                "zstyle ':completion:*' menu select\nsetopt menu_complete\n"
                + rc_extra + f"\nsource {SCRIPT}\n" + rc_post + "\n")
        e = {k: v for k, v in os.environ.items()
             if not k.startswith(("TILDY_", "ZSH", "ZLE_", "STARSHIP", "P9K", "POWERLEVEL"))}
        e.update(ZDOTDIR=zd, TERM="xterm-256color", HOME=tmp, TILDY_DEBUG_LOG=self.log,
                 TILDY_BACKEND=backend, TILDY_BACKEND_MODE=mode)
        e.update(env or {})
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.Stream(self.screen)
        self.history = []          # every distinct prompt-row text seen
        self.child = pexpect.spawn("zsh", ["-i"], env=e, cwd=os.path.join(tmp, "home"),
                                   dimensions=(rows, cols), encoding="utf-8", timeout=5)

    def pump(self, secs):
        end = time.time() + secs
        while time.time() < end:
            try:
                self.stream.feed(self.child.read_nonblocking(65536, timeout=0.02))
            except pexpect.TIMEOUT:
                pass
            except pexpect.EOF:
                return
            r = self.row()
            if not self.history or self.history[-1] != r:
                self.history.append(r)

    def send(self, s):
        self.child.send(s)

    def type_slow(self, s, delay=0.03):
        for ch in s:
            self.child.send(ch)
            self.pump(delay)

    def row(self):
        return self.screen.display[self.screen.cursor.y].rstrip()

    def text(self):
        return "\n".join(l.rstrip() for l in self.screen.display).rstrip()

    def events(self):
        out = []
        for line in open(self.log, errors="replace").read().splitlines():
            p = line.split("\t")
            if len(p) >= 2:
                out.append(p)
        return out

    def find(self, kind, gen=None, reason=None, after=0):
        for p in self.events()[after:]:
            if p[1] == kind and (gen is None or (len(p) > 2 and p[2] == str(gen))) \
                    and (reason is None or (len(p) > 3 and p[3] == reason)):
                return p
        return None

    def wait(self, cond, secs, what):
        end = time.time() + secs
        while time.time() < end:
            r = cond()
            if r:
                return r
            self.pump(0.03)
        raise Fail(f"timeout waiting for {what}")

    def wait_log(self, kind, gen=None, reason=None, secs=5, after=0):
        return self.wait(lambda: self.find(kind, gen, reason, after), secs, f"log {kind} gen={gen} reason={reason}")

    def wait_prompt_gen(self, secs=5):
        def ok():
            m = FAKE_RE.search(self.row())
            return m
        return self.wait(ok, secs, "fake prompt on cursor row")

    def cycle(self, cmd=""):
        """Run cmd (or just Enter), wait for the new cycle's send event; return gen."""
        n0 = len(self.events())
        self.send(cmd + "\r")
        g = self.wait_log("send", after=n0)[2]
        self.wait(lambda: BASE_RE.match(self.row()) or FAKE_RE.search(self.row()), 3, "new prompt")
        return g, n0

    def close(self):
        try:
            self.child.terminate(force=True)
        except Exception:
            pass

    def evidence(self):
        return "--- screen ---\n" + self.text() + "\n--- log ---\n" + "\n".join("\t".join(p) for p in self.events()[-25:])


def new_tmp():
    t = tempfile.mkdtemp(prefix="skst-", dir="/tmp")
    t = os.path.realpath(t)
    for d in ("home/dirA", "home/dirB"):
        os.makedirs(os.path.join(t, d))
    return t


SCEN = {}


def scenario(fn):
    SCEN[fn.__name__] = fn
    return fn


# (a) base prompt first, then the backend result
@scenario
def a_base_prompt_first(sh, notes):
    first_base = sh.wait(lambda: BASE_RE.match(sh.row()) and sh.row(), 10, "base prompt")
    if "[g" in sh.text():
        raise Fail("backend prompt visible before base prompt")
    t_base = time.time()
    sh.wait_prompt_gen(3)
    dt = time.time() - t_base
    send = sh.find("send", 1); acc = sh.find("accept", 1); recv = sh.find("recv", 1)
    if not (send and recv and acc):
        raise Fail("missing send/recv/accept for gen 1")
    notes.append(f"base prompt {first_base!r} seen, [g1 ...] {dt*1000:.0f} ms later; "
                 f"log send->accept {int(acc[0]) - int(send[0])} ms")


# (b) cd A (slow) then cd B (fast): B wins, A discarded
def _ab(sh, notes):
    sh.wait_prompt_gen(5)
    sh.pump(0.2)
    n0 = len(sh.events())
    sh.send("cd dirA\r")
    sh.send("cd ../dirB\r")
    t0 = time.time()
    sh.wait(lambda: re.search(r"\[g\d+ dirB\] %", sh.row()), 8, "dirB prompt")
    while time.time() - t0 < 3.0:
        sh.pump(0.1)
    evs = sh.events()[n0:]
    a = [p[2] for p in evs if p[1] == "send" and p[3].endswith("/dirA")]
    b = [p[2] for p in evs if p[1] == "send" and p[3].endswith("/dirB")]
    if not a or not b:
        raise Fail(f"send events missing: A={a} B={b}")
    for g in a:
        if any(p[1] in ("accept", "apply_deferred") and p[2] == g for p in evs):
            raise Fail(f"A gen {g} was accepted")
        if not any(p[1] == "discard" and p[2] == g and p[3] == "stale" for p in evs):
            raise Fail(f"A gen {g} has no 'discard stale'")
    if not any(p[1] == "accept" and p[2] in b for p in evs):
        raise Fail("B never accepted")
    if not re.search(r"\[g\d+ dirB\] %", sh.row()):
        raise Fail("final prompt is not dirB")
    if any(re.search(r"\[g\d+ dirA\] %", h) for h in sh.history):
        raise Fail("a dirA prompt was drawn at some point")
    notes.append("A gens " + ",".join(a) + " -> " + "; ".join(
        "\t".join(p[1:]) for p in evs if len(p) > 2 and p[2] in a and p[1] in ("discard", "recv", "backend_exit")))
    notes.append("B gens " + ",".join(b) + "; final row " + repr(sh.row()))


@scenario
def b_ab_out_of_order(sh, notes):
    _ab(sh, notes)


@scenario
def b_ab_out_of_order_persistent(sh, notes):
    _ab(sh, notes)


# (c) typing a long command while a 500 ms result lands
@scenario
def c_typing_during_refresh(sh, notes):
    sh.wait_log("accept", 1)
    gen, n0 = sh.cycle()
    text = "echo the_quick_brown_fox_jumps_over_the_lazy_dog_0123"
    t1 = time.time() * 1000
    sh.type_slow(text, 0.025)
    t2 = time.time() * 1000
    recv = sh.wait_log("recv", gen, after=n0)
    sh.wait_log("accept", gen, after=n0)
    sh.pump(0.2)
    if not (t1 <= int(recv[0]) <= t2):
        raise Fail(f"result did not land while typing (recv {int(recv[0]) - t1:.0f} ms after first key, window {t2 - t1:.0f} ms)")
    row = sh.row()
    m = FAKE_RE.search(row)
    if not m or m.group(1) != gen:
        raise Fail(f"prompt not updated to g{gen}: {row!r}")
    if not row.endswith(text):
        raise Fail(f"buffer damaged: {row!r}")
    if sh.screen.cursor.x != len(row):
        raise Fail(f"cursor {sh.screen.cursor.x} != end of line {len(row)}")
    sh.send("\r")
    want = text[len("echo "):]
    sh.wait(lambda: any(l.rstrip() == want for l in sh.screen.display), 3, "echo output")
    notes.append(f"recv landed {int(recv[0]) - t1:.0f} ms into a {t2 - t1:.0f} ms typing window; "
                 f"row after refresh {row!r}; echo output matched")


# (d) half frame / failure keep what is shown
def _keep_base(sh, notes, var, reasons):
    sh.wait(lambda: BASE_RE.match(sh.row()), 10, "base prompt")
    d = sh.wait_log("discard", 1)
    if d[3] not in reasons:
        raise Fail(f"discard reason {d[3]}, want {reasons}")
    sh.pump(0.5)
    if sh.find("accept"):
        raise Fail("bad result accepted")
    if not BASE_RE.match(sh.row()):
        raise Fail(f"prompt not kept: {sh.row()!r}")
    notes.append(f"{var}: log '{chr(9).join(d[1:])}', prompt kept {sh.row()!r}")


@scenario
def d_half_frame_keeps_base(sh, notes):
    _keep_base(sh, notes, "TILDY_FAKE_HALF_FRAME", {"half_frame"})


@scenario
def d_backend_fail_keeps_base(sh, notes):
    _keep_base(sh, notes, "TILDY_FAKE_FAIL", {"eof"})


@scenario
def d_bad_gen_keeps_base(sh, notes):
    _keep_base(sh, notes, "TILDY_FAKE_BAD_GEN", {"half_frame"})


def _keep_previous(sh, notes, kv, reason):
    """A valid [gN ...] is shown; a resize triggers a new request in the same
    cycle which fails; the [gN ...] prompt must stay."""
    m = sh.wait_prompt_gen(5)
    shown = m.group(0)
    with open(os.path.join(sh.tmp, "flag"), "w") as f:
        f.write(kv + "\n")
    n0 = len(sh.events())
    sh.child.setwinsize(30, 90)
    sh.screen.resize(30, 90)
    g = sh.wait_log("send", after=n0)[2]
    d = sh.wait_log("discard", g, after=n0)
    sh.pump(0.5)
    if d[3] != reason:
        raise Fail(f"reason {d[3]} != {reason}")
    if sh.find("accept", g, after=n0):
        raise Fail("failed result accepted")
    if shown not in sh.row():
        raise Fail(f"previous prompt {shown!r} not kept: {sh.row()!r}")
    notes.append(f"{kv}: g{g} discarded ({reason}); still showing {sh.row()!r}")


@scenario
def d_half_frame_keeps_previous(sh, notes):
    _keep_previous(sh, notes, "TILDY_FAKE_HALF_FRAME=1", "half_frame")


@scenario
def d_backend_fail_keeps_previous(sh, notes):
    _keep_previous(sh, notes, "TILDY_FAKE_FAIL=1", "eof")


# extra: completion menu must be deferred, menu kept, applied after
@scenario
def x_completion_menu_defer(sh, notes):
    menu_names = ('alpha', 'bravo', 'charlie')
    for name in menu_names:
        os.makedirs(os.path.join(sh.tmp, 'home', 'completion-menu', name))
    sh.wait_log("accept", 1)
    gen, n0 = sh.cycle()
    sh.send("ls ./completion-menu/")
    sh.pump(0.05)
    sh.send("\t")
    sh.wait(lambda: all(name in sh.text() for name in menu_names), 3, "menu")
    menu_before = sh.text()
    d = sh.wait_log("defer", gen, after=n0)
    sh.pump(0.3)
    if sh.text() != menu_before and not all(name in sh.text() for name in menu_names):
        raise Fail("menu damaged")
    if FAKE_RE.search(sh.row()):
        raise Fail("prompt redrawn while the menu was open")
    sh.send("\r")
    a = sh.wait_log("apply_deferred", gen, after=n0)
    sh.pump(0.2)
    m = FAKE_RE.search(sh.row())
    if not m or m.group(1) != gen:
        raise Fail(f"deferred prompt not shown: {sh.row()!r}")
    notes.append(f"defer reason '{d[3]}', menu intact={sh.text() != ''}, apply_deferred {int(a[0]) - int(d[0])} ms after defer; row {sh.row()!r}")


@scenario
def x_isearch_defer(sh, notes):
    sh.wait_log("accept", 1)
    sh.cycle("echo HISTMARK42")
    gen, n0 = sh.cycle()
    sh.send("\x12HISTM")
    sh.wait(lambda: "i-search: HISTM" in sh.text(), 3, "isearch UI")
    d = sh.wait_log("defer", gen, after=n0)
    sh.pump(0.3)
    if "i-search: HISTM" not in sh.text():
        raise Fail("isearch UI damaged")
    sh.send("\x1b[C")   # leave isearch keeping the match
    a = sh.wait_log("apply_deferred", gen, after=n0)
    sh.pump(0.2)
    if not sh.row().endswith("echo HISTMARK42") or not FAKE_RE.search(sh.row()):
        raise Fail(f"after isearch: {sh.row()!r}")
    notes.append(f"defer '{d[3]}', applied on exit; row {sh.row()!r}")


# extra: backend text cannot execute, with prompt_subst on
@scenario
def x_prompt_subst_injection(sh, notes):
    evil = os.path.join(sh.tmp, "home", "x$(touch PWNED)`touch PWNED2`${HOME}!y")
    os.makedirs(evil)
    sh.wait_prompt_gen(5)
    sh.send("cd 'x$(touch PWNED)`touch PWNED2`${HOME}!y'\r")
    sh.wait(lambda: "PWNED" in sh.row() and FAKE_RE.search(sh.row()), 5, "evil dir prompt")
    sh.send("ls\r")
    sh.pump(0.6)
    created = [f for f in ("PWNED", "PWNED2") if os.path.exists(os.path.join(evil, f)) or os.path.exists(os.path.join(sh.tmp, "home", f))]
    if created:
        raise Fail(f"command substitution executed: {created}")
    lit = "x$(touch PWNED)`touch PWNED2`${HOME}!y"
    rows = [l.rstrip() for l in sh.screen.display if l.startswith("[g") and ("[g" + "" in l) and (" " + lit + "] %") in l]
    if not rows:
        raise Fail("no prompt row shows the directory name literally: " + repr([l.rstrip() for l in sh.screen.display if "PWNED" in l]))
    row = rows[0]
    notes.append(f"prompt_subst+prompt_bang on; shown literally: {row!r}; no files created")


@scenario
def x_percent_and_control(sh, notes):
    sh.wait_prompt_gen(5)
    sh.pump(0.3)
    line = sh.screen.display[sh.screen.cursor.y]
    raw = open(sh.log).read()
    if "pct=100% bel=^G" not in line or "\x1b" in line or "[31mraw" not in line:
        raise Fail(f"right prompt not sanitized as expected: {line.rstrip()!r}")
    notes.append(f"row {line.rstrip()!r} (raw ESC/BEL stripped, %% shown as %)")


@scenario
def x_timeout(sh, notes):
    sh.wait(lambda: BASE_RE.match(sh.row()), 10, "base prompt")
    t0 = time.time()
    d = sh.wait_log("discard", 1, secs=6)
    if d[3] != "timeout":
        raise Fail(f"reason {d[3]}")
    ex = sh.wait_log("backend_exit", 1, secs=3)
    if not BASE_RE.match(sh.row()):
        raise Fail("prompt not kept")
    notes.append(f"discard timeout after {time.time() - t0:.1f}s (timeout 2000 ms, 1 s sched granularity); backend_exit code {ex[4]}")


@scenario
def x_persistent_crash(sh, notes):
    sh.wait_prompt_gen(5)
    st = sh.find("backend_start")
    if not st or st[4] != "persistent":
        raise Fail(f"no persistent backend_start: {st}")
    for _ in range(4):
        n0 = len(sh.events())
        sh.send("\r")
        sh.wait_log("accept", after=n0, secs=5)
        sh.pump(0.2)
    starts = [p for p in sh.events() if p[1] == "backend_start"]
    exits = [p for p in sh.events() if p[1] == "backend_exit"]
    if not exits:
        raise Fail("no backend_exit")
    notes.append(f"{len(starts)} backend_start, {len(exits)} backend_exit (codes {[p[4] for p in exits]}); every prompt accepted")


@scenario
def x_source_twice(sh, notes):
    sh.wait_prompt_gen(5)
    for _ in range(2):
        n0 = len(sh.events())
        sh.send('source "$TILDY_SCRIPT_PATH"\r')
        sh.wait_log("accept", after=n0)
    sh.send('print -r -- "P=${(j:,:)precmd_functions} C=${(j:,:)chpwd_functions}"\r')
    sh.pump(0.5)
    hooks = [p[2] for p in sh.events() if p[1] == "hook_installed"]
    line = [l for l in sh.screen.display if l.startswith("P=")]
    if sorted(hooks) != ["chpwd", "int", "precmd", "winch"]:
        raise Fail(f"hook_installed: {hooks}")
    if not line or line[0].count("_tildy_precmd") != 1 or line[0].count("_tildy_chpwd") != 1:
        raise Fail(f"hook arrays: {line}")
    notes.append(f"hook_installed={hooks}; {line[0].strip()}")


@scenario
def x_source_newer_version(sh, notes):
    # `source ~/.zshrc` after a rebuild: `tildy init zsh` sets TILDY_VERSION
    # to the new binary before the (guarded, already loaded) adapter text.
    # The loaded adapter must keep its own version and binary, say so once,
    # and keep rendering; it must not restart the worker as the new binary.
    sh.wait_prompt_gen(5)
    n0 = len(sh.events())
    sh.send('TILDY_VERSION=9.9.9-test TILDY_BIN=/nonexistent/tildy; source "$TILDY_SCRIPT_PATH"\r')
    sh.wait_log("accept", after=n0)
    sh.send('print -r -- "V=[$TILDY_VERSION] B=[$TILDY_BIN]"\r')
    sh.wait_log("accept", after=len(sh.events()))
    sh.pump(0.3)
    disp = sh.screen.display
    notice = [l for l in disp if "9.9.9-test" in l and "tildy:" in l]
    line = [l.strip() for l in disp if l.startswith("V=[")]
    if not notice:
        raise Fail(f"no notice: {[l for l in disp if l.strip()][-6:]}")
    if line != ["V=[] B=[tildy]"]:
        raise Fail(f"vars not restored: {line}")
    bad = [p for p in sh.events()[n0:] if p[1] in ("backend_degraded", "bye", "hook_installed")]
    if bad:
        raise Fail(f"unexpected events: {bad}")
    notes.append(f"notice={notice[0].strip()[:60]}; {line[0]}")


# measurement: synchronous cost of precmd (what delays the prompt) and of the
# zle -F handler (what can delay a keystroke), wrapped after tildy.zsh loads.
# The debug log is off (as for a user); the wrappers write their own samples:
# "precmd <us> <1 if the request went to the worker>" and "fd <us>".
COST_RC = r"""
functions -c _tildy_precmd _tildy_t_precmd
_tildy_precmd() {
  local -i r=$?
  local -F t0=$EPOCHREALTIME t1
  () { return $1 } $r
  _tildy_t_precmd
  t1=$EPOCHREALTIME
  print -r -- "precmd $(( (t1 - t0) * 1e6 )) $(( _tildy_w_req_gen == _tildy_gen ))" >> $TILDY_SELFTEST_COST
}
functions -c _tildy_on_fd _tildy_t_on_fd
_tildy_on_fd() {
  local -F t0=$EPOCHREALTIME t1
  _tildy_t_on_fd "$@"
  t1=$EPOCHREALTIME
  print -r -- "fd $(( (t1 - t0) * 1e6 ))" >> $TILDY_SELFTEST_COST
}
"""
COST_N = int(os.environ.get("TILDY_SELFTEST_COST_N", "200"))
REAL_BIN = os.environ.get("TILDY_SELFTEST_BIN") or os.path.join(ROOT, "target", "release", "tildy")


def _pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(q * (len(xs) - 1))))]


def _samples(path):
    pre, fd = [], []
    try:
        text = open(path).read()
    except OSError:
        text = ""
    for line in text.split("\n"):
        f = line.split(" ")
        if f[0] == "precmd" and len(f) == 3:
            pre.append((float(f[1]) / 1000.0, f[2] == "1"))
        elif f[0] == "fd" and len(f) == 2:
            fd.append(float(f[1]) / 1000.0)
    return pre, fd


def _cost(sh, notes, want_worker):
    cost = os.path.join(sh.tmp, "cost")
    sh.wait(lambda: len(_samples(cost)[1]) >= 1, 10, "first result")
    sh.pump(0.3)
    base_p, base_f = (len(x) for x in _samples(cost))   # skip the cold first prompt
    for i in range(COST_N):
        n_fd = len(_samples(cost)[1])
        sh.send("\r")
        sh.wait(lambda: len(_samples(cost)[0]) >= base_p + i + 1, 10, f"precmd {i}")
        sh.wait(lambda: len(_samples(cost)[1]) > n_fd, 10, f"result {i}")
        sh.pump(0.02)
    sh.pump(0.3)
    pre, fd = _samples(cost)
    pre, fd = pre[base_p:], fd[base_f:]
    worker = sum(1 for _, w in pre if w)
    if want_worker and worker < len(pre) * 0.95 or not want_worker and worker:
        raise Fail(f"{worker}/{len(pre)} requests went to the worker")
    xs = [t for t, _ in pre]
    f = lambda v: f"p50 {_pct(v, .5):.2f} ms, p95 {_pct(v, .95):.2f} ms, max {max(v):.2f} ms (n={len(v)})"
    notes.append(f"precmd: {f(xs)}")
    notes.append(f"zle -F handler per call: {f(fd)}")
    if _pct(xs, .95) > 20:
        raise Fail(f"precmd p95 {_pct(xs, .95):.2f} ms > 20 ms")


@scenario
def m_precmd_cost_oneshot(sh, notes):
    _cost(sh, notes, False)


@scenario
def m_precmd_cost_worker(sh, notes):
    _cost(sh, notes, True)


@scenario
def m_precmd_cost_real_oneshot(sh, notes):
    if not os.access(REAL_BIN, os.X_OK):
        raise Skip(f"{REAL_BIN} not found (cargo build --release or set TILDY_SELFTEST_BIN)")
    _cost(sh, notes, False)


@scenario
def m_precmd_cost_real_worker(sh, notes):
    if not os.access(REAL_BIN, os.X_OK):
        raise Skip(f"{REAL_BIN} not found (cargo build --release or set TILDY_SELFTEST_BIN)")
    _cost(sh, notes, True)


CONFIG = {
    "a_base_prompt_first": dict(env={"TILDY_FAKE_DELAY_MS": "400"}),
    "b_ab_out_of_order": dict(env={"TILDY_FAKE_DELAY_RULES": "dirA=1500,dirB=10"}),
    "b_ab_out_of_order_persistent": dict(env={"TILDY_FAKE_DELAY_RULES": "dirA=1500,dirB=10"}, mode="persistent"),
    "c_typing_during_refresh": dict(env={"TILDY_FAKE_DELAY_MS": "500"}),
    "d_half_frame_keeps_base": dict(env={"TILDY_FAKE_HALF_FRAME": "1", "TILDY_FAKE_DELAY_MS": "100"}),
    "d_backend_fail_keeps_base": dict(env={"TILDY_FAKE_FAIL": "1", "TILDY_FAKE_DELAY_MS": "100"}),
    "d_bad_gen_keeps_base": dict(env={"TILDY_FAKE_BAD_GEN": "1", "TILDY_FAKE_DELAY_MS": "100"}),
    "d_half_frame_keeps_previous": dict(backend=FLAKY, env={"TILDY_FAKE_DELAY_MS": "200"}),
    "d_backend_fail_keeps_previous": dict(backend=FLAKY, env={"TILDY_FAKE_DELAY_MS": "200"}),
    "x_completion_menu_defer": dict(env={"TILDY_FAKE_DELAY_MS": "600"}),
    "x_isearch_defer": dict(env={"TILDY_FAKE_DELAY_MS": "600"}),
    "x_prompt_subst_injection": dict(rc_extra="setopt prompt_subst prompt_bang"),
    "x_percent_and_control": dict(env={"TILDY_FAKE_PERCENT": "1", "TILDY_FAKE_RAW_CTRL": "1"}),
    "x_timeout": dict(env={"TILDY_FAKE_DELAY_MS": "8000", "TILDY_BACKEND_TIMEOUT_MS": "2000"}),
    "x_persistent_crash": dict(env={"TILDY_FAKE_CRASH_AFTER": "2", "TILDY_FAKE_DELAY_MS": "50"}, mode="persistent"),
    "x_source_twice": dict(env={"TILDY_SCRIPT_PATH": SCRIPT}),
    "x_source_newer_version": dict(env={"TILDY_SCRIPT_PATH": SCRIPT}),
    "m_precmd_cost_oneshot": dict(rc_post=COST_RC, mode="oneshot", env={"TILDY_DEBUG_LOG": ""}),
    "m_precmd_cost_worker": dict(rc_post=COST_RC, mode="persistent", env={"TILDY_DEBUG_LOG": ""}),
    "m_precmd_cost_real_oneshot": dict(rc_post=COST_RC, mode="oneshot", backend=REAL_BIN, env={"TILDY_DEBUG_LOG": ""}),
    "m_precmd_cost_real_worker": dict(rc_post=COST_RC, mode="persistent", backend=REAL_BIN, env={"TILDY_DEBUG_LOG": ""}),
}


def main():
    names = [a for a in sys.argv[1:] if not a.startswith("-")] or list(SCEN)
    results = []
    for name in names:
        tmp = new_tmp()
        cfg = dict(CONFIG.get(name, {}))
        env = dict(cfg.pop("env", {}))
        env["TILDY_SELFTEST_FLAG"] = os.path.join(tmp, "flag")
        env["TILDY_SELFTEST_COST"] = os.path.join(tmp, "cost")
        env.setdefault("XDG_CONFIG_HOME", os.path.join(tmp, "xdg", "config"))
        env.setdefault("XDG_STATE_HOME", os.path.join(tmp, "xdg", "state"))
        sh = Shell(tmp, env=env, **cfg)
        notes = []
        t0 = time.time()
        try:
            SCEN[name](sh, notes)
            status, why = "pass", ""
        except Fail as e:
            status, why = "FAIL", str(e)
        except Skip as e:
            status, why = "skip", str(e)
        except Exception as e:  # noqa
            status, why = "ERROR", repr(e)
        dt = time.time() - t0
        print(f"{status:5} {name} ({dt:.1f}s) {why}")
        for n in notes:
            print(f"      {n}")
        if status in ("FAIL", "ERROR") or VERBOSE:
            print("      " + sh.evidence().replace("\n", "\n      "))
        sh.close()
        subprocess.run(["pkill", "-f", tmp], capture_output=True)
        shutil.rmtree(tmp, ignore_errors=True)
        results.append((name, status))
    bad = [n for n, s in results if s in ("FAIL", "ERROR")]
    skipped = [n for n, s in results if s == "skip"]
    passed = sum(s == "pass" for _, s in results)
    print(f"\n{passed}/{len(results)} passed" + (f"; failed: {', '.join(bad)}" if bad else "")
          + (f"; skipped: {', '.join(skipped)}" if skipped else ""))
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
