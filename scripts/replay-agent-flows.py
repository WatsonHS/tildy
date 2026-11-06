#!/usr/bin/env python3
"""Replay the documented isolated CLI flow. No authentication or real user config needed."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def replay(binary, root):
    root = Path(root).resolve()
    root.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env.update(HOME=str(root / 'home'), XDG_CONFIG_HOME=str(root / 'config'), XDG_STATE_HOME=str(root / 'state'))
    for key in ['TILDY_CONFIG', 'TILDY_SESSION_ID', 'TILDY_SYMBOLS']:
        env.pop(key, None)
    Path(env['HOME']).mkdir(exist_ok=True)
    transcript = []

    def cli(*args):
        command = [str(binary), *args, '--json']
        p = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True, timeout=20)
        value = json.loads(p.stdout)
        transcript.append({'args': list(args), 'exit': p.returncode, 'result': value})
        assert p.returncode == 0 and value['ok'], (args, p.stdout, p.stderr)
        return value['data']

    capabilities = cli('capabilities')
    assert {'explain', 'timings', 'inspect'} <= {c['name'] for c in capabilities['commands']}
    original = cli('config', 'get')['revision']
    candidate = {
        'schema_version': 1,
        'preset': 'plain',  # This historical example uses plain's ssh_host module.
        'modules': {
            'git': {'rules': [{'id': 'dirty', 'when': {'field': 'git.state', 'op': 'eq', 'value': 'dirty'}, 'style': {'foreground': 'yellow'}}]},
            'ssh_host': {'show_when': {'field': 'session.ssh', 'op': 'eq', 'value': True}},
            'elapsed': {'show_when': {'field': 'command.duration_ms', 'op': 'gt', 'value': 3000}},
        },
    }
    path = root / 'candidate.json'
    path.write_text(json.dumps(candidate, ensure_ascii=False, indent=2) + '\n')
    cli('config', 'validate', '--file', str(path))
    preview = cli('preview', '--config', str(path), '--all')
    assert len(preview) >= 16
    def module(scenario, ident):
        return next(m for scene in preview if scene['scenario'] == scenario for m in scene['modules'] if m['id'] == ident)
    assert 'yellow' in module('git-dirty', 'git')['style']
    assert not module('plain-dir', 'ssh_host')['visible'] and module('ssh', 'ssh_host')['visible']
    assert module('long-command', 'elapsed')['visible']
    cli('config', 'apply', '--file', str(path), '--expect-revision', original)
    applied = cli('config', 'get')['revision']
    assert original != applied
    cli('explain', '--here', '--module', 'ssh_host')
    worker = subprocess.Popen([str(binary), 'worker'], cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        worker.stdin.write(f'REQ\t1\tagent-flow\t{root}\t100\t40\t0\t3200\t\t0\n')
        worker.stdin.flush()
        snapshot = root / 'state/tildy/sessions/agent-flow.json'
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            try:
                data = json.loads(snapshot.read_text())
                if data.get('module_reports') and data.get('timing_history') and all(not s['fetching'] for s in data['slots']):
                    break
            except (FileNotFoundError, json.JSONDecodeError):
                pass
            time.sleep(0.05)
        else:
            raise AssertionError('worker did not produce reports and cycle timings')
        sessions = cli('inspect', '--list')
        assert any(s['snapshot']['session_id'] == 'agent-flow' for s in sessions)
        cli('inspect', '--session', 'agent-flow')
        explanation = cli('explain', '--session', 'agent-flow')
        assert all(m['reason'] for m in explanation['modules'])
        timings = cli('timings', '--session', 'agent-flow', '--last', '2')
        assert timings and all(isinstance(p['executed'], bool) for c in timings for p in c['providers'])
        (root / 'last-snapshot.json').write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n')
    finally:
        worker.stdin.write('BYE\n')
        worker.stdin.flush()
        worker.communicate(timeout=5)
    cli('config', 'rollback', '--to', original, '--expect-revision', applied)
    restored = cli('config', 'get')['revision']
    assert restored == original
    (root / 'cli-transcript.json').write_text(json.dumps(transcript, ensure_ascii=False, indent=2) + '\n')
    result = {'ok': True, 'original_revision': original, 'applied_revision': applied, 'restored_revision': restored, 'commands': len(transcript), 'root': str(root)}
    print(json.dumps(result))
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--root', type=Path)
    args = parser.parse_args()
    if args.root:
        replay(args.binary.resolve(), args.root)
    else:
        with tempfile.TemporaryDirectory(prefix='tildy-agent-flow-') as root:
            replay(args.binary.resolve(), root)
