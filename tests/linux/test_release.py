"""Build-only stages four matching assets without invoking GitHub or Git."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest


class ReleaseTests(unittest.TestCase):
    def test_published_version_is_rejected_before_building(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'scripts').mkdir()
            (root / 'bin').mkdir()
            (root / 'tap/.git').mkdir(parents=True)
            source = Path(__file__).resolve().parents[2]
            shutil.copy(source / 'scripts/release.sh', root / 'scripts/release.sh')
            shutil.copy(source / 'scripts/package-archive.py', root / 'scripts/package-archive.py')
            (root / 'Cargo.toml').write_text('version = "9.8.7"\n')
            (root / 'LICENSE').write_text('project license')
            (root / 'NOTICE').write_text('third-party notice')
            (root / 'licenses').mkdir()
            (root / 'licenses/starship.txt').write_text('upstream notice')
            (root / 'scripts/collect-licenses.py').write_text('import pathlib,sys\np=pathlib.Path(sys.argv[1]); p.mkdir(); (p / "index.json").write_text("[]")\n')
            for tool, body in {
                'git': 'exit 0',  # Both checkouts are clean.
                'gh': 'test "$1 $2" = "release view"',
                'cargo': 'touch unexpected-build; exit 99',
            }.items():
                path = root / 'bin' / tool
                path.write_text('#!/usr/bin/env bash\n' + body + '\n')
                path.chmod(0o755)
            env = {**os.environ, 'PATH': str(root / 'bin') + ':' + os.environ['PATH']}
            result = subprocess.run(['bash', 'scripts/release.sh', 'tap'],
                                    cwd=root, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
            self.assertIn('already exists', result.stderr)
            self.assertFalse((root / 'unexpected-build').exists())

    def test_build_only_stages_formula_without_publishing(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'scripts').mkdir()
            (root / 'bin').mkdir()
            source = Path(__file__).resolve().parents[2]
            shutil.copy(source / 'scripts/release.sh', root / 'scripts/release.sh')
            shutil.copy(source / 'scripts/package-archive.py', root / 'scripts/package-archive.py')
            (root / 'Cargo.toml').write_text('version = "9.8.7"\n')
            (root / 'LICENSE').write_text('project license')
            (root / 'NOTICE').write_text('third-party notice')
            (root / 'licenses').mkdir()
            (root / 'licenses/starship.txt').write_text('upstream notice')
            (root / 'scripts/collect-licenses.py').write_text('import pathlib,sys\np=pathlib.Path(sys.argv[1]); p.mkdir(); (p / "index.json").write_text("[]")\n')

            def script(path, body):
                path.write_text('#!/usr/bin/env bash\nset -eu\n' + body)
                path.chmod(0o755)

            script(root / 'bin/cargo', '''
while [ "$1" != --target ]; do shift; done
build="${CARGO_TARGET_DIR:-target}/$2/release"
mkdir -p "$build"
echo "$2" > "$build/tildy"
''')
            script(root / 'scripts/package-linux.sh', '''
for target in aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  mkdir -p "target/$target/release"
  echo "$target" > "target/$target/release/tildy"
  python3 scripts/package-archive.py "$1/tildy-9.8.7-$target.tar.gz" "target/$target/release/tildy" "$TILDY_LICENSE_DIR"
done
''')
            for tool in ('gh', 'git'):
                script(root / 'bin' / tool, 'touch forbidden-external-call\nexit 99\n')
            env = {**os.environ, 'PATH': str(root / 'bin') + ':' + os.environ['PATH'],
                   'CARGO_TARGET_DIR': str(root / 'unexpected-target')}
            result = subprocess.run(['bash', 'scripts/release.sh', '--build-only'],
                                    cwd=root, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse((root / 'forbidden-external-call').exists())
            out = root / 'target/release-assets/9.8.7'
            assets = list(out.glob('*.tar.gz'))
            self.assertEqual(len(assets), 4)
            formula = (out / 'tildy.rb').read_text()
            self.assertIn('on_linux do', formula)
            self.assertIn('on_macos do', formula)
            for asset in assets:
                self.assertIn(asset.name, formula)
                self.assertIn(hashlib.sha256(asset.read_bytes()).hexdigest(), formula)
                with tarfile.open(asset) as archive:
                    names = archive.getnames()
                    self.assertIn('tildy', names)
                    self.assertIn('LICENSE', names)
                    self.assertIn('NOTICE', names)
                    self.assertIn('licenses/starship.txt', names)
                    self.assertIn('licenses/dependencies/index.json', names)
                    self.assertFalse(any(Path(n).name.startswith('._') for n in names))
                    self.assertEqual(archive.extractfile('NOTICE').read(), b'third-party notice')
                    self.assertTrue(all(m.uid == m.gid == m.mtime == 0 and not m.uname and not m.gname for m in archive.getmembers()))


if __name__ == '__main__':
    unittest.main()
