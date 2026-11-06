#!/usr/bin/env python3
"""Check/export the current source snapshot, excluding Git history and local state."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
ROOT_FILES = {'.gitattributes', '.gitignore', 'Cargo.toml', 'Cargo.lock', 'LICENSE',
              'NOTICE', 'README.md', 'README.zh-CN.md', 'CHANGELOG.md', 'CONTRIBUTING.md',
              'PLAN.md', 'ROADMAP.md'}
DIRECTORIES = {'.github', 'bench', 'docs', 'fixtures', 'licenses', 'schema',
               'scripts', 'shell', 'src', 'tests'}
PATTERNS = {
    'credential': re.compile(rb'gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}|'
                             rb'sk-(?:proj-|ant-)?[A-Za-z0-9_-]{32,}|'
                             rb'-----BEGIN (?:OPENSSH |RSA |EC )?PRIVATE KEY-----'),
    'personal home': re.compile(rb'/(?:Users|home)/(?!u/|user/|you/|test/|alice/|bob/|me/|meme/|runner/|example/|linuxbrew/)'
                                rb'[A-Za-z][A-Za-z0-9_.-]*/'),
    'private chat link': re.compile(b'chatgpt' + b'-conversation://'),
}


def source_files() -> list[Path]:
    result = subprocess.check_output(['git', 'ls-files', '--cached', '--others',
                                      '--exclude-standard', '-z'], cwd=ROOT)
    files = []
    for name in sorted(set(result.decode().split('\0')) - {''}):
        # Recompute this derived file rather than including a stale manifest.
        if name == 'SOURCE-MANIFEST.json':
            continue
        relative = Path(name)
        path = ROOT / relative
        if not path.exists() and not path.is_symlink():
            continue
        if name not in ROOT_FILES and relative.parts[0] not in DIRECTORIES:
            raise SystemExit(f'Unexpected source entry: {name}')
        if path.is_symlink() or not path.is_file():
            raise SystemExit(f'Unsupported source entry: {name}')
        if 'evidence' in relative.parts or relative.parts[:2] == ('bench', 'results'):
            raise SystemExit(f'Raw private evidence in source set: {name}')
        data = path.read_bytes()
        for label, pattern in PATTERNS.items():
            if pattern.search(data):
                raise SystemExit(f'{label} match in {name}; review before exporting')
        files.append(relative)
    return files


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path, nargs='?')
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    files = source_files()
    if args.check:
        print(f'Source hygiene checks passed for {len(files)} files (not a full security audit).')
        return
    if args.output is None:
        parser.error('OUTPUT is required unless --check is used')
    dest = args.output.resolve()
    if dest == ROOT or ROOT in dest.parents:
        parser.error('OUTPUT must be outside the development checkout')
    dest.mkdir(parents=True, exist_ok=False)
    manifest = {}
    for relative in files:
        target = dest / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / relative, target)
        data = target.read_bytes()
        manifest[str(relative)] = {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
    (dest / 'SOURCE-MANIFEST.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(f'Exported {len(files)} files without Git history to {dest}')


if __name__ == '__main__':
    main()
