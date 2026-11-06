#!/usr/bin/env python3
"""Interactive-shell startup timeline, with and without tildy.

Starts `zsh -il` in a pty the way a terminal does, with the user's own
startup files copied into a throw-away ZDOTDIR (so both variants run the
same files; the "plain" variant has the two `tildy init zsh` lines removed).
A precmd hook appended to the copied .zshrc prints a marker once; precmd runs
right before the line editor starts reading, so that moment is when the shell
could actually execute something. `--typeahead` instead types a command into
the pty before the shell has read anything and checks that it runs exactly
once (keys typed during startup must survive; terminal-title escapes that
echo the command text are stripped before matching).

Per run (milliseconds from spawn):
  first_byte   first output byte (not necessarily the prompt)
  zshrc_start  .zshenv/.zprofile finished, before .zshrc initialization
  instant      tildy debug log: cached prompt painted (tildy variant only)
  executable   first precmd ran (the line editor is about to read input)
  fresh        tildy debug log: first fresh result received (not a paint timestamp)

Usage: bench/startup.py [--runs N] [--binary PATH] [--json] [--typeahead]
"""
import argparse, glob, hashlib, json, os, platform, pty, re, select, shlex, shutil, signal, socket, statistics, subprocess, sys, tempfile, time
from pathlib import Path

HOME = os.path.expanduser("~")
MARK_OUT = re.compile(rb"__READY__")
TYPED_CMD = b"print -r -- __TY''PED__\n"
TYPED_OUT = re.compile(rb"(?:^|\n|\r)__TYPED__\r?\n")
ESCAPES = re.compile(rb"\x1b\][^\x07]*\x07|\x1b\[[0-9;?]*[A-Za-z]")
READY_HOOK = """
# bench/startup.py: mark the first precmd (just before zle reads input)
_bench_ready() { print -r -- __READY__; precmd_functions=(${precmd_functions:#_bench_ready}) }
precmd_functions+=(_bench_ready)
"""
TILDY_LINE = re.compile(r"^[ \t]*eval \"\$\((?:[^\n]*[/\"'])?tildy[\"']? init zsh(?P<instant> --instant)?\)\"[ \t]*$", re.M)
TRACE_START = '''
zmodload zsh/datetime
printf '%.0f\\tbench_zshrc\\n' "$(( EPOCHREALTIME * 1000 ))" >> "$TILDY_DEBUG_LOG"
'''


def prepare(zdot, with_tildy, binary=None):
    os.makedirs(zdot, exist_ok=True)
    # Never point compinit at a compiled .zwc (or at the user's writable cache).
    # Each variant owns its dump, so rebuilds cannot damage daily-use caches or
    # invalidate the other variant's next sample.
    if (source := compdump()):
        target = os.path.join(zdot, ".zcompdump")
        shutil.copy2(source, target)
        compiled = source + ".zwc"
        if os.path.isfile(compiled) and os.path.getmtime(compiled) > os.path.getmtime(source):
            shutil.copy2(compiled, target + ".zwc")
    for name in (".zshenv", ".zprofile", ".zshrc", ".zlogin"):
        src = os.path.join(HOME, name)
        if not os.path.exists(src):
            continue
        text = Path(src).read_text(encoding="utf-8", errors="replace")
        if name == ".zshrc" and with_tildy:
            matches = list(TILDY_LINE.finditer(text))
            if len(matches) != 2 or sum(bool(m["instant"]) for m in matches) != 1:
                raise ValueError("expected one instant and one regular tildy init line in .zshrc")
        if not with_tildy:
            text = TILDY_LINE.sub("# (tildy line removed by bench/startup.py)", text)
        elif binary:
            text = TILDY_LINE.sub(lambda m: 'eval "$(' + shlex.quote(binary) + ' init zsh' + (m['instant'] or '') + ')"', text)
        if name == ".zshrc":
            text = TRACE_START + text + READY_HOOK
        with open(os.path.join(zdot, name), "w", encoding="utf-8") as f:
            f.write(text)
    if not os.path.exists(os.path.join(zdot, ".zshrc")):
        raise ValueError("a .zshrc containing tildy initialization is required")


def compdump():
    # OMZ's source dump has no suffix. .zwc, .zwc.old, and lock files can all
    # be newer than it; choosing any of them forces a rebuild on every run.
    candidates = glob.glob(os.path.join(HOME, ".zcompdump*"))
    c = []
    for name in candidates:
        if not re.fullmatch(r"\.zcompdump(?:-.*-\d+(?:\.\d+)*)?", Path(name).name):
            continue
        try:
            with open(name, "rb") as f:
                header = f.readline(128)
            if re.fullmatch(rb"#files: \d+\s+version: [\d.]+\s*", header):
                c.append(name)
        except OSError:
            continue
    c.sort(key=os.path.getmtime)
    return c[-1] if c else None


def shell_env(zdot):
    env = {k: v for k, v in os.environ.items() if not k.startswith(("TILDY_", "ZDOTDIR"))}
    # Tool runners often export TERM=dumb; the PTY models a real terminal.
    term = env.get("TERM")
    env.update(ZDOTDIR=zdot, TERM=term if term and term != "dumb" else "xterm-256color",
               COLUMNS="100", LINES="40", TILDY_DEBUG_LOG=os.path.join(zdot, "tildy-debug.log"))
    env.setdefault("TERM_PROGRAM", "ghostty")
    env["ZSH_COMPDUMP"] = os.path.join(zdot, ".zcompdump")
    return env


def close_shell(fd, pid):
    """A broken initialization must not hang the benchmark in waitpid."""
    try:
        os.write(fd, b"exit\n")
    except OSError:
        pass
    # Darwin may keep an exiting PTY session in tty drain until the master
    # closes. Close before waitpid, including the forced-termination path.
    time.sleep(0.1)
    os.close(fd)
    deadline = time.monotonic() + 1
    while time.monotonic() < deadline:
        try:
            if os.waitpid(pid, os.WNOHANG)[0]:
                break
        except ChildProcessError:
            break
        time.sleep(0.01)
    else:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)


def typeahead(zdot, timeout=6.0):
    """Type a command before the shell has read anything; True if it ran exactly once."""
    import fcntl, termios, struct
    env = shell_env(zdot)
    fd, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 100, 0, 0))
    pid = os.fork()
    if pid == 0:
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        for n in (0, 1, 2):
            os.dup2(slave, n)
        os.chdir(HOME)
        os.execvpe("zsh", ["zsh", "-il"], env)
    os.close(slave)
    os.write(fd, TYPED_CMD)
    out = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.05)
        if not r:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
        if TYPED_OUT.search(ESCAPES.sub(b"", out)):
            time.sleep(0.3)  # let a possible second execution show up
            if select.select([fd], [], [], 0)[0]:
                try:
                    out += os.read(fd, 65536)
                except OSError:
                    pass
            break
    close_shell(fd, pid)
    return len(TYPED_OUT.findall(ESCAPES.sub(b"", out))) == 1


def one_run(zdot, with_tildy, timeout=15.0):
    log = os.path.join(zdot, "tildy-debug.log")
    env = shell_env(zdot)
    Path(log).write_text("")
    import fcntl, termios, struct
    fd, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 100, 0, 0))  # before exec: no SIGWINCH
    t0 = time.time()
    pid = os.fork()
    if pid == 0:
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        for n in (0, 1, 2):
            os.dup2(slave, n)
        os.chdir(HOME)
        os.execvpe("zsh", ["zsh", "-il"], env)
    os.close(slave)
    out = b""
    first = executable = None
    deadline = t0 + timeout
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.05)
        if not r:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        now = time.time()
        if first is None:
            first = now
        out += chunk
        if executable is None and MARK_OUT.search(out):
            executable = now
            if not with_tildy:
                break
        if with_tildy and executable is not None and b"\tfresh\t" in Path(log).read_bytes():
            break  # the first fresh result arrived
    close_shell(fd, pid)
    ms = lambda t: None if t is None else round((t - t0) * 1000)
    row = {"first_byte": ms(first), "executable": ms(executable)}
    ev = [l.split("\t") for l in Path(log).read_text(encoding="utf-8", errors="replace").splitlines()]
    base = int(t0 * 1000)
    def at(kind):
        return next((int(e[0]) - base for e in ev if len(e) > 1 and e[1] == kind), None)
    row["zshrc_start"] = at("bench_zshrc")
    if with_tildy:
        row["instant"] = at("instant")
        row["fresh"] = next((int(e[0]) - base for e in ev if len(e) > 3 and e[1] == "recv" and e[3] == "fresh"), None)
    return row


def median(rows, key):
    xs = [r[key] for r in rows if r.get(key) is not None]
    return round(statistics.median(xs)) if xs else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--typeahead", action="store_true", help="check that keys typed during startup are kept (tildy variant)")
    ap.add_argument("--binary", help="measure this binary instead of the one named in .zshrc")
    a = ap.parse_args()
    if a.runs < 1:
        ap.error("--runs must be positive")
    binary = os.path.abspath(a.binary) if a.binary else None
    metadata = {"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "platform": platform.platform(),
                "runs": a.runs, "order": "alternating plain/tildy; reversed every second pair",
                "term": shell_env("")["TERM"], "term_program": shell_env("")["TERM_PROGRAM"],
                "binary": binary, "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest() if binary else None,
                "shell": subprocess.check_output(["zsh", "--version"], text=True).strip(),
                "startup_sha256": {name: hashlib.sha256(Path(HOME, name).read_bytes()).hexdigest()
                                   for name in (".zshenv", ".zprofile", ".zshrc", ".zlogin") if os.path.isfile(os.path.join(HOME, name))}}
    tmp = tempfile.mkdtemp(prefix="tildy-startup-")
    result = {}
    if a.typeahead:
        try:
            zdot = os.path.join(tmp, "tildy")
            prepare(zdot, True, binary)
            one_run(zdot, True)  # warm-up
            kept = [typeahead(zdot) for _ in range(a.runs)]
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        if a.json:
            print(json.dumps({"metadata": metadata, "kept": kept, "passed": all(kept)}, indent=2))
        else:
            print(f"typed-ahead command kept and run once: {sum(kept)}/{len(kept)}")
        sys.exit(0 if all(kept) else 1)
    try:
        for label, with_tildy in (("plain", False), ("tildy", True)):
            zdot = os.path.join(tmp, label)
            prepare(zdot, with_tildy, binary)
            one_run(zdot, with_tildy)  # warm-up (caches, compdump, instant cache)
            result[label] = {"runs": []}
        variants = [("plain", False), ("tildy", True)]
        for i in range(a.runs):
            for label, with_tildy in variants[::1 if i % 2 == 0 else -1]:
                result[label]["runs"].append(one_run(os.path.join(tmp, label), with_tildy))
        for entry in result.values():
            entry["median"] = {k: median(entry["runs"], k) for k in ("first_byte", "zshrc_start", "instant", "executable", "fresh")}
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    valid = all(row.get(k) is not None for label in ("plain", "tildy") for row in result[label]["runs"]
                for k in (("executable", "zshrc_start", "instant", "fresh") if label == "tildy" else ("executable", "zshrc_start")))
    result["valid"] = valid
    if a.json:
        result["metadata"] = metadata
        print(json.dumps(result, indent=2))
        return 0 if valid else 1
    p, s = result["plain"]["median"], result["tildy"]["median"]
    print(f"zsh -il startup on {socket.gethostname()}, {a.runs} runs each (median ms from spawn)")
    print(f"{'':12}{'plain':>10}{'tildy':>10}")
    for k, name in (("first_byte", "first byte"), ("instant", "instant painted"), ("executable", "executable"), ("fresh", "fresh prompt")):
        print(f"{name:12}{str(p.get(k) or '-'):>10}{str(s.get(k) or '-'):>10}")
    if p["executable"] and s["executable"]:
        print(f"\ntildy adds {s['executable'] - p['executable']} ms before the shell can execute a command; "
              f"the cached prompt is on screen {s['executable'] - (s['instant'] or 0)} ms before that.")
    if not valid:
        print("incomplete startup timeline; missing samples are not an acceptance result", file=sys.stderr)
    return 0 if valid else 1


if __name__ == "__main__":
    sys.exit(main())
