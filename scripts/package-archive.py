#!/usr/bin/env python3
"""Create a deterministic binary archive without local owner/path metadata."""
import argparse
import gzip
from pathlib import Path
import tarfile


def package(output: Path, binary: Path, notices: Path) -> None:
    def normalize(info: tarfile.TarInfo) -> tarfile.TarInfo:
        if not (info.isfile() or info.isdir()):
            raise ValueError(f'Unsupported archive entry: {info.name}')
        info.uid = info.gid = 0
        info.uname = info.gname = ''
        info.mtime = 0
        info.mode = 0o755 if info.isdir() or info.name == 'tildy' else 0o644
        info.pax_headers = {}
        return info

    with output.open('wb') as raw:
        with gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode='w', format=tarfile.PAX_FORMAT) as archive:
                archive.add(binary, arcname='tildy', recursive=False, filter=normalize)
                for name in ('LICENSE', 'NOTICE', 'licenses'):
                    archive.add(notices / name, arcname=name, filter=normalize)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('binary', type=Path)
    parser.add_argument('notices', type=Path)
    args = parser.parse_args()
    package(args.output, args.binary, args.notices)
