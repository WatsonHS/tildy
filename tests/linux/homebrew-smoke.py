#!/usr/bin/env python3
"""Check an installed keg before/after brew upgrade, using an isolated profile.

Run prepare with the baseline installed, then verify after brew upgrade.
Run fresh with a different profile after a clean brew install. Requires pexpect.
The caller owns tap selection, package downloads and brew install/upgrade/test.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

import pexpect


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=('prepare', 'verify', 'fresh'))
    parser.add_argument('--profile', required=True, type=Path)
    parser.add_argument('--version', required=True)
    parser.add_argument('--prefix', default='/home/linuxbrew/.linuxbrew', type=Path)
    parser.add_argument('--m4', action='store_true', help='also check M4 project layers and import transactions')
    args = parser.parse_args()
    root = args.profile.resolve()
    if args.mode != 'verify':
        root.mkdir(parents=True, exist_ok=False)
    binary = args.prefix / 'bin/tildy'
    env = {**os.environ, 'PATH': str(args.prefix / 'bin') + ':' + os.environ['PATH'],
           'ZDOTDIR': str(root), 'TILDY_CONFIG': str(root / 'config.json'),
           'XDG_CONFIG_HOME': str(root / 'config'), 'XDG_STATE_HOME': str(root / 'state'),
           'XDG_CACHE_HOME': str(root / 'cache'), 'TERM': 'xterm-256color',
           'LANG': 'C.UTF-8', 'TILDY_LANG': 'en', 'TILDY_PROJECT_CONFIG': '1'}

    def run(*argv):
        return subprocess.check_output([str(binary), *argv], env=env, text=True).strip()

    assert run('--version') == 'tildy ' + args.version
    receipt = json.loads((binary.resolve().parents[1] / 'INSTALL_RECEIPT.json').read_text())
    assert receipt['source']['tap'].lower() == 'watsonhs/tildy'
    if args.mode != 'verify':
        assert not (root / 'config.json').exists()
        config = json.loads(run('config', 'get', '--effective', '--json'))['data']['config']
        assert config['preset'] == 'pastel-powerline'
        assert config['add_newline'] is True and len(config['layout']['rows']) == 2
        (root / '.zshrc').write_text('export TILDY_USER_SENTINEL=preserved\n')
        if args.mode == 'prepare':
            run('theme', '--preset', 'plain', '--no-add-newline')
        run('init')

    def hashes():
        return {name: hashlib.sha256((root / name).read_bytes()).hexdigest()
                for name in ('.zshrc', 'config.json') if (root / name).exists()}

    before = hashes()
    if args.mode == 'verify':
        assert before == json.loads((root / 'before.json').read_text())
    run('init')
    run('init')
    assert hashes() == before
    zshrc = (root / '.zshrc').read_text()
    assert zshrc.count('# >>> tildy instant >>>') == 1
    assert zshrc.count('# >>> tildy prompt >>>') == 1
    assert str(binary) in zshrc and '/Cellar/' not in zshrc
    assert json.loads(run('config', 'validate', '--json'))['ok']
    assert json.loads(run('preview', '--scenario', 'git-dirty', '--json'))['ok']

    # Start twice: the second startup exercises the compiled adapter cache.
    with (root / (args.mode + '-zsh.txt')).open('w') as log:
        for _ in range(2):
            child = pexpect.spawn(shutil.which('zsh') or '/bin/zsh', ['-i'], cwd=str(root), env=env,
                                  encoding='utf-8', timeout=20, dimensions=(24, 100))
            child.logfile = log
            try:
                child.expect('❯')
                child.sendline('print -r -- SHELL_VERSION=$TILDY_VERSION:$TILDY_USER_SENTINEL')
                child.expect_exact('SHELL_VERSION=' + args.version + ':preserved')
                child.sendline('exit')
                child.expect(pexpect.EOF)
                child.close()
                assert child.exitstatus == 0
            finally:
                child.close(force=True)
    caches = list((root / 'cache/tildy/zsh').iterdir())
    assert len(caches) == 1 and caches[0].name.startswith(args.version + '-')
    assert (caches[0] / 'tildy.zsh.zwc').is_file()
    assert hashes() == before
    if args.m4:
        work = root / ('m4-' + args.mode)
        work.mkdir()
        repo = work / 'repo'
        subprocess.run(['git', 'init', '-q', str(repo)], check=True)
        project = repo / '.tildy.json'
        project.write_text(json.dumps({'schema_version': 1, 'modules': {
            'cwd': {'style': {'foreground': 'cyan'}}}}))
        effective = json.loads(run('config', 'get', '--effective', '--cwd', str(repo), '--json'))['data']
        assert effective['project']['valid'] and effective['sources']['modules']['cwd'] == 'project'
        assert effective['revision'] != effective['effective_revision']
        assert json.loads(run('config', 'validate', '--project', '--file', str(project), '--json'))['ok']
        project.write_text(json.dumps({'schema_version': 1, 'modules': {
            'cwd': {'type': 'command', 'argv': ['false']}}}))
        fallback = json.loads(run('config', 'get', '--effective', '--cwd', str(repo), '--json'))['data']
        assert not fallback['project']['valid'] and fallback['project']['problem']
        assert fallback['revision'] == fallback['effective_revision']
        source = work / 'starship.toml'
        source.write_text("format = '$directory$kubernetes$custom.demo'\n[kubernetes]\ndisabled = false\n")
        imported = work / 'imported.json'
        import_env = {**env, 'TILDY_CONFIG': str(imported)}
        argv = [str(binary), 'import', 'starship', '--file', str(source), '--write', '--json']
        result = subprocess.run(argv, env=import_env, text=True, capture_output=True, check=True)
        data = json.loads(result.stdout)['data']
        assert data['report']['manual'] and data['report']['bridged']
        assert data['candidate']['modules']['kubernetes']['starship_config'] == str(source.resolve())
        saved = imported.read_bytes()
        conflict = subprocess.run(argv, env=import_env, text=True, capture_output=True)
        assert conflict.returncode != 0 and json.loads(conflict.stdout)['error']['code'] == 'revision_conflict'
        assert imported.read_bytes() == saved and hashes() == before
    if args.mode == 'prepare':
        (root / 'before.json').write_text(json.dumps(before, indent=2) + '\n')
    print(json.dumps({'mode': args.mode, 'version': args.version,
                      'binary': str(binary.resolve()), 'profile': str(root),
                      'hashes': before, 'zsh_starts': 2, 'm4_checks': args.m4, 'result': 'PASS'}, indent=2))


if __name__ == '__main__':
    main()
