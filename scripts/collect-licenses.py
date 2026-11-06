#!/usr/bin/env python3
"""Collect locked Cargo dependency notices without publishing registry paths."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess


def collect(output: Path) -> None:
    packages = {}
    for target in ('aarch64-apple-darwin', 'x86_64-apple-darwin',
                   'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu'):
        metadata = json.loads(subprocess.check_output(
            ['cargo', 'metadata', '--locked', '--format-version', '1',
             '--filter-platform', target], text=True))
        nodes = {node['id']: node for node in metadata['resolve']['nodes']}
        pending = [metadata['resolve']['root']]
        included = set()
        while pending:
            ident = pending.pop()
            if ident in included:
                continue
            included.add(ident)
            pending.extend(dep['pkg'] for dep in nodes[ident]['deps']
                           if any(kind['kind'] != 'dev' for kind in dep['dep_kinds']))
        packages.update((p['id'], p) for p in metadata['packages'] if p['id'] in included)
    output.mkdir(parents=True, exist_ok=False)
    index = []
    for package in sorted(packages.values(), key=lambda p: (p['name'], p['version'])):
        if package['source'] is None:
            continue
        root = Path(package['manifest_path']).parent
        files = {p for p in root.iterdir() if p.is_file()
                 and p.name.lower().startswith(('license', 'licence', 'copying', 'notice', 'authors'))}
        if package.get('license_file'):
            files.add(root / package['license_file'])
        # Some crates keep license texts in a dedicated directory.
        for directory in ('licenses', 'LICENSES'):
            if (root / directory).is_dir():
                files.update(p for p in (root / directory).rglob('*') if p.is_file())
        if not files:
            raise SystemExit(f"No license text for {package['name']} {package['version']}")
        dest = output / f"{package['name']}-{package['version']}"
        dest.mkdir()
        names = []
        for path in sorted(files):
            relative = path.relative_to(root)
            target = dest / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
            names.append(str(relative))
        index.append({'name': package['name'], 'version': package['version'],
                      'license': package['license'], 'repository': package['repository'],
                      'files': names})
    (output / 'index.json').write_text(json.dumps(index, indent=2) + '\n')
    print(f'Collected notices for {len(index)} locked dependencies.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    collect(parser.parse_args().output)
