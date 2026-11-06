//! Minimal FSEvents backend without the CoreServices umbrella dependency.
//!
//! ABI definitions follow the public SDK FSEvents.h / CFArray.h / CFString.h
//! and dispatch/queue.h. CoreFoundation and the FSEvents subframework are
//! loaded with dlopen the first time a watch starts, never linked: linking
//! them made every tildy process (init, instant, render, configure, ...)
//! load about 700 dylibs at launch, 8–13 ms per spawn, while only the
//! worker ever watches anything (docs/decisions/0010-fsevents-on-demand.md).
use std::ffi::{c_char, c_void, CStr, CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::{Mutex, OnceLock};

// CoreFoundation's CFIndex is signed pointer-sized; Boolean is UInt8.
type Ref = *const c_void;
type Stream = *mut c_void;
type EventCallback = unsafe extern "C" fn(Ref, *mut c_void, usize, *mut c_void, *const u32, *const u64);
#[repr(C)]
#[derive(Clone, Copy)]
struct ArrayCallbacks {
    version: isize,
    retain: Option<unsafe extern "C" fn(Ref, Ref) -> Ref>,
    release: Option<unsafe extern "C" fn(Ref, Ref)>,
    copy_description: Option<unsafe extern "C" fn(Ref) -> Ref>,
    equal: Option<unsafe extern "C" fn(Ref, Ref) -> u8>,
}
#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(Ref) -> Ref>,
    release: Option<unsafe extern "C" fn(Ref)>,
    copy_description: Option<unsafe extern "C" fn(Ref) -> Ref>,
}
/// The framework entry points, resolved once with dlopen/dlsym on first use.
/// Only the worker's watch manager ever asks for them.
struct Api {
    cf_type_array_callbacks: *const ArrayCallbacks,
    cf_string_create_with_file_system_representation: unsafe extern "C" fn(Ref, *const c_char) -> Ref,
    cf_array_create_mutable: unsafe extern "C" fn(Ref, isize, *const ArrayCallbacks) -> Ref,
    cf_array_append_value: unsafe extern "C" fn(Ref, Ref),
    cf_release: unsafe extern "C" fn(Ref),
    stream_create: unsafe extern "C" fn(Ref, EventCallback, *mut StreamContext, Ref, u64, f64, u32) -> Stream,
    stream_set_dispatch_queue: unsafe extern "C" fn(Stream, *mut c_void),
    stream_start: unsafe extern "C" fn(Stream) -> u8,
    stream_stop: unsafe extern "C" fn(Stream),
    stream_invalidate: unsafe extern "C" fn(Stream),
    stream_release: unsafe extern "C" fn(Stream),
}
// Function pointers and a pointer to process-lived immutable framework data.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

const CORE_FOUNDATION: &CStr = c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
const FSEVENTS: &CStr = c"/System/Library/Frameworks/CoreServices.framework/Frameworks/FSEvents.framework/FSEvents";

fn api() -> io::Result<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load_api).as_ref().ok_or_else(|| io::Error::other("FSEvents framework unavailable"))
}

fn load_api() -> Option<Api> {
    // RTLD_LOCAL: nothing else in the process needs these symbols by name.
    // The handles are never closed: a dlclose of CoreFoundation is unsafe.
    let open = |path: &CStr| {
        let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        (!h.is_null()).then_some(h)
    };
    let cf = open(CORE_FOUNDATION)?;
    let fs = open(FSEVENTS)?;
    let sym = |h: *mut c_void, name: &CStr| {
        let p = unsafe { libc::dlsym(h, name.as_ptr()) };
        (!p.is_null()).then_some(p)
    };
    /// A non-null dlsym result as the function type written in `Api`.
    fn fun<T: Copy>(p: *mut c_void) -> T {
        const { assert!(std::mem::size_of::<T>() == std::mem::size_of::<*mut c_void>()) };
        // SAFETY: the symbol's SDK signature is `T`; a function pointer and a
        // data pointer have the same size and representation on this target.
        unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) }
    }
    macro_rules! f {
        ($h:expr, $name:literal) => {
            fun(sym($h, $name)?)
        };
    }
    Some(Api {
        cf_type_array_callbacks: sym(cf, c"kCFTypeArrayCallBacks")?.cast(),
        cf_string_create_with_file_system_representation: f!(cf, c"CFStringCreateWithFileSystemRepresentation"),
        cf_array_create_mutable: f!(cf, c"CFArrayCreateMutable"),
        cf_array_append_value: f!(cf, c"CFArrayAppendValue"),
        cf_release: f!(cf, c"CFRelease"),
        stream_create: f!(fs, c"FSEventStreamCreate"),
        stream_set_dispatch_queue: f!(fs, c"FSEventStreamSetDispatchQueue"),
        stream_start: f!(fs, c"FSEventStreamStart"),
        stream_stop: f!(fs, c"FSEventStreamStop"),
        stream_invalidate: f!(fs, c"FSEventStreamInvalidate"),
        stream_release: f!(fs, c"FSEventStreamRelease"),
    })
}

extern "C" {
    // libdispatch is exported by libSystem, already linked by Rust/libc.
    fn dispatch_queue_create(label: *const c_char, attributes: Ref) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: unsafe extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}
struct CfOwned(Ref);
impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: this owns one Create reference; never constructed with NULL,
        // and only after the API loaded.
        if let Ok(api) = api() {
            unsafe {
                (api.cf_release)(self.0);
            }
        }
    }
}
struct Queue(*mut c_void);
impl Drop for Queue {
    fn drop(&mut self) {
        // SAFETY: owns the queue_create reference. Stream invalidation and the
        // serial queue barrier have completed before the watch drops this field.
        unsafe {
            dispatch_release(self.0);
        }
    }
}
type Emit = Box<dyn FnMut(Result<Vec<PathBuf>, ()>) + Send>;
struct CallbackContext {
    paths: Vec<(PathBuf, bool)>,
    emit: Mutex<Emit>,
}
impl CallbackContext {
    fn emit(&self, paths: Result<Vec<PathBuf>, ()>) {
        (self.emit.lock().unwrap_or_else(|e| e.into_inner()))(paths);
    }
}

pub(super) struct NativeWatch {
    stream: Stream,
    started: bool,
    // Context is borrowed by the C stream, not owned through its retain/release
    // callbacks. Keep it and the retained CF path array alive through teardown.
    _context: Box<CallbackContext>,
    _paths: CfOwned,
    queue: Queue,
}
impl NativeWatch {
    pub(super) fn start(
        paths: Vec<(PathBuf, bool)>,
        emit: impl FnMut(Result<Vec<PathBuf>, ()>) + Send + 'static,
    ) -> io::Result<Self> {
        let api = api()?;
        // SAFETY: null allocator selects the default. The immutable callback
        // table is process-lived and retains/releases every appended CF string.
        let array = unsafe { (api.cf_array_create_mutable)(ptr::null(), 0, api.cf_type_array_callbacks) };
        if array.is_null() {
            return Err(io::Error::other("cannot create FSEvents path array"));
        }
        let array = CfOwned(array);
        for (path, _) in &paths {
            let path =
                CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other("event path contains NUL"))?;
            // SAFETY: copies a terminated filesystem representation. Conversion
            // failure is a normal watch failure, never a lossy path substitution.
            let string = unsafe { (api.cf_string_create_with_file_system_representation)(ptr::null(), path.as_ptr()) };
            if string.is_null() {
                return Err(io::Error::other("cannot convert FSEvents path"));
            }
            let string = CfOwned(string);
            unsafe {
                (api.cf_array_append_value)(array.0, string.0);
            }
        }
        let context = Box::new(CallbackContext { paths, emit: Mutex::new(Box::new(emit)) });
        // NULL attributes creates a serial queue: the callback cannot reenter.
        let queue = unsafe { dispatch_queue_create(c"tildy.fsevents".as_ptr(), ptr::null()) };
        if queue.is_null() {
            return Err(io::Error::other("cannot create event dispatch queue"));
        }
        let queue = Queue(queue);
        let mut c_context = StreamContext {
            version: 0,
            info: (&*context as *const CallbackContext).cast_mut().cast(),
            retain: None,
            release: None,
            copy_description: None,
        };
        // FileEvents + NoDefer + WatchRoot; no UseCFTypes, so eventPaths is char**.
        // u64::MAX is the public kFSEventStreamEventIdSinceNow constant.
        let stream = unsafe {
            (api.stream_create)(ptr::null(), callback, &mut c_context, array.0, u64::MAX, 0.0, 0x10 | 0x02 | 0x04)
        };
        if stream.is_null() {
            return Err(io::Error::other("cannot create FSEvents stream"));
        }
        let mut watch = Self { stream, started: false, _context: context, _paths: array, queue };
        // From here every failure is cleaned up by Drop; invalidate is legal
        // because this stream has been scheduled on a non-NULL queue.
        unsafe {
            (api.stream_set_dispatch_queue)(stream, watch.queue.0);
        }
        if unsafe { (api.stream_start)(stream) } == 0 {
            return Err(io::Error::other("cannot start FSEvents stream"));
        }
        watch.started = true;
        Ok(watch)
    }
}
unsafe extern "C" fn barrier(_: *mut c_void) {}
impl Drop for NativeWatch {
    fn drop(&mut self) {
        // This handle lives exclusively on the WatchManager thread, never on
        // its callback queue. SDK contract: stop only after successful start;
        // invalidate while scheduled; then release. The serial queue barrier
        // waits out callbacks already in progress before freeing context/paths.
        let Ok(api) = api() else { return };
        unsafe {
            if self.started {
                (api.stream_stop)(self.stream);
            }
            (api.stream_invalidate)(self.stream);
            dispatch_sync_f(self.queue.0, ptr::null_mut(), barrier);
            (api.stream_release)(self.stream);
        }
    }
}
unsafe extern "C" fn callback(
    _: Ref,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _: *const u64,
) {
    // No Rust unwind may cross the system callback ABI. Context stays alive
    // until Stop + Invalidate + serial-queue barrier have all completed.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let context = unsafe { &*info.cast::<CallbackContext>() };
        if count > 4096 || (count > 0 && (paths.is_null() || flags.is_null())) {
            context.emit(Err(()));
            return;
        }
        let mut changed = Vec::new();
        for i in 0..count {
            let flag = unsafe { *flags.add(i) };
            // MustScanSubDirs, dropped/wrapped IDs, moved root, mount/unmount:
            // fall back rather than pretending a potentially incomplete batch
            // is authoritative. HistoryDone alone carries no live path change.
            if flag & (0x01 | 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80) != 0 {
                context.emit(Err(()));
                return;
            }
            if flag == 0x10 {
                continue;
            }
            let path = unsafe { *(paths.cast::<*const c_char>().add(i)) };
            if path.is_null() {
                context.emit(Err(()));
                return;
            }
            let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
            let path = PathBuf::from(OsStr::from_bytes(bytes));
            if context.paths.iter().any(|(root, recursive)| {
                path == *root || if *recursive { path.starts_with(root) } else { path.parent() == Some(root) }
            }) {
                if changed.len() == 256 {
                    context.emit(Err(()));
                    return;
                }
                changed.push(path);
            }
        }
        if !changed.is_empty() {
            context.emit(Ok(changed));
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn invalid_path_is_not_a_successful_watch() {
        let path = PathBuf::from(OsStr::from_bytes(b"/invalid\0path"));
        assert!(NativeWatch::start(vec![(path, true)], |_| {}).is_err());
    }
    #[test]
    fn callback_handles_non_utf8_paths_and_preserves_nonrecursive_filter() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let copy = received.clone();
        let context = Box::new(CallbackContext {
            paths: vec![("/repo".into(), false)],
            emit: Mutex::new(Box::new(move |v| copy.lock().unwrap().push(v))),
        });
        let direct = CString::new(b"/repo/odd\xff".as_slice()).unwrap();
        let nested = c"/repo/sub/ignored";
        let mut paths = [direct.as_ptr(), nested.as_ptr()];
        let flags = [0x1000_u32; 2]; // ItemModified
        unsafe {
            callback(
                ptr::null(),
                (&*context as *const CallbackContext).cast_mut().cast(),
                2,
                paths.as_mut_ptr().cast(),
                flags.as_ptr(),
                ptr::null(),
            );
        }
        let records = received.lock().unwrap();
        assert_eq!(records[0].as_ref().unwrap(), &vec![PathBuf::from(OsStr::from_bytes(direct.as_bytes()))]);
    }
    #[test]
    fn dropped_event_batch_requests_fallback() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let copy = received.clone();
        let context = Box::new(CallbackContext {
            paths: vec![("/repo".into(), true)],
            emit: Mutex::new(Box::new(move |v| copy.lock().unwrap().push(v))),
        });
        let mut path = [c"/repo/file".as_ptr()];
        let flags = [0x02];
        unsafe {
            callback(
                ptr::null(),
                (&*context as *const CallbackContext).cast_mut().cast(),
                1,
                path.as_mut_ptr().cast(),
                flags.as_ptr(),
                ptr::null(),
            );
        }
        assert!(received.lock().unwrap()[0].is_err());
    }
    #[test]
    fn stream_start_failure_releases_callback_context() {
        let guard = Arc::new(());
        let weak = Arc::downgrade(&guard);
        let result = NativeWatch::start(Vec::new(), move |_| {
            let _keep_alive = &guard;
        });
        assert!(result.is_err(), "an empty watch cannot report readiness");
        assert!(weak.upgrade().is_none(), "failed creation/start must release the borrowed context");
    }
    #[test]
    fn active_drop_drains_callback_before_releasing_its_context() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let guard = Arc::new(());
        let weak = Arc::downgrade(&guard);
        let (tx, rx) = std::sync::mpsc::channel();
        let watch = NativeWatch::start(vec![(path.clone(), true)], move |event| {
            let _keep_alive = &guard;
            let _ = tx.send(event);
        })
        .unwrap();
        std::fs::write(path.join("changed"), "one").unwrap();
        assert!(rx.recv_timeout(std::time::Duration::from_millis(1500)).is_ok());
        drop(watch);
        assert!(weak.upgrade().is_none(), "serial barrier must finish before context release");
        while rx.try_recv().is_ok() {}
        std::fs::write(path.join("after-stop"), "two").unwrap();
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
