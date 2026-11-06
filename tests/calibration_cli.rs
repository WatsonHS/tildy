//! A controlled PTY emulator verifies the foreground CPR contract and cleanup.
//! Real terminal evidence is a separate explicit replay, never inferred here.
use std::process::Command;
const DRIVER: &str = r#"
import os,sys,pty,fcntl,termios,struct,select,time,signal,json,re,errno,tty
binary,root,mode=sys.argv[1:]
os.makedirs(root,exist_ok=True)
profile=os.path.join(root,'cache/tildy/width/profile.json')
os.makedirs(os.path.dirname(profile),exist_ok=True)
sentinel=b'previous profile must survive failed calibration\n'
open(profile,'wb').write(sentinel)
master,slave=pty.openpty()
fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',24,80,0,0))
def attrs():
    a=termios.tcgetattr(slave)
    a[3] &= ~getattr(termios,'PENDIN',0)
    return a
before=attrs()
status_r,status_w=os.pipe();release_r,release_w=os.pipe()
pid=os.fork()
if pid==0:
    os.close(status_r);os.close(release_w)
    os.setsid();fcntl.ioctl(slave,termios.TIOCSCTTY,0);os.tcsetpgrp(slave,os.getpgrp())
    command_pid=os.fork()
    if command_pid:
        # Linux sends the controlling session SIGHUP when master closes. Keep
        # only this supervisor alive until release; the calibrator retains its
        # normal signal handling, including the explicit SIGHUP scenario.
        signal.signal(signal.SIGHUP,signal.SIG_IGN)
        os.close(master);os.write(status_w,(str(command_pid)+"\n").encode())
        _,st=os.waitpid(command_pid,0);os.write(status_w,(str(st)+"\n").encode())
        os.read(release_r,1);os._exit(0)
    os.close(status_w);os.close(release_r)
    os.dup2(slave,0)
    for fd,name in [(1,'stdout'),(2,'stderr')]:
        f=os.open(os.path.join(root,name),os.O_WRONLY|os.O_CREAT|os.O_TRUNC,0o600);os.dup2(f,fd);os.close(f)
    os.close(master);os.close(slave)
    if mode=='backpressure':
        flags=fcntl.fcntl(0,fcntl.F_GETFL);fcntl.fcntl(0,fcntl.F_SETFL,flags|os.O_NONBLOCK)
        for _ in range(256):
            try:os.write(0,b'x'*4096)
            except BlockingIOError:break
    env=os.environ.copy();env.update(HOME=root,XDG_CACHE_HOME=os.path.join(root,'cache'),TERM='xterm-256color',TERM_PROGRAM='test-pty',TERM_PROGRAM_VERSION='1',LANG='C.UTF-8')
    os.execve(binary,[binary,'calibrate','--font-revision','test-font','--cluster','中','--json'],env)
os.close(status_w);os.close(release_r)
os.set_blocking(status_r,False);os.set_blocking(master,False)
select.select([status_r],[],[],2)
status_bytes=os.read(status_r,100);command_pid=int(status_bytes.splitlines()[0])
buf=b'';screen=b'';row=1;col=1;queries=0;pending=[];status=None;end=time.monotonic()+7
while time.monotonic()<end:
    now=time.monotonic()
    for item in pending[:]:
        if now>=item[0]:
            try:os.write(master,item[1])
            except OSError:pass
            pending.remove(item)
    ready,_,_=select.select([] if mode=='backpressure' else [master],[],[],0.01)
    if ready:
        try:data=os.read(master,4096)
        except OSError:data=b''
        screen+=data;buf+=data
        while b'\x1b[6n' in buf:
            prefix,buf=buf.split(b'\x1b[6n',1);queries+=1
            cups=list(re.finditer(rb'\x1b\[(\d+);(\d+)H',prefix))
            if cups:
                last=cups[-1];row,col=map(int,last.groups());prefix=prefix[last.end():]
            text=re.sub(rb'\x1b\[[0-9;?]*[A-Za-z]',b'',prefix)
            if '中'.encode() in text:col+=2
            response=('\x1b[%d;%dR'%(row,col)).encode()
            if mode=='timeout':continue
            if mode=='late':pending.append((time.monotonic()+0.30,response));continue
            if mode=='keyboard':os.write(master,b'q');continue
            if mode.startswith('signal-'):
                os.kill(command_pid,getattr(signal,mode[7:]));pending.append((time.monotonic()+0.05,response));continue
            if mode=='wrong-position' and queries==1:response=b'\x1b[3;2R'
            if mode=='inconsistent' and queries==4:response=b'\x1b[2;5R'
            if mode=='extra-reply':response+=b'\x1b[2;2R'
            if mode=='split':
                os.write(master,response[:2]);pending.append((time.monotonic()+0.002,response[2:]))
            else:os.write(master,response)
    try:status_bytes+=os.read(status_r,100)
    except BlockingIOError:pass
    lines=status_bytes.splitlines()
    if lines:command_pid=int(lines[0])
    if len(lines)>=2:status=int(lines[1]);break
if status is None:
    if command_pid:os.kill(command_pid,signal.SIGKILL)
    os.write(release_w,b'x');os.waitpid(pid,0);raise AssertionError('calibration hung')
assert attrs()==before,('termios not restored',mode,before,attrs())
if mode!='backpressure':
    assert b'\x1b[?1049h' in screen and b'\x1b[?1049l' in screen,('alternate screen not restored',mode,screen,open(os.path.join(root,'stdout')).read())
# Detect bytes left for a later shell after the bounded cleanup window.
old=termios.tcgetattr(slave);tty.setraw(slave,termios.TCSANOW);os.set_blocking(slave,False)
try:left=os.read(slave,4096)
except BlockingIOError:left=b''
finally:termios.tcsetattr(slave,termios.TCSANOW,old)
assert not left,('input was left for shell',mode,left)
v=json.load(open(os.path.join(root,'stdout')))
assert open(os.path.join(root,'stderr'),'rb').read()==b''
if mode in ['success','split']:
    assert os.waitstatus_to_exitcode(status)==0 and v['ok'],v
    p=json.load(open(profile));assert p['entries']==[{'cluster':'中','columns':2}],p
    assert v['data']['entries']==p['entries']
    assert queries==4,queries
else:
    assert os.waitstatus_to_exitcode(status)==4 and not v['ok'],v
    assert v['error']['code']=='environment_unavailable',v
    assert open(profile,'rb').read()==sentinel,'failed run replaced profile'
# Closing the PTY master releases Darwin's session-leader close drain when
# backpressure intentionally left the output queue full. Assertions above
# inspect the restored slave while the controlling session is still alive.
os.close(master)
os.write(release_w,b'x');os.waitpid(pid,0)
os.close(slave);os.close(status_r);os.close(release_w)
print(json.dumps({'mode':mode,'ok':True,'queries':queries}))
"#;

#[test]
fn foreground_probe_restores_terminal_and_never_persists_failed_samples() {
    for mode in [
        "success",
        "split",
        "backpressure",
        "timeout",
        "late",
        "keyboard",
        "wrong-position",
        "inconsistent",
        "extra-reply",
        "signal-SIGINT",
        "signal-SIGTERM",
        "signal-SIGHUP",
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new("python3")
            .args(["-c", DRIVER, env!("CARGO_BIN_EXE_tildy")])
            .arg(root.path())
            .arg(mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn invalid_samples_are_rejected_before_terminal_io() {
    let root = tempfile::tempdir().unwrap();
    for cluster in ["ab", "\x1b", "\n"] {
        let output = Command::new(env!("CARGO_BIN_EXE_tildy"))
            .args([
                "calibrate",
                "--font-revision",
                "test",
                "--cluster",
                cluster,
                "--json",
            ])
            .env("HOME", root.path())
            .env("TERM", "xterm")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "validation_failed");
        assert!(!root.path().join(".cache/tildy/width/profile.json").exists());
    }
}
