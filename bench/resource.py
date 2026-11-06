#!/usr/bin/env python3
"""macOS/Linux worker resource comparison. Stdlib; no zsh or global process cleanup."""
import argparse
import ctypes as C
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import queue
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
import time


class Rusage(C.Structure):
    # sys/resource.h rusage_info_v0: time fields are nanoseconds, sizes bytes.
    _fields_ = [('uuid', C.c_uint8 * 16)] + [(name, C.c_uint64) for name in (
        'user_ns', 'system_ns', 'package_idle_wakeups', 'interrupt_wakeups',
        'pageins', 'wired_bytes', 'rss_bytes', 'phys_footprint_bytes',
        'start_abstime', 'exit_abstime')]


class TaskInfo(C.Structure):
    # sys/proc_info.h proc_taskinfo; flavor PROC_PIDTASKINFO = 4.
    _fields_ = [(name, C.c_uint64) for name in (
        'virtual_size', 'resident_size', 'total_user', 'total_system',
        'threads_user', 'threads_system')] + [(name, C.c_int32) for name in (
        'policy', 'faults', 'pageins', 'cow_faults', 'messages_sent',
        'messages_received', 'syscalls_mach', 'syscalls_unix', 'csw',
        'thread_count', 'numrunning', 'priority')]


class Metrics:
    def __init__(self):
        self.lib = C.CDLL('/usr/lib/libproc.dylib', use_errno=True)
        self.lib.proc_pid_rusage.argtypes = [C.c_int, C.c_int, C.c_void_p]
        self.lib.proc_pid_rusage.restype = C.c_int
        self.lib.proc_pidinfo.argtypes = [C.c_int, C.c_int, C.c_uint64, C.c_void_p, C.c_int]
        self.lib.proc_pidinfo.restype = C.c_int

    def read(self, pid):
        usage, task = Rusage(), TaskInfo()
        if self.lib.proc_pid_rusage(pid, 0, C.byref(usage)) != 0:
            raise OSError(C.get_errno(), f'proc_pid_rusage({pid})')
        if self.lib.proc_pidinfo(pid, 4, 0, C.byref(task), C.sizeof(task)) != C.sizeof(task):
            raise OSError(C.get_errno(), f'proc_pidinfo({pid})')
        return dict(pid=pid, rss_bytes=usage.rss_bytes,
                    phys_footprint_bytes=usage.phys_footprint_bytes,
                    cpu_user_ns=usage.user_ns, cpu_system_ns=usage.system_ns,
                    thread_count=task.thread_count)


class LinuxMetrics:
    """Counters for the owned worker PID; RSS is not macOS physical footprint."""
    def __init__(self, proc_root=Path('/proc')):
        self.root = proc_root
        self.ticks = os.sysconf('SC_CLK_TCK')

    def read(self, pid):
        directory = self.root / str(pid)
        stat = (directory / 'stat').read_text()
        # comm may contain spaces and parentheses. Fields after its final ')'
        # start at field 3 (state), not at a fixed whitespace token.
        fields = stat[stat.rindex(')') + 2:].split()
        status = dict(line.split(':', 1) for line in (directory / 'status').read_text().splitlines() if ':' in line)
        rss, unit = status['VmRSS'].split()
        if unit != 'kB':
            raise ValueError(f'unexpected /proc RSS unit: {unit}')
        return dict(pid=pid, rss_bytes=int(rss) * 1024, phys_footprint_bytes=None,
                    cpu_user_ns=int(fields[11]) * 1_000_000_000 // self.ticks,
                    cpu_system_ns=int(fields[12]) * 1_000_000_000 // self.ticks,
                    thread_count=int(fields[17]))


def native_watch_mode():
    return 'fsevents' if sys.platform == 'darwin' else 'events'


def run(command, **kwargs):
    return subprocess.run(command, check=True, capture_output=True, text=True, timeout=30, **kwargs)


def atomic_json(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def fixture(path, env):
    path.mkdir()
    run(['git', 'init', '-q', str(path)], env=env)
    for i in range(100):
        directory = path / f'dir-{i % 10}'
        directory.mkdir(exist_ok=True)
        (directory / f'file-{i}.txt').write_text(f'fixture {i}\n')
    (path / 'tracked.txt').write_text('initial\n')
    run(['git', '-C', str(path), 'add', '.'], env=env)
    run(['git', '-C', str(path), '-c', 'user.name=Resource fixture',
         '-c', 'user.email=resource@example.invalid', 'commit', '-qm', 'fixture'], env=env)


class Worker:
    def __init__(self, binary, root, index, env, config, logs, submission_mode):
        self.name = f'worker-{index}'
        self.gen = 0
        self.expects_watch = None
        self.submission_mode = submission_mode
        self.root = root / self.name
        self.root.mkdir()
        self.snapshot = self.root / 'snapshot.json'
        private_env = dict(env, HOME=str(self.root), XDG_STATE_HOME=str(self.root / 'state'),
                           XDG_CACHE_HOME=str(self.root / 'cache'), XDG_CONFIG_HOME=str(self.root / 'config'))
        self.stderr = (logs / f'{self.name}.stderr.txt').open('w')
        self.stdout = (logs / f'{self.name}.stdout.txt').open('w')
        self.frames = queue.Queue()
        self.proc = subprocess.Popen([str(binary), 'worker', '--config', str(config),
                                      '--snapshot', str(self.snapshot)],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
                                     env=private_env, text=True, start_new_session=True, bufsize=1)
        self.reader = threading.Thread(target=self.drain, daemon=True)
        self.reader.start()
        self.last_snapshot = None
        self.request_log = []

    def drain(self):
        frame = None
        for raw in self.proc.stdout:
            self.stdout.write(raw)
            line = raw.rstrip('\n')
            fields = line.split('\t')
            if fields[0] == 'RES' and len(fields) >= 4:
                frame = {'gen': int(fields[1]), 'status': fields[2], 'epoch': fields[3]}
            elif fields[0] == 'END' and frame is not None:
                self.frames.put(frame)
                frame = None
        self.frames.put({'eof': True})

    def request(self, cwd):
        self.gen += 1
        # Keep M2 provider env revision stable unless explicitly modeling submissions.
        submission = self.gen if self.submission_mode == "incrementing" else 0
        env = f'TILDY_SUBMISSION_ID={submission}'
        request = f'REQ\t{self.gen}\t{self.name}\t{cwd}\t120\t40\t0\t0\t{env}\t0\n'
        started = time.monotonic()
        self.proc.stdin.write(request)
        self.proc.stdin.flush()
        self.request_log.append({'gen': self.gen, 'cwd': str(cwd), 'sent_monotonic': started})
        return started

    def wait(self, timeout, require_rescan):
        deadline = time.monotonic() + timeout
        fresh = False
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f'{self.name} exited {self.proc.returncode}')
            while True:
                try:
                    frame = self.frames.get_nowait()
                except queue.Empty:
                    break
                if frame.get('eof'):
                    raise RuntimeError(f'{self.name} stdout closed')
                if frame.get('gen') == self.gen and frame['status'] == 'fresh':
                    fresh = True
            try:
                snapshot = json.loads(self.snapshot.read_text())
            except (FileNotFoundError, json.JSONDecodeError):
                snapshot = {}
            self.last_snapshot = snapshot
            if not isinstance(snapshot, dict):
                raise RuntimeError(f'{self.name} snapshot is not an object')
            if self.expects_watch is True and not isinstance(snapshot.get('watch'), dict):
                raise RuntimeError(f'{self.name} previously watch-capable snapshot lost watch metadata')
            result = snapshot.get('last_result') or {}
            slots = snapshot.get('slots', [])
            current = result.get('gen') == self.gen
            bad = [slot for slot in slots if slot.get('error') or slot.get('freshness') in ('timeout', 'error')]
            if current and snapshot.get('degraded'):
                raise RuntimeError(f'{self.name} degraded: {snapshot["degraded"]}')
            if current and bad:
                raise RuntimeError(f'{self.name} provider failure: {bad}')
            settled = fresh and current and result.get('status') == 'fresh' and bool(slots) and all(
                not slot.get('fetching') and slot.get('freshness') == 'fresh' for slot in slots)
            if settled and self.expects_watch is None:
                # Pin capability once; a later malformed M3 snapshot must never
                # be reinterpreted as a legacy M2 snapshot lacking this feature.
                self.expects_watch = 'watch' in snapshot
            if settled and self.expects_watch:
                settled = isinstance(snapshot.get('watch'), dict) and snapshot['watch'].get('mode') == native_watch_mode()
                if require_rescan:
                    cycles = [cycle for cycle in snapshot.get('timing_history', []) if cycle.get('gen') == self.gen]
                    settled = settled and any(
                        provider.get('provider') == 'git' and provider.get('executed') and provider.get('completed')
                        and not provider.get('timed_out')
                        for cycle in cycles[1:] for provider in cycle.get('providers', []))
            if settled:
                self.request_log[-1].update(settled=True, elapsed_seconds=time.monotonic() - self.request_log[-1]['sent_monotonic'],
                                            watch=snapshot.get('watch'), counters=snapshot.get('counters'),
                                            provider_stats=snapshot.get('providers'),
                                            timing_history=snapshot.get('timing_history'),
                                            provider_runs_total=sum(p.get('runs', 0) for p in snapshot.get('providers', [])),
                                            ready_rescan_executed=sum(provider.get('executed', False)
                                                for cycle in [c for c in snapshot.get('timing_history', []) if c.get('gen') == self.gen][1:]
                                                for provider in cycle.get('providers', [])))
                return
            time.sleep(.01)
        self.request_log[-1].update(settled=False, timeout_seconds=timeout)
        raise TimeoutError(f'{self.name} gen {self.gen} did not settle; last={self.last_snapshot}')

    def close(self):
        # Only this worker's private process group is targeted, never a name/user.
        info = self.cleanup_info = {'graceful': False, 'forced_signals': [],
                                    'group_disappeared': False, 'reader_alive': None}
        try:
            if self.proc.poll() is None:
                try:
                    self.proc.stdin.write('BYE\n')
                    self.proc.stdin.flush()
                    self.proc.wait(timeout=3)
                    info['graceful'] = self.proc.returncode == 0
                except (BrokenPipeError, subprocess.TimeoutExpired):
                    pass
            else:
                info['graceful'] = self.proc.returncode == 0
            def group_exists():
                try:
                    os.killpg(self.proc.pid, 0)
                    return True
                except ProcessLookupError:
                    return False
            for sig in (signal.SIGTERM, signal.SIGKILL):
                if not group_exists():
                    break
                try:
                    os.killpg(self.proc.pid, sig)
                    info['forced_signals'].append(signal.Signals(sig).name)
                except ProcessLookupError:
                    break
                # Reap the leader independently: children may outlive it.
                try:
                    self.proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass
                deadline = time.monotonic() + 1
                while group_exists() and time.monotonic() < deadline:
                    time.sleep(.02)
            info['group_disappeared'] = not group_exists()
            self.reader.join(timeout=2)
            info['reader_alive'] = self.reader.is_alive()
            info['exit_code'] = self.proc.poll()
            if not info['group_disappeared'] or info['reader_alive'] or info['exit_code'] is None:
                raise RuntimeError(f'worker cleanup left a process group or reader alive: {info}')
            return info
        finally:
            # Avoid blocking on the pipe lock of a still-running reader after
            # an abnormal cleanup. That case is a recorded FAIL, never PASS.
            if not self.reader.is_alive():
                self.stdout.close()
                self.proc.stdout.close()
            self.stderr.close()
            self.proc.stdin.close()


def sample(metrics, workers, phase, origin, prior=None):
    rows = [dict(worker=worker.name, **metrics.read(worker.proc.pid)) for worker in workers]
    states = []
    for worker in workers:
        try:
            snapshot = json.loads(worker.snapshot.read_text())
        except (FileNotFoundError, json.JSONDecodeError) as error:
            raise RuntimeError(f'{worker.name} unreadable snapshot during {phase}: {error}') from error
        if not isinstance(snapshot, dict):
            raise RuntimeError(f'{worker.name} snapshot is not an object during {phase}')
        result = snapshot.get('last_result') or {}
        slots = snapshot.get('slots')
        if phase != 'startup' and (not isinstance(slots, list) or not slots or result.get('gen') != worker.gen):
            raise RuntimeError(f'{worker.name} invalid or stale snapshot during {phase}: {snapshot}')
        if snapshot.get('degraded') or any(slot.get('error') or slot.get('freshness') in ('timeout', 'error') for slot in snapshot.get('slots', [])):
            raise RuntimeError(f'{worker.name} error during {phase}: {snapshot}')
        if phase != 'startup' and worker.expects_watch is True and (not isinstance(snapshot.get('watch'), dict) or snapshot['watch'].get('mode') != native_watch_mode()):
            raise RuntimeError(f'{worker.name} watcher lost readiness during {phase}: {snapshot.get("watch")}')
        states.append({'worker': worker.name, 'watch': snapshot.get('watch'),
                       'counters': snapshot.get('counters'), 'slots': snapshot.get('slots')})
    totals = {key: None if any(row[key] is None for row in rows) else sum(row[key] for row in rows)
              for key in rows[0] if key not in ('worker', 'pid')}
    result = {'phase': phase, 'elapsed_seconds': time.monotonic() - origin, 'workers': rows, 'total': totals, 'snapshot_states': states}
    if prior:
        elapsed = result['elapsed_seconds'] - prior['elapsed_seconds']
        result['since_previous_seconds'] = elapsed
        for row, old in zip(rows, prior['workers']):
            delta = row['cpu_user_ns'] + row['cpu_system_ns'] - old['cpu_user_ns'] - old['cpu_system_ns']
            row['cpu_delta_ns'] = delta
            row['cpu_percent_one_core'] = delta / (elapsed * 1e9) * 100
        result['total']['cpu_delta_ns'] = sum(row['cpu_delta_ns'] for row in rows)
        result['total']['cpu_percent_one_core'] = sum(row['cpu_percent_one_core'] for row in rows)
    return result


def details(args, phase, workers, out, record):
    if phase not in args.detail_phases:
        return
    selected = workers if args.detail_pids == 'all' else workers[:1]
    for worker in selected:
        for tool in (['vmmap', 'heap'] if args.heap else ['vmmap']):
            command = [tool, '-summary', str(worker.proc.pid)] if tool == 'vmmap' else [tool, str(worker.proc.pid)]
            name = f'{phase}-{worker.name}-{tool}.txt'
            try:
                done = subprocess.run(command, text=True, capture_output=True, timeout=args.detail_timeout)
                (out / name).write_text(done.stdout + done.stderr)
                record['details'].append({'phase': phase, 'pid': worker.proc.pid, 'tool': tool,
                                          'exit_code': done.returncode, 'file': name})
            except subprocess.TimeoutExpired as error:
                record['details'].append({'phase': phase, 'pid': worker.proc.pid, 'tool': tool, 'error': str(error)})


def idle(args, metrics, workers, phase, origin, record):
    start = time.monotonic()
    tick = 0
    while True:
        target = start + min(tick * args.interval, args.duration)
        time.sleep(max(0, target - time.monotonic()))
        prior = record['samples'][-1] if record['samples'] else None
        record['samples'].append(sample(metrics, workers, phase, origin, prior))
        if time.monotonic() >= start + args.duration:
            break
        tick += 1


def cell(args, label, binary, count, layout, metrics, out):
    out.mkdir(parents=True)
    record = dict(binary=label, count=count, layout=layout, mode=args.mode, preset=args.preset, status='RUNNING',
                  samples=[], startup_per_worker=[], requests={}, cleanup={}, churn=[], errors=[], details=[])
    origin = time.monotonic()
    workers = []
    temporary = None
    try:
        # Explicit lifetime: no context manager/finalizer may remove fixtures
        # while live workers are still writing their snapshots or cache.
        temporary = tempfile.mkdtemp(prefix='tildy-resource-')
        root = Path(temporary)
        home = root / 'fixture-home'
        home.mkdir()
        # Explicit minimal environment prevents inherited agent/language/config toggles.
        locale = 'C.UTF-8' if sys.platform == 'linux' else 'en_US.UTF-8'
        env = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(home), 'LANG': locale,
               'LC_ALL': locale, 'TERM': 'xterm-256color', 'USER': 'resource', 'LOGNAME': 'resource',
               'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null',
               'GIT_AUTHOR_DATE': '2020-01-01T00:00:00Z', 'GIT_COMMITTER_DATE': '2020-01-01T00:00:00Z'}
        config = root / 'config.json'
        config.write_text(json.dumps({'schema_version': 1, **({'preset': args.preset} if args.preset != 'builtin' else {})}) + '\n')
        record['config'] = json.loads(config.read_text())
        record['environment'] = {**env, 'HOME': '<isolated-per-worker>', 'XDG_STATE_HOME': '<isolated-per-worker>',
                                 'XDG_CACHE_HOME': '<isolated-per-worker>', 'XDG_CONFIG_HOME': '<isolated-per-worker>'}
        pool = args.pool_size if args.mode == 'churn' else 1
        repos = []
        for group in range(1 if layout == 'same' else count):
            paths = []
            for index in range(pool):
                path = root / f'repo-{group}-{index}'
                fixture(path, env)
                paths.append(path)
            repos.append(paths)
        record['fixture'] = {'tracked_files_per_repo': 101, 'pool_per_group': pool,
                             'groups': len(repos), 'git_version': run(['git', '--version']).stdout.strip(),
                             'head_commit': run(['git', '-C', str(repos[0][0]), 'rev-parse', 'HEAD'], env=env).stdout.strip(),
                             'head_tree': run(['git', '-C', str(repos[0][0]), 'rev-parse', 'HEAD^{tree}'], env=env).stdout.strip()}
        # Settle each new worker before starting the next: avoid a burst of
        # 30 concurrent git providers hitting their normal request deadlines.
        for index in range(count):
            worker = Worker(binary, root, index, env, config, out, args.submission_mode)
            workers.append(worker)
            record['startup_per_worker'].append(dict(worker=worker.name, elapsed_seconds=time.monotonic() - origin,
                                                     **metrics.read(worker.proc.pid)))
            if args.detail_pids == 'all' or index == 0:
                details(args, 'startup', [worker], out, record)
            worker.request(repos[0 if layout == 'same' else index][0])
            worker.wait(args.timeout, require_rescan=True)
        record['samples'].append(sample(metrics, workers, 'all-started', origin))
        time.sleep(args.settle)
        record['samples'].append(sample(metrics, workers, 'settled', origin, record['samples'][-1]))
        details(args, 'settled', workers, out, record)
        if args.mode == 'churn':
            began = time.monotonic()
            round_index = 0
            # A duration defines a fixed schedule, not a cutoff that would
            # silently give a slower version fewer requests/data mutations.
            planned_rounds = math.ceil(args.churn_duration / args.churn_period) if args.churn_duration is not None else args.churn_rounds
            record['churn_schedule'] = {'planned_rounds': planned_rounds, 'period_seconds': args.churn_period,
                                         'planned_duration_seconds': args.churn_duration}
            while round_index < planned_rounds:
                target = began + round_index * args.churn_period
                time.sleep(max(0, target - time.monotonic()))
                index = (round_index + 1) % pool
                for paths in repos:
                    (paths[index] / 'tracked.txt').write_text(f'round {round_index}\n')
                step = {'round': round_index + 1, 'repo_index': index,
                        'started_seconds': time.monotonic() - origin,
                        'schedule_lag_seconds': max(0, time.monotonic() - target)}
                for number, worker in enumerate(workers):
                    worker.request(repos[0 if layout == 'same' else number][index])
                for worker in workers:
                    worker.wait(args.timeout, require_rescan=True)
                step['completed_seconds'] = time.monotonic() - origin
                record['churn'].append(step)
                record['samples'].append(sample(metrics, workers, 'churn', origin, record['samples'][-1]))
                round_index += 1
            record['churn_schedule']['actual_seconds'] = time.monotonic() - began
            details(args, 'after-churn', workers, out, record)
            idle(args, metrics, workers, 'idle-tail', origin, record)
        else:
            idle(args, metrics, workers, 'idle', origin, record)
        details(args, 'end', workers, out, record)
        record['status'] = 'PASS'
    except (Exception, KeyboardInterrupt) as error:
        record['status'] = 'FAIL'
        record['errors'].append(f'{type(error).__name__}: {error}')
        record['interrupted'] = isinstance(error, KeyboardInterrupt)
    finally:
        for worker in workers:
            try:
                record['requests'][worker.name] = worker.request_log
                if worker.last_snapshot is not None:
                    atomic_json(out / f'{worker.name}.last-snapshot.json', worker.last_snapshot)
            except Exception as error:
                record['errors'].append(f'persist {worker.name}: {error}')
                record['status'] = 'FAIL'
            finally:
                try:
                    record['cleanup'][worker.name] = worker.close()
                except Exception as error:
                    record['cleanup'][worker.name] = getattr(worker, 'cleanup_info', {})
                    record['errors'].append(f'cleanup {worker.name}: {error}')
                    record['status'] = 'FAIL'
        # Process cleanup and fixture cleanup are independent failure domains.
        # Every close above is attempted even if an evidence write/close fails.
        # Never remove directories underneath a known or unverified residue.
        record['fixture_cleanup'] = {'path': temporary, 'removed': False}
        if temporary is not None:
            residual = [name for name, info in record['cleanup'].items()
                        if not info.get('group_disappeared', False) or info.get('reader_alive', False)]
            if residual:
                record['fixture_cleanup']['retained_for_residual_workers'] = residual
                record['errors'].append(f'fixture cleanup skipped for residual workers: {residual}')
                record['status'] = 'FAIL'
            else:
                try:
                    shutil.rmtree(temporary)
                    record['fixture_cleanup']['removed'] = True
                except Exception as error:
                    record['fixture_cleanup']['error'] = str(error)
                    record['errors'].append(f'fixture cleanup: {error}')
                    record['status'] = 'FAIL'
        record['elapsed_seconds'] = time.monotonic() - origin
        record['phase_summary'] = {}
        for phase in ('idle', 'idle-tail'):
            rows = [row for row in record['samples'] if row['phase'] == phase]
            if len(rows) < 2:
                continue
            first, last = rows[0], rows[-1]
            elapsed = last['elapsed_seconds'] - first['elapsed_seconds']
            cpu_delta = sum(last['total'][key] - first['total'][key] for key in ('cpu_user_ns', 'cpu_system_ns'))
            record['phase_summary'][phase] = {
                'wall_seconds': elapsed, 'cpu_delta_ns': cpu_delta,
                'cpu_percent_one_core': cpu_delta / (elapsed * 1e9) * 100 if elapsed > 0 else None,
                'last_total': last['total'],
                'min_total_rss_bytes': min(row['total']['rss_bytes'] for row in rows),
                'max_total_rss_bytes': max(row['total']['rss_bytes'] for row in rows)}
        failed_details = [detail for detail in record['details'] if detail.get('error') or detail.get('exit_code', 0) != 0]
        if failed_details:
            record['status'] = 'FAIL'
            record['errors'].append(f'requested detail capture failed: {failed_details}')
        atomic_json(out / 'result.json', record)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', action='append', required=True, metavar='LABEL=ABS_PATH')
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--preset', choices=['builtin', 'pastel-powerline'], default='builtin')
    parser.add_argument('--counts', default='1,10,30')
    parser.add_argument('--layouts', default='same,different')
    parser.add_argument('--cell', action='append', metavar='COUNT:LAYOUT', help='restrict matrix; repeatable')
    parser.add_argument('--duration', type=float, default=30)
    parser.add_argument('--interval', type=float, default=5)
    parser.add_argument('--settle', type=float, default=1)
    parser.add_argument('--timeout', type=float, default=10)
    parser.add_argument('--mode', choices=['idle', 'churn'], default='idle')
    parser.add_argument('--submission-mode', choices=['fixed', 'incrementing'], default='fixed')
    parser.add_argument('--pool-size', type=int, default=4)
    churn = parser.add_mutually_exclusive_group()
    churn.add_argument('--churn-rounds', type=int, default=20)
    churn.add_argument('--churn-duration', type=float)
    parser.add_argument('--churn-period', type=float, default=1)
    parser.add_argument('--detail-phases', default='', help='startup,settled,after-churn,end; default none')
    parser.add_argument('--detail-pids', choices=['first', 'all'], default='first')
    parser.add_argument('--detail-timeout', type=float, default=15)
    parser.add_argument('--heap', action='store_true', help='also run heap at detail phases')
    args = parser.parse_args()
    if sys.platform not in ('darwin', 'linux'):
        parser.error('requires macOS libproc or Linux /proc')
    if sys.platform != 'darwin' and (args.detail_phases or args.heap):
        parser.error('--detail-phases/--heap require macOS vmmap/heap; Linux supports /proc counters')
    if args.duration < 0 or args.interval <= 0 or args.pool_size < 2 or args.churn_period < 0 or args.timeout <= 0 or args.settle < 0 or args.churn_rounds < 0 or (args.churn_duration is not None and args.churn_duration <= 0):
        parser.error('invalid duration/interval/pool-size/churn-period/timeout')
    if args.churn_duration is not None and args.churn_period <= 0:
        parser.error('--churn-duration requires a positive --churn-period')
    args.detail_phases = set(filter(None, args.detail_phases.split(',')))
    if not args.detail_phases <= {'startup', 'settled', 'after-churn', 'end'}:
        parser.error('unknown detail phase')
    binaries = []
    for raw in args.binary:
        label, separator, path = raw.partition('=')
        if not separator or not label or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-' for c in label):
            parser.error('binary must be LABEL=ABS_PATH with a safe unique label')
        binary = Path(path)
        if not binary.is_absolute() or not binary.is_file() or label in [item[0] for item in binaries]:
            parser.error('binary path must exist and be absolute; labels must be unique')
        binaries.append((label, binary.resolve()))
    try:
        if args.cell:
            cells = [(int(raw.split(':')[0]), raw.split(':')[1]) for raw in args.cell if raw.count(':') == 1]
            if len(cells) != len(args.cell):
                raise ValueError('invalid cell')
        else:
            cells = [(int(count), layout) for count in args.counts.split(',') for layout in args.layouts.split(',')
                     if not (int(count) == 1 and layout == 'different')]
    except (ValueError, IndexError):
        parser.error('invalid --cell/--counts; use COUNT:same|different and comma-separated counts')
    if any(count < 1 or layout not in ('same', 'different') or (count == 1 and layout != 'same') for count, layout in cells):
        parser.error('cells need positive COUNT:same|different; count 1 supports same only')
    if args.out.exists():
        parser.error('--out must be a new directory, to preserve previous evidence')
    args.out.mkdir(parents=True)
    manifest = {'schema_version': 1, 'scope': 'direct worker and its resource counters; excludes zsh and child-process counters',
                'metrics': {'memory': 'bytes', 'cpu': 'nanoseconds, cumulative per PID', 'cpu_percent': 'delta CPU / wall time, one core = 100%',
                            'threads': 'proc_taskinfo.pti_threadnum' if sys.platform == 'darwin' else '/proc/PID/stat num_threads',
                            'sampling': 'in-process libproc' if sys.platform == 'darwin' else '/proc/PID/stat and status; CPU quantized to SC_CLK_TCK',
                            'unavailable': [] if sys.platform == 'darwin' else ['phys_footprint_bytes']},
                'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                'instrumented': bool(args.detail_phases),
                'instrumentation': {'phases': sorted(args.detail_phases), 'heap': args.heap, 'detail_pids': args.detail_pids},
                'started_unix_seconds': time.time(), 'platform': platform.platform(), 'machine': platform.machine(),
                'python': sys.version, 'argv': sys.argv, 'binaries': {}, 'cells': []}
    for label, binary in binaries:
        manifest['binaries'][label] = {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                                        'version': run([str(binary), '--version']).stdout.strip()}
    metrics = Metrics() if sys.platform == 'darwin' else LinuxMetrics()
    for count, layout in cells:
        for label, binary in binaries:
            name = f'{label}-{count}-{layout}-{args.mode}'
            record = cell(args, label, binary, count, layout, metrics, args.out / name)
            manifest['cells'].append({'name': name, 'status': record['status'], 'errors': record['errors']})
            atomic_json(args.out / 'manifest.json', manifest)
            print(json.dumps(manifest['cells'][-1]), flush=True)
            if record.get('interrupted'):
                return 130
    return int(any(item['status'] != 'PASS' for item in manifest['cells']))


if __name__ == '__main__':
    raise SystemExit(main())
