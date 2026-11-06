#!/usr/bin/env python3
"""Tildy PTY harness: M0 scenarios (task B) plus the M1 protocol-v1 scenarios
(task A). See tests/pty/README.md, docs/architecture.md and docs/protocol.md.

Default: shell/zsh/tildy.zsh with bench/fake-backend.sh. Scenarios that need
the real binary (target/debug/tildy, loaded with `eval "$(tildy init zsh)"`)
run when it is usable and skip otherwise; `--real` runs every scenario that
supports the real binary in real-binary mode and skips the fake-only ones.
"""
from __future__ import annotations

import argparse
import fnmatch
import json
import shlex
import os
import random
import re
import shutil
import signal
import string
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

import pexpect
import pyte

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
PLUGINS = ("autosuggestions", "syntax-highlighting", "fzf")
PLUGIN_FILES = {
    "autosuggestions": "zsh-autosuggestions/zsh-autosuggestions.zsh",
    "syntax-highlighting": "zsh-syntax-highlighting/zsh-syntax-highlighting.zsh",
}
STRIP_PREFIXES = ("TILDY_", "ZLE_", "ZSH", "STARSHIP", "POWERLEVEL", "P9K", "GHOSTTY",
                  "TERM_PROGRAM", "ITERM", "KITTY", "WEZTERM", "VSCODE", "TMUX", "ZDOTDIR",
                  "COLUMNS", "LINES", "PROMPT", "PS1", "RPROMPT", "RPS1", "XDG_")
SEED_CMD = "echo seedcommand_for_suggest"
TIMEOUT_SCALE = 1.0
# Contract (docs/protocol.md): the only discard reasons.
DISCARD_REASONS = {"stale", "half_frame", "eof", "timeout", "bad_frame", "lines_mismatch"}
FAKE_VERSION = "0.1.0-m1"


class Fail(Exception):
    pass


class Skip(Exception):
    pass


def now_ms() -> float:
    return time.time() * 1000


def T(x: float) -> float:
    return x * TIMEOUT_SCALE


class Ev:
    __slots__ = ("idx", "ts", "kind", "f", "raw")

    def __init__(self, idx, ts, kind, f, raw):
        self.idx, self.ts, self.kind, self.f, self.raw = idx, ts, kind, f, raw

    @property
    def gen(self):
        if self.kind in ("hook_installed", "hook_skipped", "backend_degraded", "hello", "bye") or not self.f:
            return None
        try:
            return int(self.f[0])
        except ValueError:
            return None

    def __repr__(self):
        return self.raw


BASE_RE = re.compile(r"^(?!\[g)/\S* % *$")
FAKE_RE = re.compile(r"\[g(\d+) ([^\]]+)\] %")
# Real binary default config: the prompt character is the last thing before
# the cursor ("❯ ", or "> " with ascii symbols).
REAL_RE = re.compile(r"(❯|>) ?$")


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    out = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return bool(out) and not out.startswith("Z")


class ReflowMixin:
    """pyte screen that reflows like Ghostty/iTerm2 when its width changes.

    pyte itself clips rows on a narrower resize. This mixin remembers which
    rows were soft-wrapped (the cursor ran past the last column), and on a
    width change joins them into logical lines, drops their trailing blank
    cells, re-wraps them at the new width, keeps the cursor on the same cell
    of its logical line, and scrolls rows off the top when the cursor would
    leave the screen. Erasing to the end of a row (EL 0/2, ED 0/2) clears its
    wrap flag, scrolling moves the flags with the rows. It is a model, not a
    copy of any terminal; it lets the harness check the cursor arithmetic of
    the resize redraw (docs/decisions/0002-safe-redraw.md).
    """

    def _wrap_init(self):
        if not hasattr(self, "wrapped"):
            self.wrapped = set()

    def draw(self, data):
        self._wrap_init()
        for ch in data:
            if self.cursor.x == self.columns and pyte.modes.DECAWM in self.mode:
                self.wrapped.add(self.cursor.y)
            super().draw(ch)

    def index(self):
        self._wrap_init()
        top, bottom = self.margins or (0, self.lines - 1)
        if self.cursor.y == bottom:
            self.wrapped = {r - 1 for r in self.wrapped if top < r <= bottom} | \
                           {r for r in self.wrapped if r < top or r > bottom}
        super().index()

    def reverse_index(self):
        self._wrap_init()
        top, bottom = self.margins or (0, self.lines - 1)
        if self.cursor.y == top:
            self.wrapped = {r + 1 for r in self.wrapped if top <= r < bottom} | \
                           {r for r in self.wrapped if r < top or r > bottom}
        super().reverse_index()

    def erase_in_display(self, how=0, *args, **kwargs):
        self._wrap_init()
        y = self.cursor.y
        if how == 0:
            self.wrapped = {r for r in self.wrapped if r < y}
        elif how == 1:
            self.wrapped = {r for r in self.wrapped if r >= y}
        else:
            self.wrapped = set()
        super().erase_in_display(how, *args, **kwargs)

    def erase_in_line(self, how=0, private=False):
        self._wrap_init()
        if how in (0, 2):
            self.wrapped.discard(self.cursor.y)
        super().erase_in_line(how, private)

    def insert_lines(self, count=None):
        self._wrap_init()
        self.wrapped = {r for r in self.wrapped if r < self.cursor.y}
        super().insert_lines(count)

    def delete_lines(self, count=None):
        self._wrap_init()
        self.wrapped = {r for r in self.wrapped if r < self.cursor.y}
        super().delete_lines(count)

    def resize(self, lines=None, columns=None):
        self._wrap_init()
        lines, columns = lines or self.lines, columns or self.columns
        if columns == self.columns:
            return super().resize(lines, columns)
        old = self.columns
        cy, cx = self.cursor.y, min(self.cursor.x, old)
        logical, acc, cur = [], [], None
        for y in range(self.lines):
            if y == cy:
                cur = (len(logical), len(acc) + cx)
            acc.extend(self.buffer[y][x] for x in range(old))
            if y in self.wrapped and y < self.lines - 1:
                continue
            logical.append(acc)
            acc = []
        blank = self.default_char
        rows, wrapped, pos = [], set(), (0, 0)
        for i, cells in enumerate(logical):
            n = len(cells)
            while n and cells[n - 1] == blank:
                n -= 1
            if cur and cur[0] == i:
                n = max(n, cur[1])
            cells = cells[:n]
            chunks = [cells[k:k + columns] for k in range(0, len(cells), columns)] or [[]]
            if cur and cur[0] == i:
                r, c = divmod(cur[1], columns)
                while r >= len(chunks):
                    chunks.append([])
                pos = (len(rows) + r, c)
            for j, chunk in enumerate(chunks):
                if j < len(chunks) - 1:
                    wrapped.add(len(rows))
                rows.append(chunk)
        drop = max(0, pos[0] - (lines - 1))
        rows = rows[drop:drop + lines]
        super().resize(lines, columns)
        self.buffer.clear()
        for y, chunk in enumerate(rows):
            line = self.buffer[y]
            for x, ch in enumerate(chunk):
                line[x] = ch
        self.wrapped = {r - drop for r in wrapped if drop <= r < drop + lines}
        self.cursor.y, self.cursor.x = pos[0] - drop, pos[1]
        self.dirty.update(range(lines))


class Session:
    def __init__(self, ctx, name, env_extra, plugins, mode, rows=30, cols=100, backend=None, cwd=None, reflow=False):
        self.ctx, self.name, self.plugins, self.mode = ctx, name, list(plugins), mode
        self.reflow = reflow
        self.cwd = str(cwd or ctx.home0)
        self.real = ctx.real
        self.rows, self.cols = rows, cols
        self.backend = backend or ctx.backend
        self.logfile = ctx.tmp / f"{name}.debug.log"
        self.logfile.write_text("")
        self.histfile = ctx.tmp / f"{name}.history"
        self.histfile.write_text(SEED_CMD + "\n")
        self.tracefile = ctx.tmp / f"{name}.trace"
        self.env_extra = env_extra
        self.child = None
        self.eof = False
        self.pids: set[int] = set()
        self.screen = None
        self.start()

    # ---- lifecycle -------------------------------------------------
    def build_env(self):
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(STRIP_PREFIXES)}
        cwd = self.cwd
        xdg = self.ctx.tmp / "xdg"
        for d in ("config", "state", "cache"):
            (xdg / d).mkdir(parents=True, exist_ok=True)
        env.update(
            TERM="xterm-256color", LANG="en_US.UTF-8", LC_ALL="en_US.UTF-8",
            SHELL_SESSIONS_DISABLE="1", PWD=cwd,
            ZDOTDIR=str(HERE / "zdotdir"),
            XDG_CONFIG_HOME=str(xdg / "config"), XDG_STATE_HOME=str(xdg / "state"),
            XDG_CACHE_HOME=str(xdg / "cache"),
            TILDY_DEBUG_LOG=str(self.logfile),
            TILDY_SCRIPT=str(self.ctx.script),
            TILDY_TEST_PLUGINS=",".join(self.plugins),
            TILDY_TEST_PLUGIN_DIR=str(self.ctx.plugin_dir),
            TILDY_TEST_HISTFILE=str(self.histfile),
            TILDY_TEST_TMP=str(self.ctx.tmp),
        )
        if self.real:
            # Exactly the user setup: eval "$(tildy init zsh)", default backend.
            env["TILDY_TEST_INIT_BIN"] = str(self.ctx.tildy_bin)
        else:
            env.update(TILDY_BACKEND=str(self.backend), TILDY_BACKEND_MODE=self.mode,
                       TILDY_FAKE_TRACE=str(self.tracefile))
        env.update(self.env_extra)
        if self.real:
            for k in [k for k in env if k.startswith("TILDY_FAKE_")]:
                del env[k]
        return env

    @property
    def source_cmd(self):
        if self.real:
            return 'eval "$("$TILDY_TEST_INIT_BIN" init zsh)"'
        return 'source "$TILDY_SCRIPT"'

    def start(self):
        outer = self

        bases = (ReflowMixin, pyte.Screen) if self.reflow else (pyte.Screen,)

        class Scr(*bases):
            def write_process_input(self, data):
                try:
                    outer.child.send(data.encode())
                except Exception:
                    pass

        self.screen = Scr(self.cols, self.rows)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = bytearray()   # everything zsh wrote (escape sequences pyte drops, e.g. OSC 133)
        self.child = pexpect.spawn("zsh", ["-i"], env=self.build_env(), cwd=self.cwd,
                                   dimensions=(self.rows, self.cols), encoding=None, timeout=5)
        self.child.delaybeforesend = None

    def descendants(self, root=None):
        out = subprocess.run(["ps", "-axo", "pid=,ppid="], capture_output=True, text=True).stdout
        kids: dict[int, list[int]] = {}
        for line in out.splitlines():
            try:
                p, pp = map(int, line.split())
            except ValueError:
                continue
            kids.setdefault(pp, []).append(p)
        res, stack = [], [root or self.child.pid]
        while stack:
            for c in kids.get(stack.pop(), []):
                res.append(c)
                stack.append(c)
        return res

    def close(self):
        if self.child is None:
            return
        pid = self.child.pid
        try:
            desc = self.descendants()
        except Exception:
            desc = []
        # Workers the shell told us about (they may have been reparented).
        desc += [p for p in self.worker_pids() if pid_alive(p)]
        pgids = set()
        for p in desc + [pid]:
            try:
                pgids.add(os.getpgid(p))
            except OSError:
                pass
        for pg in pgids:
            if pg > 1:
                try:
                    os.killpg(pg, signal.SIGKILL)
                except OSError:
                    pass
        for p in desc + [pid]:
            try:
                os.kill(p, signal.SIGKILL)
            except OSError:
                pass
        try:
            self.child.close(force=True)
        except Exception:
            pass
        self.child = None

    # ---- io --------------------------------------------------------
    def pump(self, dur=0.05):
        end = time.time() + dur
        while True:
            rem = end - time.time()
            if rem <= 0 or self.eof:
                break
            try:
                data = self.child.read_nonblocking(65536, timeout=min(0.05, rem))
            except pexpect.TIMEOUT:
                continue
            except pexpect.EOF:
                self.eof = True
                break
            self.raw += data
            self.stream.feed(data)

    def send(self, s):
        self.child.send(s)

    def send_slow(self, s, per_char=0.03):
        for ch in s:
            self.send(ch)
            self.pump(per_char)

    def resize(self, rows, cols):
        self.child.setwinsize(rows, cols)
        self.screen.resize(rows, cols)
        self.rows, self.cols = rows, cols

    # ---- screen ----------------------------------------------------
    def lines(self):
        return [l.rstrip() for l in self.screen.display]

    def screen_text(self):
        return "\n".join(self.lines()).rstrip()

    def cursor(self):
        return self.screen.cursor.x, self.screen.cursor.y

    def row_text(self, y=None):
        y = self.screen.cursor.y if y is None else y
        return self.screen.display[y]

    def at_prompt(self):
        x, y = self.cursor()
        if self.real:
            before = self.row_text(y)[:x]
            return bool(re.search(r"% ?$", before) or REAL_RE.search(before))
        return bool(re.search(r"% ?$", self.row_text(y)[:x]))

    def cell_fg(self, x, y):
        return self.screen.buffer[y][x].fg

    def wait_screen(self, pattern, timeout, what=None):
        rx = re.compile(pattern, re.M)
        end = time.time() + T(timeout)
        while True:
            m = rx.search("\n".join(self.lines()))
            if m:
                return m
            if time.time() > end:
                raise Fail(f"timeout waiting for screen {what or pattern!r}")
            if self.eof:
                raise Fail(f"zsh exited while waiting for screen {what or pattern!r}")
            self.pump(0.03)

    wait_for_screen_regex = wait_screen

    def wait_line_equals(self, text, timeout, count=1):
        end = time.time() + T(timeout)
        while True:
            n = sum(1 for l in self.lines() if l == text)
            if n >= count:
                return n
            if time.time() > end:
                raise Fail(f"timeout waiting for output line {text!r} (x{count}), saw {n}")
            self.pump(0.03)

    def wait_base_prompt(self, timeout=15):
        end = time.time() + T(timeout)
        while not (self.at_prompt() and BASE_RE.match(self.row_text())):
            if time.time() > end:
                raise Fail("timeout waiting for base prompt '%~ %# '")
            self.pump(0.03)

    # Backend-agnostic check that a backend result (not the base prompt) is
    # shown on the cursor row; with gen, also that it is that gen's result.
    def backend_prompt_ok(self, gen=None):
        if not self.real:
            return fake_prompt_ok(self, gen)
        x, y = self.cursor()
        row = self.row_text(y)
        if BASE_RE.match(row) or not REAL_RE.search(row[:x].rstrip() + " "):
            return False
        if gen is None:
            return True
        last = self.last_applied()
        return last is not None and last.gen == gen

    def wait_backend_prompt(self, gen=None, timeout=6, what="backend prompt"):
        end = time.time() + T(timeout)
        while not self.backend_prompt_ok(gen):
            if time.time() > end:
                raise Fail(f"timeout waiting for {what} (gen={gen}) on the cursor row: {self.row_text().rstrip()!r}")
            if self.eof:
                raise Fail(f"zsh exited while waiting for {what}")
            self.pump(0.03)

    # ---- debug log -------------------------------------------------
    def log_events(self, path=None):
        evs = []
        try:
            raw = Path(path or self.logfile).read_text(errors="replace").splitlines()
        except OSError:
            raw = []
        for i, line in enumerate(raw):
            p = line.split("\t")
            if len(p) < 2:
                continue
            try:
                ts = int(float(p[0]))
            except ValueError:
                continue
            evs.append(Ev(i, ts, p[1], p[2:], line))
        return evs

    def log_count(self):
        return len(self.log_events())

    def find_log(self, kinds, gen=None, after=0, where=None):
        if isinstance(kinds, str):
            kinds = (kinds,)
        for e in self.log_events()[after:]:
            if e.kind in kinds and (gen is None or e.gen == gen) and (where is None or where(e)):
                return e
        return None

    def find_all(self, kinds, gen=None, after=0, where=None):
        if isinstance(kinds, str):
            kinds = (kinds,)
        return [e for e in self.log_events()[after:]
                if e.kind in kinds and (gen is None or e.gen == gen) and (where is None or where(e))]

    def wait_log(self, kinds, gen=None, timeout=5, after=0, where=None):
        end = time.time() + T(timeout)
        while True:
            e = self.find_log(kinds, gen, after, where)
            if e:
                return e
            if time.time() > end:
                raise Fail(f"timeout waiting for log event {kinds} gen={gen}")
            if self.eof:
                raise Fail(f"zsh exited while waiting for log event {kinds}")
            self.pump(0.03)

    wait_for_log_event = wait_log

    def wait_new_prompt(self, after, timeout=15):
        ev = self.wait_log("send", after=after, timeout=timeout)
        end = time.time() + T(5)
        while not self.at_prompt():
            if time.time() > end:
                raise Fail("prompt not visible after send event")
            self.pump(0.03)
        return ev

    def last_applied(self):
        evs = self.find_all(("accept", "apply_deferred"))
        return evs[-1] if evs else None

    def worker_pids(self):
        out = []
        for e in self.log_events():
            if e.kind == "backend_start" and e.f[2:3] == ["persistent"]:
                try:
                    out.append(int(e.f[1]))
                except (ValueError, IndexError):
                    pass
        return out

    def wait_worker_ready(self, after=0, timeout=8):
        """Wait for a HELLO the shell accepted; return its pid."""
        e = self.wait_log("hello", after=after, timeout=timeout)
        return int(e.f[3])

    def cycle(self, cmd="", timeout=10):
        """Run cmd (or just Enter); return the new prompt's send event."""
        after = self.log_count()
        self.send(cmd + "\r")
        return self.wait_new_prompt(after, timeout=timeout)

    def worker_cycle(self, cmd="", tries=4):
        """Enter until a prompt's request goes to the worker; return send event."""
        for _ in range(tries):
            ev = self.cycle(cmd)
            if ev.f[2:3] == ["worker"]:
                return ev
            self.pump(0.3)
        raise Fail("no request was routed to the worker")

    def evidence(self, tail=80):
        try:
            log = self.logfile.read_text(errors="replace").splitlines()[-tail:]
        except OSError:
            log = []
        return {"screen": self.screen_text(), "cursor": list(self.cursor()), "log": log}


class Ctx:
    def __init__(self, cfg, real=False):
        self.cfg = cfg
        self.real = real
        self.script, self.backend, self.plugin_dir = cfg.script, cfg.backend, cfg.plugin_dir
        self.tildy_bin = cfg.tildy_bin
        self.tmp = Path(os.path.realpath(tempfile.mkdtemp(prefix="sk-", dir="/tmp")))
        self.home0 = self.tmp / "home0"
        (self.home0 / "dirA").mkdir(parents=True)
        (self.home0 / "dirB").mkdir()
        self.sessions: list[Session] = []
        self.notes: list[str] = []

    def session(self, name="s", env=None, plugins=(), mode=None, backend=None, rows=30, cols=100, cwd=None,
                reflow=False):
        s = Session(self, name, env or {}, plugins, mode or self.cfg.fake_mode, backend=backend,
                    rows=rows, cols=cols, cwd=cwd, reflow=reflow)
        self.sessions.append(s)
        return s

    def note(self, msg):
        self.notes.append(msg)

    def cleanup(self):
        for s in self.sessions:
            s.close()
        if not self.cfg.keep:
            shutil.rmtree(self.tmp, ignore_errors=True)


# ====================================================================
# scenarios
# ====================================================================
SCENARIOS: list[dict] = []


def scenario(name, desc, plugin_ok=False, backends=("fake",)):
    """backends: "fake" (bench/fake-backend.sh) and/or "real" (target/debug/tildy)."""
    def deco(fn):
        SCENARIOS.append({"name": name, "desc": desc, "fn": fn, "plugins": (), "backends": backends})
        if plugin_ok:
            for p in PLUGINS:
                SCENARIOS.append({"name": f"plugins_{p}_{name}", "desc": f"{desc} [plugin: {p}]",
                                  "fn": fn, "plugins": (p,), "plugin_variant": p, "backends": backends})
        return fn
    return deco


def fake_prompt_ok(s, gen=None):
    m = FAKE_RE.search(s.row_text())
    if not m:
        return False
    return gen is None or int(m.group(1)) == gen


def uniq_text(prefix="echo "):
    u = "".join(random.choices(string.ascii_lowercase, k=6))
    return f"{prefix}MARK{u}_0123456789_abcdefghijklmnopqrstuvwxyz"


def completion_command(ctx):
    # A controlled ambiguous listing, independent of /Volumes or distro roots.
    directory = ctx.tmp / "completion-menu"
    for name in ("alpha", "bravo", "charlie"):
        (directory / name).mkdir(parents=True)
    return "ls " + shlex.quote(str(directory)) + "/"


# --------------------------------------------------------------------
# M0 scenarios (task B), unchanged except source_twice's source command
# --------------------------------------------------------------------
@scenario("baseline_prompt", "base prompt appears first, then [g1 ...] from the backend", plugin_ok=True)
def sc_baseline(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_DELAY_MS": "400"}, plugins=plugins)
    s.wait_base_prompt()
    if "[g1" in s.screen_text():
        raise Fail("[g1 ...] appeared before the base prompt could be observed")
    s.wait_log("send", gen=1, timeout=5)
    s.wait_screen(r"\[g1 home0\] %", 6, "[g1 home0] %")
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=3)
    if s.row_text().strip().startswith("/"):
        raise Fail("base prompt still shown after accept")


@scenario("slow_backend_not_blocking", "2s backend delay must not block typing/executing")
def sc_slow(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_DELAY_MS": "2000"})
    s.wait_base_prompt()
    s.pump(0.1)
    t0 = time.time()
    s.send("echo hi\r")
    s.wait_line_equals("hi", 4, 1)
    dt = time.time() - t0
    if dt >= 2.0:
        raise Fail(f"command took {dt:.2f}s (>= 2s): input blocked by backend")
    if s.find_log("accept", gen=1):
        ctx.note("accept for gen1 already logged before command output (unexpected but not fatal)")
    ctx.note(f"echo hi completed in {dt:.2f}s")


@scenario("accepted_frame_is_visible", "accept is emitted after the async frame reaches the terminal",
          backends=("fake", "real"))
def sc_accepted_visible(ctx, plugins):
    # Hold the callback immediately after accept. This checks the screen before
    # channel cleanup or another turn of ZLE's input loop can repaint it.
    gate = ctx.tmp / "accept-gate"
    os.mkfifo(gate)
    gate_fd = os.open(gate, os.O_RDWR | os.O_NONBLOCK)
    post = r'''
      sysopen -r -o cloexec -u _test_gate_fd "$TILDY_TEST_TMP/accept-gate"
      functions -c _tildy_log _test_original_log
      _tildy_log() {
        _test_original_log "$@"
        if [[ $1 == accept && -n $_test_gate_fd ]]; then
          local byte
          sysread -i $_test_gate_fd -s 1 byte
          exec {_test_gate_fd}<&-
          _test_gate_fd=
        fi
      }
      false
    '''
    try:
        s = ctx.session(env=instant_test_env(ctx, {"TILDY_TEST_POST_INIT": post}))
        s.wait_log("accept", gen=1, timeout=6)
        s.wait_backend_prompt(gen=1, timeout=1)
        if ctx.real and "✗ 1" not in s.row_text():
            raise Fail("accepted frame is missing its right prompt")
        ctx.note("left/right prompt visible while the accept callback is still held")
    finally:
        os.write(gate_fd, b".")
        os.close(gate_fd)


@scenario("ab_out_of_order", "cd A (slow) then immediately cd B (fast); A must never win")
def sc_ab(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_DELAY_RULES": "dirA=1500,dirB=10"})
    s.wait_new_prompt(0)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=5)
    s.pump(0.2)
    t0 = time.time()
    s.send("cd dirA\r")
    s.send("cd ../dirB\r")
    s.wait_screen(r"\[g\d+ dirB\] %", 8, "prompt for dirB")
    # let A's slow result (1.5s) have every chance to land
    while time.time() - t0 < 2.6:
        s.pump(0.1)
    evs = s.log_events()
    a_gens = sorted({e.gen for e in evs if e.kind == "send" and e.f[1:2] and e.f[1].endswith("/dirA")})
    b_gens = sorted({e.gen for e in evs if e.kind == "send" and e.f[1:2] and e.f[1].endswith("/dirB")})
    if not a_gens:
        raise Fail("no send event for dirA in debug log")
    if not b_gens:
        raise Fail("no send event for dirB in debug log")
    for g in a_gens:
        if any(e.kind in ("accept", "apply_deferred") and e.gen == g for e in evs):
            raise Fail(f"stale result for dirA gen {g} was accepted")
    stale = [g for g in a_gens if any(e.kind == "discard" and e.gen == g and e.f[1:2] == ["stale"] for e in evs)]
    killed = [g for g in a_gens if g not in stale and
              any(e.kind == "backend_exit" and e.gen == g for e in evs)]
    unexplained = [g for g in a_gens if g not in stale and g not in killed]
    if unexplained and not stale:
        raise Fail(f"dirA gens {unexplained} have neither 'discard stale' nor backend_exit (kill) in log")
    ctx.note(f"dirA gens={a_gens} discard-stale={stale} killed-by-chpwd={killed} unexplained={unexplained}")
    if not any(e.kind in ("accept", "apply_deferred") and e.gen in b_gens for e in evs):
        raise Fail("no accept for dirB gen")
    last = s.lines()
    prompts = [l for l in last if FAKE_RE.search(l)]
    if re.search(r"\[g\d+ dirA\] %\s*$", s.row_text()):
        raise Fail("final prompt shows dirA")
    if not re.search(r"\[g\d+ dirB\] %", s.row_text()):
        raise Fail("final prompt does not show dirB")
    _ = prompts


def typing_flow(s, ctx, after, vi=False, plugins=()):
    ev = s.wait_new_prompt(after)
    gen = ev.gen
    text = uniq_text()
    t_first = now_ms()
    s.send_slow(text, 0.03)
    t_last = now_ms()
    recv = s.wait_log("recv", gen=gen, after=after, timeout=5)
    s.wait_log(("accept", "apply_deferred"), gen=gen, after=after, timeout=5)
    if not (t_first - 20 <= recv.ts <= t_last + 20):
        raise Fail(f"reply did not land mid-typing (recv ts offset {recv.ts - t_first:.0f}ms, "
                   f"typing window {t_last - t_first:.0f}ms)")
    s.pump(0.3)
    if not fake_prompt_ok(s, gen):
        raise Fail(f"prompt for gen {gen} not shown after refresh")
    row = s.row_text()
    start = row.rfind(text)
    if start < 0:
        raise Fail(f"typed text not intact on screen: {row.strip()!r}")
    x, y = s.cursor()
    if x != start + len(text):
        raise Fail(f"cursor col {x}, expected {start + len(text)}")
    if "syntax-highlighting" in plugins:
        fg = s.cell_fg(start, y)
        if fg == "default":
            raise Fail("syntax highlighting lost after refresh (command word has default fg)")
    if vi:
        s.send("\x1b")
        s.pump(0.7)
        s.send("0")
        s.pump(0.3)
        x, _ = s.cursor()
        if x != start:
            raise Fail(f"vi: after ESC 0 cursor col {x}, expected {start}")
        s.send("A")
        s.pump(0.2)
        x, _ = s.cursor()
        if x != start + len(text):
            raise Fail(f"vi: after A cursor col {x}, expected {start + len(text)}")
    else:
        s.send("\x1b[D" * 5)
        s.pump(0.3)
        x, _ = s.cursor()
        if x != start + len(text) - 5:
            raise Fail(f"after 5x Left cursor col {x}, expected {start + len(text) - 5}")
    enter_idx = s.log_count()
    s.send("\r")
    s.wait_line_equals(text[len("echo "):], 5, 1)
    return s.wait_new_prompt(enter_idx)


@scenario("typing_during_refresh", "40+ char command typed while the reply lands", plugin_ok=True)
def sc_typing(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_DELAY_MS": "500"}, plugins=plugins)
    ev = typing_flow(s, ctx, 0, plugins=plugins)
    after = ev.idx
    if "autosuggestions" in plugins:
        s.send("echo seedc")
        s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=5)
        s.wait_screen(r"echo seedcommand_for_suggest", 4, "autosuggestion suffix after refresh")
        s.send("\x03")
        s.pump(0.2)


@scenario("completion_during_refresh", "Tab completion while the reply lands; result must be deferred", plugin_ok=True)
def sc_completion(ctx, plugins):
    menu_command = completion_command(ctx)
    s = ctx.session(env={"TILDY_FAKE_DELAY_MS": "500"}, plugins=plugins)
    # Phase A: `ls /us` + Tab -> /usr/ ; then the result lands; Enter runs it.
    ev = s.wait_new_prompt(0)
    g1 = ev.gen
    s.send("ls /us")
    s.pump(0.05)
    s.send("\t")
    s.pump(0.15)
    row = s.row_text().rstrip()
    if not re.search(r"ls /usr/?$", row):
        raise Fail(f"Tab did not complete /us to /usr: {row!r}")
    s.wait_log(("accept", "apply_deferred"), gen=g1, timeout=5)
    s.pump(0.3)
    row = s.row_text().rstrip()
    if not (fake_prompt_ok(s, g1) and re.search(r"ls /usr/?$", row)):
        raise Fail(f"line damaged after refresh: {row!r}")
    after = s.log_count()
    s.send("\r")
    ev = s.wait_new_prompt(after)
    txt = s.screen_text()
    if not re.search(r"(?m)^.*\bbin\b", txt):
        raise Fail("`ls /usr/` output not found after Enter")
    # Phase B: ambiguous completion keeps the menu open while the reply lands.
    after = ev.idx
    g2 = ev.gen
    s.send(menu_command)
    s.pump(0.05)
    s.send("\t")
    s.wait_screen(r"bravo", 3, "completion menu (bravo)")
    d = s.wait_log("defer", gen=g2, after=after, timeout=4)
    ctx.note("defer logged: " + " ".join(d.f))
    s.pump(0.3)
    if not all(name in s.screen_text() for name in ("alpha", "bravo", "charlie")):
        raise Fail("completion menu damaged after reply arrived")
    s.send("\r")  # leave the menu, accepting the selection
    s.wait_log(("apply_deferred", "accept"), gen=g2, after=after, timeout=4)
    s.pump(0.3)
    if not fake_prompt_ok(s, g2):
        raise Fail("deferred prompt not applied after leaving the menu")
    after = s.log_count()
    s.send(" ;echo ok$((7*6))\r")
    s.wait_line_equals("ok42", 5, 1)


@scenario("history_search_during_refresh", "Ctrl-R incremental search while the reply lands")
def sc_hist(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_DELAY_MS": "500"})
    s.wait_new_prompt(0)
    after = s.log_count()
    s.send("echo HISTMARK123\r")
    s.wait_line_equals("HISTMARK123", 5, 1)
    ev = s.wait_new_prompt(after)
    after = ev.idx
    g = ev.gen
    t_search = now_ms()
    s.send("\x12")
    s.pump(0.05)
    s.send("HISTM")
    s.wait_screen(r"i-search: HISTM", 3, "isearch UI")
    recv = s.wait_log("recv", gen=g, after=after, timeout=4)
    if recv.ts < t_search:
        raise Fail("reply arrived before Ctrl-R (timing)")
    s.pump(0.5)
    if not re.search(r"i-search: HISTM", s.screen_text()):
        raise Fail("isearch UI destroyed after reply")
    ls = s.lines()
    si = next(i for i, l in enumerate(ls) if "i-search" in l)
    if si == 0 or "echo HISTMARK123" not in ls[si - 1]:
        raise Fail("isearch match not displayed on prompt line")
    d = s.find_log(("defer", "accept"), gen=g, after=after)
    ctx.note(f"event for gen {g} during search: {d.kind if d else 'none'}")
    s.send("\r")
    s.wait_line_equals("HISTMARK123", 5, 2)


@scenario("vi_mode", "bindkey -v, then repeat typing_during_refresh")
def sc_vi(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_DELAY_MS": "500"})
    s.wait_new_prompt(0)
    after = s.log_count()
    s.send("bindkey -v\r")
    typing_flow(s, ctx, after, vi=True)


@scenario("resize_during_refresh", "SIGWINCH while a delayed reply is pending")
def sc_resize(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_DELAY_MS": "1000"})
    ev = s.wait_new_prompt(0)
    g = ev.gen
    s.pump(0.1)
    s.resize(30, 80)
    s.wait_log("discard", gen=g, timeout=6, where=lambda e: e.f[1:2] == ["stale"])
    newer = s.wait_log(("accept", "apply_deferred"), timeout=6, where=lambda e: e.gen and e.gen > g)
    s.pump(0.3)
    if not fake_prompt_ok(s, None):
        raise Fail("no backend prompt displayed after resize")
    shown = int(FAKE_RE.search(s.row_text()).group(1))
    if shown <= g:
        raise Fail(f"prompt shows gen {shown}, not newer than {g}")
    ctx.note(f"old gen {g} discarded stale; gen {newer.gen} accepted; shown g{shown}")


def failure_flow(ctx, var, reasons, plugins):
    s = ctx.session(env={var: "1", "TILDY_FAKE_DELAY_MS": "100"})
    s.wait_base_prompt()
    d = s.wait_log("discard", gen=1, timeout=6)
    if d.f[1] not in reasons:
        raise Fail(f"discard reason {d.f[1]!r}, expected one of {sorted(reasons)}")
    s.pump(0.4)
    if s.find_log(("accept", "apply_deferred")):
        raise Fail("a bad frame was accepted")
    if not (s.at_prompt() and BASE_RE.match(s.row_text())):
        raise Fail(f"prompt not retained as base prompt: {s.row_text().strip()!r}")
    after = s.log_count()
    # Fake knobs are inherited process environment: retire the persistent
    # fixture producer so clearing one takes effect for the next request too.
    s.send(f"unset {var}; _tildy_w_lost fixture_reset bye\r")
    ev = s.wait_new_prompt(after)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=5)
    s.pump(0.3)
    if not fake_prompt_ok(s, ev.gen):
        raise Fail("did not recover after the failure flag was cleared")
    ctx.note(f"discard reason: {d.f[1]}")


@scenario("half_frame", "RES+L without END: discard, prompt retained")
def sc_half(ctx, plugins):
    failure_flow(ctx, "TILDY_FAKE_HALF_FRAME", {"half_frame", "eof"}, plugins)


@scenario("bad_gen", "END gen mismatch: discard, prompt retained")
def sc_badgen(ctx, plugins):
    failure_flow(ctx, "TILDY_FAKE_BAD_GEN", {"bad_frame", "half_frame"}, plugins)


@scenario("backend_fail", "backend exits non-zero without output: discard, prompt retained")
def sc_fail(ctx, plugins):
    failure_flow(ctx, "TILDY_FAKE_FAIL", {"eof", "half_frame"}, plugins)


@scenario("backend_timeout", "8s backend with a short timeout: 'discard timeout', later prompts work")
def sc_timeout(ctx, plugins):
    # Cwd travels with every request. A running worker cannot observe a later
    # `export` in its parent shell, so choose the fast recovery request by cwd.
    s = ctx.session(env={"TILDY_FAKE_DELAY_RULES": "home0=8000,dirA=0", "TILDY_BACKEND_TIMEOUT_MS": "2000"})
    s.wait_base_prompt()
    t0 = time.time()
    d = s.wait_log("discard", gen=1, timeout=6)
    if d.f[1] != "timeout":
        raise Fail(f"discard reason {d.f[1]!r}, expected 'timeout'")
    ctx.note(f"timeout logged after {time.time() - t0:.1f}s")
    if s.find_log(("accept", "apply_deferred")):
        raise Fail("result accepted despite timeout")
    if not (s.at_prompt() and BASE_RE.match(s.row_text())):
        raise Fail("prompt not retained after timeout")
    try:
        left = [p for p in s.descendants()
                if "sleep" in subprocess.run(["ps", "-o", "command=", "-p", str(p)],
                                             capture_output=True, text=True).stdout]
        if left:
            ctx.note(f"warning: {len(left)} sleep process(es) of the backend still alive after timeout")
    except Exception:
        pass
    after = s.log_count()
    s.send("cd dirA\r")
    ev = s.wait_new_prompt(after)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=5)
    s.pump(0.3)
    if not fake_prompt_ok(s, ev.gen):
        raise Fail("new prompt after timeout did not show backend result")


@scenario("persistent_crash", "persistent backend crashes after 2 requests; prompt stays usable")
def sc_persist(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_CRASH_AFTER": "2", "TILDY_FAKE_DELAY_MS": "50"}, mode="persistent")
    s.wait_base_prompt()
    s.pump(0.5)
    st = s.find_log("backend_start")
    if st is None:
        raise Skip("no backend_start logged: persistent mode not supported")
    if len(st.f) < 3 or st.f[2] != "persistent":
        raise Skip(f"persistent mode unsupported (backend_start mode={st.f[2:3]})")
    s.wait_log(("accept", "apply_deferred"), timeout=5)
    for _ in range(3):
        after = s.log_count()
        s.send("\r")
        s.wait_new_prompt(after)
        s.pump(0.4)
    s.wait_log("backend_exit", timeout=5)
    s.send("echo alive$((1+1))\r")
    s.wait_line_equals("alive2", 5, 1)
    restarts = len([e for e in s.log_events() if e.kind == "backend_start"])
    ctx.note(f"backend_start events: {restarts} ({'restarted' if restarts > 1 else 'no restart observed'})")


@scenario("source_twice", "sourcing the script twice must not duplicate hooks", backends=("fake", "real"))
def sc_source_twice(ctx, plugins):
    s = ctx.session()
    s.wait_new_prompt(0)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=5)

    def counts():
        c = {}
        for e in s.log_events():
            if e.kind == "hook_installed":
                c[e.f[0]] = c.get(e.f[0], 0) + 1
        return c

    before = counts()
    for _ in range(2):
        after = s.log_count()
        s.send(s.source_cmd + "\r")
        s.wait_new_prompt(after)
        s.pump(0.3)
    after_c = counts()
    for k in ("precmd", "chpwd", "winch"):
        if after_c.get(k, 0) != 1:
            raise Fail(f"hook_installed {k} logged {after_c.get(k, 0)} times (expected exactly 1); before={before}")
    s.send('print -r -- "DUP:${#precmd_functions}:${#${(@u)precmd_functions}}:'
           '${#chpwd_functions}:${#${(@u)chpwd_functions}}"\r')
    m = s.wait_screen(r"(?m)^DUP:(\d+):(\d+):(\d+):(\d+)\s*$", 5, "DUP line")
    a, b, c, d = map(int, m.groups())
    if a != b or c != d:
        raise Fail(f"duplicate hook function entries: precmd {a}/{b} unique, chpwd {c}/{d} unique")
    # `source ~/.zshrc` in a running shell: the already-loaded script must not
    # `return` out of the rc file (under eval a top-level return would).
    rc = ctx.tmp / "reload.zshrc"
    rc.write_text(s.source_cmd + "\nprint -r -- RELOAD_TAIL_$((6*7))\n")
    s.send(f"source {rc}; print -r -- RELOAD_STATUS_$?\r")
    s.wait_line_equals("RELOAD_STATUS_0", 5)
    if "RELOAD_TAIL_42" not in s.lines():
        raise Fail("re-sourcing an rc file that loads tildy skipped the lines after the load")



# --------------------------------------------------------------------
# M1 scenarios (protocol v1, docs/protocol.md "PTY 回归（任务 A）")
# --------------------------------------------------------------------
def write_exec(path: Path, body: str) -> Path:
    path.write_text(body)
    path.chmod(0o755)
    return path


def hello_events(s, after=0):
    return s.find_all("hello", after=after)


@scenario("zero_config_real_binary", "real tildy, no config file: default prompt via eval \"$(tildy init zsh)\"",
          backends=("real",))
def sc_zero_config(ctx, plugins):
    s = ctx.session()
    s.wait_log("ready", gen=1, timeout=8)
    s.wait_log(("accept", "apply_deferred"), timeout=8)
    s.wait_backend_prompt(timeout=5, what="default prompt")
    row = s.row_text().rstrip()
    x, y = s.cursor()
    if y != 2 or s.row_text(0).strip() or "home0" not in s.row_text(1):
        raise Fail(f"default prompt must immediately show a blank line and two rows: {s.screen_text()!r}")
    h = s.wait_log("hello", timeout=8)
    ctx.note(f"first prompt {row!r}; hello epoch={h.f[0]} version={h.f[1]} protocol={h.f[2]}")
    ev = s.worker_cycle()
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    s.send("echo zc_$((40+2))\r")
    s.wait_line_equals("zc_42", 5)
    bad = [e for e in s.find_all("backend_degraded")]
    if bad:
        raise Fail(f"backend degraded with the default setup: {bad}")


@scenario("worker_degrade_to_oneshot", "kill the worker: the next prompt still appears, rendered oneshot; then the worker is back",
          backends=("fake", "real"))
def sc_degrade(ctx, plugins):
    env = {} if ctx.real else {"TILDY_FAKE_DELAY_RULES": "dirA=1500"}
    s = ctx.session(env=env, mode="persistent")
    s.wait_backend_prompt(timeout=8, what="first prompt")
    pid = s.wait_worker_ready()
    ev = s.worker_cycle()
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    # Idle means the fresh result is in: killing during a partial re-sends the
    # request and restarts the worker at once, so the next cycle may use it.
    wait_fresh(s, ev.gen, ev.idx)
    # Phase 1: kill an idle worker.
    after = s.log_count()
    os.kill(pid, signal.SIGKILL)
    s.wait_log("backend_exit", after=after, timeout=5, where=lambda e: e.f[1:2] == [str(pid)])
    deg = s.wait_log("backend_degraded", after=after, timeout=3)
    if deg.f[:1] != ["eof"]:
        raise Fail(f"backend_degraded reason {deg.f}, expected eof")
    ev = s.cycle()
    if ev.f[2:3] != ["oneshot"]:
        raise Fail(f"request after the kill went to {ev.f[2:3]}, expected oneshot")
    recv = s.wait_log("recv", gen=ev.gen, after=ev.idx, timeout=6)
    if recv.f[2:3] != ["0"]:
        raise Fail(f"result after the kill has worker_epoch {recv.f[2:3]}, expected 0 (render)")
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    s.pump(0.2)
    s.wait_backend_prompt(gen=ev.gen, timeout=3, what="prompt rendered by oneshot")
    ctx.note(f"killed idle worker {pid}: g{ev.gen} rendered oneshot (epoch 0)")
    pid2 = s.wait_worker_ready(after=after)
    ev = s.worker_cycle()
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    ctx.note(f"restarted worker {pid2} serves g{ev.gen}")
    if ctx.real:
        return
    # Phase 2 (fake): kill the worker while a request is in flight.
    after = s.log_count()
    s.send("cd dirA\r")
    sent = s.wait_log("send", after=after, timeout=5, where=lambda e: e.f[1].endswith("/dirA") and e.f[2:3] == ["worker"])
    s.pump(0.3)
    os.kill(pid2, signal.SIGKILL)
    d = s.wait_log("discard", gen=sent.gen, after=after, timeout=5)
    if d.f[1:2] != ["eof"]:
        raise Fail(f"in-flight request discard reason {d.f[1:2]}, expected eof")
    again = s.wait_log("send", gen=sent.gen, after=d.idx, timeout=3)
    if again.f[2:3] != ["oneshot"]:
        raise Fail("in-flight request was not re-sent as oneshot")
    s.wait_log(("accept", "apply_deferred"), gen=sent.gen, after=after, timeout=6)
    s.pump(0.2)
    s.wait_backend_prompt(gen=sent.gen, timeout=3)
    if not re.search(r"\[g\d+ dirA\] %", s.row_text()):
        raise Fail(f"prompt after in-flight kill does not show dirA: {s.row_text().rstrip()!r}")
    ctx.note(f"in-flight kill: g{sent.gen} discard eof, re-sent oneshot, accepted")


@scenario("version_mismatch_restart", "HELLO with a wrong version: BYE, restart, oneshot meanwhile; persistent mismatch hits the restart limit",
          backends=("fake", "real"))
def sc_version(ctx, plugins):
    if ctx.real:
        # Every worker of this binary mismatches the shell's (bogus) version.
        s = ctx.session(env={"TILDY_TEST_POST_INIT": "TILDY_VERSION=0.0.0-bogus"})
        s.wait_log("backend_degraded", timeout=10, where=lambda e: e.f[:1] == ["restart_limit"])
        byes = s.find_all("bye")
        starts = [e for e in s.find_all("backend_start") if e.f[2:3] == ["persistent"]]
        if not byes or len(starts) < 2:
            raise Fail(f"expected BYE and restarts, got bye={len(byes)} starts={len(starts)}")
        ev = s.cycle()
        if ev.f[2:3] != ["oneshot"]:
            raise Fail("request after restart_limit did not go oneshot")
        s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=6)
        s.pump(1.0)
        alive = [p for p in s.worker_pids() if pid_alive(p)]
        if alive:
            raise Fail(f"workers still alive after BYE: {alive}")
        ctx.note(f"{len(byes)} BYE, {len(starts)} worker starts, all exited; prompts from render")
        return
    # Fake, phase A: only the first worker reports a wrong version.
    marker = ctx.tmp / "mismatch-once"
    wrapper = write_exec(ctx.tmp / "fake-mismatch-once.sh",
                         "#!/bin/sh\n"
                         f"if [ \"$1\" = worker ] && [ ! -e '{marker}' ]; then\n"
                         f"  : > '{marker}'\n"
                         "  TILDY_FAKE_VERSION=9.9.9-bogus; export TILDY_FAKE_VERSION\n"
                         "fi\n"
                         f"exec '{ctx.backend}' \"$@\"\n")
    vers = {"TILDY_VERSION": FAKE_VERSION, "TILDY_PROTOCOL_VERSION": "1"}
    s = ctx.session("a", env=dict(vers), mode="persistent", backend=wrapper)
    h1 = s.wait_log("hello", timeout=8)
    if h1.f[1] != "9.9.9-bogus":
        raise Fail(f"first HELLO version {h1.f[1]!r}")
    bye = s.wait_log("bye", timeout=3)
    if bye.f[:2] != [h1.f[0], "version_mismatch"]:
        raise Fail(f"bye event {bye.f}, expected epoch {h1.f[0]} version_mismatch")
    s.wait_log("backend_degraded", timeout=3, where=lambda e: e.f[:1] == ["version_mismatch"])
    h2 = s.wait_log("hello", after=h1.idx + 1, timeout=8)
    if h2.f[1] != FAKE_VERSION or h2.f[0] == h1.f[0]:
        raise Fail(f"restarted worker HELLO {h2.f}")
    s.wait_log("backend_exit", timeout=5, where=lambda e: e.f[1:2] == [h1.f[3]])
    trace = s.tracefile.read_text() if s.tracefile.exists() else ""
    if f"bye {h1.f[0]}" not in trace:
        raise Fail("fake worker never received BYE (trace)")
    s.wait_backend_prompt(timeout=6, what="first prompt (oneshot meanwhile)")
    first = s.find_log("send", gen=1)
    ev = s.worker_cycle()
    recv = s.wait_log("recv", gen=ev.gen, after=ev.idx, timeout=5)
    if recv.f[2:3] != [h2.f[0]]:
        raise Fail(f"result epoch {recv.f[2:3]}, expected the new worker's {h2.f[0]}")
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    ctx.note(f"A: hello {h1.f[1]} -> bye -> restart -> hello {h2.f[1]}; g1 via {first.f[2] if first else '?'}; g{ev.gen} via worker epoch {h2.f[0]}")
    # Phase B: every worker mismatches -> restart limit -> oneshot for good.
    s2 = ctx.session("b", env=dict(vers, TILDY_FAKE_VERSION="9.9.9-bogus"), mode="persistent")
    s2.wait_log("backend_degraded", timeout=10, where=lambda e: e.f[:1] == ["restart_limit"])
    starts = [e for e in s2.find_all("backend_start") if e.f[2:3] == ["persistent"]]
    byes = s2.find_all("bye")
    if len(starts) != 4 or len(byes) != 4:
        raise Fail(f"expected 4 worker starts (1 + 3 restarts) and 4 BYE, got {len(starts)}/{len(byes)}")
    ev = s2.cycle()
    if ev.f[2:3] != ["oneshot"]:
        raise Fail("request after restart_limit did not go oneshot")
    s2.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    ev = s2.cycle()
    if s2.find_all("backend_start", after=ev.idx, where=lambda e: e.f[2:3] == ["persistent"]):
        raise Fail("worker restarted after restart_limit")
    s2.pump(0.5)
    alive = [p for p in s2.worker_pids() if pid_alive(p)]
    if alive:
        raise Fail(f"mismatched workers still alive: {alive}")
    ctx.note(f"B: {len(starts)} starts, {len(byes)} BYE, restart_limit, later prompts oneshot")


@scenario("transient_keeps_blank_line", "add_newline: the collapsed line keeps the blank line above it (multi-line T)",
          backends=("real",))
def sc_transient_blank_line(ctx, plugins):
    cfgf = ctx.tmp / "newline-config.json"
    cfgf.write_text('{"schema_version": 1, "add_newline": true}\n')
    s = ctx.session(env={"TILDY_CONFIG": str(cfgf)}, mode="persistent")
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    s.pump(0.2)
    after = s.log_count()
    s.send("echo BLANK_ONE\r")
    s.wait_line_equals("BLANK_ONE", 5)
    s.wait_log("transient", after=after, timeout=3)
    s.wait_new_prompt(after)
    s.pump(0.2)
    lines = s.lines()
    hits = [i for i, l in enumerate(lines) if l.rstrip() == "❯ echo BLANK_ONE"]
    if not hits:
        raise Fail("submitted line was not collapsed to '❯ echo BLANK_ONE'")
    i = hits[0]
    if i == 0 or lines[i - 1].strip():
        raise Fail(f"row above the collapsed line is not blank: {lines[i - 1]!r}")
    # The output follows the collapsed line directly; the next prompt has its own blank line.
    if i + 1 >= len(lines) or lines[i + 1].rstrip() != "BLANK_ONE":
        raise Fail(f"output does not follow the collapsed line: {lines[i:i + 3]!r}")


@scenario("blank_line_clears_exit_status", "empty input clears the error display while preserving $? and the failure summary",
          plugin_ok=True, backends=("real",))
def sc_blank_line_exit_status(ctx, plugins):
    for history_mode in ("compact", "full"):
        cfg = ctx.tmp / ("blank-line-" + history_mode + ".json")
        cfg.write_text(json.dumps({
            "schema_version": 1, "preset": "plain", "add_newline": False,
            "history": {"mode": history_mode},
            "modules": {
                "exit_probe": {"type": "exit_status", "format": "ERR=$status"},
                "char_probe": {"type": "character", "symbol": ">", "error_symbol": ">",
                               "success_style": {"foreground": "green"},
                               "error_style": {"foreground": "red"}},
            },
            "layout": {"rows": [{"left": ["exit_probe", "char_probe"]}]},
        }))
        s = ctx.session(name=history_mode, env={"TILDY_CONFIG": str(cfg)}, plugins=plugins)
        s.wait_backend_prompt(timeout=8)

        def check_prompt(code):
            x, y = s.cursor()
            row = s.row_text(y)[:x].rstrip()
            found = re.search(r"ERR=(\d+)", row)
            actual = int(found.group(1)) if found else 0
            color = "red" if code else "green"
            if actual != code or not row.endswith(">") or s.cell_fg(len(row) - 1, y) != color:
                raise Fail(f"{history_mode}: expected exit {code} / {color}, got {row!r}")

        def submit(command, code):
            ev = s.cycle(command)
            s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
            s.wait_backend_prompt(gen=ev.gen)
            check_prompt(code)

        start = len(s.raw)
        submit("false", 1)
        # Resizing alone must retain the failure; it is not an empty submission.
        after = s.log_count()
        s.resize(30, 90)
        ev = s.wait_log("send", after=after, timeout=5)
        s.wait_backend_prompt(gen=ev.gen)
        check_prompt(1)
        submit("", 0)
        submit("   ", 0)
        after = s.log_count()
        s.resize(30, 100)
        ev = s.wait_log("send", after=after, timeout=5)
        s.wait_backend_prompt(gen=ev.gen)
        check_prompt(0)
        summary = "↳ 退出码 1".encode()
        expected = 1 if history_mode == "compact" else 0
        if bytes(s.raw[start:]).count(summary) != expected:
            raise Fail(f"{history_mode}: failure summary missing or repeated after empty input")
        submit("print -r -- EMPTY_RC=$?", 0)
        s.wait_line_equals("EMPTY_RC=1", 5)
        submit("(exit 7)", 7)
        submit("   ", 0)
        submit("print -r -- NEXT_RC=$?", 0)
        s.wait_line_equals("NEXT_RC=7", 5)
        # Syntax errors do not run preexec, but must still show their real status.
        submit(")", 1)
        submit("true", 0)
    ctx.note("compact/full: blank and whitespace input clear status/color; resize, $? and summaries preserved")


@scenario("transient_prompt", "T line: Enter and Ctrl-C collapse the prompt; $? still from the real command",
          backends=("fake", "real"))
def sc_transient(ctx, plugins):
    env = {} if ctx.real else {"TILDY_FAKE_TRANSIENT": "1"}
    s = ctx.session(env=env, mode="persistent")
    s.wait_backend_prompt(timeout=8, what="first prompt")
    g = s.last_applied().gen
    after = s.log_count()
    s.send("echo TRANSIENT_ONE\r")
    s.wait_line_equals("TRANSIENT_ONE", 5)
    tr = s.wait_log("transient", after=after, timeout=3)
    if tr.gen != g or tr.f[1:2] == ["int"]:
        raise Fail(f"transient event {tr.f}, expected gen {g} from zle-line-finish")
    if "❯ echo TRANSIENT_ONE" not in s.lines():
        raise Fail("submitted line was not collapsed to '❯ echo TRANSIENT_ONE'")
    # Exit status: from the real command, not from any hook.
    ev = s.wait_new_prompt(after)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=5)
    after = s.log_count()
    s.send("false\r")
    ev = s.wait_new_prompt(after)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=5)
    s.pump(0.2)
    after = s.log_count()
    s.send("echo rc=$?\r")
    s.wait_line_equals("rc=1", 5)
    if "❯ false" not in s.lines() or "❯ echo rc=$?" not in s.lines():
        raise Fail("'false' / 'echo rc=$?' lines not collapsed")
    if not ctx.real:
        reqs = [l.split("\t") for l in s.tracefile.read_text().splitlines() if l.startswith("req ")]
        req = next((r for r in reqs if r[1] == str(ev.gen)), None)
        if not req or req[6] != "1":
            raise Fail(f"REQ for g{ev.gen} after `false` has last_exit {req[6] if req else None}, expected 1")
    ev = s.wait_new_prompt(after)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=5)
    s.pump(0.2)
    # Ctrl-C: no zle-line-finish, no precmd; collapse, then the full prompt back.
    after = s.log_count()
    s.send("echo abort_me")
    s.pump(0.2)
    s.send("\x03")
    tr = s.wait_log("transient", after=after, timeout=3)
    if tr.f[1:2] != ["int"]:
        raise Fail(f"Ctrl-C transient event {tr.f}")
    s.pump(0.4)
    if "❯ echo abort_me" not in s.lines():
        raise Fail("Ctrl-C line not collapsed to '❯ echo abort_me'")
    if not s.backend_prompt_ok():
        raise Fail(f"full prompt not restored after Ctrl-C: {s.row_text().rstrip()!r}")
    # The default real theme's input row is also just '❯'; its context lives
    # on the powerline row above. A transient prompt has no such live row.
    if ctx.real and "home0" not in s.row_text(s.cursor()[1] - 1):
        raise Fail("full directory row not restored after Ctrl-C")
    if not ctx.real and s.row_text().startswith("❯"):
        raise Fail("prompt after Ctrl-C is still the transient form")
    s.send("echo after_int\r")
    s.wait_line_equals("after_int", 5)
    if "❯ echo after_int" not in s.lines():
        raise Fail("line after Ctrl-C not collapsed")
    # Isearch accept goes through zle-line-finish too.
    ev = s.wait_new_prompt(after)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=5)
    s.pump(0.2)
    after = s.log_count()
    s.send("\x12TRANSIENT_O")
    s.wait_screen(r"i-search: TRANSIENT_O", 3, "isearch")
    s.send("\r")
    s.wait_line_equals("TRANSIENT_ONE", 5, count=2)
    s.wait_log("transient", after=after, timeout=3)
    ctx.note("Enter, Ctrl-C and isearch accept collapse to '❯ '; rc=1 preserved")
    if ctx.real:
        return
    # Multi-line prompt (fake): two lines collapse to one.
    s2 = ctx.session("ml", env={"TILDY_FAKE_TRANSIENT": "1", "TILDY_FAKE_LINES": "2"}, mode="persistent")
    s2.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    ev = s2.cycle()
    s2.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    s2.pump(0.2)
    top = f"-- g{ev.gen} line 1 --"
    if top not in s2.lines():
        raise Fail(f"two-line prompt not shown before Enter ({top!r} missing)")
    after = s2.log_count()
    s2.send("echo ML_ONE\r")
    s2.wait_line_equals("ML_ONE", 5)
    s2.wait_log("transient", gen=ev.gen, after=after, timeout=3)
    s2.pump(0.3)
    if top in s2.lines():
        raise Fail(f"top line {top!r} still on screen after the transient collapse")
    if "❯ echo ML_ONE" not in s2.lines():
        raise Fail("two-line prompt not collapsed to '❯ echo ML_ONE'")
    ctx.note("two-line prompt collapsed to one line")


@scenario("multi_line_result", "two L lines appear in the first empty editor; typing remains intact")
def sc_multiline(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_LINES": "2", "TILDY_FAKE_DELAY_MS": "100"}, mode="persistent")
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=6)
    s.pump(0.2)
    x, y = s.cursor()
    if not fake_prompt_ok(s, 1) or s.row_text(y - 1).rstrip() != "-- g1 line 1 --":
        raise Fail(f"two-line prompt not shown in cycle 1: {s.screen_text()!r}")
    text = uniq_text()
    s.send_slow(text, 0.01)
    s.pump(0.2)
    start = s.row_text().rfind(text)
    if start < 0 or s.cursor()[0] != start + len(text):
        raise Fail(f"typing on a two-line prompt: {s.row_text().rstrip()!r} cursor {s.cursor()}")
    s.send("\r")
    s.wait_line_equals(text[len("echo "):], 5)
    ctx.note("first result drew both lines without Enter; typing intact")


@scenario("height_change_with_input", "height changes wait for an empty buffer, then apply without Enter")
def sc_height_input(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_LINES": "3", "TILDY_FAKE_DELAY_MS": "800"}, mode="persistent")
    s.wait_base_prompt()
    text = "echo KEEP_TYPED_TEXT"
    s.send(text)
    s.wait_log("defer", gen=1, timeout=6, where=lambda e: "height_with_input" in e.f)
    s.pump(0.2)
    if s.find_log(("accept", "apply_deferred"), gen=1):
        raise Fail("height changed while a command was being edited")
    if not s.row_text().rstrip().endswith(text) or s.cursor()[0] != s.row_text().rfind(text) + len(text):
        raise Fail(f"buffer or cursor changed: {s.evidence()}")
    s.send("\x15")  # Ctrl-U empties the editor; no command is executed.
    s.wait_log("apply_deferred", gen=1, timeout=5)
    s.pump(0.2)
    if s.row_text(s.cursor()[1] - 2).rstrip() != "-- g1 line 1 --":
        raise Fail("pending three-line prompt was not applied when input became empty")
    ctx.note("typed input and cursor preserved; Ctrl-U applied the pending height")


@scenario("reload_newline_height", "newline changes and rc reload redraw immediately, preserving command output", backends=("real",))
def sc_reload_newline_height(ctx, plugins):
    cfg = ctx.tmp / "reload-newline.json"
    cfg.write_text('{"preset":"pastel-powerline","add_newline":true}')
    s = ctx.session(env={"TILDY_CONFIG": str(cfg)}, cols=80)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    for on in (False, True, False):
        content = json.dumps({"preset": "pastel-powerline", "add_newline": on})
        after = s.log_count()
        s.send(f"print -r -- {shlex.quote(content)} > {shlex.quote(str(cfg))}; "
               'source "$ZDOTDIR/.zshrc"; print -r -- RELOAD_OUTPUT_$((6*7))\r')
        ev = s.wait_log("send", after=after, timeout=8)
        s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=8)
        s.pump(0.2)
        x, y = s.cursor()
        height = 3 if on else 2
        if s.row_text(y - height).rstrip() != "RELOAD_OUTPUT_42":
            raise Fail(f"height change overwrote output or left stale rows: {s.evidence()}")
        if s.row_text(y)[:x].rstrip() != "❯" or "home0" not in s.row_text(y - 1):
            raise Fail(f"theme missing after reload: {s.evidence()}")
        if on and s.row_text(y - 2).strip():
            raise Fail("enabled newline not blank")
    ctx.note("cold start and 3->2->3->2 reloads displayed immediately; output and cursor correct")


def resize_idle_flow(ctx, s):
    s.wait_backend_prompt(timeout=8, what="first prompt")
    g0 = s.last_applied().gen
    s.pump(0.5)
    for rows, cols in ((30, 80), (30, 100)):
        after = s.log_count()
        t0 = now_ms()
        s.resize(rows, cols)
        sent = s.wait_log("send", after=after, timeout=3)
        acc = s.wait_log(("accept", "apply_deferred"), gen=sent.gen, after=after, timeout=3)
        s.pump(0.2)
        s.wait_backend_prompt(gen=sent.gen, timeout=2)
        ctx.note(f"{cols} cols: g{sent.gen} via {sent.f[2]}, drawn {acc.ts - t0:.0f} ms after resize, no keypress")
        if sent.gen <= g0:
            raise Fail("resize did not start a new generation")
        g0 = sent.gen


@scenario("resize_idle", "oneshot: resize with no request in flight refreshes without a keypress (self-pipe)")
def sc_resize_idle(ctx, plugins):
    resize_idle_flow(ctx, ctx.session(mode="oneshot"))


@scenario("resize_idle_worker", "worker: resize with no request in flight refreshes without a keypress",
          backends=("fake", "real"))
def sc_resize_idle_worker(ctx, plugins):
    resize_idle_flow(ctx, ctx.session(mode="persistent"))


@scenario("nested_shell", "zsh inside zsh: own worker, works, exits with the inner shell; outer keeps working",
          backends=("fake", "real"))
def sc_nested(ctx, plugins):
    s = ctx.session(mode="persistent")
    s.wait_backend_prompt(timeout=8, what="outer prompt")
    outer_workers = set(s.worker_pids())
    nlog = ctx.tmp / "nested.debug.log"
    nlog.write_text("")
    after = s.log_count()
    s.send(f"TILDY_DEBUG_LOG={nlog} zsh -i\r")
    end = time.time() + T(10)
    while not any(e.kind in ("accept", "apply_deferred") for e in s.log_events(nlog)):
        if time.time() > end:
            raise Fail("nested shell never drew a backend prompt")
        s.pump(0.05)
    s.pump(0.3)
    s.wait_backend_prompt(timeout=3, what="nested prompt")
    nested = s.log_events(nlog)
    npids = [int(e.f[1]) for e in nested if e.kind == "backend_start" and e.f[2:3] == ["persistent"]]
    if not npids or set(npids) & outer_workers:
        raise Fail(f"nested shell has no worker of its own: {npids} vs outer {outer_workers}")
    s.send("echo nested_$((6*7))\r")
    s.wait_line_equals("nested_42", 5)
    s.send("exit\r")
    ev = s.wait_log("send", after=after, timeout=8)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=6)
    t0 = time.time()
    while any(pid_alive(p) for p in npids):
        if time.time() - t0 > 2.0:
            raise Fail(f"nested worker(s) {[p for p in npids if pid_alive(p)]} alive 2 s after the nested shell exited")
        time.sleep(0.05)
    dt = time.time() - t0
    s.send("echo outer_$((1+1))\r")
    s.wait_line_equals("outer_2", 5)
    if not any(pid_alive(p) for p in outer_workers):
        raise Fail("outer worker died with the nested shell")
    ctx.note(f"nested worker(s) {npids} exited {dt * 1000:.0f} ms after `exit`; outer worker alive")


@scenario("fast_cd_burst", "10 cd in a row: last directory wins, results never go backwards, no leaked processes",
          backends=("fake", "real"))
def sc_burst(ctx, plugins):
    env = {} if ctx.real else {"TILDY_FAKE_DELAY_MS": "30"}
    s = ctx.session(env=env, mode="persistent")
    s.wait_backend_prompt(timeout=8, what="first prompt")
    s.wait_worker_ready()
    s.pump(0.2)
    g0 = s.last_applied().gen
    after = s.log_count()
    s.send("cd dirA\r" + "cd ../dirB\rcd ../dirA\r" * 4 + "cd ../dirB\r")
    # A `cd` command bumps the generation twice (chpwd, then precmd) but sends
    # one request: chpwd runs while the command executes (zle inactive), so only
    # the precmd that follows asks. 10 cd -> 10 prompts, 10 requests, gen +20.
    end = time.time() + T(10)
    while True:
        sends = s.find_all("send", after=after)
        readies = s.find_all("ready", after=after)
        last = sends[-1] if sends else None
        if last and last.f[1].endswith("/dirB") and len(readies) >= 10 and last.gen == g0 + 20 and s.find_log(
                ("accept", "apply_deferred"), gen=last.gen, after=after):
            break
        if time.time() > end:
            raise Fail(f"burst did not settle: {len(sends)} sends, {len(readies)} prompts, "
                       f"last {last.f if last else None} (expected gen {g0 + 20})")
        s.pump(0.05)
    if len(sends) < 10:
        raise Fail(f"{len(sends)} requests for 10 prompts")
    s.pump(0.5)
    if s.find_all("send", after=last.idx + 1):
        raise Fail("more requests after the burst settled")
    s.wait_backend_prompt(gen=last.gen, timeout=3, what="final prompt")
    if not ctx.real and not re.search(rf"\[g{last.gen} dirB\] %", s.row_text()):
        raise Fail(f"final prompt is not g{last.gen} dirB: {s.row_text().rstrip()!r}")
    if ctx.real and "dirB" not in s.row_text(s.cursor()[1] - 1):
        raise Fail(f"final prompt directory row does not show dirB: {s.row_text(s.cursor()[1] - 1).rstrip()!r}")
    applied = [e.gen for e in s.find_all(("accept", "apply_deferred"), after=after)]
    if any(b < a for a, b in zip(applied, applied[1:])):
        raise Fail(f"an older result was drawn after a newer one: {applied}")
    time.sleep(1.0)
    procs = []
    for p in s.descendants():
        cmd = subprocess.run(["ps", "-o", "command=", "-p", str(p)], capture_output=True, text=True).stdout.strip()
        if cmd:
            procs.append(cmd)
    if len(procs) > 1:
        raise Fail(f"processes left under zsh after the burst: {procs}")
    stale = len(s.find_all("discard", after=after, where=lambda e: e.f[1:2] == ["stale"]))
    ctx.note(f"{len(sends)} requests, {len(applied)} drawn (monotonic), {stale} stale discards, children={procs}")


@scenario("terminal_kill_no_orphan", "SIGKILL / SIGHUP of zsh: the worker exits within 2 s",
          backends=("fake", "real"))
def sc_term_kill(ctx, plugins):
    for sig in (signal.SIGKILL, signal.SIGHUP):
        s = ctx.session(f"k{int(sig)}", mode="persistent")
        s.wait_backend_prompt(timeout=8, what="first prompt")
        pid = s.wait_worker_ready()
        s.pump(0.2)
        os.kill(s.child.pid, sig)
        t0 = time.time()
        while pid_alive(pid):
            if time.time() - t0 > 2.0:
                raise Fail(f"worker {pid} still alive 2 s after {sig.name} to zsh")
            time.sleep(0.02)
        ctx.note(f"{sig.name}: worker exited after {(time.time() - t0) * 1000:.0f} ms")


@scenario("partial_then_fresh",
          "same gen answered three times (partial, fresh, late fresh): each drawn, the last wins, typing intact")
def sc_partial(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_PARTIAL": "1", "TILDY_FAKE_DELAY_MS": "900",
                         "TILDY_FAKE_SECOND_FRESH_MS": "1500"}, mode="persistent")
    s.wait_backend_prompt(timeout=8, what="first prompt")
    s.wait_worker_ready()
    after = s.log_count()
    s.send("\r")
    ev = s.wait_log("send", after=after, timeout=5, where=lambda e: e.f[2:3] == ["worker"])
    g = ev.gen
    p = s.wait_log("recv", gen=g, after=after, timeout=3)
    if p.f[1:2] != ["partial"]:
        raise Fail(f"first result for g{g} is {p.f[1:2]}, expected partial")
    s.wait_log(("accept", "apply_deferred"), gen=g, after=p.idx, timeout=3)
    s.pump(0.1)
    if not (fake_prompt_ok(s, g) and "pending" in s.row_text()):
        raise Fail(f"partial result not shown: {s.row_text().rstrip()!r}")
    text = uniq_text()
    s.send_slow(text, 0.02)
    fr = s.wait_log("recv", gen=g, after=p.idx + 1, timeout=3)
    if fr.f[1:2] != ["fresh"]:
        raise Fail(f"second result for g{g} is {fr.f[1:2]}, expected fresh")
    s.wait_log(("accept", "apply_deferred"), gen=g, after=fr.idx, timeout=3)
    s.pump(0.1)
    row = s.row_text()
    if "ready" not in row or "pending" in row:
        raise Fail(f"fresh result did not supersede partial: {row.rstrip()!r}")
    start = row.rfind(text)
    if start < 0 or s.cursor()[0] != start + len(text):
        raise Fail(f"buffer/cursor damaged by the second redraw: {row.rstrip()!r} cursor {s.cursor()}")
    # A second fresh for the same gen (late provider result) supersedes the first.
    s.send_slow("X", 0.02)
    late = s.wait_log("recv", gen=g, after=fr.idx + 1, timeout=3)
    if late.f[1:2] != ["fresh"]:
        raise Fail(f"third result for g{g} is {late.f[1:2]}, expected fresh")
    s.wait_log(("accept", "apply_deferred"), gen=g, after=late.idx, timeout=3)
    s.pump(0.3)
    row = s.row_text()
    if "late" not in row or "ready" in row:
        raise Fail(f"late fresh result did not supersede the first fresh: {row.rstrip()!r}")
    start = row.rfind(text + "X")
    if start < 0 or s.cursor()[0] != start + len(text) + 1:
        raise Fail(f"buffer/cursor damaged by the third redraw: {row.rstrip()!r} cursor {s.cursor()}")
    if s.find_all("backend_degraded"):
        raise Fail(f"worker given up on: {s.find_all('backend_degraded')}")
    s.send("\r")
    s.wait_line_equals(text[len("echo "):] + "X", 5)
    ctx.note(f"g{g}: partial, fresh {fr.ts - p.ts} ms later, second fresh {late.ts - fr.ts} ms after that; buffer intact")


@scenario("worker_epoch_mismatch",
          "RES epoch differs from the HELLO epoch: bad_frame, worker retired, request re-sent oneshot")
def sc_epoch(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_RES_EPOCH": "424242", "TILDY_FAKE_EPOCH": "777"}, mode="persistent")
    h = s.wait_worker_ready()
    after = s.log_count()
    s.send("\r")
    ev = s.wait_log("send", after=after, timeout=5, where=lambda e: e.f[2:3] == ["worker"])
    d = s.wait_log("discard", gen=ev.gen, after=ev.idx, timeout=3)
    if d.f[1:2] != ["bad_frame"]:
        raise Fail(f"discard reason {d.f[1:2]}, expected bad_frame")
    if s.find_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx,
                  where=lambda e: e.idx < d.idx):
        raise Fail("a RES with a foreign epoch was drawn")
    deg = s.wait_log("backend_degraded", after=ev.idx, timeout=3)
    if deg.f[:1] != ["bad_frame"]:
        raise Fail(f"backend_degraded {deg.f}, expected bad_frame")
    again = s.wait_log("send", gen=ev.gen, after=d.idx, timeout=3)
    if again.f[2:3] != ["oneshot"]:
        raise Fail(f"g{ev.gen} re-sent via {again.f[2:3]}, expected oneshot")
    rc = s.wait_log("recv", gen=ev.gen, after=again.idx, timeout=5)
    if rc.f[2:3] != ["0"]:
        raise Fail(f"re-sent result epoch {rc.f[2:3]}, expected 0")
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=rc.idx, timeout=3)
    s.pump(0.2)
    s.wait_backend_prompt(gen=ev.gen, timeout=3)
    s.pump(0.5)
    if pid_alive(h):
        raise Fail(f"worker {h} with the foreign epoch still alive")
    ctx.note(f"g{ev.gen}: RES epoch 424242 != HELLO 777 -> bad_frame, worker {h} killed, oneshot result drawn")


def slow_git_env(ctx, delay="0.4"):
    """PATH with a `git` that sleeps first: the real worker's partial RES comes
    at once, its fresh RES (with Git) about a second later."""
    real_git = shutil.which("git")
    if not real_git:
        raise Skip("git not found")
    stub = ctx.tmp / "slowgit"
    stub.mkdir(exist_ok=True)
    write_exec(stub / "git", f"#!/bin/sh\nsleep {delay}\nexec '{real_git}' \"$@\"\n")
    repo = ctx.home0 / "repo"
    subprocess.run([real_git, "init", "-q", str(repo)], check=True,
                   env=dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1"))
    (repo / "untracked.txt").write_text("x\n")
    return {"PATH": f"{stub}:{os.environ.get('PATH', '/usr/bin:/bin')}",
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}


def wait_fresh(s, gen, after, timeout=6):
    return s.wait_log("recv", gen=gen, after=after, timeout=timeout, where=lambda e: e.f[1:2] == ["fresh"])


@scenario("real_slow_git_refresh",
          "real worker, slow git: fresh RES lands while typing and while a completion menu is open",
          plugin_ok=True, backends=("real",))
def sc_real_slow_git(ctx, plugins):
    menu_command = completion_command(ctx)
    s = ctx.session(env=slow_git_env(ctx), plugins=plugins)
    s.wait_backend_prompt(timeout=8, what="first prompt")
    s.wait_worker_ready()
    after = s.log_count()
    s.send("cd repo\r")
    ev = s.wait_log("send", after=after, timeout=5, where=lambda e: e.f[1].endswith("/repo"))
    wait_fresh(s, ev.gen, ev.idx)
    s.pump(0.3)
    if "main" not in s.row_text() and "master" not in s.row_text():
        ctx.note(f"git segment not recognised in {s.row_text().rstrip()!r}")
    # 1. Typing while the fresh result lands.
    ev = s.worker_cycle()
    g = ev.gen
    p = s.wait_log("recv", gen=g, after=ev.idx, timeout=3)
    if p.f[1:2] != ["partial"]:
        raise Fail(f"first RES for g{g} is {p.f[1:2]}, expected partial (git is slow)")
    text = uniq_text()[:32]   # the real prompt is long; keep the line from wrapping at 100 columns
    t_first = now_ms()
    s.send_slow(text, 0.03)
    t_last = now_ms()
    fr = wait_fresh(s, g, ev.idx)
    s.wait_log(("accept", "apply_deferred"), gen=g, after=fr.idx, timeout=3)
    if not (t_first - 20 <= fr.ts <= t_last + 20):
        raise Fail(f"fresh RES did not land mid-typing ({fr.ts - t_first:.0f} ms into a {t_last - t_first:.0f} ms window)")
    s.pump(0.3)
    row = s.row_text()
    start = row.rfind(text)
    x, y = s.cursor()
    if start < 0 or x != start + len(text):
        raise Fail(f"typed text/cursor damaged by the fresh redraw: {row.rstrip()!r} cursor {x}")
    last = s.last_applied()
    if not REAL_RE.search(row[:start]) or BASE_RE.match(row[:start]) or last is None or last.gen != g:
        raise Fail(f"prompt for g{g} not shown: {row.rstrip()!r}")
    if "syntax-highlighting" in plugins and s.cell_fg(start, y) == "default":
        raise Fail("syntax highlighting lost after the fresh redraw")
    s.send("\x1b[D" * 5)
    s.pump(0.3)
    if s.cursor()[0] != start + len(text) - 5:
        raise Fail(f"after 5x Left cursor col {s.cursor()[0]}, expected {start + len(text) - 5}")
    after = s.log_count()
    s.send("\r")
    s.wait_line_equals(text[len("echo "):], 5)
    ev = s.wait_new_prompt(after)
    ctx.note(f"g{g}: partial then fresh {fr.ts - p.ts} ms later, {fr.ts - t_first:.0f} ms into typing; line intact")
    # 2. A completion menu is open when the fresh result lands.
    if ev.f[2:3] != ["worker"]:
        ev = s.worker_cycle()
    g2 = ev.gen
    s.wait_log("recv", gen=g2, after=ev.idx, timeout=3)
    s.send(menu_command)
    s.pump(0.05)
    s.send("\t")
    s.wait_screen(r"bravo", 3, "completion menu (bravo)")
    fr = wait_fresh(s, g2, ev.idx)
    d = s.wait_log("defer", gen=g2, after=ev.idx, timeout=3)
    s.pump(0.3)
    if not all(name in s.screen_text() for name in ("alpha", "bravo", "charlie")):
        raise Fail("completion menu damaged when the fresh result arrived")
    s.send("\r")
    s.wait_log("apply_deferred", gen=g2, after=fr.idx, timeout=4)
    s.pump(0.3)
    after = s.log_count()
    s.send(" ;echo ok$((7*6))\r")
    s.wait_line_equals("ok42", 5)
    s.wait_new_prompt(after)   # its prompt must exist before the next Enter
    s.pump(0.2)
    ctx.note(f"g{g2}: fresh during menu -> defer ({' '.join(d.f[1:])}) -> apply_deferred")
    if "autosuggestions" in plugins:
        # 3. The suggestion suffix survives the fresh redraw.
        ev = s.worker_cycle()
        s.send("echo seedc")
        fr = wait_fresh(s, ev.gen, ev.idx)
        s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=fr.idx, timeout=3)
        s.pump(0.3)
        s.wait_screen(r"echo seedcommand_for_suggest", 2, "autosuggestion suffix after the fresh redraw")
        s.send("\x03")
        s.pump(0.2)


@scenario("prompt_subst_injection",
          "setopt prompt_subst prompt_bang: directory names with $(...) and backticks show literally, nothing runs",
          backends=("fake", "real"))
def sc_prompt_subst(ctx, plugins):
    # The renderer leaves "$" and "`" alone (only "%" is escaped): the shell
    # must never put backend text through parameter/command substitution.
    pwned = Path(f"/tmp/tildy-pwned-{os.getpid()}")
    names = ["x$(touch PWNED1)", "y`touch PWNED2`", f"x$(touch {pwned})"]   # the last one is a nested path
    for n in names:
        (ctx.home0 / n).mkdir(parents=True, exist_ok=True)
    s = ctx.session(env={"TILDY_TEST_POST_INIT": "setopt prompt_subst prompt_bang"}, mode="persistent")
    s.wait_backend_prompt(timeout=8, what="first prompt")
    s.send('print -r -- "OPTS=$options[promptsubst],$options[promptbang]"\r')
    s.wait_line_equals("OPTS=on,on", 5)
    seen = []
    try:
        for n in names:
            after = s.log_count()
            s.send(f"cd '{n}'\r")
            ev = s.wait_log("send", after=after, timeout=5, where=lambda e: e.f[1].endswith(n.rsplit("/", 1)[-1]))
            s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=6)
            s.pump(0.2)
            s.wait_backend_prompt(gen=ev.gen, timeout=3, what=f"prompt in {n!r}")
            row = s.row_text(s.cursor()[1] - 1 if ctx.real else None).rstrip()
            want = n if ctx.real or "/" not in n else n.rsplit("/", 1)[-1]
            if want not in row:
                raise Fail(f"directory name not shown literally: want {want!r} in {row!r}")
            seen.append(row)
            # One more full cycle: transient collapse + a fresh prompt in the same directory.
            ev = s.cycle()
            s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=6)
            s.pump(0.2)
            after = s.log_count()
            s.send(f"cd {ctx.home0}\r")
            s.wait_new_prompt(after)
        s.pump(0.5)
        created = sorted(str(f) for f in ctx.tmp.rglob("PWNED*"))
        if pwned.exists():
            created.append(str(pwned))
        if created:
            raise Fail(f"backend text was executed by prompt expansion: {created}")
    finally:
        try:
            pwned.unlink()
        except OSError:
            pass
    ctx.note("shown literally, no file created: " + " | ".join(r[-60:] for r in seen))


# ====================================================================
# runner
# ====================================================================
# --------------------------------------------------------------------
# M1b scenarios (task C): jobs/env fields, powerline rows, Nerd widths
# --------------------------------------------------------------------
SEP = "\ue0b0"


def trace_reqs(s):
    """Parsed REQ lines the fake backend traced: list of field lists
    (index 0 is "req REQ"; 1 gen, 3 cwd, 6 last_exit, 8 env, 9 jobs)."""
    try:
        lines = s.tracefile.read_text(errors="replace").splitlines()
    except OSError:
        return []
    return [l.split("\t") for l in lines if l.startswith("req ")]


def req_for(s, gen):
    for r in trace_reqs(s):
        if r[1] == str(gen):
            return r
    raise Fail(f"no traced REQ for g{gen}")


def env_pairs_of(req):
    raw = req[8]
    return [] if raw == "" else raw.split("\x1f")


def pl_cycle(s, cmd=""):
    """Enter (with cmd) on a powerline prompt; return the next prompt's send event once it is drawn.
    (wait_new_prompt looks for a trailing '%' or the default ❯, not for the powerline row.)"""
    after = s.log_count()
    s.send(cmd + "\r")
    ev = s.wait_log("send", after=after, timeout=10)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=after, timeout=5)
    s.pump(0.3)
    return ev


def nonblank(s):
    return sum(1 for l in s.lines() if l.strip())


@scenario("powerline_two_rows",
          "two-line powerline prompt (Nerd glyphs): row 2 is the prompt char; Enter collapses the old prompt to one line")
def sc_powerline_rows(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_POWERLINE": "1"}, mode="persistent")
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    ev = pl_cycle(s)
    x, y = s.cursor()
    top, bottom = s.row_text(y - 1).rstrip(), s.row_text(y)[:x].rstrip()
    if SEP not in top or " u " not in top or "home0" not in top:
        raise Fail(f"powerline row not shown above the cursor row: {top!r}")
    if bottom != "❯":
        raise Fail(f"second row is {bottom!r}, expected the prompt character")
    n0 = nonblank(s)
    after = s.log_count()
    s.send("echo PL_ONE\r")
    s.wait_line_equals("PL_ONE", 5)
    s.wait_log("transient", gen=ev.gen, after=after, timeout=3)
    nxt = s.wait_log("send", after=after, timeout=5)
    s.wait_log(("accept", "apply_deferred"), gen=nxt.gen, after=after, timeout=5)
    s.pump(0.3)
    if "❯ echo PL_ONE" not in s.lines():
        raise Fail("old two-line prompt not collapsed to '❯ echo PL_ONE'")
    rows_with_sep = [l for l in s.lines() if SEP in l]
    if len(rows_with_sep) != 1:
        raise Fail(f"{len(rows_with_sep)} powerline rows on screen after Enter, expected 1 (the live prompt)")
    # old prompt 2 rows -> 1 row, +1 output row, +2 rows for the new prompt
    if nonblank(s) - n0 != 2:
        raise Fail(f"screen grew by {nonblank(s) - n0} non-blank rows, expected 2")
    ctx.note("2-row prompt, Enter -> 1 collapsed row + output + new 2-row prompt (+2 rows)")


@scenario("jobs_field", "REQ field 10 carries ${#jobstates}: sleep 100 & -> 1, after kill -> 0")
def sc_jobs(ctx, plugins):
    # Presentation identities are fully forwarded but are not module text in
    # the fixture RPROMPT. An oversized identity must not hide its jobs echo.
    terminal_identity = "fixture-terminal-" + "x" * 200
    s = ctx.session(env={"TILDY_FAKE_ECHO_REQ": "1", "TERM_PROGRAM": terminal_identity}, mode="persistent", cols=200)
    s.wait_log("accept", timeout=8)
    for r in trace_reqs(s):
        if len(r) != 10 or r[9] != "0":
            raise Fail(f"initial REQ should have 10 fields, jobs=0: {r!r}")
    ev = s.cycle("sleep 100 &")
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    s.pump(0.2)
    r = req_for(s, ev.gen)
    if len(r) != 10 or r[9] != "1":
        raise Fail(f"REQ after `sleep 100 &`: {len(r) - 1} fields, jobs={r[9] if len(r) > 9 else None}, expected 1")
    if "TERM_PROGRAM=" + terminal_identity not in env_pairs_of(r):
        raise Fail("presentation identity was truncated on the REQ wire")
    s.wait_screen(r"jobs=1\b", 3, "R line jobs=1")
    ev = s.cycle("kill %1; wait")
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    s.pump(0.2)
    r = req_for(s, ev.gen)
    if r[9] != "0":
        raise Fail(f"REQ after kill has jobs={r[9]}, expected 0")
    s.wait_screen(r"jobs=0\b", 3, "R line jobs=0")
    ctx.note("jobs 0 -> 1 (sleep 100 &) -> 0 (kill); backend echoed jobs=1/0")


@scenario("env_pairs", "REQ env is KEY=VALUE pairs joined by U+001F: set/unset VIRTUAL_ENV, escaping, missing keys omitted")
def sc_env_pairs(ctx, plugins):
    allowed = {"SSH_CONNECTION", "SSH_TTY", "USER", "HOST", "VIRTUAL_ENV", "CONDA_DEFAULT_ENV", "PATH",
               "DOCKER_CONTEXT", "DOCKER_HOST", "SHLVL", "IN_NIX_SHELL", "name", "DIRENV_FILE", "DIRENV_DIR", "TILDY_SUBMISSION_ID", "TILDY_FONT_REVISION", "TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "LC_ALL", "LC_CTYPE", "LANG"}
    s = ctx.session(env={"TILDY_FAKE_ECHO_REQ": "1"}, mode="persistent", cols=200)
    s.wait_log("accept", timeout=8)
    first = trace_reqs(s)[0]
    keys0 = [p.split("=", 1)[0] for p in env_pairs_of(first)]
    if any("VIRTUAL_ENV" == k for k in keys0):
        raise Fail(f"VIRTUAL_ENV in the first REQ env: {first[8]!r}")
    # Set, plus a backslash value and a value with U+001F (must be stripped).
    cmd = r"export VIRTUAL_ENV=/tmp/venvX CONDA_DEFAULT_ENV='a\b' DOCKER_HOST=$'tcp://h\x1f1'"
    ev = s.cycle(cmd)
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    s.pump(0.2)
    r = req_for(s, ev.gen)
    pairs = env_pairs_of(r)
    for p in pairs:
        if "=" not in p or p.split("=", 1)[0] not in allowed or (p.split("=", 1)[1] == "" and p.split("=",1)[0] != "TILDY_FONT_REVISION"):
            raise Fail(f"bad env pair {p!r} in {r[8]!r}")
    d = dict(p.split("=", 1) for p in pairs)
    submission = d.get("TILDY_SUBMISSION_ID", "")
    initial_submission = dict(p.split("=", 1) for p in env_pairs_of(first)).get("TILDY_SUBMISSION_ID", "")
    if not submission.isdigit() or not initial_submission.isdigit() or int(submission) <= int(initial_submission):
        raise Fail(f"submission counter did not advance across command execution: {initial_submission!r} -> {submission!r}")
    if d.get("VIRTUAL_ENV") != "/tmp/venvX":
        raise Fail(f"VIRTUAL_ENV missing from REQ env: {r[8]!r}")
    if d.get("CONDA_DEFAULT_ENV") != "a\\\\b":     # one backslash escaped to two on the wire
        raise Fail(f"CONDA_DEFAULT_ENV not escaped: {d.get('CONDA_DEFAULT_ENV')!r}")
    if d.get("DOCKER_HOST") != "tcp://h1":
        raise Fail(f"U+001F not stripped from DOCKER_HOST: {d.get('DOCKER_HOST')!r}")
    s.wait_screen(r"VIRTUAL_ENV=/tmp/venvX", 3, "backend echoed env")
    ev = s.cycle("unset VIRTUAL_ENV CONDA_DEFAULT_ENV DOCKER_HOST")
    s.wait_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx, timeout=5)
    s.pump(0.2)
    r = req_for(s, ev.gen)
    # the unset runs before this precmd, so this REQ already lacks the keys
    left = [p for p in env_pairs_of(r) if p.split("=", 1)[0] in ("VIRTUAL_ENV", "CONDA_DEFAULT_ENV", "DOCKER_HOST")]
    if left:
        raise Fail(f"unset keys still in REQ env: {left!r}")
    ctx.note(f"env keys {sorted(d)} -> after unset {[p.split('=')[0] for p in env_pairs_of(r)]}")


@scenario("nerd_width", "powerline prompt with Nerd (PUA) glyphs on the input row: cursor column = visible width + typed length")
def sc_nerd_width(ctx, plugins):
    s = ctx.session(env={"TILDY_FAKE_POWERLINE": "2"}, mode="persistent")
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    ev = pl_cycle(s)
    visible = " \uf007 u " + SEP + " \uf07c home0 " + SEP + " "
    x, y = s.cursor()
    if SEP not in s.row_text(y):
        raise Fail(f"powerline row not on the cursor row: {s.row_text(y).rstrip()!r}")
    if x != len(visible):
        raise Fail(f"cursor col {x} at the empty prompt, expected {len(visible)} ({s.row_text(y).rstrip()!r})")
    text = "abcdefghij"
    s.send_slow(text, 0.02)
    s.pump(0.2)
    if s.cursor() != (len(visible) + len(text), y):
        raise Fail(f"cursor {s.cursor()} after typing, expected ({len(visible) + len(text)}, {y})")
    s.send("\x1b[D" * 4)
    s.pump(0.2)
    if s.cursor()[0] != len(visible) + len(text) - 4:
        raise Fail(f"cursor col {s.cursor()[0]} after 4x Left")
    s.send("\x01")   # Ctrl-A: beginning of line
    s.pump(0.2)
    if s.cursor()[0] != len(visible):
        raise Fail(f"cursor col {s.cursor()[0]} after Ctrl-A, expected {len(visible)}")
    ctx.note(f"prompt width {len(visible)} cols matches the cursor; typing, Left and Ctrl-A land correctly")


@scenario("language_async",
          "real worker in a package.json repo: `ready` within 20 ms of Enter, a later fresh RES follows",
          backends=("real",))
def sc_language_async(ctx, plugins):
    git = shutil.which("git")
    if not git:
        raise Skip("git not found")
    repo = ctx.home0 / "proj"
    repo.mkdir()
    genv = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
    subprocess.run([git, "init", "-q", str(repo)], check=True, env=genv)
    (repo / "package.json").write_text('{"name":"proj","version":"1.0.0"}\n')
    (repo / "x.txt").write_text("x\n")
    s = ctx.session(env={"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"})
    s.wait_backend_prompt(timeout=8, what="first prompt")
    s.wait_worker_ready()
    best = None
    for i in range(3):
        after = s.log_count()
        t_enter = now_ms()
        s.send("cd proj\r" if i == 0 else "\r")
        ready = s.wait_log("ready", after=after, timeout=5)
        lat = ready.ts - t_enter
        best = lat if best is None else min(best, lat)
        gen = ready.gen
        fr = wait_fresh(s, gen, after)
        s.pump(0.3)
        if i == 0:
            row = s.row_text().rstrip()
            fresh_row = row
        if best <= 20:
            break
    if best > 20:
        raise Fail(f"`ready` came {best:.0f} ms after Enter (best of {i + 1}), expected <= 20 ms")
    rcv = [e for e in s.find_all("recv", gen=gen, after=after)]
    ctx.note(f"ready {best:.0f} ms after Enter; recv {[e.f[1] for e in rcv]}; fresh {fr.ts - ready.ts} ms after ready")
    if not re.search(r"v?\d+\.\d+(\.\d+)?", fresh_row):
        ctx.note(f"no node version in the fresh prompt (config types not ready?): {fresh_row!r}")
    else:
        ctx.note(f"fresh prompt row: {fresh_row!r}")


# --------------------------------------------------------------------
# Instant prompt (docs/decisions/0006-instant-prompt.md): `tildy init zsh
# --instant` at the top of .zshrc, then a slow init (TILDY_TEST_PRE_INIT).
# --------------------------------------------------------------------
PROMPT_ESC_RE = re.compile(r"%[FK]\{[^}]*\}|%\{[^%]*%\}|%[BbUuSsFfKk]")


def prompt_visible(line):
    """Visible text of a Zsh prompt line as the renderer writes it (escapes removed)."""
    return PROMPT_ESC_RE.sub("", line).replace("%%", "%")


def instant_files(ctx):
    d = ctx.tmp / "xdg" / "cache" / "tildy" / "instant"
    return sorted(d.glob("*.prompt")) if d.is_dir() else []


def instant_test_env(ctx, env=None):
    # Existing single-row scenarios explicitly select their fixture appearance.
    # The zero-config cache scenario below exercises the actual default.
    config = ctx.tmp / "instant-plain.json"
    if not config.exists():
        config.write_text('{"preset":"plain","add_newline":false}')
    return dict({"TILDY_CONFIG": str(config)}, **(env or {}))


def populate_instant(ctx, env=None, cwd=None):
    """One session with the snippet that runs a command, so the worker writes the
    instant cache for home0 (or cwd) at 100 columns. Returns (path, L lines)."""
    s = ctx.session("instant_seed", env=dict({"TILDY_TEST_INSTANT": "1"}, **instant_test_env(ctx, env)), cwd=cwd)
    s.wait_backend_prompt(timeout=8, what="seed prompt")
    s.wait_worker_ready()
    ev = s.worker_cycle()
    s.wait_log("recv", gen=ev.gen, after=ev.idx, timeout=6, where=lambda e: e.f[1:2] == ["fresh"])
    end = time.time() + T(3)
    while not instant_files(ctx):
        if time.time() > end:
            raise Fail("the worker wrote no instant cache file after a fresh result")
        s.pump(0.05)
    s.close()
    f = instant_files(ctx)[0]
    lines = f.read_text().splitlines()
    head = lines[0].split("\t")
    if head[:2] != ["tildy-instant", "1"]:
        raise Fail(f"bad cache header {lines[0]!r}")
    left = [l[2:] for l in lines[1:] if l.startswith("L\t")]
    if len(left) != int(head[2]):
        raise Fail(f"cache says {head[2]} L lines, has {len(left)}")
    return f, left


def zshrc_start_ms(ctx):
    raw = (ctx.tmp / "zshrc-start").read_text().split()
    return float(raw[-1]) * 1000


def nonempty_rows(s):
    return [(y, l) for y, l in enumerate(s.lines()) if l.strip()]


def instant_session(ctx, name, pre_init, env=None, cwd=None):
    e = {"TILDY_TEST_INSTANT": "1", "TILDY_TEST_PRE_INIT": pre_init, "TMPDIR": str(ctx.tmp)}
    e.update(instant_test_env(ctx, env))
    return ctx.session(name, env=e, cwd=cwd)


@scenario("instant_prompt_default_three_rows",
          "default blank line and two-row prompt are cached and shown before init, with no Enter needed",
          backends=("real",))
def sc_instant_default(ctx, plugins):
    env = {"TILDY_CONFIG": str(ctx.tmp / "missing-config.json")}
    _, left = populate_instant(ctx, env=env)
    if len(left) != 3 or left[0] != "":
        raise Fail(f"expected a three-row default cache: {left!r}")
    s = instant_session(ctx, "instant-default", "sleep .5", env=env)
    event = s.wait_log("instant", timeout=5)
    s.pump(.1)
    if event.f[:2] != ["3", "hit"] or s.find_log("instant_done"):
        raise Fail(f"default cache did not appear before init: {event.f}")
    if s.row_text(0).strip() or "home0" not in s.row_text(1):
        raise Fail(f"incorrect default instant layout: {s.screen_text()!r}")
    s.wait_log("instant_done", timeout=5)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    s.pump(.2)
    if s.cursor()[1] != 2 or not s.backend_prompt_ok() or s.row_text(0).strip():
        raise Fail(f"default live prompt did not replace cache in place: {s.screen_text()!r}")
    s.send("echo default_cache_ok\r")
    s.wait_line_equals("default_cache_ok", 5)
    ctx.note("three-row cache -> live prompt in cycle 1; typed command executed once")


@scenario("instant_prompt_shows_before_init",
          "cached prompt is on screen while a 500 ms init runs, then replaced in place by the real prompt",
          backends=("real",))
def sc_instant_shows(ctx, plugins):
    f, left = populate_instant(ctx)
    want = prompt_visible(left[-1]).rstrip()
    if "home0" not in want:
        raise Fail(f"cached prompt does not show the directory: {left!r}")
    t_spawn = now_ms()
    s = instant_session(ctx, "instant", "sleep 0.5")
    end = time.time() + T(5)
    seen = None
    while seen is None:
        rows = nonempty_rows(s)
        if rows and rows[-1][1].rstrip() == want:
            seen = now_ms()
            done_before = s.find_log("instant_done")
            rows_at_seen = rows
        if time.time() > end:
            raise Fail(f"cached prompt {want!r} never appeared; screen={s.screen_text()!r}")
        if seen is None:
            s.pump(0.005)
    if done_before:
        raise Fail("the cached prompt only appeared after init had finished")
    if len(rows_at_seen) != len(left):
        raise Fail(f"instant prompt took {len(rows_at_seen)} rows, cache has {len(left)} L lines")
    ev = s.wait_log("instant", timeout=3)
    if ev.f[1:2] != ["hit"]:
        raise Fail(f"instant event {ev.f}, expected a cache hit")
    shown_ms = ev.ts - zshrc_start_ms(ctx)
    if shown_ms > 100:
        raise Fail(f"instant prompt printed {shown_ms:.0f} ms after .zshrc started, expected < 100 ms")
    done = s.wait_log("instant_done", timeout=5)
    init_ms = done.ts - ev.ts
    if init_ms < 450:
        raise Fail(f"instant_done {init_ms} ms after the instant prompt; the 500 ms init was not waited for")
    if int(done.f[2]) != len(left) or int(ev.f[0]) != len(left):
        raise Fail(f"rows: instant={ev.f[0]} done={done.f[2]}, cache L lines={len(left)}")
    s.wait_log("ready", gen=1, timeout=5)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=6)
    s.pump(0.3)
    rows = nonempty_rows(s)
    if len(rows) != len(left) or rows[0][0] != 0:
        raise Fail(f"after init expected the {len(left)}-line prompt alone at the top, got {rows!r}")
    if s.cursor()[1] != len(left) - 1 or not s.backend_prompt_ok():
        raise Fail(f"real prompt not on row {len(left) - 1}: cursor={s.cursor()} row={s.row_text().rstrip()!r}")
    # Transient prompt on the first prompt still works.
    after = s.log_count()
    s.send("echo first_cmd_ok\r")
    s.wait_line_equals("first_cmd_ok", 5)
    tr = s.wait_log("transient", after=after, timeout=3)
    if tr.gen != 1:
        raise Fail(f"transient for gen {tr.gen}, expected 1")
    if not any(l.strip().startswith("❯ echo first_cmd_ok") for l in s.lines()):
        raise Fail(f"first prompt not collapsed to the transient form: {s.screen_text()!r}")
    ctx.note(f"visible {seen - t_spawn:.0f} ms after spawn (pexpect overhead incl.), "
             f"{shown_ms:.0f} ms after .zshrc start; init {init_ms} ms; {len(left)} row(s), none left over")


@scenario("instant_prompt_keeps_typed_input",
          "keys typed during a 500 ms init are not echoed, land in the line editor, and Enter runs them",
          backends=("real",))
def sc_instant_typed(ctx, plugins):
    f, left = populate_instant(ctx)
    want = prompt_visible(left[-1]).rstrip()
    s = instant_session(ctx, "instant", "sleep 0.5")
    s.wait_log("instant", timeout=5)
    s.wait_screen(re.escape(want), 3, "instant prompt")
    if s.find_log("instant_done"):
        raise Fail("init finished before typing started (too slow to test)")
    s.send("echo marker")
    s.pump(0.15)
    if "echo marker" in s.screen_text():
        raise Fail("typed text was echoed over the instant prompt during init")
    if s.find_log("instant_done"):
        raise Fail("init finished before the echo check")
    s.wait_log("instant_done", timeout=5)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=6)
    end = time.time() + T(3)
    # (The right prompt may show the status of .zshrc's last command.)
    while not re.search(r"❯ echo marker( |$)", s.row_text().rstrip()):
        if time.time() > end:
            raise Fail(f"typed text not in the line editor: row={s.row_text().rstrip()!r}")
        s.pump(0.03)
    x, y = s.cursor()
    if not s.row_text()[:x].endswith("echo marker"):
        raise Fail(f"cursor not after the typed text: x={x} row={s.row_text().rstrip()!r}")
    s.send("\r")
    s.wait_line_equals("marker", 5)
    if sum(1 for l in s.lines() if "echo marker" in l) != 1:
        raise Fail(f"typed command appears more than once: {s.screen_text()!r}")
    ctx.note("typed during init: not echoed, buffer 'echo marker', Enter printed 'marker'")


@scenario("instant_prompt_shows_init_output",
          "stdout/stderr written during init appear above the real prompt; fds are restored afterwards",
          backends=("real",))
def sc_instant_output(ctx, plugins):
    populate_instant(ctx)
    s = instant_session(ctx, "instant", "sleep 0.3; echo warning-from-plugin; print -u2 error-from-plugin")
    s.wait_log("instant", timeout=5)
    s.pump(0.15)
    if "warning-from-plugin" in s.screen_text():
        raise Fail("init output reached the screen during init (not captured)")
    done = s.wait_log("instant_done", timeout=5)
    if int(done.f[0]) < len("warning-from-plugin\nerror-from-plugin\n"):
        raise Fail(f"instant_done reports {done.f[0]} captured bytes")
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=6)
    s.pump(0.3)
    rows = [l for _, l in nonempty_rows(s)]
    if rows[:2] != ["warning-from-plugin", "error-from-plugin"] or len(rows) != 3 or not s.backend_prompt_ok():
        raise Fail(f"expected the two init lines then the prompt, got {rows!r}")
    s.send("echo out-after; print -u2 err-after; read -t 1 x <&0 || echo stdin-ok\r")
    s.wait_line_equals("out-after", 5)
    s.wait_line_equals("err-after", 5)
    left = [p for p in ctx.tmp.glob("tildy-instant.*")]
    if left:
        raise Fail(f"capture file left behind: {left}")
    ctx.note("init stdout+stderr shown above the prompt; stdout/stderr work afterwards; capture file removed")


@scenario("instant_prompt_exit_during_init",
          ".zshrc exits the shell during init: the zshexit hook erases the instant prompt and prints the captured output",
          backends=("real",))
def sc_instant_exit(ctx, plugins):
    populate_instant(ctx)
    s = instant_session(ctx, "instant", "sleep 0.3; echo output-before-exit; exit 3")
    s.wait_log("instant", timeout=5)
    done = s.wait_log("instant_done", timeout=5)
    end = time.time() + T(5)
    while not s.eof:
        if time.time() > end:
            raise Fail("shell did not exit")
        s.pump(0.05)
    rows = [l for _, l in nonempty_rows(s)]
    if done.f[1:2] != ["exit"] or rows != ["output-before-exit"]:
        raise Fail(f"instant_done {done.f}, screen rows {rows!r}; expected the output alone (prompt erased)")
    ctx.note("exit during init: instant prompt erased by the zshexit hook, captured output printed")


@scenario("instant_prompt_off", "TILDY_INSTANT_PROMPT=off: nothing is printed before init ends",
          backends=("real",))
def sc_instant_off(ctx, plugins):
    populate_instant(ctx)
    s = instant_session(ctx, "instant", "sleep 0.5", env={"TILDY_INSTANT_PROMPT": "off"})
    s.pump(0.4)
    if s.screen_text().strip():
        raise Fail(f"something was printed during init: {s.screen_text()!r}")
    s.wait_log("ready", gen=1, timeout=6)
    s.wait_backend_prompt(timeout=6, what="prompt after init")
    if s.find_log(("instant", "instant_done")):
        raise Fail("instant events logged although TILDY_INSTANT_PROMPT=off")
    s.send("echo off_ok\r")
    s.wait_line_equals("off_ok", 5)
    ctx.note("no output during the 500 ms init; normal prompt afterwards")


@scenario("instant_prompt_two_lines",
          "two-line prompt: a stale one-line cache is replaced by the base prompt; the two-line cache is "
          "shown on two rows and replaced in place", backends=("real",))
def sc_instant_two_lines(ctx, plugins):
    cfgf = ctx.tmp / "powerline.json"
    cfgf.write_text('{"preset":"pastel-powerline","add_newline":false}')
    env = {k: v for k, v in os.environ.items() if not k.startswith(("TILDY_", "XDG_"))}
    env.update(TILDY_CONFIG=str(cfgf), XDG_CONFIG_HOME=str(ctx.tmp), XDG_STATE_HOME=str(ctx.tmp), XDG_CACHE_HOME=str(ctx.tmp))
    r = subprocess.run([str(ctx.tildy_bin), "render"], input="REQ\t1\ts\t/tmp\t100\t30\t0\t0\t\t0\n",
                       capture_output=True, text=True, timeout=10, env=env)
    if r.stdout.count("\nL\t") != 2:
        raise Skip("pastel-powerline preset does not render two L lines with this binary")
    # (a) one-line cache from the explicit plain fixture.
    f, left = populate_instant(ctx)
    if len(left) != 1:
        raise Fail(f"plain fixture cached {len(left)} lines")
    # (b) the config is now two lines: replace the stale cache on the first result.
    s = instant_session(ctx, "instant_stale", "sleep 0.3", env={"TILDY_CONFIG": str(cfgf)})
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    s.pump(0.2)
    if len(nonempty_rows(s)) != 2 or s.cursor()[1] != 1:
        raise Fail(f"stale instant prompt not replaced by the two-line theme: {s.screen_text()!r}")
    ev = s.worker_cycle()
    s.wait_log("recv", gen=ev.gen, after=ev.idx, timeout=6, where=lambda e: e.f[1:2] == ["fresh"])
    end = time.time() + T(3)
    while f.read_text().count("\nL\t") != 2:
        if time.time() > end:
            raise Fail(f"cache not updated to two lines: {f.read_text()!r}")
        s.pump(0.05)
    s.close()
    left = [l[2:] for l in f.read_text().splitlines() if l.startswith("L\t")]
    # (c) two-line instant prompt, replaced in place.
    s = instant_session(ctx, "instant_two", "sleep 0.5", env={"TILDY_CONFIG": str(cfgf)})
    ev = s.wait_log("instant", timeout=5)
    s.pump(0.1)
    rows = nonempty_rows(s)
    if ev.f[:2] != ["2", "hit"] or [y for y, _ in rows] != [0, 1] or "home0" not in rows[0][1]:
        raise Fail(f"two-line instant prompt: event={ev.f} rows={rows!r}")
    if s.find_log("instant_done"):
        raise Fail("init finished before the two-line instant prompt was checked")
    done = s.wait_log("instant_done", timeout=5)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=6)
    s.pump(0.3)
    rows = nonempty_rows(s)
    if done.f[2:3] != ["2"] or [y for y, _ in rows] != [0, 1] or s.cursor()[1] != 1:
        raise Fail(f"after init: done={done.f} rows={rows!r} cursor={s.cursor()}")
    if s.find_log("discard", gen=1, where=lambda e: e.f[1:2] == ["lines_mismatch"]):
        raise Fail("first result discarded as lines_mismatch although the cache had its line count")
    ctx.note("stale 1-line cache -> 2-line theme; 2-line cache shown on rows 0-1, replaced in place, "
             "first result accepted (no lines_mismatch)")


@scenario("instant_prompt_miss_uses_sibling_height",
          "new directory (cache miss): the base prompt is padded to the height of another directory's cache, "
          "so the first two-line result is drawn at once instead of waiting for Enter", backends=("real",))
def sc_instant_miss_height(ctx, plugins):
    cfgf = ctx.tmp / "powerline-miss.json"
    cfgf.write_text('{"preset":"pastel-powerline","add_newline":false}')
    env = {k: v for k, v in os.environ.items() if not k.startswith(("TILDY_", "XDG_"))}
    env.update(TILDY_CONFIG=str(cfgf), XDG_CONFIG_HOME=str(ctx.tmp), XDG_STATE_HOME=str(ctx.tmp), XDG_CACHE_HOME=str(ctx.tmp))
    r = subprocess.run([str(ctx.tildy_bin), "render"], input="REQ\t1\ts\t/tmp\t100\t30\t0\t0\t\t0\n",
                       capture_output=True, text=True, timeout=10, env=env)
    if r.stdout.count("\nL\t") != 2:
        raise Skip("pastel-powerline preset does not render two L lines with this binary")
    # Seed a two-line cache for home0; even a cold start draws without Enter.
    s = ctx.session("instant_seed", env={"TILDY_TEST_INSTANT": "1", "TILDY_CONFIG": str(cfgf)})
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    after = s.log_count()
    s.send("\r")
    ev = s.wait_log("send", after=after, timeout=5)
    s.wait_log("recv", gen=ev.gen, after=after, timeout=6, where=lambda e: e.f[1:2] == ["fresh"])
    end = time.time() + T(3)
    while not instant_files(ctx):
        if time.time() > end:
            raise Fail("the worker wrote no instant cache file after a fresh result")
        s.pump(0.05)
    s.close()
    f = instant_files(ctx)[0]
    left = [l[2:] for l in f.read_text().splitlines() if l.startswith("L\t")]
    if len(left) != 2:
        raise Fail(f"seed cached {len(left)} lines, expected 2")
    # A directory with no cache of its own.
    other = ctx.tmp / "elsewhere"
    other.mkdir(exist_ok=True)
    s = instant_session(ctx, "instant_miss", "sleep 0.3", env={"TILDY_CONFIG": str(cfgf)}, cwd=str(other))
    ev = s.wait_log("instant", timeout=5)
    if ev.f[:3] != ["2", "miss", "2"]:
        raise Fail(f"instant event {ev.f}, expected rows=2 miss hint=2")
    s.wait_log("instant_done", timeout=5)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=6)
    s.pump(0.3)
    if s.find_log("discard", gen=1, where=lambda e: e.f[1:2] == ["lines_mismatch"]):
        raise Fail("first result discarded as lines_mismatch although the sibling cache gave the height")
    rows = nonempty_rows(s)
    if [y for y, _ in rows] != [0, 1] or "elsewhere" not in rows[0][1] or s.cursor()[1] != 1:
        raise Fail(f"two-line prompt not drawn in the first cycle: rows={rows!r} cursor={s.cursor()}")
    ctx.note("miss with a 2-line sibling cache: base prompt padded to 2 rows, first result accepted in cycle 1")


@scenario("instant_prompt_subst_injection",
          "prompt_subst on, directory named x$(touch PWNED): the cached prompt is printed and reused as the "
          "first base prompt without command substitution", backends=("real",))
def sc_instant_injection(ctx, plugins):
    evil = ctx.home0 / "x$(touch PWNED1)y`touch PWNED2`"
    evil.mkdir()
    env = {"TILDY_TEST_POST_INIT": "setopt prompt_subst prompt_bang"}
    f, left = populate_instant(ctx, env=env, cwd=evil)
    if "$(touch PWNED1)" not in left[-1]:
        raise Fail(f"cached prompt does not carry the directory name verbatim: {left!r}")
    s = instant_session(ctx, "instant", "sleep 0.3", env=env, cwd=evil)
    s.wait_log("instant", timeout=5)
    s.wait_screen(re.escape("x$(touch PWNED1)y`touch PWNED2`"), 3, "directory name in the instant prompt")
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    s.pump(0.3)
    if "x$(touch PWNED1)" not in s.row_text():
        raise Fail(f"real prompt does not show the name verbatim: {s.row_text().rstrip()!r}")
    s.cycle()
    s.pump(0.3)
    hits = [p.name for p in evil.iterdir()] + [p.name for p in ctx.home0.iterdir() if p.name.startswith("PWNED")]
    if any(h.startswith("PWNED") for h in hits):
        raise Fail(f"command substitution ran: {hits}")
    ctx.note("cached prompt with $(...) and backticks shown verbatim under prompt_subst; nothing executed")


# --------------------------------------------------------------------
# Agent status (docs/agent-status.md). The builtin config shows the
# highlighted record on the right; the worker pushes changes of the store
# directories it watches (session, worktree, global) and applies the OSC 7501
# prompt rule; known agent programs run through `tildy agent wrap`.
# --------------------------------------------------------------------
GIT_ENV = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}


def agent_cli(ctx, *args, cwd=None):
    """`tildy agent ... --json` against the scenario's state directory."""
    env = {k: v for k, v in os.environ.items() if not k.startswith(STRIP_PREFIXES)}
    env.update(GIT_ENV, XDG_STATE_HOME=str(ctx.tmp / "xdg" / "state"))
    out = subprocess.run([str(ctx.tildy_bin), "agent", *args, "--json"], capture_output=True, text=True,
                         env=env, cwd=str(cwd or ctx.home0))
    try:
        doc = json.loads(out.stdout)
    except ValueError:
        raise Fail(f"tildy agent {' '.join(args)}: no JSON (exit {out.returncode}): {out.stderr.strip()}")
    if not doc.get("ok"):
        raise Fail(f"tildy agent {' '.join(args)}: {doc.get('error')}")
    return doc["data"]


def wait_row(s, pred, timeout, what):
    """Wait until pred(cursor row); return the time it became true."""
    end = time.time() + T(timeout)
    while True:
        row = s.row_text()
        if pred(row):
            return time.time()
        if time.time() > end:
            raise Fail(f"timeout ({timeout:.1f}s) waiting for {what}: {row.rstrip()!r}")
        if s.eof:
            raise Fail(f"zsh exited while waiting for {what}")
        s.pump(0.03)


def stays(s, pred, secs, what):
    """pred(cursor row) holds for secs."""
    end = time.time() + secs
    while time.time() < end:
        if not pred(s.row_text()):
            raise Fail(f"{what}: {s.row_text().rstrip()!r}")
        s.pump(0.05)


AGENT_OPT_IN_CONFIG = (
    '{"schema_version":1,"preset":"plain","add_newline":false,"modules":{"agent":{"type":"agent_status"}},'
    '"layout":{"rows":[{"left":["cwd","git","ssh_host","docker","c","go","java","node","py","rust","jobs"],'
    '"right":["agent","elapsed","result"]}]},"agent":{"wrap":true}}\n'
)


def agent_session(ctx, cwd=None, env=None):
    # The agent_status module and auto-wrap are opt-in (author decision
    # 2026-10-20): these scenarios enable both through a config file.
    cfgf = ctx.tmp / "agent-config.json"
    if not cfgf.exists():
        cfgf.write_text(AGENT_OPT_IN_CONFIG)
    validation = subprocess.run([str(ctx.tildy_bin), "config", "validate", "--json"],
                                env=dict(os.environ, TILDY_CONFIG=str(cfgf)), capture_output=True, text=True)
    if validation.returncode:
        raise Fail(f"agent fixture config is invalid: {validation.stdout} {validation.stderr}")
    s = ctx.session(env=dict(GIT_ENV, TILDY_CONFIG=str(cfgf), **(env or {})), cwd=cwd)
    s.wait_backend_prompt(timeout=8, what="first prompt")
    s.wait_worker_ready()
    ev = s.worker_cycle()
    wait_fresh(s, ev.gen, ev.idx)
    s.pump(0.2)
    return s


def git_repo(path):
    subprocess.run(["git", "init", "-q", str(path)], check=True, env=dict(os.environ, **GIT_ENV))
    return path


@scenario("agent_status_push",
          "a record published elsewhere shows in the idle prompt within 1.5 s, typed input intact, "
          "and leaves within 1.5 s of its TTL", backends=("real",))
def sc_agent_push(ctx, plugins):
    s = agent_session(ctx)
    s.send_slow("echo kept_line", 0.01)
    s.pump(0.2)
    t0 = time.time()
    agent_cli(ctx, "publish", "--scope", "global", "--state", "blocked", "--kind", "permission", "--app", "claude",
              "--msg", "Bash: rm -rf target", "--ttl", "3")
    t_show = wait_row(s, lambda r: "Bash: rm -rf target" in r, 1.5, "the published record in the prompt")
    row = s.row_text()
    if "echo kept_line" not in row:
        raise Fail(f"typed input lost by the push: {row.rstrip()!r}")
    gone_by = 3 + 1.5 - (time.time() - t0)
    t_gone = wait_row(s, lambda r: "rm -rf target" not in r, gone_by, "the record to expire")
    if t_gone - t0 < 2.5:
        raise Fail(f"record left after {t_gone - t0:.1f}s, before its 3 s TTL")
    if "echo kept_line" not in s.row_text():
        raise Fail(f"typed input lost at expiry: {s.row_text().rstrip()!r}")
    s.send("\r")
    s.wait_line_equals("kept_line", 5)
    ctx.note(f"shown {t_show - t0:.2f}s after publish; gone {t_gone - t0:.2f}s after publish (TTL 3 s)")


@scenario("agent_status_scopes",
          "a record of another worktree or another session never shows; the shell's own worktree does",
          backends=("real",))
def sc_agent_scopes(ctx, plugins):
    if not shutil.which("git"):
        raise Skip("git not found")
    a = git_repo(ctx.home0 / "repoA")
    b = git_repo(ctx.home0 / "repoB")
    s = agent_session(ctx, cwd=a)
    agent_cli(ctx, "publish", "--scope", "worktree", "--cwd", str(b), "--state", "blocked", "--app", "codex",
              "--msg", "other-worktree")
    agent_cli(ctx, "publish", "--scope", "session", "--scope-id", "1.2", "--state", "error", "--app", "codex",
              "--msg", "other-session")
    stays(s, lambda r: "other-" not in r, 2.0, "a record of another worktree/session showed")
    t0 = time.time()
    agent_cli(ctx, "publish", "--scope", "worktree", "--cwd", str(a / "."), "--state", "working", "--app", "codex",
              "--msg", "this-worktree")
    t = wait_row(s, lambda r: "this-worktree" in r, 1.5, "the record of the shell's worktree")
    s.cycle("cd ../repoB")
    s.pump(1.0)
    row = s.row_text()
    if "this-worktree" in row or "other-session" in row or "other-worktree" not in row:
        raise Fail(f"after cd into repoB the prompt shows the wrong records: {row.rstrip()!r}")
    ctx.note(f"own worktree shown {t - t0:.2f}s after publish; repoB shows only its own record")


@scenario("agent_status_prompt_rule",
          "same session: a working record goes at the next prompt; a done record shows once, then goes",
          backends=("real",))
def sc_agent_prompt_rule(ctx, plugins):
    s = agent_session(ctx)
    s.send("echo SID=$TILDY_SESSION_ID\r")
    m = s.wait_screen(r"^SID=(\d+\.\d+)$", 5, "TILDY_SESSION_ID output")
    sid = m.group(1)
    s.pump(0.3)
    agent_cli(ctx, "publish", "--scope", "global", "--state", "working", "--app", "codex", "--msg", "W-rec",
              "--origin", sid)
    wait_row(s, lambda r: "W-rec" in r, 1.5, "the working record")
    ev = s.cycle()
    wait_fresh(s, ev.gen, ev.idx)
    s.pump(0.3)
    if "W-rec" in s.row_text():
        raise Fail(f"working record still shown after the session's next prompt: {s.row_text().rstrip()!r}")
    agent_cli(ctx, "publish", "--scope", "global", "--state", "done", "--app", "codex", "--msg", "D-rec",
              "--origin", sid)
    wait_row(s, lambda r: "D-rec" in r, 1.5, "the done record")
    ev = s.cycle()
    wait_fresh(s, ev.gen, ev.idx)
    s.pump(0.3)
    if "D-rec" not in s.row_text():
        raise Fail(f"done record not shown at the first prompt after it: {s.row_text().rstrip()!r}")
    ev = s.cycle()
    wait_fresh(s, ev.gen, ev.idx)
    s.pump(0.3)
    if "D-rec" in s.row_text():
        raise Fail(f"done record still shown at the second prompt: {s.row_text().rstrip()!r}")
    left = agent_cli(ctx, "list", "--all")["records"]
    if left:
        raise Fail(f"records left in the store: {[r.get('msg') for r in left]}")
    ctx.note(f"session {sid}: working dropped at the next prompt, done shown once")


@scenario("agent_wrap_known_program",
          "a known agent program on PATH runs through `tildy agent wrap` (zero config): its OSC 7501 report "
          "reaches the store, exit status is kept; user aliases win; TILDY_AGENT_WRAP=0 runs it directly",
          backends=("real",))
def sc_agent_wrap(ctx, plugins):
    stub = ctx.tmp / "agentbin"
    stub.mkdir()
    write_exec(stub / "claude",
               "#!/bin/sh\n"
               "printf '\\033]7501;state=blocked:kind=permission:msg=%s\\033\\\\' \"$(printf 'Allow Bash?' | base64)\"\n"
               "echo fake_claude_running\n"
               "sleep 1.5\n"
               "exit 3\n")
    write_exec(stub / "codex", "#!/bin/sh\necho real_codex\n")
    env = {"PATH": f"{stub}:{os.environ.get('PATH', '/usr/bin:/bin')}",
           "TILDY_TEST_PRE_INIT": "alias codex='echo user_alias_codex'"}
    s = agent_session(ctx, env=env)
    s.send("whence -w claude codex\r")
    s.wait_line_equals("claude: function", 5)
    s.wait_line_equals("codex: alias", 5)
    s.pump(0.2)
    s.send("claude --flag; echo rc=$?\r")
    s.wait_line_equals("fake_claude_running", 5)
    s.pump(0.3)
    recs = agent_cli(ctx, "list", "--all")["records"]
    blocked = [r for r in recs if r.get("state") == "blocked" and r.get("msg") == "Allow Bash?"]
    if not blocked:
        raise Fail(f"no blocked record from the wrapped program: {recs}")
    r = blocked[0]
    if r.get("source") != "osc" or not str(r.get("id", "")).startswith("wrap/claude/"):
        raise Fail(f"unexpected record: {r}")
    s.wait_line_equals("rc=3", 6)
    s.pump(0.5)
    # Directly: no proxy, no record.
    agent_cli(ctx, "clear", "--all")
    s.send("TILDY_AGENT_WRAP=0 claude; echo rc2=$?\r")
    s.wait_line_equals("rc2=3", 6)
    left = agent_cli(ctx, "list", "--all")["records"]
    if left:
        raise Fail(f"TILDY_AGENT_WRAP=0 still published: {left}")
    ctx.note(f"wrapped record {r['id']} ({r['state']}, {r.get('kind')}); exit status kept; alias not shadowed")


# --------------------------------------------------------------------
# Resize: debounce, fitting the shown prompt, reflow cleanup
# (docs/decisions/0002-safe-redraw.md "缩放与终端重排")
# --------------------------------------------------------------------
WIDE = "WIDEEND"


def wide_session(ctx, cols=100, delay="0", reflow=False, extra=None):
    """Fake two-line powerline prompt whose first row is min(90, cols) wide
    (TILDY_FAKE_WIDE), with one command's output above it. reflow: the screen
    reflows on width changes and the shell is told so (TILDY_REFLOW=1)."""
    env = {"TILDY_FAKE_POWERLINE": "1", "TILDY_FAKE_WIDE": "90", "TILDY_FAKE_DELAY_MS": delay,
           "TILDY_REFLOW": "1" if reflow else "0"}
    env.update(extra or {})
    s = ctx.session(env=env, mode="persistent", cols=cols, rows=30, reflow=reflow)
    s.wait_log(("accept", "apply_deferred"), gen=1, timeout=8)
    # Establish collapsed history before the output every resize must preserve.
    pl_cycle(s)
    ev = pl_cycle(s, "echo ABOVE_1")
    s.wait_line_equals("ABOVE_1", 5)
    return s, ev


def prompt_block(s, typed):
    """Check the screen around the cursor: cursor row is '❯ <typed>' with the
    cursor after it, the row above is the powerline row, the row above that is
    the command output ABOVE_1, and no other row holds a piece of a prompt row.
    Returns the powerline row (right-stripped)."""
    x, y = s.cursor()
    row = s.row_text(y)
    if row[:x].rstrip() != ("❯ " + typed).rstrip() or x != len("❯ " + typed):
        raise Fail(f"cursor row {row.rstrip()!r}, cursor x={x}: typed input not intact")
    top = s.row_text(y - 1).rstrip()
    if not top.startswith("[g"):
        raise Fail(f"row above the cursor is not the prompt row: {top!r}")
    if len(top) > s.cols:
        raise Fail(f"prompt row wider than {s.cols}: {top!r}")
    if s.row_text(y - 2).rstrip() != "ABOVE_1":
        raise Fail(f"rows above the prompt changed: {s.row_text(y - 2).rstrip()!r} directly above it, expected ABOVE_1")
    frags = [i for i, l in enumerate(s.lines()) if i not in (y - 1, y) and ("[g" in l or "---" in l or WIDE in l)]
    if frags:
        raise Fail(f"leftover prompt fragments on rows {frags}: {[s.lines()[i] for i in frags]!r}")
    return top


def fit_events(s, after):
    return s.find_all("resize_fit", after=after)


@scenario("resize_narrow_fits",
          "narrower than the prompt row: one fitted redraw at once, typed input kept, one debounced request, fresh result fits")
def sc_resize_narrow_fits(ctx, plugins):
    s, _ = wide_session(ctx, delay="800")
    s.send_slow("echo keepme", 0.01)
    s.pump(0.2)
    before = prompt_block(s, "echo keepme")
    if WIDE not in before or len(before) != 90:
        raise Fail(f"setup: expected a 90-column prompt row, got {before!r}")
    after = s.log_count()
    g0 = s.last_applied().gen
    t0 = now_ms()
    s.resize(30, 50)
    s.pump(0.35)
    fits = fit_events(s, after)
    if len(fits) != 1 or fits[0].f[1:5] != ["100", "50", "0", "1"]:
        raise Fail(f"expected one resize_fit 100->50 g=0 fitted=1, got {[e.raw for e in fits]}")
    interim = prompt_block(s, "echo keepme")
    if WIDE in interim or not interim.startswith(f"[g{g0} ") or len(interim) != 49:
        raise Fail(f"interim prompt should be g{g0}'s row cut to 49 columns: {interim!r}")
    sends = s.find_all("send", after=after)
    if len(sends) != 1:
        raise Fail(f"{len(sends)} requests after one resize, expected 1")
    if sends[0].ts - t0 < 50:
        raise Fail(f"request sent {sends[0].ts - t0:.0f} ms after the resize: not debounced")
    acc = s.wait_log(("accept", "apply_deferred"), gen=sends[0].gen, after=after, timeout=4)
    s.pump(0.2)
    fresh = prompt_block(s, "echo keepme")
    if not fresh.startswith(f"[g{sends[0].gen} ") or WIDE not in fresh or len(fresh) != 50:
        raise Fail(f"fresh result for 50 columns not shown as one row: {fresh!r}")
    if s.find_log("discard", gen=sends[0].gen, after=after):
        raise Fail("the result for the new width was discarded")
    s.send("\r")
    s.wait_line_equals("keepme", 5)
    ctx.note(f"100->50: fitted at once (49 cols), request {sends[0].ts - t0:.0f} ms later, "
             f"g{acc.gen} drawn {acc.ts - t0:.0f} ms after the resize; typed input kept")


@scenario("resize_burst_debounced",
          "a burst of SIGWINCH: one generation bump, the in-flight result discarded stale, one request after the burst")
def sc_resize_burst(ctx, plugins):
    s, _ = wide_session(ctx, delay="500")
    s.pump(0.3)
    after = s.log_count()
    # One resize puts a request in flight (debounce, then a 500 ms backend)...
    s.resize(30, 99)
    first = s.wait_log("send", after=after, timeout=3)
    s.pump(0.05)
    # ...then a fast drag down to 60 columns, 15 ms apart.
    burst = s.log_count()
    t_first = now_ms()
    for cols in range(96, 59, -4):
        s.resize(30, cols)
        s.pump(0.015)
    t_last = now_ms()
    acc = s.wait_log(("accept", "apply_deferred"), after=burst, timeout=4, where=lambda e: e.gen and e.gen > first.gen)
    s.pump(0.3)
    stale = s.find_log("discard", gen=first.gen, after=after)
    if not stale or stale.f[1:2] != ["stale"]:
        raise Fail(f"in-flight g{first.gen} not discarded stale: {stale.raw if stale else None}")
    if s.find_log(("accept", "apply_deferred"), gen=first.gen, after=after):
        raise Fail(f"g{first.gen} (for 99 columns) was drawn")
    sends = s.find_all("send", after=burst)
    if len(sends) != 1:
        raise Fail(f"{len(sends)} requests for one burst of 10 resizes, expected 1: {[e.raw for e in sends]}")
    if sends[0].gen != first.gen + 1:
        raise Fail(f"burst request is g{sends[0].gen}, expected g{first.gen + 1} (one bump per burst)")
    if sends[0].ts < t_last + 40:
        raise Fail(f"request sent {t_last - sends[0].ts:.0f} ms before the burst was over")
    fresh = prompt_block(s, "")
    if not fresh.startswith(f"[g{acc.gen} ") or len(fresh) != 60 or WIDE not in fresh:
        raise Fail(f"final prompt row for 60 columns: {fresh!r}")
    fits = fit_events(s, burst)
    ctx.note(f"10 resizes in {t_last - t_first:.0f} ms: g{first.gen} discarded stale, one request "
             f"g{sends[0].gen} {sends[0].ts - t_last:.0f} ms after the last resize, {len(fits)} fitted redraws")


def reflow_check(ctx, s, steps, typed="echo keepme"):
    """Resize through `steps` (pumping in between) and check the screen after each."""
    for cols in steps:
        after = s.log_count()
        s.resize(30, cols)
        s.pump(0.3)
        prompt_block(s, typed)
        fits = fit_events(s, after)
        acc = s.wait_log(("accept", "apply_deferred"), after=after, timeout=4)
        s.pump(0.2)
        row = prompt_block(s, typed)
        ctx.note(f"->{cols}: {', '.join('g=' + e.f[3] for e in fits) or 'no fix-up'}; g{acc.gen} row {len(row)} cols")


@scenario("resize_reflow_no_fragments",
          "reflowing screen (model): narrowing leaves no pieces of the old prompt, widening erases nothing above it")
def sc_resize_reflow(ctx, plugins):
    s, _ = wide_session(ctx, delay="300", reflow=True)
    s.send_slow("echo keepme", 0.01)
    s.pump(0.2)
    prompt_block(s, "echo keepme")
    above = s.row_text(s.cursor()[1] - 3).rstrip()
    if above != "❯ echo ABOVE_1":
        raise Fail(f"setup: expected the collapsed '❯ echo ABOVE_1' three rows up, got {above!r}")
    after = s.log_count()
    # 90 columns at 40: the old row reflows into 3 rows; zsh's own redraw only
    # goes up 1, the fix-up moves up the other 2 (g=2).
    reflow_check(ctx, s, [40])
    fits = fit_events(s, after)
    if not fits or fits[0].f[3] != "2":
        raise Fail(f"expected a fix-up of 2 rows for 100->40, got {[e.raw for e in fits]}")
    reflow_check(ctx, s, [70, 55, 100])
    y = s.cursor()[1]
    if s.row_text(y - 3).rstrip() != "❯ echo ABOVE_1":
        raise Fail(f"output above the prompt was erased: {s.lines()[:y + 1]!r}")


@scenario("resize_reflow_without_fixup_leaves_fragments",
          "control: the same reflowing screen with TILDY_REFLOW=0 shows the bug the fix-up removes")
def sc_resize_reflow_control(ctx, plugins):
    s, _ = wide_session(ctx, delay="300", reflow=True, extra={"TILDY_REFLOW": "0"})
    s.send_slow("echo keepme", 0.01)
    s.pump(0.2)
    after = s.log_count()
    s.resize(30, 40)
    s.pump(0.3)
    try:
        prompt_block(s, "echo keepme")
    except Fail as e:
        ctx.note(f"without the fix-up: {e}")
        return
    raise Fail("the model screen shows no fragments even without the fix-up; the reflow scenario proves nothing")


# A stand-in for Ghostty's zsh integration (cmux loads it): a precmd hook that
# runs after tildy's and wraps PS1 in OSC 133 marks, like _ghostty_precmd.
FAKE_OSC133_HOOK = r"""
_fake_osc133_precmd() {
  emulate -L zsh
  local nl=$'\n' p=$'%{\e]133;P;k=s\a%}'
  PS1=$'%{\e]133;A;cl=line\a%}'${PS1//$nl/$nl$p}$'%{\e]133;B\a%}'
}
precmd_functions+=(_fake_osc133_precmd)
"""


@scenario("semantic_marks_kept",
          "OSC 133 marks a shell integration put around PS1 survive tildy's async redraw, the resize fit and the transient prompt")
def sc_semantic_marks(ctx, plugins):
    s, _ = wide_session(ctx, extra={"TILDY_TEST_POST_INIT": FAKE_OSC133_HOOK})
    A, P, B = b"\x1b]133;A;cl=line\x07", b"\x1b]133;P;k=s\x07", b"\x1b]133;B\x07"
    s.pump(0.3)
    # The last redraw of the prompt (the async result after `echo ABOVE_1`)
    # carries all three marks.
    tail = bytes(s.raw[s.raw.rfind(b"\x1b[J"):])
    for name, m in (("A", A), ("P;k=s", P), ("B", B)):
        if m not in tail:
            raise Fail(f"OSC 133 {name} missing from the redraw of the async result: {tail[:200]!r}")
    prompt_block(s, "")
    # The fitted prompt after a narrower resize keeps them too.
    n = len(s.raw)
    s.resize(30, 50)
    s.pump(0.3)
    if not s.find_log("resize_fit"):
        raise Fail("no resize_fit")
    seg = bytes(s.raw[n:])
    if seg.count(A) < 2 or P not in seg or B not in seg:
        raise Fail(f"marks lost on the resize redraws: A x{seg.count(A)}, P {P in seg}, B {B in seg}")
    prompt_block(s, "")
    # And the collapsed (transient) prompt.
    n = len(s.raw)
    s.send("echo MARKED\r")
    s.wait_line_equals("MARKED", 5)
    seg = bytes(s.raw[n:])
    if A not in seg or B not in seg:
        raise Fail(f"transient redraw without marks: {seg[:200]!r}")
    ctx.note("A/P/B marks present in the async redraw, the fitted redraw and the transient prompt")


@scenario("resize_narrow_real",
          "real binary, pastel-powerline in a deep repo: 120 -> 50 -> 36 columns on a reflowing screen; rows fit, no fragments",
          backends=("real",))
def sc_resize_narrow_real(ctx, plugins):
    git = shutil.which("git")
    if not git:
        raise Skip("git not found")
    repo = ctx.home0 / "code" / "work" / "ai-notes"
    deep = repo / "services" / "api"
    deep.mkdir(parents=True)
    subprocess.run([git, "init", "-q", "-b", "feature/resize-redraw", str(repo)], check=True, env=dict(os.environ, **GIT_ENV))
    (deep / "x.txt").write_text("x\n")
    cfgf = ctx.tmp / "pastel.json"
    cfgf.write_text('{"schema_version":1,"preset":"pastel-powerline","add_newline":true}')
    s = ctx.session(env=dict(GIT_ENV, TILDY_CONFIG=str(cfgf), TILDY_REFLOW="1"), cwd=deep, cols=120, reflow=True)
    s.wait_worker_ready()
    for _ in range(2):   # the first cycle settles the two-line height
        ev = s.cycle("echo ABOVE_1")
        wait_fresh(s, ev.gen, ev.idx)
    s.pump(0.3)

    def check(cols):
        x, y = s.cursor()
        top = s.row_text(y - 1).rstrip()
        if s.row_text(y)[:x].rstrip() != "❯":
            raise Fail(f"{cols} cols: cursor row {s.row_text(y).rstrip()!r}")
        if SEP not in top or len(top) > cols:
            raise Fail(f"{cols} cols: powerline row {len(top)} wide: {top!r}")
        if s.row_text(y - 2).strip():
            raise Fail(f"{cols} cols: the blank line above the prompt was lost: {s.lines()[:y + 1]!r}")
        if s.row_text(y - 3).rstrip() != "ABOVE_1":
            raise Fail(f"{cols} cols: {s.row_text(y - 3).rstrip()!r} above the blank line, expected ABOVE_1; screen {s.lines()[:y + 1]!r}")
        if sum(1 for l in s.lines() if SEP in l) != 1:
            raise Fail(f"{cols} cols: more than one powerline row on screen: {s.lines()[:y + 1]!r}")
        return top

    wide = check(120)
    for cols in (50, 36):
        after = s.log_count()
        s.resize(30, cols)
        s.pump(0.3)
        check(cols)
        sent = s.wait_log("send", after=after, timeout=3)
        wait_fresh(s, sent.gen, after)
        s.pump(0.3)
        row = check(cols)
        ctx.note(f"{cols} cols: {len(row)}-col row (was {len(wide)}), fix-ups {[e.f[3] for e in fit_events(s, after)]}")


def check_plugin(cfg, p):
    if p == "fzf":
        if not shutil.which("fzf"):
            return "fzf not found in PATH"
        probe = subprocess.run(["zsh", "-dfi", "-c", 'source "$1"', "zsh", str(ROOT / "scripts/fzf.zsh")],
                               capture_output=True, text=True, timeout=10)
        if probe.returncode:
            return f"fzf history widget unavailable: {probe.stderr.strip()}"
        return None
    f = Path(cfg.plugin_dir) / PLUGIN_FILES[p]
    if not f.is_file():
        return f"plugin file missing: {f}"
    return None


_REAL_STATUS = None


def real_binary_status(cfg):
    """(usable, reason): the binary exists, embeds the current tildy.zsh, and
    `tildy render` answers a REQ with a RES frame (not a todo!() stub)."""
    global _REAL_STATUS
    if _REAL_STATUS is not None:
        return _REAL_STATUS
    b = Path(cfg.tildy_bin)

    def done(ok, why):
        global _REAL_STATUS
        _REAL_STATUS = (ok, why)
        return _REAL_STATUS

    if not (b.is_file() and os.access(b, os.X_OK)):
        return done(False, f"{b} not found (cargo build)")
    env = {k: v for k, v in os.environ.items() if not k.startswith(("TILDY_", "XDG_"))}
    tmp = tempfile.mkdtemp(prefix="sk-real-", dir="/tmp")
    env.update(XDG_CONFIG_HOME=tmp, XDG_STATE_HOME=tmp, XDG_CACHE_HOME=tmp)
    try:
        try:
            # --raw: the normal output wraps the script in an alias-safe loader; compare the raw script.
            r = subprocess.run([str(b), "init", "zsh", "--raw"], capture_output=True, text=True, timeout=10, env=env)
        except Exception as e:
            return done(False, f"`tildy init zsh` failed: {e!r}")
        if r.returncode != 0 or not r.stdout or "TILDY_BIN=" not in r.stdout.splitlines()[0]:
            return done(False, f"`tildy init zsh` exit {r.returncode}: {(r.stderr or r.stdout).strip()[:120]}")
        # Header: TILDY_BIN, TILDY_VERSION, TILDY_PROTOCOL_VERSION, the agent program list; then the script.
        if not r.stdout.endswith("\n" + Path(cfg.script).read_text()):
            return done(False, f"{b} embeds an outdated shell/zsh/tildy.zsh (rebuild: cargo build)")
        req = "REQ\t1\ts\t/\t80\t24\t0\t0\t\n"
        try:
            r = subprocess.run([str(b), "render"], input=req, capture_output=True, text=True, timeout=10, env=env)
        except subprocess.TimeoutExpired:
            return done(False, "`tildy render` did not answer within 10 s")
        if r.returncode != 0 or "RES\t1\t" not in r.stdout or "\nEND\t1" not in r.stdout:
            why = (r.stderr.strip().splitlines() or [""])[-1][:120]
            return done(False, f"`tildy render` not usable yet (exit {r.returncode}): {why}")
        return done(True, "")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


_REAL10 = None


def real_accepts_10(cfg):
    """(ok, reason): `tildy render` parses the 10-field REQ (jobs) the shell now sends.
    While M1b task B has not landed it exits 5 with a protocol error."""
    global _REAL10
    if _REAL10 is not None:
        return _REAL10
    env = {k: v for k, v in os.environ.items() if not k.startswith(("TILDY_", "XDG_"))}
    tmp = tempfile.mkdtemp(prefix="sk-real10-", dir="/tmp")
    env.update(XDG_CONFIG_HOME=tmp, XDG_STATE_HOME=tmp, XDG_CACHE_HOME=tmp)
    try:
        r = subprocess.run([str(cfg.tildy_bin), "render"], input="REQ\t1\ts\t/\t80\t24\t0\t0\tUSER=a\t1\n",
                           capture_output=True, text=True, timeout=10, env=env)
        if r.returncode == 5 or "RES\t1\t" not in r.stdout:
            _REAL10 = (False, "backend rejects 10-field REQ: "
                       + (r.stderr.strip().splitlines() or ["exit %d" % r.returncode])[-1][:80])
        else:
            _REAL10 = (True, "")
    except Exception as e:
        _REAL10 = (False, f"10-field probe failed: {e!r}")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return _REAL10


def contract_violations(ctx):
    bad = []
    for s in ctx.sessions:
        logs = [s.logfile] + sorted(ctx.tmp.glob("nested*.debug.log"))
        for lf in logs:
            for e in s.log_events(lf):
                if e.kind == "discard" and (len(e.f) < 2 or e.f[1] not in DISCARD_REASONS):
                    bad.append(f"{lf.name}: {e.raw}")
    return sorted(set(bad))


def run_one(cfg, sc):
    res = {"name": sc["name"], "status": "pass", "reason": "", "duration_s": 0.0, "evidence": {}, "notes": [],
           "backend": "fake"}
    backends = sc.get("backends", ("fake",))
    real = False
    if cfg.real:
        real = True
        if "real" not in backends:
            res["status"], res["reason"] = "skip", "fake-backend scenario (not run with --real)"
            return res
    elif "fake" not in backends:
        real = True
    if real:
        res["backend"] = "real"
        ok, why = real_binary_status(cfg)
        if not ok:
            res["status"], res["reason"] = "skip", f"real binary: {why}"
            return res
        ok10, why10 = real_accepts_10(cfg)
        if not ok10:
            res["status"], res["reason"] = "blocked", why10
            return res
    ctx = Ctx(cfg, real=real)
    t0 = time.time()
    try:
        if sc.get("plugin_variant"):
            miss = check_plugin(cfg, sc["plugin_variant"])
            if miss:
                raise Skip(miss)
        sc["fn"](ctx, sc["plugins"])
        bad = contract_violations(ctx)
        if bad:
            raise Fail("discard reason outside the contract enum: " + "; ".join(bad[:3]))
    except Skip as e:
        res["status"], res["reason"] = "skip", str(e)
    except Fail as e:
        res["status"], res["reason"] = "fail", str(e)
    except Exception as e:  # harness/scenario bug
        res["status"], res["reason"] = "fail", f"harness error: {e!r}"
        res["evidence"]["traceback"] = traceback.format_exc()
    finally:
        if ctx.sessions:
            try:
                ctx.sessions[-1].pump(0.05)
                res["evidence"].update(ctx.sessions[-1].evidence())
            except Exception:
                pass
        res["notes"] = ctx.notes
        res["duration_s"] = round(time.time() - t0, 2)
        ctx.cleanup()
    return res


def m3_session(ctx, preset="plain", history=None, extra=None):
    repo = git_repo(ctx.home0 / ("m3-" + preset))
    subprocess.run(["git", "-C", str(repo), "-c", "user.name=test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-qm", "init"], check=True, env=dict(os.environ, **GIT_ENV))
    subprocess.run(["git", "-C", str(repo), "branch", "-M", "main"], check=True)
    cfg = ctx.tmp / (preset + "-m3.json")
    cfg.write_text(json.dumps({"schema_version": 1, "preset": preset, "add_newline": preset == "pastel-powerline", "history": history or {}}))
    sess = ctx.session(name="m3-" + preset + "-" + str(len(ctx.sessions)), cwd=repo, env=dict(GIT_ENV, TILDY_CONFIG=str(cfg), **(extra or {})), cols=180)
    sess.wait_worker_ready()
    for _ in range(3):
        ev = sess.worker_cycle(); wait_fresh(sess, ev.gen, ev.idx); sess.pump(.15)
        if sess.find_log(("accept", "apply_deferred"), gen=ev.gen, after=ev.idx): break
    else: raise Fail("M3 prompt never applied at stable height")
    sess.pump(.2)
    return sess, repo


@scenario("m3_history_context", "same context stays compact; branch transition uses existing rows or a suffix after command", backends=("real",))
def sc_m3_history(ctx, plugins):
    for preset in ("plain", "pastel-powerline"):
        s, repo = m3_session(ctx, preset)
        stable=s.log_count()
        for i in range(3):
            ev = s.cycle("echo CTX%d" % i); wait_fresh(s, ev.gen, ev.idx); s.pump(.1)
        if "── tildy" in s.screen_text(): raise Fail("same context gained separator")
        ev = s.cycle("git checkout -qb feature"); wait_fresh(s, ev.gen, ev.idx)
        s.pump(.2)
        ev = s.cycle("echo CTX_NEW"); wait_fresh(s, ev.gen, ev.idx); s.pump(.2)
        if preset == "plain":
            if not any("❯ echo CTX_NEW  · feature" in line for line in s.lines()):
                # Built-in plain has no explicit character; Unicode fallback is ❯.
                raise Fail("context suffix did not follow command: " + s.screen_text())
            if s.lines().count("CTX_NEW") != 1: raise Fail("command executed other than once")
        elif "── tildy · feature ──" not in s.screen_text(): raise Fail("multiline separator missing")
        mismatches = [e for e in s.log_events()[stable:] if e.kind == "discard" and e.f[1:2] == ["lines_mismatch"]]
        if mismatches: raise Fail("history changed editor height")
    ctx.note("plain suffix and pastel separator; repeated commands kept compact")


@scenario("m3_summary_marks", "summary before A with terminal hooks installed in either order; terminal owns D placement", backends=("real",))
def sc_m3_summary(ctx, plugins):
    # A preexec mark models command start, while D is emitted by precmd.
    hook = r"""
_m3_done() { print -rn -- $'\e]133;D;0\a' }
precmd_functions+=(_m3_done)
""" + FAKE_OSC133_HOOK
    for before in (False, True):
        setup = hook if not before else hook + "\nprecmd_functions=(_m3_done ${precmd_functions:#_m3_done})\n"
        s, _ = m3_session(ctx, "pastel-powerline" if before else "plain", extra={"TILDY_TEST_POST_INIT": setup})
        start = len(s.raw)
        ev = s.cycle("false"); wait_fresh(s, ev.gen, ev.idx); s.pump(.2)
        raw = bytes(s.raw[start:]); summary = "↳ 退出码 1".encode()
        at = raw.find(summary); done = raw.find(b"\x1b]133;D;0\x07"); prompt = raw.find(b"\x1b]133;A;cl=line\x07", done)
        if not (done >= 0 and 0 <= at < prompt): raise Fail(f"summary OSC order D={done} summary={at} A={prompt}: {raw!r}")
        if raw.count(summary) != 1: raise Fail("summary duplicated")
    ctx.note("both precmd orders print summary before A and preserve D")


@scenario("m4_terminal_hook_order", "terminal integration retains the last precmd slot, marks and one summary across commands", backends=("real",))
def sc_m4_terminal_hook_order(ctx, plugins):
    # Ghostty only wraps PS1 when it owns the final precmd slot. Otherwise it
    # emits A directly and moves itself to the end for the next cycle. A theme
    # that also moves a hook last on every submission prevents B/continuation
    # marks from ever surviving its async redraws. Model that contract, not
    # just an unconditional PS1 wrapper as in semantic_marks_kept.
    hook = r"""
_m4_terminal_precmd() {
  print -rn -- $'\e]133;D;0\a'
  if [[ ${precmd_functions[-1]} != _m4_terminal_precmd ]]; then
    precmd_functions=(${precmd_functions:#_m4_terminal_precmd} _m4_terminal_precmd)
    print -rn -- $'\e]133;A;cl=line\a'
    return 0
  fi
  local nl=$'\n' p=$'%{\e]133;A;k=s\a%}'
  PS1=$'%{\e]133;A;cl=line\a%}'${PS1//$nl/$nl$p}$'%{\e]133;B\a%}'
}
precmd_functions+=(_m4_terminal_precmd)
"""
    for phase in ("TILDY_TEST_PRE_INIT", "TILDY_TEST_POST_INIT"):
        s, _ = m3_session(ctx, "pastel-powerline", extra={phase: hook})
        for command in ("false", "true", "false"):
            start = len(s.raw)
            ev = s.cycle(command); wait_fresh(s, ev.gen, ev.idx); s.pump(.2)
            raw = bytes(s.raw[start:])
            done = raw.find(b"\x1b]133;D;0\x07")
            tail = raw[raw.rfind(b"\x1b[J"):]
            for mark in (b"\x1b]133;A;cl=line\x07", b"\x1b]133;A;k=s\x07", b"\x1b]133;B\x07"):
                if mark not in tail: raise Fail(f"{phase}: async redraw lost {mark!r}: {tail!r}")
            summary = "↳ 退出码 1".encode()
            if command == "false":
                at = raw.find(summary)
                prompt = raw.find(b"\x1b]133;A;cl=line\x07", done)
                if not (done >= 0 and 0 <= at < prompt) or raw.count(summary) != 1:
                    raise Fail(f"{phase}: expected one summary before A and a completion D, got {raw!r}")
            elif summary in raw: raise Fail("successful command repeated the previous summary")
    ctx.note("before/after init, three submissions: last-hook ownership and A/continuation/B preserved; summary before A")


@scenario("m4_early_terminal_marks", "terminal decoration before tildy's precmd survives base replacement and async redraw", backends=("real",))
def sc_m4_early_terminal_marks(ctx, plugins):
    s, _ = m3_session(ctx, "pastel-powerline", extra={"TILDY_TEST_PRE_INIT": FAKE_OSC133_HOOK})
    for _ in range(2):
        start = len(s.raw)
        ev = s.cycle("true"); wait_fresh(s, ev.gen, ev.idx); s.pump(.2)
        tail = bytes(s.raw[start:]); tail = tail[tail.rfind(b"\x1b[J"):]
        for mark in (b"\x1b]133;A;cl=line\x07", b"\x1b]133;P;k=s\x07", b"\x1b]133;B\x07"):
            if mark not in tail: raise Fail(f"early terminal hook lost {mark!r}: {tail!r}")
    ctx.note("early prompt wrapper preserved through base and fresh frames")


@scenario("m4_ghostty_resize_ownership", "Ghostty integration owns prompt repositioning; theme must not erase earlier output", backends=("real",))
def sc_m4_ghostty_resize_ownership(ctx, plugins):
    # This screen does not reflow the marked prompt: Ghostty clears/repositions
    # it itself before SIGWINCH, unlike our generic reflow model. Native GUI
    # captures are the integration proof; this checks the shell's double-move
    # regression deterministically without requiring Ghostty in Linux CI.
    hook = FAKE_OSC133_HOOK + r"""
_ghostty_precmd() { _fake_osc133_precmd }
precmd_functions=(${precmd_functions:#_fake_osc133_precmd} _ghostty_precmd)
"""
    s, _ = m3_session(ctx, "pastel-powerline", extra={"TERM_PROGRAM": "ghostty", "TILDY_TEST_POST_INIT": hook})
    ev = s.cycle("echo ABOVE_M4"); wait_fresh(s, ev.gen, ev.idx); s.pump(.2)
    s.send("echo KEPT")
    start = s.log_count()
    s.resize(30, 40); s.pump(.4)
    fits = s.find_all("resize_fit", after=start)
    if not fits or any(e.f[3] != "0" for e in fits):
        raise Fail(f"Ghostty received extra cursor compensation: {[e.raw for e in fits]}")
    if s.lines().count("ABOVE_M4") != 1: raise Fail("resize erased command output: " + s.screen_text())
    s.send("\r"); s.wait_line_equals("KEPT", 3)
    ctx.note("no extra cursor-up; prior output and pending input retained")


@scenario("m4_terminal_native_marks", "Apple Terminal keeps accepted prompt rows to preserve its native navigation marks", backends=("real",))
def sc_m4_terminal_native_marks(ctx, plugins):
    s, _ = m3_session(ctx, "pastel-powerline", extra={"TERM_PROGRAM": "Apple_Terminal"})
    start = s.log_count()
    ev = s.cycle("echo TERMINAL_MARK"); wait_fresh(s, ev.gen, ev.idx); s.pump(.2)
    if s.find_log("transient", after=start) or not s.find_log("transient_skipped", after=start):
        raise Fail("Apple Terminal submission did not retain its native marked row")
    if s.lines().count("TERMINAL_MARK") != 1: raise Fail("command execution changed")
    start = s.log_count()
    s.send("cancelled"); s.pump(.1); s.send("\x03"); s.pump(.2)
    if s.find_log("transient", after=start): raise Fail("Ctrl-C erased Terminal's native mark")
    ctx.note("compact frame received; Enter and Ctrl-C skipped destructive collapse")


@scenario("m3_changes_resize_submit", "external branch change survives resize and clears on command submission", backends=("real",))
def sc_m3_changes(ctx, plugins):
    s, repo = m3_session(ctx)
    subprocess.run(["git", "-C", str(repo), "checkout", "-qb", "external"], check=True)
    s.wait_screen("main → external", 1.5, "branch change hint")
    after = s.log_count(); s.resize(30, 160)
    ev = s.wait_log("send", after=after, timeout=4); wait_fresh(s, ev.gen, after)
    s.wait_screen("main → external", 2, "hint retained after resize")
    ev = s.cycle("true"); wait_fresh(s, ev.gen, ev.idx); s.pump(.2)
    if "main → external" in s.row_text(): raise Fail("hint survived submission")
    ev = s.cycle("git checkout -qb own"); wait_fresh(s, ev.gen, ev.idx); s.pump(.3)
    if "external → own" in s.row_text(): raise Fail("own checkout produced idle change")
    ctx.note("external hint, resize retained it, submission cleared it, own checkout quiet")


@scenario("m3_context_literal_input", "single-line context preserves literal percent/bang/substitution input and executes it once", backends=("real",))
def sc_m3_literals(ctx, plugins):
    s, repo = m3_session(ctx)
    ev = s.cycle("setopt promptsubst; git checkout -qb 'feature%literal'"); wait_fresh(s, ev.gen, ev.idx); s.pump(.1)
    s.resize(30, 350); s.pump(.3)
    target=ctx.tmp / "once.txt"; forbidden=ctx.tmp / "must-not-exist"
    literal="100%!$(touch " + str(forbidden) + ")`touch " + str(forbidden) + "`"
    cmd="printf '%s\\n' " + shlex.quote(literal) + " >> " + shlex.quote(str(target))
    ev=s.cycle(cmd); wait_fresh(s,ev.gen,ev.idx); s.pump(.2)
    if forbidden.exists(): raise Fail("literal command text executed during redraw")
    if target.read_text().splitlines()!=[literal]: raise Fail("command did not execute exactly once")
    if not any(cmd + "  · feature%literal" in line for line in s.lines()): raise Fail("literal command/context not preserved in accepted line")
    ev=s.cycle("git checkout -qb multiline"); wait_fresh(s,ev.gen,ev.idx); s.pump(.1)
    multiline="printf '%s\\n' LINE_A >> "+shlex.quote(str(target))+"\nprintf '%s\\n' LINE_B >> "+shlex.quote(str(target))
    after=s.log_count(); s.send("\x1b[200~"+multiline+"\x1b[201~\r")
    ev=s.wait_log("send",after=after,timeout=5); wait_fresh(s,ev.gen,after); s.pump(.2)
    if target.read_text().splitlines()!=[literal,"LINE_A","LINE_B"]: raise Fail("multiline accepted command was altered or executed twice")
    if not any("LINE_B" in line and "  · multiline" in line for line in s.lines()): raise Fail("context suffix did not follow multiline input")
    ctx.note("literal %, !, $(), backticks and multiline paste; promptsubst enabled; one execution")


@scenario("m3_summary_full_disabled", "full history and summary=false omit every failure summary", backends=("real",))
def sc_m3_summary_disabled(ctx, plugins):
    for preset, history in (("plain", {"summary":False}), ("pastel-powerline", {"mode":"full"})):
        s,_=m3_session(ctx,preset,history=history)
        start=len(s.raw); ev=s.cycle("false"); wait_fresh(s,ev.gen,ev.idx); s.pump(.1)
        if "↳".encode() in s.raw[start:]: raise Fail("disabled summary printed")
    ctx.note("summary=false and history.mode=full disable summaries")


@scenario("m3_ack_applied_only", "ACK follows actual applied frame; deferred height changes and duplicate metadata are not acknowledged")
def sc_m3_ack(ctx, plugins):
    for bad in (False, True):
        env={"TILDY_FAKE_POWERLINE":"1", "TILDY_FAKE_OBSERVATION":"7", "TILDY_FAKE_DELAY_MS":"800",
             "TILDY_TEST_POST_INIT":"sleep 0.1"}
        if bad: env["TILDY_FAKE_DUPLICATE_OBSERVATION"]="1"
        s=ctx.session(name="ackbad" if bad else "ack",env=env,mode="persistent")
        s.wait_base_prompt()
        s.send("echo WAIT_FOR_ACK")
        if bad:
            s.wait_log("discard",timeout=6,where=lambda e:e.f[1:2]==["bad_frame"])
            if s.find_log("ack"): raise Fail("malformed frame acknowledged")
        else:
            s.wait_log("defer",gen=1,timeout=6,where=lambda e:"height_with_input" in e.f)
            if s.find_log("ack",gen=1): raise Fail("deferred height change acknowledged")
            s.send("\x15")
            s.wait_log("ack",gen=1,timeout=6)
            s.pump(.1)
            if "ack ACK" not in s.tracefile.read_text(): raise Fail("worker never received ACK")
    ctx.note("ACK delivered after application only, malformed metadata rejected")


@scenario("m3_hanging_half_frame", "a persistent producer that stalls mid-frame is retired as half_frame without ACK")
def sc_m3_hanging_half(ctx, plugins):
    backend=ctx.tmp / "hang-backend.py"
    backend.write_text("#!/usr/bin/env python3\nimport sys,time,os\nif sys.argv[1]=='worker':\n print('HELLO\\t1\\t0.1.0-m1\\t42\\t'+str(os.getpid()),flush=True)\nfor line in sys.stdin:\n if line.startswith('REQ\\t'):\n  gen=line.split('\\t')[1]\n  print('RES\\t'+gen+'\\tfresh\\t'+('42' if sys.argv[1]=='worker' else '0')+'\\nL\\thalf',flush=True)\n  time.sleep(20)\n")
    backend.chmod(0o755)
    # Keep the normal handshake budget: Python startup is not the behavior
    # under test. Set the short request deadline only after HELLO was accepted.
    s=ctx.session(backend=backend,mode="persistent",env={"TILDY_BACKEND_TIMEOUT_MS":"5000"})
    s.wait_worker_ready(); s.pump(.1)
    after=s.log_count(); s.send("TILDY_BACKEND_TIMEOUT_MS=100\r")
    ev=s.wait_log("send",after=after,timeout=4)
    if ev.f[2:3] != ["worker"]: raise Fail("hanging-frame request did not use the ready worker")
    d=s.wait_log("discard",gen=ev.gen,after=after,timeout=4,where=lambda e:e.f[1:2]==["half_frame"])
    # The adapter's timer ticks once per second; the 100ms deadline must
    # retire the incomplete frame within two ticks, not the 5s HELLO budget.
    if d.ts - ev.ts > 2000: raise Fail("hanging partial frame exceeded the short request deadline")
    s.wait_log("backend_degraded",after=d.idx,timeout=2,where=lambda e:e.f[:1]==["half_frame"])
    if s.find_log("ack",gen=ev.gen,after=after): raise Fail("stalled partial frame acknowledged")
    ctx.note("normal HELLO budget; 100ms request deadline retired hanging producer as half_frame; no ACK")


@scenario("m3_narrow_long_context", "long context labels fit existing rows; near-edge commands omit suffix without wrapping", backends=("real",))
def sc_m3_narrow_context(ctx, plugins):
    for preset in ("plain", "pastel-powerline"):
        s,repo=m3_session(ctx,preset)
        branch="feature/"+"long"*12
        ev=s.cycle("git checkout -qb "+branch); wait_fresh(s,ev.gen,ev.idx); s.pump(.1)
        after=s.log_count(); s.resize(30,40)
        ev=s.wait_log("send",after=after,timeout=5); wait_fresh(s,ev.gen,after); s.pump(.1)
        # Narrowing may leave the directory alone wider than 40: this test
        # isolates collapse geometry from that established full-prompt policy.
        command=":"+" "*36
        after=s.log_count(); s.send(command+"\r")
        ev=s.wait_log("send",after=after,timeout=5); wait_fresh(s,ev.gen,after); s.pump(.2)
        lines=s.lines()
        for line in lines:
            if "── tildy" in line and len(line)>39: raise Fail("divider wrapped")
        if preset=="plain" and any("  · " in line and command in line for line in lines):
            raise Fail("near-edge command retained a suffix without room")
        if any(e.kind=="discard" and e.f[1:2]==["lines_mismatch"] for e in s.log_events()[after:]): raise Fail("collapse changed prompt height")
    ctx.note("40 columns, long branch; divider bounded and suffix omitted at edge")


@scenario("m3_summary_duration", "a command exceeding elapsed threshold prints duration summary once", backends=("real",))
def sc_m3_duration(ctx,plugins):
    s,_=m3_session(ctx)
    start=len(s.raw); ev=s.cycle("sleep 3.1"); wait_fresh(s,ev.gen,ev.idx); s.pump(.1)
    if len(re.findall("↳ 3\\.[0-9]s".encode(), bytes(s.raw[start:])))!=1: raise Fail("duration summary missing or duplicated")
    ctx.note("elapsed threshold 3000ms; 3.1s command summarized once")


@scenario("m3_measured_profile_cursor", "an exact width profile drives backend columns and the actual ZLE prompt/input cursor", backends=("real",))
def sc_m3_measured_cursor(ctx, plugins):
    # These exact CJK/combining/PUA advances match the PTY model. This is
    # model evidence, not a claim about a physical terminal's font.
    cache=ctx.tmp / "xdg/cache/tildy/width"; cache.mkdir(parents=True,exist_ok=True)
    identity={"term":"xterm-256color","program":"tildy-pty","program_version":"model-1","locale":"en_US.UTF-8","font_revision":"pty-font-1"}
    profile={"version":1,"identity":identity,"entries":[{"cluster":"é","columns":1},{"cluster":"%","columns":1},{"cluster":"中","columns":2},{"cluster":"","columns":1}]}
    (cache / "profile.json").write_text(json.dumps(profile))
    cfg=ctx.tmp / "measured-config.json"; cfg.write_text(json.dumps({"schema_version":1,"symbols":"unicode","modules":{"sample":{"type":"text","text":"中é%"}},"layout":{"rows":[{"left":["sample"]}]}}))
    s=ctx.session(env={"TILDY_CONFIG":str(cfg),"TERM_PROGRAM":"tildy-pty","TERM_PROGRAM_VERSION":"model-1","TILDY_FONT_REVISION":"pty-font-1"})
    # The generic harness prompt detector slices a Unicode display string by
    # terminal columns, which differs for CJK/combining cells. Here the actual
    # cursor coordinate is the assertion, so await application directly.
    s.wait_worker_ready(); s.wait_log(("accept","apply_deferred"),gen=1,timeout=8); s.pump(.1)
    if s.cursor()[0] != 8: raise Fail(f"measured prompt cursor {s.cursor()[0]} != 8: {s.row_text()!r}")
    s.send_slow("echo WIDTH_OK",.01); s.pump(.1)
    if s.cursor()[0] != 21: raise Fail("profile/typed input cursor disagreed")
    s.send("\r"); s.wait_line_equals("WIDTH_OK",5)
    ctx.note("PTY exact CJK/combining/PUA profile; ZLE cursor 8 then 21; literal % preserved")


@scenario("m3_width_profile_disables_instant", "an explicit font revision neither reads nor rewrites a previous instant cache", backends=("real",))
def sc_m3_width_no_instant(ctx,plugins):
    path,_=populate_instant(ctx)
    original=path.read_bytes(); stamp=path.stat().st_mtime_ns
    s=instant_session(ctx,"width-no-instant","sleep .2",env={"TILDY_FONT_REVISION":"explicit-new-font"})
    s.wait_backend_prompt(timeout=8); s.wait_worker_ready(); ev=s.worker_cycle(); wait_fresh(s,ev.gen,ev.idx); s.pump(.2)
    if s.find_log("instant") or s.find_log("instant_done"): raise Fail("calibrated session reused instant prompt")
    if path.read_bytes()!=original or path.stat().st_mtime_ns!=stamp: raise Fail("calibrated session rewrote default-width instant cache")
    ctx.note("nonempty font revision disabled instant read/write")


@scenario("m3_instant_language_handoff", "an actual instant hit retains its language version until the first settled live frame", backends=("real",))
def sc_m3_instant_language(ctx,plugins):
    tools=ctx.tmp / "slow-node-bin"; tools.mkdir()
    node=tools / "node"; node.write_text("#!/bin/sh\nsleep .6\nprintf 'v99.1.0\\n'\n"); node.chmod(0o755)
    # Prepare the executable before testing the cache-to-live handoff. A newly
    # created macOS script can spend the provider budget on its first launch.
    probe = subprocess.run([str(node), "--version"], capture_output=True, text=True, timeout=5)
    if probe.returncode or probe.stdout.strip() != "v99.1.0":
        raise Fail(f"invalid node fixture: {probe.returncode}, {probe.stdout!r}, {probe.stderr!r}")
    (ctx.home0 / "package.json").write_text("{}")
    cfg=ctx.tmp / "slow-node-config.json"; cfg.write_text(json.dumps({"schema_version":1,"modules":{"node":{"type":"language","language":"nodejs","timeout_ms":2000,"cache_ttl_s":1}},"layout":{"rows":[{"left":["cwd","node"]}]}}))
    env={"PATH":str(tools)+":"+os.environ.get("PATH",""),"TILDY_CONFIG":str(cfg)}
    _,left=populate_instant(ctx,env=env)
    if "99.1.0" not in " ".join(left): raise Fail("seed cache contains no language version")
    s=instant_session(ctx,"slow-node-live","sleep .2",env=env)
    hit=s.wait_log("instant",timeout=4)
    if hit.f[1:2]!=["hit"]: raise Fail("language handoff did not exercise actual instant hit")
    end=time.time()+T(5); missing_since=None
    while time.time()<end:
        s.pump(.02)
        if "99.1.0" not in s.screen_text():
            missing_since=missing_since or time.time()
            if time.time()-missing_since>.08: raise Fail("cached language disappeared during live-provider pending")
        else: missing_since=None
        accepted=s.find_log(("accept","apply_deferred"),gen=1)
        if accepted: break
    else: raise Fail("first live language frame never applied")
    if "99.1.0" not in s.row_text(): raise Fail("settled live prompt lost language version")
    ctx.note("actual cache hit retained node 99.1.0 across 600ms first live query")


def main():
    global TIMEOUT_SCALE
    ap = argparse.ArgumentParser(description="Tildy PTY harness")
    ap.add_argument("--scenario", action="append", help="name or glob (repeatable, comma separated)")
    ap.add_argument("--plugins", default=",".join(PLUGINS),
                    help="plugin variants to run (comma list, or 'none')")
    ap.add_argument("--json", metavar="OUT")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--tildy-script")
    ap.add_argument("--backend")
    ap.add_argument("--real", action="store_true",
                    help="real-binary mode for the whole suite (fake-only scenarios skip)")
    ap.add_argument("--tildy-bin", default=str(ROOT / "target/debug/tildy"),
                    help="tildy binary for real-binary scenarios (default target/debug/tildy)")
    ap.add_argument("--fake-mode", choices=("oneshot", "persistent"), default="oneshot",
                    help="backend mode for scenarios that do not pin one (the M0 set); default oneshot")
    ap.add_argument("--plugin-dir", default=os.environ.get(
        "TILDY_TEST_PLUGIN_DIR", os.path.expanduser("~/.oh-my-zsh/custom/plugins")))
    ap.add_argument("--timeout-scale", type=float, default=1.0)
    ap.add_argument("--keep", action="store_true", help="keep temp dirs")
    ap.add_argument("-v", "--verbose", action="store_true", help="print evidence for every scenario")
    cfg = ap.parse_args()
    TIMEOUT_SCALE = cfg.timeout_scale

    if cfg.list:
        for sc in SCENARIOS:
            print(f"{sc['name']:55s} [{'/'.join(sc.get('backends', ('fake',)))}] {sc['desc']}")
        return 0

    real_s, real_b = ROOT / "shell/zsh/tildy.zsh", ROOT / "bench/fake-backend.sh"
    stubs = []
    if cfg.tildy_script:
        cfg.script = Path(cfg.tildy_script).resolve()
    elif real_s.is_file():
        cfg.script = real_s
    else:
        cfg.script = HERE / "stubs/tildy.zsh"
        stubs.append("tildy.zsh")
    if cfg.backend:
        cfg.backend = Path(cfg.backend).resolve()
    elif real_b.is_file():
        cfg.backend = real_b
    else:
        cfg.backend = HERE / "stubs/backend.sh"
        stubs.append("backend.sh")
    cfg.tildy_bin = Path(cfg.tildy_bin).resolve()
    for what, p in (("tildy script", cfg.script), ("backend", cfg.backend)):
        if not p.is_file():
            print(f"error: {what} not found: {p}", file=sys.stderr)
            return 2
    if stubs:
        print(f"NOTICE: using stub {', '.join(stubs)} (real files missing); results are harness self-test only")
    ok, why = real_binary_status(cfg)
    print(f"tildy script: {cfg.script}\nbackend:      {cfg.backend} (mode for M0 scenarios: {cfg.fake_mode})\n"
          f"tildy bin:    {cfg.tildy_bin} ({'usable' if ok else 'unusable: ' + why})"
          f"{'  [--real]' if cfg.real else ''}\n")

    enabled = set() if cfg.plugins == "none" else {x for x in cfg.plugins.split(",") if x}
    bad = enabled - set(PLUGINS)
    if bad:
        print(f"error: unknown plugins {sorted(bad)}", file=sys.stderr)
        return 2
    pats = [x for a in (cfg.scenario or []) for x in a.split(",") if x]
    chosen = []
    for sc in SCENARIOS:
        if sc.get("plugin_variant") and sc["plugin_variant"] not in enabled:
            continue
        if pats and not any(fnmatch.fnmatch(sc["name"], p) for p in pats):
            continue
        chosen.append(sc)
    if not chosen:
        print("error: no scenario matched", file=sys.stderr)
        return 2

    results = []
    for sc in chosen:
        r = run_one(cfg, sc)
        results.append(r)
        print(f"[{r['status'].upper():4s}] {r['name']:50s} {r['duration_s']:6.1f}s  {r['reason']}", flush=True)
        if r["status"] == "fail" or cfg.verbose:
            print("  --- screen ---")
            for l in r["evidence"].get("screen", "").splitlines():
                print("  | " + l)
            print("  --- debug log (tail) ---")
            for l in r["evidence"].get("log", [])[-25:]:
                print("  > " + l.replace("\t", " | "))
            if "traceback" in r["evidence"]:
                print(r["evidence"]["traceback"])

    counts = {k: sum(1 for r in results if r["status"] == k) for k in ("pass", "fail", "skip", "blocked")}
    print("\n{:<52s} {:<6s} {:<5s} {:>7s}  {}".format("scenario", "status", "bknd", "secs", "reason / notes"))
    print("-" * 116)
    for r in results:
        extra = r["reason"] or "; ".join(r["notes"])
        print("{:<52s} {:<6s} {:<5s} {:>7.1f}  {}".format(r["name"], r["status"], r["backend"], r["duration_s"],
                                                         extra[:90]))
    print(f"\npass={counts['pass']} fail={counts['fail']} skip={counts['skip']} blocked={counts['blocked']}")

    if cfg.json:
        out = {"tildy_script": str(cfg.script), "backend": str(cfg.backend), "using_stub": stubs,
               "tildy_bin": str(cfg.tildy_bin), "real_mode": cfg.real, "fake_mode": cfg.fake_mode,
               "real_binary_usable": ok, "real_binary_reason": why,
               "summary": counts, "results": results}
        Path(cfg.json).parent.mkdir(parents=True, exist_ok=True)
        Path(cfg.json).write_text(json.dumps(out, ensure_ascii=False, indent=2))
    return 1 if counts["fail"] else 0


if __name__ == "__main__":
    sys.exit(main())
