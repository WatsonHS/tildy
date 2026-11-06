#!/usr/bin/env python3
"""Tildy M0 task C: performance baseline.

Usage: python3 bench/run.py [--group spawn_cost|prompt_lag|first_prompt|idle_resource|instant_prompt|all]
                            [--samples N] [--out DIR] [--quick]

Standard library only (pexpect is optional, version recording only). Never reads the user's ~/.zshrc or ~/.p10k.zsh:
every configuration runs under its own ZDOTDIR (bench/zdotdir-<name>/).
"""
import argparse, csv, datetime, json, os, platform, re, select, shutil, statistics
import fcntl, pty, signal, struct, subprocess, sys, tempfile, termios, time

try:
    import pexpect
    PEXPECT_VERSION = pexpect.__version__
except ImportError:  # only used to record the version; PTYs are opened with pty.fork
    PEXPECT_VERSION = "not installed"

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "bench")
TILDY_ZSH = os.path.join(ROOT, "shell", "zsh", "tildy.zsh")
FAKE_BACKEND = os.path.join(BENCH, "fake-backend.sh")
P10K_THEME = os.path.expanduser("~/.oh-my-zsh/custom/themes/powerlevel10k/powerlevel10k.zsh-theme")
STARSHIP_BIN = shutil.which("starship") or "/opt/homebrew/bin/starship"
STARSHIP_CFG = os.path.join(BENCH, "zdotdir-starship", "starship.toml")
COLS, ROWS = 100, 30
MARK = b"SKRDY"
TILDY_RE = re.compile(rb"\[g(\d+) ")

TOKEN = "tildybench%d" % os.getpid()
TILDY_BIN = os.environ.get("TILDY_BENCH_BIN") or os.path.join(ROOT, "target", "release", "tildy")
CONFIGS = ["clean", "starship", "starship_user", "p10k", "tildy", "tildy_worker", "tildy_powerline"]
LOG_CFGS = ("tildy_worker", "tildy_powerline", "tildy_sleep", "tildy_instant", "tildy_instant_static")   # real binary, timed via the debug log
# instant_prompt group: every config runs the same 150 ms `sleep` during init (docs/decisions/0006-instant-prompt.md).
INSTANT_CONFIGS = ["clean_sleep", "tildy_sleep", "tildy_instant", "tildy_instant_static", "p10k_instant"]
INSTANT_CACHE = [None]   # XDG_CACHE_HOME for tildy_instant*/p10k_instant (bench temp dir, never ~/.cache)
INSTANT_FILE = [None]    # pre-generated `tildy init zsh --instant` output for tildy_instant_static
GLYPH = "\u276f".encode()   # the default config's prompt character (instant prompt hit, real prompt)
BASE_EMPTY = b"empty % "      # base prompt '%~ %# ' in the empty fixture dir (tildy_sleep, instant miss)
PL_CONFIG = [None]   # temp file holding {"preset":"pastel-powerline"}, set in main()
_PL_PROBE = {}
LOG_RE = re.compile(r"^(\d+)\t(\w+)\t(\d+)(?:\t(\S*))?", re.M)


def now_ms():
    return time.perf_counter_ns() / 1e6


def missing_reason(cfg):
    if cfg in ("p10k", "p10k_instant") and not os.path.exists(P10K_THEME):
        return "missing: p10k theme not found"
    if cfg == "starship" and not os.path.exists(STARSHIP_BIN):
        return "missing: starship not installed"
    if cfg == "tildy":
        absent = [p for p in (TILDY_ZSH, FAKE_BACKEND) if not os.path.exists(p)]
        if absent:
            return "missing: not yet implemented (" + ", ".join(os.path.relpath(p, ROOT) for p in absent) + ")"
    if cfg in LOG_CFGS and not os.path.exists(TILDY_BIN):
        return "missing: target/release/tildy not built (cargo build --release)"
    if cfg == "starship_user" and not os.path.exists(os.path.join(BENCH, "zdotdir-starship_user", "starship.toml")):
        return "missing: bench/zdotdir-starship_user/starship.toml (copy ~/.config/starship.toml)"
    if cfg == "tildy_powerline":
        if "r" not in _PL_PROBE:
            _PL_PROBE["r"] = powerline_probe()
        return _PL_PROBE["r"]
    return None


def powerline_probe():
    """None when the binary renders the pastel-powerline preset (two L rows, no config error)."""
    import tempfile as _t
    d = _t.mkdtemp(prefix="tildypl-")
    try:
        cfgf = os.path.join(d, "config.json")
        with open(cfgf, "w") as f:
            f.write('{"preset":"pastel-powerline"}')
        env = {"PATH": "/usr/bin:/bin", "TILDY_CONFIG": cfgf, "XDG_CONFIG_HOME": d, "XDG_STATE_HOME": d, "XDG_CACHE_HOME": d}
        r = subprocess.run([TILDY_BIN, "render"], input="REQ\t1\ts\t/tmp\t80\t24\t0\t0\t\n", capture_output=True,
                           text=True, timeout=10, env=env)
    except Exception as e:  # noqa
        return "missing: preset probe failed (%s)" % e
    finally:
        shutil.rmtree(d, ignore_errors=True)
    if "invalid config" in r.stderr or r.returncode != 0:
        return "missing: binary rejects the pastel-powerline preset (%s)" % re.sub(r"invalid config \S+: ", "", (r.stderr.strip().splitlines() or ["exit %d" % r.returncode])[-1])[:110]
    if r.stdout.count("\nL\t") < 2:
        return "missing: preset accepted but renders %d L row(s); task A has not landed" % r.stdout.count("\nL\t")
    return None


def shell_env(cfg, log=None):
    env = {
        "PATH": "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": os.path.expanduser("~"),
        "TERM": "xterm-256color",
        "LANG": "en_US.UTF-8",
        "LC_ALL": "en_US.UTF-8",
        "ZDOTDIR": os.path.join(BENCH, "zdotdir-" + cfg),
        "COLUMNS": str(COLS), "LINES": str(ROWS),
        "STARSHIP_CONFIG": STARSHIP_CFG,
        "TILDY_BENCH_TOKEN": TOKEN,   # lets idle_resource find helpers that escape the process tree
    }
    if cfg == "tildy":
        env.update(TILDY_ROOT=ROOT, TILDY_BACKEND=FAKE_BACKEND, TILDY_FAKE_DELAY_MS="0",
                   TILDY_BACKEND_MODE="oneshot")
    if cfg == "starship_user":
        env["STARSHIP_CONFIG"] = os.path.join(BENCH, "zdotdir-starship_user", "starship.toml")
    if cfg in LOG_CFGS:
        # Real binary, persistent worker. Readiness comes from the debug log (ready / recv fresh),
        # because the real prompt carries no screen marker.
        env.update(TILDY_BIN_PATH=TILDY_BIN, TILDY_DEBUG_LOG=log or "")
        if cfg == "tildy_powerline":
            env["TILDY_CONFIG"] = PL_CONFIG[0]
    if cfg in ("tildy_instant", "tildy_instant_static", "p10k_instant"):
        env["XDG_CACHE_HOME"] = os.path.join(INSTANT_CACHE[0], cfg)
    if cfg == "tildy_instant_static":
        env["TILDY_INSTANT_FILE"] = INSTANT_FILE[0]
    return env


def run_out(cmd, **kw):
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=10, **kw)
        return (r.stdout + r.stderr).strip()
    except Exception as e:  # noqa
        return "unavailable: %s" % e


def p10k_version():
    d = os.path.dirname(P10K_THEME)
    if not os.path.isdir(d):
        return "unavailable"
    v = run_out(["git", "-C", d, "log", "-1", "--format=%h %cs"])
    return "git " + v if v and "unavailable" not in v and "fatal" not in v else "unknown"


def environment():
    return {
        "timestamp": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "macos": run_out(["sw_vers"]).replace("\n", "; ").replace("\t", ""),
        "cpu": run_out(["sysctl", "-n", "machdep.cpu.brand_string"]),
        "ncpu": os.cpu_count(),
        "zsh": run_out(["zsh", "--version"]),
        "git": run_out(["git", "--version"]),
        "starship": run_out([STARSHIP_BIN, "--version"]).splitlines()[0] if os.path.exists(STARSHIP_BIN) else "missing",
        "p10k": p10k_version(),
        "python": sys.version.split()[0] + " (" + platform.platform() + ")",
        "pexpect": PEXPECT_VERSION + " (installed; not used for spawning, see baseline.md)",
        "terminal_size": "%dx%d (cols x rows), TERM=xterm-256color, stdlib pty.fork PTY (not a real terminal)" % (COLS, ROWS),
        "load_avg_at_start": run_out(["sysctl", "-n", "vm.loadavg"]),
        "power": run_out(["pmset", "-g", "batt"]).splitlines()[0] if shutil.which("pmset") else "unknown",
    }


def pct(sorted_vals, p):
    # nearest-rank percentile
    n = len(sorted_vals)
    k = max(1, int(-(-p * n // 100)))
    return sorted_vals[min(n, k) - 1]


def summarize(vals):
    s = sorted(vals)
    return {"n": len(s), "median": statistics.median(s), "p95": pct(s, 95), "p99": pct(s, 99),
            "min": s[0], "max": s[-1], "mean": statistics.fmean(s)}


class Group:
    """Collects raw rows + summary for one group, writes CSV and JSON."""

    def __init__(self, name, out, header):
        self.name, self.out, self.header = name, out, header
        self.rows = []
        self.cells = {}      # "config|cell" -> list of values
        self.notes = {}      # "config|cell" -> status/notes
        self.meta = {}

    def add(self, config, cell, idx, val, extra=()):
        self.rows.append([config, cell, idx, round(val, 4), *extra])
        self.cells.setdefault(config + "|" + cell, []).append(val)

    def note(self, config, cell, text):
        self.notes[config + "|" + cell] = text

    def write(self, env, quick):
        os.makedirs(self.out, exist_ok=True)
        with open(os.path.join(self.out, self.name + ".csv"), "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(self.header)
            w.writerows(self.rows)
        summ = {k: summarize(v) for k, v in self.cells.items() if v}
        doc = {"group": self.name, "quick": quick, "environment": env, "meta": self.meta,
               "summary": summ, "notes": self.notes}
        with open(os.path.join(self.out, self.name + ".summary.json"), "w") as f:
            json.dump(doc, f, indent=2, ensure_ascii=False)
        print("[%s] wrote %s.csv / .summary.json" % (self.name, self.name))
        for k, s in summ.items():
            print("  %-34s n=%-4d median=%8.2f p95=%8.2f p99=%8.2f" % (k, s["n"], s["median"], s["p95"], s["p99"]))
        for k, t in self.notes.items():
            print("  %-34s %s" % (k, t))


# ---------------------------------------------------------------- fixtures
def make_fixtures(tmp):
    empty = os.path.join(tmp, "empty")
    os.makedirs(empty)
    repo = os.path.join(tmp, "gitrepo")
    os.makedirs(repo)
    genv = dict(os.environ, GIT_AUTHOR_NAME="b", GIT_AUTHOR_EMAIL="b@x", GIT_COMMITTER_NAME="b",
                GIT_COMMITTER_EMAIL="b@x", GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_SYSTEM="/dev/null")

    def g(*a):
        subprocess.run(["git", *a], cwd=repo, env=genv, check=True, capture_output=True)
    g("init", "-q", "-b", "main")
    for c in range(5):
        for i in range(10):
            p = os.path.join(repo, "src", "d%d" % (i % 3))
            os.makedirs(p, exist_ok=True)
            with open(os.path.join(p, "f%d_%d.txt" % (c, i)), "w") as f:
                f.write("commit %d file %d\n" % (c, i) * 20)
        g("add", "-A")
        g("commit", "-q", "-m", "commit %d" % c)
    # dirty state: a modified tracked file, a staged file, untracked files
    with open(os.path.join(repo, "src", "d0", "f0_0.txt"), "a") as f:
        f.write("modified\n")
    with open(os.path.join(repo, "staged.txt"), "w") as f:
        f.write("s\n")
    g("add", "staged.txt")
    for i in range(3):
        with open(os.path.join(repo, "untracked%d.txt" % i), "w") as f:
            f.write("u\n")
    nfiles = int(run_out(["git", "-C", repo, "ls-files"]).count("\n")) + 1
    lang = os.path.join(tmp, "langrepo")
    shutil.copytree(repo, lang)
    with open(os.path.join(lang, "package.json"), "w") as f:
        f.write('{"name":"bench","version":"1.0.0"}\n')
    with open(os.path.join(lang, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "bench"\nversion = "0.1.0"\nedition = "2021"\n')
    return {"empty": empty, "gitrepo": repo, "langrepo": lang, "project": ROOT}, {
        "gitrepo_commits": 5, "gitrepo_tracked_files": nfiles, "gitrepo_dirty": "1 modified, 1 staged, 3 untracked",
        "project_is_git_repo": subprocess.run(["git", "-C", ROOT, "rev-parse"], capture_output=True).returncode == 0}


# ---------------------------------------------------------------- PTY session
class Session:
    def __init__(self, cfg, cwd):
        self.cfg = cfg
        self.buf = b""
        self.last_gen = 0
        self.log = None
        if cfg in LOG_CFGS:
            self.log = os.path.join(tempfile.gettempdir(), "tildy-bench-%d-%d.log" % (os.getpid(), id(self)))
            open(self.log, "w").close()
        self.log_pos = 0
        # wall-clock (time.time) minus perf-clock (now_ms): the debug log is wall-clock ms
        self.wall_off = time.time() * 1000 - now_ms()
        self.t_spawn = now_ms()
        # pty.fork instead of pexpect.spawn: pexpect/ptyprocess adds ~150 ms of spawn overhead
        # here (measured), which would swamp first_prompt. The PTY itself is equivalent.
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
                os.chdir(cwd)
                os.execve("/bin/zsh", ["zsh", "-i"], shell_env(cfg, self.log))
            finally:
                os._exit(127)

    def _read(self, timeout):
        r, _, _ = select.select([self.fd], [], [], timeout)
        if not r:
            return None
        try:
            d = os.read(self.fd, 65536)
        except OSError:
            return b""
        return d

    def _ready_in(self, buf):
        if self.cfg == "tildy":
            for m in TILDY_RE.finditer(buf):
                if int(m.group(1)) > self.last_gen:
                    return int(m.group(1)), m.end()
            return None
        i = buf.find(MARK)
        return (0, i + len(MARK)) if i >= 0 else None

    def _log_events(self):
        """New (ts_ms, event, gen, extra) tuples appended to the debug log since the last call."""
        if not self.log:
            return []
        with open(self.log, "rb") as f:
            f.seek(self.log_pos)
            chunk = f.read()
            self.log_pos += len(chunk)
        return [(int(m.group(1)), m.group(2), int(m.group(3)), m.group(4) or "")
                for m in LOG_RE.finditer(chunk.decode("utf-8", "replace"))]

    def wait_log(self, want, timeout, gen=None):
        """Drain the PTY while waiting for a log event. `want(ev)` returns True on the match.
        Returns perf-clock ms of the event (converted from its wall-clock timestamp)."""
        end = now_ms() + timeout * 1000
        while True:
            for ev in self._log_events():
                if want(ev):
                    return ev[0] - self.wall_off, ev[2]
            if now_ms() > end:
                raise TimeoutError("%s: log event not seen; tail=%r" % (self.cfg, self.buf[-200:]))
            d = self._read(0.002)
            if d == b"":
                raise EOFError("%s: shell exited; buf=%r" % (self.cfg, self.buf[-200:]))
            if d:
                self.buf += d

    def wait_fresh_shown(self, gen, timeout=10.0):
        """Time when a `fresh` result for `gen` was drawn (recv fresh followed by accept/apply_deferred)."""
        seen_fresh = [False]
        def want(ev):
            if ev[1] == "recv" and ev[2] == gen and ev[3] == "fresh":
                seen_fresh[0] = True
            return seen_fresh[0] and ev[1] in ("accept", "apply_deferred") and ev[2] == gen
        return self.wait_log(want, timeout)[0]

    def wait_ready(self, timeout=10.0):
        """Block until the readiness marker is seen; return perf time (ms) of the read that held it."""
        if self.cfg in LOG_CFGS:
            t, g = self.wait_log(lambda ev: ev[1] == "ready" and ev[2] > self.last_gen, timeout)
            self.last_gen = g
            return t
        end = now_ms() + timeout * 1000
        self.buf = b""
        while True:
            left = (end - now_ms()) / 1000
            if left <= 0:
                raise TimeoutError("%s: prompt marker not seen; tail=%r" % (self.cfg, self.buf[-200:]))
            d = self._read(left)
            if d is None:
                continue
            if d == b"":
                raise EOFError("%s: shell exited; buf=%r" % (self.cfg, self.buf[-200:]))
            t = now_ms()
            self.buf += d
            hit = self._ready_in(self.buf)
            if hit:
                self.last_gen = max(self.last_gen, hit[0])
                return t

    def drain(self, quiet=0.03, limit=1.0, until=None):
        """Consume output until `quiet` seconds of silence (async redraws settle)."""
        end = time.monotonic() + limit
        t_until = None
        while time.monotonic() < end:
            d = self._read(quiet)
            if d is None:
                if until is None or t_until is not None:
                    return t_until
                continue
            if d == b"":
                return t_until
            self.buf += d
            if until and t_until is None and until in self.buf:
                t_until = now_ms()
        return t_until

    def send(self, b):
        os.write(self.fd, b)

    def close(self):
        try:
            os.kill(self.pid, signal.SIGHUP)
        except OSError:
            pass
        if self.log:
            try:
                os.unlink(self.log)
            except OSError:
                pass
        try:
            os.close(self.fd)
        except OSError:
            pass
        for _ in range(100):
            try:
                if os.waitpid(self.pid, os.WNOHANG)[0]:
                    return
            except ChildProcessError:
                return
            time.sleep(0.005)
        try:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
        except Exception:
            pass


# ---------------------------------------------------------------- groups
def g_spawn_cost(a, env, fx, meta):
    n = a.samples or (20 if a.quick else 300)
    warm = 5 if a.quick else 20
    grp = Group("spawn_cost", a.out, ["config", "cell", "sample", "value_ms"])
    grp.meta.update(samples=n, warmup_discarded=warm, method="subprocess.run + perf_counter_ns, stdout piped, stdin devnull (or REQ line)")
    senv = {"PATH": shell_env("clean")["PATH"], "HOME": os.path.expanduser("~"), "STARSHIP_CONFIG": STARSHIP_CFG,
            "LANG": "en_US.UTF-8"}
    req = "REQ\t1\tbench\t%s\t%d\t%d\t0\t0\n" % (ROOT, COLS, ROWS)
    cases = [("true", "/usr/bin/true", ["/usr/bin/true"], ROOT, None, {}),
             ("zsh_f_exit", "zsh -f -c :", ["/bin/zsh", "-f", "-c", ":"], ROOT, None, {}),
             ("starship_prompt", "project_root", [STARSHIP_BIN, "prompt"], ROOT, None, {}),
             ("starship_prompt", "empty_dir", [STARSHIP_BIN, "prompt"], fx["empty"], None, {}),
             ("starship_prompt", "git_repo", [STARSHIP_BIN, "prompt"], fx["gitrepo"], None, {})]
    if os.path.exists(FAKE_BACKEND):
        cases.append(("fake_backend", "delay0", ["/bin/bash", FAKE_BACKEND], ROOT, req,
                      {"TILDY_FAKE_DELAY_MS": "0"}))
    else:
        grp.note("fake_backend", "delay0", "missing: not yet implemented (bench/fake-backend.sh)")
    for cfg, cell, cmd, cwd, stdin, extra in cases:
        e = dict(senv, **extra)
        if cfg == "fake_backend":
            cmd = [FAKE_BACKEND]  # run it the way tildy would: via its shebang
        for i in range(warm + n):
            t0 = time.perf_counter_ns()
            r = subprocess.run(cmd, cwd=cwd, env=e, input=stdin.encode() if stdin else None,
                               stdin=None if stdin else subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL)
            dt = (time.perf_counter_ns() - t0) / 1e6
            if i >= warm:
                grp.add(cfg, cell, i - warm, dt)
        if cfg == "fake_backend" and r.returncode != 0:
            grp.note(cfg, cell, "WARNING: backend exited %d; check REQ handling" % r.returncode)
        print("[spawn_cost] %s/%s done" % (cfg, cell), flush=True)
    return grp


def g_prompt_lag(a, env, fx, meta):
    n = a.samples or (20 if a.quick else 300)
    warm = 3 if a.quick else 10
    grp = Group("prompt_lag", a.out, ["config", "cell", "sample", "value_ms"])
    grp.meta.update(samples=n, warmup_discarded=warm, method="send \\r, time to SKRDY marker (tildy: new [gN ] prompt; tildy_worker: debug-log `ready` event = input-ready, and `<cell>/fresh_shown` = fresh result drawn), quiet-drain between samples")
    cells = [("empty_dir", fx["empty"]), ("project_root", fx["project"]), ("git_repo", fx["gitrepo"]),
             ("lang_repo", fx["langrepo"])]
    vcs = Group("prompt_lag_vcs", a.out, ["config", "cell", "sample", "value_ms", "vcs_in_first_paint"])
    for cfg in CONFIGS:
        why = missing_reason(cfg)
        for cell, cwd in cells:
            if why:
                grp.note(cfg, cell, why)
                continue
            s = Session(cfg, cwd)
            try:
                s.wait_ready(15)
                s.drain(0.2, 3.0)
                for i in range(warm + n):
                    s.drain(0.03, 1.0)
                    s.buf = b""
                    t0 = now_ms()
                    s.send(b"\r")
                    t1 = s.wait_ready(10)
                    first_paint_buf = s.buf
                    tv = None
                    if cfg == "p10k" and cell == "git_repo":
                        in_first = b"main" in first_paint_buf
                        tv = t1 if in_first else s.drain(0.03, 1.0, until=b"main")
                        if i >= warm and tv is not None:
                            vcs.add(cfg, cell, i - warm, tv - t0, (int(in_first),))
                    if cfg in LOG_CFGS:
                        ts = s.wait_fresh_shown(s.last_gen, 10)
                        if i >= warm:
                            grp.add(cfg, cell + "/fresh_shown", i - warm, ts - t0)
                    if i >= warm:
                        grp.add(cfg, cell, i - warm, t1 - t0)
            except Exception as e:
                grp.note(cfg, cell, "FAILED: %s" % str(e)[:300])
            finally:
                s.close()
            print("[prompt_lag] %s/%s done" % (cfg, cell), flush=True)
    if vcs.rows:
        grp.meta["vcs_group"] = "prompt_lag_vcs"
        vcs.meta.update(note="p10k git_repo: time from \\r to branch name 'main' visible; vcs_in_first_paint=1 means a (possibly cached) branch was already in the first paint that contained the marker")
        vcs.write(env, a.quick)
    return grp


def g_first_prompt(a, env, fx, meta):
    n = a.samples or (5 if a.quick else 100)
    grp = Group("first_prompt", a.out, ["config", "cell", "sample", "value_ms"])
    grp.meta.update(samples=n, warmup_discarded=0, method="time from before pty.fork + exec zsh -i to first marker; cold process each sample; cwd=empty temp dir; one untimed spawn before sampling to warm the OS file cache")
    for cfg in CONFIGS:
        why = missing_reason(cfg)
        if why:
            grp.note(cfg, "empty_dir", why)
            continue
        for i in range(-1, n):
            s = Session(cfg, fx["empty"])
            try:
                t = s.wait_ready(15)
                if i >= 0:
                    grp.add(cfg, "empty_dir", i, t - s.t_spawn)
            except Exception as e:
                grp.note(cfg, "empty_dir", "FAILED: %s" % str(e)[:300])
                break
            finally:
                s.close()
        print("[first_prompt] %s done" % cfg, flush=True)
    return grp


def instant_sample(s, warm=False):
    """One cold start of an instant_prompt config. Returns perf-clock ms of
    (first prompt visible, real prompt usable, first fresh result drawn or None, cache hit or None).
    clean_sleep: the marker prompt is both. p10k_instant: the first SKRDY is its instant prompt,
    the second the real prompt zle draws after the first precmd. tildy_*: visible = first prompt
    bytes on the PTY (the instant prompt's ❯, or the base prompt for tildy_sleep), usable = the
    `ready` debug-log event, fresh = first `accept` after `recv <gen> fresh`. With `warm` (the
    untimed first start, no caches yet) p10k shows no instant prompt: one SKRDY is enough."""
    cfg = s.cfg
    if cfg == "clean_sleep":
        t = s.wait_ready(15)
        return t, t, None, None
    end = now_ms() + 15000
    vis = ready = fresh = hit = None
    seen_fresh = set()
    buf = b""
    while True:
        d = s._read(0.002)
        if d == b"":
            raise EOFError("%s: shell exited; buf=%r" % (cfg, buf[-200:]))
        if d:
            buf += d
            t = now_ms()
            if cfg == "p10k_instant":
                n = buf.count(MARK)
                if n >= 1 and vis is None:
                    vis = t
                if (n >= 2 or (warm and n >= 1)) and ready is None:
                    return vis, t, None, None
            elif vis is None and (GLYPH in buf or BASE_EMPTY in buf):
                vis = t
        for ev in s._log_events():
            ts = ev[0] - s.wall_off
            if ev[1] == "instant":
                hit = ev[3] == "hit"
            elif ev[1] == "ready" and ready is None:
                ready = ts
            elif ev[1] == "recv" and ev[3] == "fresh":
                seen_fresh.add(ev[2])
            elif ev[1] in ("accept", "apply_deferred") and ev[2] in seen_fresh and fresh is None:
                fresh = ts
        if vis is not None and ready is not None and fresh is not None:
            return vis, ready, fresh, hit
        if now_ms() > end:
            raise TimeoutError("%s: vis=%s ready=%s fresh=%s; tail=%r" % (cfg, vis, ready, fresh, buf[-200:]))


def instant_warm(s):
    """Untimed: let the caches fill. tildy: run one command so the worker (not the oneshot
    first prompt) sends a fresh result and writes the instant cache; p10k: give its
    asynchronous instant-prompt dump time to land."""
    if s.cfg in ("tildy_instant", "tildy_instant_static"):
        cache = os.path.join(INSTANT_CACHE[0], s.cfg, "tildy", "instant")
        for _ in range(5):
            s.drain(0.2, 2.0)
            s.send(b"\r")
            s.drain(0.3, 2.0)
            if os.path.isdir(cache) and any(f.endswith(".prompt") for f in os.listdir(cache)):
                return
        raise RuntimeError("tildy instant cache was not written during warm-up")
    s.drain(0.5, 3.0)


def g_instant_prompt(a, env, fx, meta):
    n = a.samples or (5 if a.quick else 50)
    grp = Group("instant_prompt", a.out, ["config", "cell", "sample", "value_ms"])
    grp.meta.update(samples=n, init_sleep_ms=150, cwd="empty temp dir",
                    method="cold zsh -i per sample, time from before pty.fork; one untimed warm-up start that "
                           "fills the instant caches (tildy: one command so the worker writes the cache; p10k: "
                           "its own dump). Cells: visible = first prompt on the PTY, ready = prompt usable "
                           "(tildy: debug-log `ready`; clean: marker; p10k: second SKRDY = real prompt drawn), "
                           "fresh = first fresh tildy result drawn. tildy_instant uses eval \"$(tildy init zsh "
                           "--instant)\" (one tildy spawn); tildy_instant_static sources a pre-generated file.",
                    instant_cache_dir="bench temp dir (XDG_CACHE_HOME)")
    for cfg in INSTANT_CONFIGS:
        why = missing_reason(cfg)
        if why:
            grp.note(cfg, "visible", why)
            continue
        hits = 0
        for i in range(-1, n):
            s = Session(cfg, fx["empty"])
            try:
                vis, ready, fresh, hit = instant_sample(s, warm=i < 0)
                if i < 0:
                    instant_warm(s)
                    continue
                grp.add(cfg, "visible", i, vis - s.t_spawn)
                grp.add(cfg, "ready", i, ready - s.t_spawn)
                if fresh is not None:
                    grp.add(cfg, "fresh", i, fresh - s.t_spawn)
                hits += bool(hit)
            except Exception as e:
                grp.note(cfg, "visible", "FAILED: %s" % str(e)[:300])
                break
            finally:
                s.close()
        if cfg in ("tildy_instant", "tildy_instant_static"):
            grp.meta.setdefault("cache_hits", {})[cfg] = "%d/%d" % (hits, n)
        print("[instant_prompt] %s done" % cfg, flush=True)
    return grp


def parse_cputime(t):
    # ps cputime: [[h:]m:]s.cc
    parts = t.strip().split(":")
    sec = 0.0
    for p in parts:
        sec = sec * 60 + float(p)
    return sec


def ps_all():
    out = run_out(["ps", "-axwwE", "-o", "pid=,ppid=,command="])
    res = {}
    for ln in out.splitlines():
        m = re.match(r"\s*(\d+)\s+(\d+)\s+(.*)", ln)
        if m:
            res[int(m.group(1))] = (int(m.group(2)), m.group(3))
    return res




def proc_tree(roots):
    """Descendants of the session shells, plus processes that escaped the tree (p10k's
    gitstatus helpers double-fork and are reparented to launchd). Those are found by the
    TILDY_BENCH_TOKEN env var, which `ps -E` shows and children inherit."""
    allp = ps_all()
    kids = {}
    for p, (pp, _) in allp.items():
        kids.setdefault(pp, []).append(p)
    seen, stack = set(), list(roots)
    while stack:
        p = stack.pop()
        if p in seen:
            continue
        seen.add(p)
        stack.extend(kids.get(p, []))
    for p, (pp, cmd) in allp.items():
        if "TILDY_BENCH_TOKEN=" + TOKEN in cmd:
            seen.add(p)
    return seen & set(allp)


def ps_sample(pids):
    out = run_out(["ps", "-o", "rss=,%cpu=,pid=,cputime=,command=", "-p", ",".join(map(str, sorted(pids)))])
    rows = []
    for ln in out.splitlines():
        m = re.match(r"\s*(\d+)\s+([\d.]+)\s+(\d+)\s+(\S+)\s+(.*)", ln)
        if m:
            rows.append((int(m.group(1)), float(m.group(2)), int(m.group(3)), parse_cputime(m.group(4)), m.group(5)))
    return rows


def g_idle_resource(a, env, fx, meta):
    nsess = 10
    idle = 15 if a.quick else 60
    step = 3 if a.quick else 5
    grp = Group("idle_resource", a.out, ["config", "cell", "sample", "total_rss_kb", "ps_cpu_pct_sum", "cputime_s_sum", "nproc", "elapsed_s"])
    grp.meta.update(sessions=nsess, idle_s=idle, interval_s=step, cwd="temp git repo (so gitstatusd is exercised)",
                    note="ps %cpu on macOS is a decaying lifetime average, so cpu is also derived from cputime deltas (10 ms resolution)")
    procs = Group("idle_resource_procs", a.out, ["config", "cell", "sample", "rss_kb", "pid", "cputime_s", "command"])
    for cfg in CONFIGS:
        if cfg == "tildy":
            grp.note(cfg, "idle", "not applicable: oneshot mode has no resident backend process (zsh process cost equals clean zsh + tildy.zsh hooks)" if not missing_reason(cfg) else missing_reason(cfg))
            continue
        why = missing_reason(cfg)
        if why:
            grp.note(cfg, "idle", why)
            continue
        sess = []
        try:
            for _ in range(nsess):
                s = Session(cfg, fx["gitrepo"])
                sess.append(s)
            for s in sess:
                s.wait_ready(20)
            for s in sess:
                s.drain(0.2, 3.0)
            time.sleep(2)  # let startup work settle (compinit-free configs, gitstatusd handshake)
            t_start = time.monotonic()
            roots = [s.pid for s in sess]
            first_cpu = None
            k = 0
            while True:
                el = time.monotonic() - t_start
                for s in sess:
                    s.drain(0.001, 0.01)  # keep PTY buffers empty
                pids = proc_tree(roots)
                rows = ps_sample(pids)
                tot_rss = sum(r[0] for r in rows)
                tot_cpu = sum(r[1] for r in rows)
                tot_ct = sum(r[3] for r in rows)
                if first_cpu is None:
                    first_cpu = tot_ct
                grp.add(cfg, "idle", k, tot_rss, (round(tot_cpu, 2), round(tot_ct, 2), len(rows), round(el, 1)))
                if k == 0 or el >= idle:
                    for r in rows:
                        procs.rows.append([cfg, "idle", k, r[0], r[2], r[3], r[4][:80]])
                k += 1
                if el >= idle:
                    break
                time.sleep(max(0, step - ((time.monotonic() - t_start) % step)))
            ct_delta = tot_ct - first_cpu
            grp.meta.setdefault("cpu_from_cputime_delta", {})[cfg] = {
                "window_s": round(el, 1), "cpu_seconds": round(ct_delta, 3),
                "pct_of_one_core": round(100 * ct_delta / el, 3), "nproc_last": len(rows)}
            grp.meta.setdefault("proc_commands_last", {})[cfg] = sorted({re.sub(r"^\S*/", "", r[4].split()[0]) for r in rows})
        except Exception as e:
            grp.note(cfg, "idle", "FAILED: %s" % str(e)[:300])
        finally:
            for s in sess:
                s.close()
        print("[idle_resource] %s done" % cfg, flush=True)
    procs.meta = {}
    os.makedirs(a.out, exist_ok=True)
    with open(os.path.join(a.out, "idle_resource_procs.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(procs.header)
        w.writerows(procs.rows)
    return grp


GROUPS = {"spawn_cost": g_spawn_cost, "prompt_lag": g_prompt_lag, "first_prompt": g_first_prompt,
          "idle_resource": g_idle_resource, "instant_prompt": g_instant_prompt}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--group", default="all", choices=list(GROUPS) + ["all"])
    ap.add_argument("--samples", type=int, default=0, help="override sample count (not idle_resource)")
    ap.add_argument("--out", default=os.path.join(BENCH, "results"))
    ap.add_argument("--quick", action="store_true", help="small sample counts for development")
    a = ap.parse_args()
    if a.quick and a.out == os.path.join(BENCH, "results"):
        a.out = os.path.join(BENCH, "results-quick")
    names = list(GROUPS) if a.group == "all" else [a.group]
    tmp = tempfile.mkdtemp(prefix="tildybench-")
    PL_CONFIG[0] = os.path.join(tmp, "powerline.json")
    with open(PL_CONFIG[0], "w") as f:
        f.write('{"preset":"pastel-powerline"}')
    INSTANT_CACHE[0] = os.path.join(tmp, "cache")
    INSTANT_FILE[0] = os.path.join(tmp, "tildy-instant.zsh")
    if os.path.exists(TILDY_BIN):
        with open(INSTANT_FILE[0], "w") as f:
            f.write(run_out([TILDY_BIN, "init", "zsh", "--instant"]) + "\n")
    try:
        fx, meta = make_fixtures(tmp)
        for name in names:
            env = environment()
            env["fixtures"] = meta
            env["configs_missing"] = {c: missing_reason(c) for c in CONFIGS if missing_reason(c)}
            t0 = time.time()
            grp = GROUPS[name](a, env, fx, meta)
            grp.meta["duration_s"] = round(time.time() - t0, 1)
            grp.write(env, a.quick)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
