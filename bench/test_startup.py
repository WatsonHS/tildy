"""Protect the startup comparison from missing/incorrect Tildy initialization."""
import os
from pathlib import Path
import tempfile
import subprocess
import sys
import unittest
from unittest.mock import patch

import startup


class StartupProbeTests(unittest.TestCase):
    def test_plain_removes_only_the_two_initialization_calls(self):
        with tempfile.TemporaryDirectory() as root, patch.object(startup, "HOME", root):
            original = '# tildy configuration\neval "$(tildy init zsh --instant)"\nsource plugins.zsh\neval "$(\'/some path/tildy\' init zsh)"\n'
            Path(root, ".zshrc").write_text(original)
            dest = Path(root, "plain")
            startup.prepare(str(dest), False)
            copied = (dest / ".zshrc").read_text()
            self.assertNotIn("init zsh", copied)
            self.assertIn("source plugins.zsh", copied)
            self.assertEqual(Path(root, ".zshrc").read_text(), original)

    def test_explicit_binary_replaces_both_calls_and_preserves_instant(self):
        with tempfile.TemporaryDirectory() as root, patch.object(startup, "HOME", root):
            Path(root, ".zshrc").write_text('eval "$(/old/tildy init zsh --instant)"\neval "$(tildy init zsh)"\n')
            dest = Path(root, "tildy")
            startup.prepare(str(dest), True, "/candidate build/prompt")
            copied = (dest / ".zshrc").read_text()
            self.assertEqual(copied.count("'/candidate build/prompt' init zsh"), 2)
            self.assertEqual(copied.count("--instant"), 1)
            self.assertNotIn("/old/tildy", copied)

    def test_missing_initialization_fails_instead_of_measuring_plain_twice(self):
        with tempfile.TemporaryDirectory() as root, patch.object(startup, "HOME", root):
            Path(root, ".zshrc").write_text("source plugins.zsh\n")
            with self.assertRaises(ValueError):
                startup.prepare(str(Path(root, "tildy")), True)

    def test_tool_runner_dumb_terminal_does_not_disable_instant_prompt(self):
        with patch.dict(os.environ, {"TERM": "dumb", "TILDY_INSTANT_PROMPT": "off"}):
            env = startup.shell_env("/tmp/probe")
            self.assertEqual(env["TERM"], "xterm-256color")
            self.assertNotIn("TILDY_INSTANT_PROMPT", env)
            self.assertEqual(env["ZDOTDIR"], "/tmp/probe")

    def test_completion_dump_ignores_newer_compiled_backups_and_lock(self):
        with tempfile.TemporaryDirectory() as root, patch.object(startup, "HOME", root):
            dump = Path(root, ".zcompdump-host-5.9")
            dump.write_text("#files: 1234\tversion: 5.9\n")
            os.utime(dump, (1, 1))
            for suffix in (".zwc", ".zwc.old", ".zwc.old.zwc"):
                # Even a backup with a text header must not be selected.
                Path(str(dump) + suffix).write_text(dump.read_text())
            Path(str(dump) + ".lock").mkdir()
            Path(root, ".zcompdump-other-5.9").write_bytes(b"not a source dump")
            self.assertEqual(startup.compdump(), str(dump))

    def test_completion_caches_are_isolated_from_home_and_other_variant(self):
        with tempfile.TemporaryDirectory() as root, patch.object(startup, "HOME", root):
            Path(root, ".zshrc").write_text('eval "$(tildy init zsh --instant)"\neval "$(tildy init zsh)"\n')
            dump = Path(root, ".zcompdump-host-5.9")
            original = "#files: 1234\tversion: 5.9\n"
            dump.write_text(original)
            os.utime(dump, (1, 1))
            compiled = Path(str(dump) + ".zwc")
            compiled.write_bytes(b"compiled")
            for label in ("plain", "tildy"):
                dest = Path(root, label)
                startup.prepare(str(dest), label == "tildy")
                self.assertEqual(startup.shell_env(str(dest))["ZSH_COMPDUMP"], str(dest / ".zcompdump"))
                self.assertEqual((dest / ".zcompdump.zwc").read_bytes(), b"compiled")
            Path(root, "plain", ".zcompdump").write_text("rebuilt")
            self.assertEqual(dump.read_text(), original)
            self.assertEqual(Path(root, "tildy", ".zcompdump").read_text(), original)

    def test_absent_completion_cache_still_uses_private_dump(self):
        with tempfile.TemporaryDirectory() as root, patch.object(startup, "HOME", root):
            self.assertIsNone(startup.compdump())
            with patch.dict(os.environ, {"ZSH_COMPDUMP": "/do/not/write/here"}):
                self.assertEqual(startup.shell_env(root)["ZSH_COMPDUMP"], str(Path(root, ".zcompdump")))

    def test_cleanup_reaps_normal_and_hung_pty_sessions(self):
        # A separate interpreter's timeout also bounds a cleanup regression.
        script = '''
import fcntl, os, pty, termios, time
import startup
for command in ("exit", "trap '' HUP; sleep 10"):
    master, slave = pty.openpty()
    pid = os.fork()
    if pid == 0:
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        for fd in (0, 1, 2): os.dup2(slave, fd)
        os.execvp("zsh", ["zsh", "-fc", command])
    os.close(slave)
    time.sleep(.1)
    startup.close_shell(master, pid)
    try:
        os.waitpid(pid, os.WNOHANG)
        raise AssertionError("child was not reaped")
    except ChildProcessError:
        pass
'''
        subprocess.run([sys.executable, "-c", script], cwd=Path(startup.__file__).parent, timeout=5, check=True)


if __name__ == "__main__":
    unittest.main()
