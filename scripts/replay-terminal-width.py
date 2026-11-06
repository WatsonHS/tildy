#!/usr/bin/env python3
"""Explicit real-terminal M3 width acceptance. Run inside a dedicated Ghostty window.

Only --out is written. Calibration and validation use /dev/tty, so this script
can also redirect stdout. It never modifies shell startup files or user config.
"""
import argparse
import json
import os
from pathlib import Path
import re
import select
import subprocess
import termios
import time
import tty


def json_attrs(fd):
    attrs = termios.tcgetattr(fd)
    return attrs[:6] + [[x if isinstance(x, int) else x.hex() for x in attrs[6]]]


def normalized_attrs(attrs):
    normalized = json.loads(json.dumps(attrs))
    normalized[3] &= ~getattr(termios, "PENDIN", 0)
    return normalized


def query(fd):
    os.write(fd, b'\x1b[6n')
    deadline = time.monotonic() + 0.4
    reply = b''
    while time.monotonic() < deadline:
        if not select.select([fd], [], [], 0.01)[0]:
            continue
        reply += os.read(fd, 64)
        match = re.fullmatch(rb'\x1b\[(\d+);(\d+)R', reply)
        if match:
            return tuple(map(int, match.groups()))
        if len(reply) > 32 or any(x not in b'\x1b[;R0123456789' for x in reply):
            raise RuntimeError('interfering keyboard/terminal input during cursor validation')
    raise RuntimeError('cursor validation timed out')


def cursor_advance(fd, data):
    saved = termios.tcgetattr(fd)
    try:
        tty.setraw(fd, termios.TCSANOW)
        os.write(fd, b'\x1b[?1049h\x1b[2;2H\x1b[2K')
        start = query(fd)
        if start != (2, 2):
            raise RuntimeError('terminal did not confirm the known cursor position')
        os.write(fd, data)
        end = query(fd)
        if end[0] != start[0]:
            raise RuntimeError('validation text wrapped unexpectedly')
        return end[1] - start[1]
    finally:
        # Bounded drain only. CPR has no transaction IDs; replies arriving
        # arbitrarily later cannot be completely isolated by this protocol.
        until = time.monotonic() + 0.2
        while time.monotonic() < until:
            if select.select([fd], [], [], 0.01)[0]:
                os.read(fd, 256)
        os.write(fd, b'\x1b[?1049l')
        termios.tcsetattr(fd, termios.TCSANOW, saved)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--font-revision', default='ghostty-default-m3')
    args = parser.parse_args()
    binary = args.binary.resolve()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.update(XDG_CACHE_HOME=str(out / 'cache'), XDG_STATE_HOME=str(out / 'state'),
                       XDG_CONFIG_HOME=str(out / 'config'), TILDY_CONFIG=str(out / 'config/config.json'),
                       TILDY_FONT_REVISION=args.font_revision)
    fd = os.open('/dev/tty', os.O_RDWR)
    before = json_attrs(fd)
    result = {'ok': False, 'terminal': {k: environment.get(k, '') for k in ['TERM', 'TERM_PROGRAM', 'TERM_PROGRAM_VERSION', 'LANG']},
              'font_revision': args.font_revision, 'termios_before': before, 'commands': []}

    def cli(label, *argv):
        command = [str(binary), *map(str, argv)]
        process = subprocess.run(command, env=environment, cwd=out, capture_output=True, timeout=8)
        (out / (label + '.stdout')).write_bytes(process.stdout)
        (out / (label + '.stderr')).write_bytes(process.stderr)
        result['commands'].append({'argv': command, 'exit': process.returncode})
        if process.returncode:
            raise RuntimeError(label + ' failed: ' + process.stdout.decode(errors='replace'))
        return json.loads(process.stdout)

    try:
        calibration = cli('calibration', 'calibrate', '--font-revision', args.font_revision, '--json')
        entries = calibration['data']['entries']
        cluster_text = ''.join(entry['cluster'] for entry in entries)
        measured = sum(entry['columns'] for entry in entries)
        result['calibration'] = calibration
        config = {'schema_version': 1, 'symbols': 'unicode', 'add_newline': False,
                  'modules': {'left': {'type': 'text', 'text': cluster_text, 'priority': 100},
                              'right': {'type': 'text', 'text': 'RIGHT', 'priority': 1}},
                  'layout': {'rows': [{'left': ['left'], 'right': ['right']}]}}
        config_path = out / 'mixed-width.json'
        config_path.write_text(json.dumps(config, ensure_ascii=False, indent=2) + '\n')
        previews = {}
        for columns in [40, measured + 4]:
            preview = cli('preview-' + str(columns), 'preview', '--config', config_path, '--scenario', 'plain-dir', '--cols', columns, '--json')['data']
            left = next(module for module in preview['modules'] if module['id'] == 'left')
            right = next(module for module in preview['modules'] if module['id'] == 'right')
            assert left['columns'] == measured and left['visible'], (left, measured)
            assert right['visible'] == (columns == 40), right
            if columns != 40:
                assert right['hidden_reason'] == 'no_room', right
            # Expand actual Zsh prompt text (including measured %NG groups).
            expanded = subprocess.check_output(['/bin/zsh', '-dfc', 'print -Pn -- "$1"', 'width-check', preview['lines'][0]], env=environment)
            # Zsh's prompt-width condition counts the complete rendered line,
            # including the automatically appended prompt character/spaces.
            width_program = r'''p=$1; integer COLUMNS=1024 x=0 y=${#p} m
while (( ${${(%):-$p%$y(l.1.0)}[-1]} )); do x=y; (( y *= 2 )); done
while (( y > x+1 )); do (( m=x+(y-x)/2 )); (( ${${(%):-$p%$m(l.x.y)}[-1]} = m )); done
print -rn -- $x'''
            expected = int(subprocess.check_output(['/bin/zsh', '-dfc', width_program, 'width-count', preview['lines'][0]], env=environment))
            physical = cursor_advance(fd, expanded)
            assert physical == expected and expected <= columns, (physical, expected, columns)
            previews[str(columns)] = {'left': left, 'right': right, 'physical_left_columns': physical, 'zsh_full_prompt_columns': expected}
        result['previews'] = previews
        result['ok'] = True
    except Exception as error:
        result['error'] = str(error)
    finally:
        result['termios_after'] = json_attrs(fd)
        result['termios_normalized_before'] = normalized_attrs(before)
        result['termios_normalized_after'] = normalized_attrs(result['termios_after'])
        result['termios_ignored_kernel_flags'] = {'PENDIN': getattr(termios, 'PENDIN', 0)}
        result['termios_raw_equal'] = before == result['termios_after']
        result['termios_restored'] = result['termios_normalized_before'] == result['termios_normalized_after']
        result['ok'] &= result['termios_restored']
        (out / 'result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
        os.close(fd)
    print(json.dumps({'ok': result['ok'], 'result': str(out / 'result.json')}))
    return 0 if result['ok'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
