//! JNI entry points for `dev.luksandroid.LuksNative`.
//!
//! This file does marshalling and nothing else: Java values in, Rust values out,
//! and the reverse. All behaviour lives in [`bridge`], which has no JNI types in
//! it and is tested on the host.
//!
//! Two rules hold for every function here, and both have teeth:
//!
//! 1. **Wrapped in `catch_unwind`.** We parse GPT, LUKS and ext4 metadata that
//!    an attacker or a failing drive controls. A panic crossing the JNI boundary
//!    is undefined behaviour and shows up as a process abort with no Java stack
//!    trace. Caught, it becomes a `LuksException` the app can display. This is
//!    also why the release profile does not set `panic = "abort"`.
//! 2. **Passwords arrive as `byte[]`, never `String`.** A Java `String` is
//!    immutable and lives on the GC heap until collected, so it cannot be
//!    scrubbed. The array is copied into a [`Secret`](luks_core::secret::Secret)
//!    here and zeroed on drop; Kotlin overwrites its own copy in a `finally`.

pub mod bridge;

use std::panic::{catch_unwind, AssertUnwindSafe};

#[cfg(feature = "dangerous-write-support")]
use jni::objects::JByteArray;
use jni::objects::{JByteBuffer, JClass, JString, JValue};
use jni::sys::{jbyteArray, jint, jlong, jstring};
use jni::JNIEnv;

use luks_core::error::LuksError;

#[cfg(target_os = "android")]
#[allow(dead_code)]
mod log {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    const ANDROID_LOG_DEBUG: c_int = 3;
    const ANDROID_LOG_INFO: c_int = 4;
    const ANDROID_LOG_WARN: c_int = 5;
    const ANDROID_LOG_ERROR: c_int = 6;
    const TAG: &[u8] = b"luks_native\0";

    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub fn write(prio: c_int, msg: &str) {
        if let Ok(c_msg) = CString::new(msg) {
            unsafe {
                __android_log_write(prio, TAG.as_ptr() as *const c_char, c_msg.as_ptr());
            }
        }
    }

    pub fn d(msg: &str) { write(ANDROID_LOG_DEBUG, msg); }
    pub fn i(msg: &str) { write(ANDROID_LOG_INFO, msg); }
    pub fn w(msg: &str) { write(ANDROID_LOG_WARN, msg); }
    pub fn e(msg: &str) { write(ANDROID_LOG_ERROR, msg); }
}

#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
mod log {
    pub fn d(_msg: &str) {}
    pub fn i(_msg: &str) {}
    pub fn w(_msg: &str) {}
    pub fn e(_msg: &str) {}
}

const EXCEPTION_CLASS: &str = "dev/luksandroid/LuksException";

/// What a failed entry point reports back to Java.
enum Fail {
    Luks(LuksError),
    Msg(i32, String),
}

impl From<LuksError> for Fail {
    fn from(e: LuksError) -> Self {
        Fail::Luks(e)
    }
}

#[cfg(feature = "dangerous-write-support")]
impl From<bridge::WriterError> for Fail {
    fn from(e: bridge::WriterError) -> Self {
        match e {
            bridge::WriterError::BadHandle(m) => Fail::Msg(bridge::code::BAD_HANDLE, m.to_string()),
            bridge::WriterError::Luks(e) => Fail::Luks(e),
        }
    }
}

/// The text of `e` that may cross into Java, logcat or the UI.
///
/// Core's `Display` stays as it is (tests and tools read it), but several
/// variants interpolate a path or a name that lives on the encrypted volume or
/// on the host. Those keep their kind and lose the payload; the error code
/// travels separately. Every other variant carries only numbers or static
/// text, so it passes through unchanged.
fn redacted(e: &LuksError) -> String {
    use LuksError::*;
    match e {
        NotFound(_) => "not found".into(),
        NotADirectory(_) => "not a directory".into(),
        IsADirectory(_) => "is a directory".into(),
        DirectoryNotEmpty(_) => "directory is not empty".into(),
        SymlinkLoop(_) => "too many levels of symbolic links".into(),
        WrongWriteTarget { expected, detail, .. } => format!(
            "refusing to write: the target is not the {expected}-byte device it was confirmed as ({detail})"
        ),
        UnverifiableWriteTarget { .. } => {
            "refusing to write: could not determine the target's size to confirm it".into()
        }
        Io { source, .. } => format!("I/O error: {source}"),
        other => other.to_string(),
    }
}

impl Fail {
    fn parts(&self) -> (i32, String) {
        match self {
            Fail::Luks(e) => (bridge::error_code(e), redacted(e)),
            Fail::Msg(c, m) => (*c, m.clone()),
        }
    }
}

type R<T> = std::result::Result<T, Fail>;

/// Run `f`, turning any error *or panic* into a thrown `LuksException` and
/// returning `default`.
///
/// `AssertUnwindSafe` is honest here rather than a papering-over: on the panic
/// path we touch none of the state the closure was mutating. We read the panic
/// payload, throw, and return a default value the caller discards because an
/// exception is pending.
fn guard<'l, T>(env: &mut JNIEnv<'l>, default: T, f: impl FnOnce(&mut JNIEnv<'l>) -> R<T>) -> T {
    install_panic_hook();
    let outcome = catch_unwind(AssertUnwindSafe(|| f(env)));
    match outcome {
        Ok(Ok(value)) => value,
        Ok(Err(fail)) => {
            let (code, msg) = fail.parts();
            // `msg` is redacted (see `redacted`): variant kind, no payload.
            log::e(&format!("guard failed [{code}] {msg}"));
            throw(env, code, &msg);
            default
        }
        Err(_payload) => {
            let err_msg = report_panic();
            throw(env, bridge::code::PANIC, &err_msg);
            default
        }
    }
}

thread_local! {
    /// Where the most recent panic on this thread happened. The panic payload
    /// is formatted from whatever values the panicking code held, which can be
    /// a path or a name, so it is never read: only the source location is.
    static PANIC_SITE: std::cell::RefCell<Option<(String, u32, u32)>> =
        const { std::cell::RefCell::new(None) };
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(loc) = info.location() {
                let site = (loc.file().to_string(), loc.line(), loc.column());
                PANIC_SITE.with(|s| *s.borrow_mut() = Some(site));
            }
            // The default hook prints the payload to stderr (logcat). Under
            // the host test harness keep it, so failures stay readable.
            if cfg!(test) {
                previous(info);
            }
        }));
    });
}

/// Log and forensically record the panic just caught on this thread, and
/// return the text to throw. Never touches the payload.
fn report_panic() -> String {
    let (file, line, col) = PANIC_SITE
        .with(|s| s.borrow_mut().take())
        .unwrap_or_else(|| (file!().to_string(), line!(), 0));
    let file = file.as_str();
    log::e(&format!("guard panicked at {file}:{line}"));
    luks_core::forensic::record_panic(file, line, col, "panic (payload withheld)");
    format!("internal error (this is a bug): panic at {file}:{line}")
}

/// Throw `LuksException(message, code)`.
///
/// Falls back to `RuntimeException` if our own class is missing, which happens
/// if the `.so` and the APK drift apart. Silently swallowing that would produce
/// a call that neither returns a value nor throws — the worst possible outcome.
fn throw(env: &mut JNIEnv, code: i32, message: &str) {
    if env.exception_check().unwrap_or(false) {
        return; // something already threw; do not mask it
    }
    let attempt = (|| -> jni::errors::Result<()> {
        let class = env.find_class(EXCEPTION_CLASS)?;
        let jmsg = env.new_string(message)?;
        let ex = env.new_object(
            class,
            "(Ljava/lang/String;I)V",
            &[JValue::Object(&jmsg), JValue::Int(code)],
        )?;
        env.throw(jni::objects::JThrowable::from(ex))?;
        Ok(())
    })();

    if attempt.is_err() {
        let _ = env.exception_clear();
        let _ = env.throw_new("java/lang/RuntimeException", message);
    }
}

fn jstr(env: &mut JNIEnv, s: &JString) -> R<String> {
    env.get_string(s)
        .map(|v| v.into())
        .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("bad string argument: {e}")))
}

fn out_string(env: &mut JNIEnv, s: impl AsRef<str>) -> R<jstring> {
    env.new_string(s.as_ref())
        .map(|v| v.into_raw())
        .map_err(|e| {
            Fail::Msg(
                bridge::code::GENERIC,
                format!("cannot allocate string: {e}"),
            )
        })
}

fn out_bytes(env: &mut JNIEnv, b: &[u8]) -> R<jbyteArray> {
    env.byte_array_from_slice(b)
        .map(|v| v.into_raw())
        .map_err(|e| {
            Fail::Msg(
                bridge::code::GENERIC,
                format!("cannot allocate a {} byte array: {e}", b.len()),
            )
        })
}

fn bad_handle(msg: &'static str) -> Fail {
    Fail::Msg(bridge::code::BAD_HANDLE, msg.to_string())
}

// ------------------------------------------------------------------ lifecycle

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeDumpForensicLog<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let dump = luks_core::forensic::dump_text();
        out_string(env, dump)
    })
}

/// Take the lossless notable-event queue (F9 C4) as
/// `{"dropped":n,"events":[..]}`. Destructive: what is returned is removed.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeDrainEvents<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        out_string(env, luks_core::forensic::drain_notable_json())
    })
}

/// Snapshot of the whole forensic ring as `{"dropped":n,"events":[..]}`. The
/// ring is NOT cleared.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeRingJson<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        out_string(env, luks_core::forensic::ring_json())
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeClearForensicLog<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) {
    guard(&mut env, (), |_env| {
        luks_core::forensic::clear();
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeVersion<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        out_string(env, luks_core::VERSION)
    })
}

/// Take ownership of a USB device already opened by Java.
///
/// `fd` comes from `UsbDeviceConnection.getFileDescriptor()` and remains owned
/// by Java. It must outlive the returned handle: Kotlin must not close the
/// connection until after `nativeCloseDevice`.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeOpenDevice<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    fd: jint,
    ep_in: jint,
    ep_out: jint,
    interface: jint,
    max_transfer: jint,
) -> jlong {
    guard(&mut env, 0, |_env| {
        if fd < 0 {
            return Err(Fail::Msg(
                bridge::code::TRANSPORT,
                format!("invalid file descriptor {fd}"),
            ));
        }
        let ep_in = u8::try_from(ep_in)
            .map_err(|_| Fail::Msg(bridge::code::TRANSPORT, format!("bad IN endpoint {ep_in}")))?;
        let ep_out = u8::try_from(ep_out).map_err(|_| {
            Fail::Msg(
                bridge::code::TRANSPORT,
                format!("bad OUT endpoint {ep_out}"),
            )
        })?;
        let interface = u8::try_from(interface).map_err(|_| {
            Fail::Msg(
                bridge::code::TRANSPORT,
                format!("bad interface number {interface}"),
            )
        })?;

        // SAFETY: the fd is owned by the Java `UsbDeviceConnection`; the contract
        // that it outlives this handle is documented on the Kotlin side and
        // enforced by `LuksDevice` closing in the reverse order.
        let handle = unsafe {
            bridge::open_usb_device(fd, ep_in, ep_out, interface, max_transfer.max(0) as usize)
        }?;
        static OPEN_DEVICE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let open_idx = OPEN_DEVICE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        log::i(&format!(
            "nativeOpenDevice [open #{}]: opened fd={} ep_in={} ep_out={} interface={}",
            open_idx, fd, ep_in, ep_out, interface
        ));
        Ok(bridge::into_raw(bridge::Payload::Device(handle)))
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCloseDevice<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
) {
    guard(&mut env, (), |_env| {
        log::i("nativeCloseDevice");
        bridge::drop_device(handle);
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCloseVolume<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
) {
    guard(&mut env, (), |_env| {
        log::i("nativeCloseVolume");
        // Throws LuksException if the final close (open-writer release or the
        // btrfs batch commit) failed; the handle is dropped either way.
        Ok(bridge::close_volume(handle)?)
    })
}

// ----------------------------------------------------------- cancel tokens

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCreateCancelToken<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jlong {
    guard(&mut env, 0, |_env| {
        let token = bridge::register_cancel_token();
        Ok(token as jlong)
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCancelOperation<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    token_id: jlong,
) {
    guard(&mut env, (), |_env| {
        if token_id > 0 {
            bridge::cancel_operation(token_id as u64);
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCloseCancelToken<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    token_id: jlong,
) {
    guard(&mut env, (), |_env| {
        if token_id > 0 {
            bridge::unregister_cancel_token(token_id as u64);
        }
        Ok(())
    })
}

// --------------------------------------------------------------------- device

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeDeviceInfo<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let dev = bridge::device_ref(handle).map_err(bad_handle)?;
        let json = dev.info_json();
        out_string(env, &json)
    })
}

/// Derive the master key and mount the filesystem. Seconds, and up to a
/// gigabyte of allocation — never call this on the UI thread, and hold a
/// foreground service while it runs.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeUnlock<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    partition_offset: jlong,
    password: JByteBuffer<'l>,
    length: jint,
) -> jlong {
    guard(&mut env, 0, |env| {
        let dev = bridge::device_ref(handle).map_err(bad_handle)?;
        let offset = u64::try_from(partition_offset).map_err(|_| {
            Fail::Msg(
                bridge::code::GENERIC,
                format!("negative partition offset {partition_offset}"),
            )
        })?;

        let len = usize::try_from(length)
            .map_err(|_| Fail::Msg(bridge::code::GENERIC, "negative passphrase length".into()))?;

        let ptr = env
            .get_direct_buffer_address(&password)
            .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("not a direct ByteBuffer: {e}")))?;
        let cap = env
            .get_direct_buffer_capacity(&password)
            .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("cannot query direct buffer capacity: {e}")))?;

        if len > cap {
            return Err(Fail::Msg(
                bridge::code::GENERIC,
                "passphrase length exceeds buffer capacity".into(),
            ));
        }
        if ptr.is_null() {
            return Err(Fail::Msg(bridge::code::GENERIC, "direct buffer address is null".into()));
        }

        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };

        let start = std::time::Instant::now();
        let volume = dev.unlock(offset, slice)?;
        log::i(&format!("nativeUnlock: volume unlocked in {} ms", start.elapsed().as_millis()));
        Ok(bridge::into_raw(bridge::Payload::Volume(volume)))
    })
}

/// Open an unencrypted (plain) partition volume and mount the filesystem.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeOpenPlain<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    partition_offset: jlong,
) -> jlong {
    guard(&mut env, 0, |_env| {
        let dev = bridge::device_ref(handle).map_err(bad_handle)?;
        let offset = u64::try_from(partition_offset).map_err(|_| {
            Fail::Msg(
                bridge::code::GENERIC,
                format!("negative partition offset {partition_offset}"),
            )
        })?;

        let start = std::time::Instant::now();
        let volume = dev.open_plain(offset)?;
        log::i(&format!(
            "nativeOpenPlain: volume opened in {} ms",
            start.elapsed().as_millis()
        ));
        Ok(bridge::into_raw(bridge::Payload::Volume(volume)))
    })
}

// --------------------------------------------------------------------- volume

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeVolumeInfo<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let json = vol.info_json();
        out_string(env, &json)
    })
}

/// Record the user's explicit consent to write to an unencrypted volume.
///
/// Until this is called, every mutating operation on a plain volume is refused
/// with `ReadOnlyVolume`. Call it only in response to a deliberate user
/// confirmation naming the drive — never automatically on mount, which would
/// restore exactly the ungated state this exists to prevent.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeArmPlainWrites<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
) {
    let _ = guard(&mut env, 0i32, |_env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        vol.arm_plain_writes();
        log::i("nativeArmPlainWrites: plain-volume writes armed for this session");
        Ok(0i32)
    });
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeListDir<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path = jstr(env, &path)?;
        let json = vol.list_dir_json(&path)?;
        out_string(env, &json)
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeListDirPaged<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
    offset: jlong,
    limit: jlong,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path = jstr(env, &path)?;
        let offset_val = usize::try_from(offset.max(0)).unwrap_or(0);
        let limit_val = usize::try_from(limit.max(0)).unwrap_or(0);
        let json = vol.list_directory_paged(&path, offset_val, limit_val)?;
        out_string(env, &json)
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeFileInfo<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path = jstr(env, &path)?;
        let json = vol.file_info_json(&path)?;
        out_string(env, &json)
    })
}

/// Whole-file read. Fails rather than allocating past `max_bytes`, because the
/// result has to fit in the app heap as a `byte[]`.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeReadFile<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
    max_bytes: jlong,
) -> jbyteArray {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path = jstr(env, &path)?;
        let cap = if max_bytes <= 0 {
            32 * 1024 * 1024
        } else {
            max_bytes as u64
        };
        let data = vol.read_file(&path, cap)?;
        out_bytes(env, &data)
    })
}

/// Pin the file at `path` by identity and return a read handle (F4 B1).
///
/// The path is resolved exactly once, here. Reads go through
/// [`nativeReadAt`](Java_dev_luksandroid_LuksNative_nativeReadAt) by inode and
/// are refused with `FILE_CHANGED` if the file is deleted, replaced or modified
/// after this call. Close with `nativeCloseFile`.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeOpenFile<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
) -> jlong {
    guard(&mut env, 0, |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path = jstr(env, &path)?;
        let file = vol.open_file(&path)?;
        Ok(bridge::into_raw(bridge::Payload::File(file)))
    })
}

/// Read up to `len` bytes at `offset` of an open file. A shorter result means
/// end of file.
///
/// Returns a fresh array rather than filling a caller-supplied one: writing into
/// a Java array from Rust means handing out a `&mut [i8]` aliasing JVM memory,
/// and the copy is not the bottleneck at USB speeds.
///
/// Failures name the offset and the handle, never a path.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeReadAt<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
    file: jlong,
    offset: jlong,
    len: jint,
) -> jbyteArray {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(volume).map_err(bad_handle)?;
        let fh = bridge::file_ref(file).map_err(bad_handle)?;
        if fh.volume_id() != vol.id {
            return Err(bad_handle("file belongs to another volume"));
        }
        if offset < 0 || len <= 0 {
            return Err(Fail::Msg(
                bridge::code::GENERIC,
                format!("bad read request file={file} offset={offset} len={len}"),
            ));
        }
        let mut buf = vec![0u8; (len as usize).min(16 * 1024 * 1024)];
        let got = vol.read_at(&fh, offset as u64, &mut buf).map_err(|e| {
            Fail::Msg(
                bridge::error_code(&e),
                format!("read of file handle {file} at offset {offset} failed: {e}"),
            )
        })?;
        out_bytes(env, &buf[..got])
    })
}

/// Size of an open file, taken from the handle (identity-checked), not from a
/// second path lookup. `FILE_CHANGED` if the file is no longer the one opened.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeFileSize<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
    file: jlong,
) -> jlong {
    guard(&mut env, 0, |_env| {
        let vol = bridge::volume_ref(volume).map_err(bad_handle)?;
        let fh = bridge::file_ref(file).map_err(bad_handle)?;
        if fh.volume_id() != vol.id {
            return Err(bad_handle("file belongs to another volume"));
        }
        let size = vol.file_size(&fh).map_err(|e| {
            Fail::Msg(
                bridge::error_code(&e),
                format!("size of file handle {file} failed: {e}"),
            )
        })?;
        Ok(size as jlong)
    })
}

/// Cumulative `[verified, unverified]` bytes `nativeReadAt` has returned on
/// this handle. Verified = matched against a stored data checksum (btrfs).
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeFileCoverage<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    file: jlong,
) -> jni::sys::jlongArray {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let fh = bridge::file_ref(file).map_err(bad_handle)?;
        let (verified, unverified) = fh.coverage();
        let arr = env.new_long_array(2).map_err(|e| {
            Fail::Msg(bridge::code::GENERIC, format!("jni: new long[2] failed: {e}"))
        })?;
        env.set_long_array_region(&arr, 0, &[verified as jlong, unverified as jlong])
            .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("jni: set long[] failed: {e}")))?;
        Ok(arr.into_raw())
    })
}

/// Release an open-file handle. Idempotent: an unknown or already-closed
/// handle is not an error (a close must be safe to call from a `finally`).
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCloseFile<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    file: jlong,
) {
    guard(&mut env, (), |_env| {
        bridge::drop_file(file);
        Ok(())
    })
}

/// SHA-256 a file by streaming it, returning JSON with the digest, byte count
/// and elapsed time.
///
/// This is the acceptance check for the stack end to end. It never materialises
/// the file, so it works on the 1 GiB test file that `nativeReadFile` refuses.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeSha256<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
    chunk_bytes: jint,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path = jstr(env, &path)?;
        let json = vol.sha256_json(&path, chunk_bytes.max(0) as usize)?;
        out_string(env, &json)
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeSha256WithCancel<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
    chunk_bytes: jint,
    cancel_token: jlong,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path = jstr(env, &path)?;
        let token = if cancel_token > 0 { cancel_token as u64 } else { 0 };
        let json = vol.sha256_json_with_cancel(&path, chunk_bytes.max(0) as usize, token)?;
        out_string(env, &json)
    })
}

/// F8: drive-health check. JSON `{verdict, unknown, writeSafe, findings,
/// elapsedMs}`; see `bridge::health_json`. Never throws for a damaged drive:
/// damage is the answer. `cancel_token` <= 0 means none.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeHealthCheck<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    cancel_token: jlong,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let token = if cancel_token > 0 { cancel_token as u64 } else { 0 };
        let json = vol.health_check_json(token);
        out_string(env, &json)
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeReadFileStream<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
    chunk_bytes: jint,
    cancel_token: jlong,
) -> jlong {
    guard(&mut env, 0, |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path = jstr(env, &path)?;
        let token = if cancel_token > 0 { cancel_token as u64 } else { 0 };
        let mut sink = std::io::sink();
        let bytes = vol.read_file_stream(&path, &mut sink, chunk_bytes.max(0) as usize, token)?;
        Ok(bytes as jlong)
    })
}

/// Time a raw block read: no decryption, no filesystem, just the transport.
///
/// The diagnostic that separates "our layers are slow" from "the link is
/// slow". Returns JSON including the transfer size the kernel actually allowed.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeBenchmarkRead<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    bytes: jlong,
    chunk_bytes: jint,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let dev = bridge::device_ref(handle).map_err(bad_handle)?;
        let json = dev.benchmark_json(bytes.max(0) as u64, chunk_bytes.max(0) as usize)?;
        out_string(env, &json)
    })
}

/// Time a raw block write: no encryption, no filesystem, no Java array.
///
/// The mirror of `nativeBenchmarkRead`, and the only remaining way to tell
/// whether the read/write gap on this phone is ours or the hardware's. Writes
/// past the end of every partition — see `benchmark_write_json`.
#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeBenchmarkWrite<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    bytes: jlong,
    chunk_bytes: jint,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let dev = bridge::device_ref(handle).map_err(bad_handle)?;
        let json = dev.benchmark_write_json(bytes.max(0) as u64, chunk_bytes.max(0) as usize)?;
        out_string(env, &json)
    })
}

/// Measure AES-XTS and SHA-256 throughput in memory, with no USB involved.
///
/// Attributes the gap between the raw-read benchmark and the full-stack figure
/// to the layer actually responsible, instead of inferring it.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeSelfTest<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    mib: jint,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let json = bridge::self_test_json(mib.max(1) as usize);
        out_string(env, &json)
    })
}

// ---------------------------------------------------------------------- write

/// Whether this library was built with the write path in it.
///
/// Present in **both** builds, and answering honestly in each. It exists so
/// Kotlin can ask rather than assume its own build flavour matches the `.so`
/// it happened to load — those are produced by different tools and can
/// disagree. Without it the only way to find out is to call `nativeWriteFile`
/// and catch `UnsatisfiedLinkError`, which is a poor way to learn a static
/// fact.
///
/// Nothing declares this on the Kotlin side yet; wiring it into `LuksNative`
/// is part of the Android pass, not this one.
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeWriteSupported<'l>(
    _env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jni::sys::jboolean {
    u8::from(cfg!(feature = "dangerous-write-support"))
}

/// Create a file in the volume's root directory. Returns its inode number.
///
/// **This symbol does not exist in a default build.** That is the point: a
/// release `.so` cannot be made to write by any argument, because there is
/// nothing in it to call. `nativeWriteSupported` above is how Kotlin finds
/// that out before trying.
///
/// Slow and blocking — it allocates, writes every block, and then flushes all
/// the way through the USB bridge. Same rule as `nativeUnlock`: never on the
/// UI thread.
#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeWriteFile<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    parent_path: JString<'l>,
    name: JString<'l>,
    data: JByteArray<'l>,
) -> jlong {
    guard(&mut env, 0, |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let parent_path = jstr(env, &parent_path)?;
        let name = jstr(env, &name)?;
        let bytes = env
            .convert_byte_array(&data)
            .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("cannot read data: {e}")))?;
        let start = std::time::Instant::now();
        let ino = vol.write_file(&parent_path, &name, &bytes)?;
        log::i(&format!("nativeWriteFile: wrote {} bytes in {} ms", bytes.len(), start.elapsed().as_millis()));
        Ok(ino as jlong)
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeBeginFile<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
    size: jlong,
) -> jlong {
    guard(&mut env, 0, |_env| {
        let vol = bridge::volume_ref(volume).map_err(bad_handle)?;
        let size = u64::try_from(size)
            .map_err(|_| Fail::Msg(bridge::code::GENERIC, "negative file size".into()))?;
        let writer_id = vol.begin_file(size)?;
        Ok(bridge::into_raw(bridge::Payload::Writer(
            bridge::WriterHandle {
                volume_handle: volume,
                volume_id: vol.id,
                writer_id,
            },
        )))
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeBeginFileStreaming<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
) -> jlong {
    guard(&mut env, 0, |_env| {
        let vol = bridge::volume_ref(volume).map_err(bad_handle)?;
        let writer_id = vol.begin_file_streaming()?;
        Ok(bridge::into_raw(bridge::Payload::Writer(
            bridge::WriterHandle {
                volume_handle: volume,
                volume_id: vol.id,
                writer_id,
            },
        )))
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeWriteChunk<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
    writer: jlong,
    data: JByteBuffer<'l>,
    len: jint,
) {
    guard(&mut env, (), |env| {
        let vol = bridge::volume_ref(volume).map_err(bad_handle)?;
        let wh = bridge::writer_ref(writer).map_err(bad_handle)?;
        if wh.volume_id != vol.id {
            return Err(bad_handle("writer belongs to another volume"));
        }
        let len = usize::try_from(len)
            .map_err(|_| Fail::Msg(bridge::code::GENERIC, "negative chunk length".into()))?;

        let ptr = env.get_direct_buffer_address(&data)
            .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("not a direct ByteBuffer: {e}")))?;
        let cap = env.get_direct_buffer_capacity(&data)
            .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("cannot query direct buffer capacity: {e}")))?;

        if len > cap {
            return Err(Fail::Msg(bridge::code::GENERIC, "chunk length exceeds buffer capacity".into()));
        }
        if ptr.is_null() {
            return Err(Fail::Msg(bridge::code::GENERIC, "direct buffer address is null".into()));
        }

        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        Ok(vol.write_file_chunk(wh.writer_id, slice)?)
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeWriteChunkWithCancel<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
    writer: jlong,
    data: JByteBuffer<'l>,
    len: jint,
    cancel_token: jlong,
) {
    guard(&mut env, (), |env| {
        let vol = bridge::volume_ref(volume).map_err(bad_handle)?;
        let wh = bridge::writer_ref(writer).map_err(bad_handle)?;
        if wh.volume_id != vol.id {
            return Err(bad_handle("writer belongs to another volume"));
        }
        let len = usize::try_from(len)
            .map_err(|_| Fail::Msg(bridge::code::GENERIC, "negative chunk length".into()))?;

        let ptr = env.get_direct_buffer_address(&data)
            .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("not a direct ByteBuffer: {e}")))?;
        let cap = env.get_direct_buffer_capacity(&data)
            .map_err(|e| Fail::Msg(bridge::code::GENERIC, format!("cannot query direct buffer capacity: {e}")))?;

        if len > cap {
            return Err(Fail::Msg(bridge::code::GENERIC, "chunk length exceeds buffer capacity".into()));
        }
        if ptr.is_null() {
            return Err(Fail::Msg(bridge::code::GENERIC, "direct buffer address is null".into()));
        }

        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        let token = if cancel_token > 0 { cancel_token as u64 } else { 0 };
        Ok(vol.write_file_chunk_with_cancel(wh.writer_id, slice, token)?)
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeFinishFile<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
    writer: jlong,
    parent_path: JString<'l>,
    name: JString<'l>,
) -> jlong {
    guard(&mut env, 0, |env| {
        let vol = bridge::volume_ref(volume).map_err(bad_handle)?;
        let wh = bridge::writer_ref(writer).map_err(bad_handle)?;
        if wh.volume_id != vol.id {
            return Err(bad_handle("writer belongs to another volume"));
        }
        let parent_path = jstr(env, &parent_path)?;
        let name = jstr(env, &name)?;
        let start = std::time::Instant::now();
        // The volume releases the writer on every error path (F4-A3); the
        // global handle is spent either way.
        let result = vol.finish_file(wh.writer_id, &parent_path, &name);
        bridge::drop_writer(writer);
        let ino = result?;
        log::i(&format!("nativeFinishFile: file finished in {} ms", start.elapsed().as_millis()));
        Ok(ino as jlong)
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCommitActiveBatch<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
) {
    guard(&mut env, (), |_env| {
        let vol = bridge::volume_ref(volume).map_err(bad_handle)?;
        let start = std::time::Instant::now();
        vol.commit_active_batch()?;
        log::i(&format!("nativeCommitActiveBatch: committed in {} ms", start.elapsed().as_millis()));
        Ok(())
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCloseWriter<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    volume: jlong,
    writer: jlong,
) {
    guard(&mut env, (), |_env| {
        // Idempotent after the volume is gone (F4-A4): see `close_writer_handle`.
        Ok(bridge::close_writer_handle(volume, writer)?)
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeDeleteFile<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    path: JString<'l>,
) {
    guard(&mut env, (), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let path_str = jstr(env, &path)?;
        vol.delete_file(&path_str)?;
        log::i("nativeDeleteFile: file deleted");
        Ok(())
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeCreateDirectory<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    parent_path: JString<'l>,
    name: JString<'l>,
) -> jlong {
    guard(&mut env, 0, |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let parent_path_str = jstr(env, &parent_path)?;
        let name_str = jstr(env, &name)?;
        let ino = vol.create_directory(&parent_path_str, &name_str)?;
        log::i("nativeCreateDirectory: directory created");
        Ok(ino as jlong)
    })
}

#[cfg(feature = "dangerous-write-support")]
#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeRename<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    old_parent: JString<'l>,
    old_name: JString<'l>,
    new_parent: JString<'l>,
    new_name: JString<'l>,
) {
    guard(&mut env, (), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let old_parent_str = jstr(env, &old_parent)?;
        let old_name_str = jstr(env, &old_name)?;
        let new_parent_str = jstr(env, &new_parent)?;
        let new_name_str = jstr(env, &new_name)?;
        vol.rename(&old_parent_str, &old_name_str, &new_parent_str, &new_name_str)?;
        log::i("nativeRename: item renamed");
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_luksandroid_LuksNative_nativeStatFs<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
) -> jstring {
    guard(&mut env, std::ptr::null_mut(), |env| {
        let vol = bridge::volume_ref(handle).map_err(bad_handle)?;
        let json = vol.statfs_json()?;
        out_string(env, &json)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "Volumes/secret-folder/tax-return.pdf";

    fn assert_clean(what: &str, text: &str) {
        assert!(!text.contains("secret-folder"), "{what} leaked a path: {text}");
        assert!(!text.contains("tax-return"), "{what} leaked a name: {text}");
    }

    /// Every variant that interpolates a path or name, through the one
    /// function that builds the Java exception message. The control at the
    /// end proves the assertion can fail: core's own Display does leak.
    #[test]
    fn exception_message_never_carries_a_path() {
        let io = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let errors = [
            LuksError::NotFound(SECRET.into()),
            LuksError::NotADirectory(SECRET.into()),
            LuksError::IsADirectory(SECRET.into()),
            LuksError::DirectoryNotEmpty(SECRET.into()),
            LuksError::SymlinkLoop(SECRET.into()),
            LuksError::AlreadyExists(SECRET.into()),
            LuksError::WrongWriteTarget { path: SECRET.into(), expected: 7, detail: "d" },
            LuksError::UnverifiableWriteTarget { path: SECRET.into() },
            LuksError::Io { path: SECRET.into(), source: io() },
        ];
        let mut leaked_by_display = 0;
        for e in errors {
            let (code, msg) = Fail::Luks(e).parts();
            assert_clean("Fail::parts", &msg);
            assert!(!msg.is_empty() && code != 0);
        }
        for e in [LuksError::NotFound(SECRET.into()), LuksError::SymlinkLoop(SECRET.into())] {
            if e.to_string().contains("secret-folder") {
                leaked_by_display += 1;
            }
        }
        assert_eq!(leaked_by_display, 2, "control: raw Display must leak, or this test is vacuous");
    }

    /// A panic whose payload is a formatted path reaches neither the thrown
    /// text nor the forensic ring.
    #[test]
    fn panic_report_never_carries_the_payload() {
        install_panic_hook();
        let outcome = catch_unwind(|| {
            let name = SECRET;
            if !name.is_empty() {
                panic!("cannot open {name}");
            }
        });
        assert!(outcome.is_err());
        let text = report_panic();
        assert_clean("panic text", &text);
        assert!(text.contains("lib.rs"), "location kept: {text}");
        let panics: Vec<_> = luks_core::forensic::snapshot()
            .into_iter()
            .filter_map(|r| match r.event {
                luks_core::forensic::ForensicEvent::Panic(p) => Some(p),
                _ => None,
            })
            .collect();
        assert!(!panics.is_empty(), "panic was not recorded");
        for p in panics {
            assert_clean("forensic panic", &format!("{} {}", p.file, p.message));
        }
    }

    /// The two F9 entry points exist with the exact exported names Kotlin
    /// binds to (a rename here would be an UnsatisfiedLinkError at runtime).
    #[test]
    fn event_export_symbols_exist() {
        let drain: *const () = Java_dev_luksandroid_LuksNative_nativeDrainEvents as *const ();
        let ring: *const () = Java_dev_luksandroid_LuksNative_nativeRingJson as *const ();
        assert!(!drain.is_null() && !ring.is_null() && drain != ring);
    }

    /// A panic whose payload is a formatted path reaches the JSON handed to
    /// Kotlin as file:line only: no message, no payload text.
    #[test]
    fn event_json_for_a_panic_carries_no_path_text() {
        install_panic_hook();
        luks_core::forensic::drain_notable();
        let outcome = catch_unwind(|| {
            let name = SECRET;
            if !name.is_empty() {
                panic!("cannot open {name}");
            }
        });
        assert!(outcome.is_err());
        let _ = report_panic();
        for (what, json) in [
            ("ring", luks_core::forensic::ring_json()),
            ("drain", luks_core::forensic::drain_notable_json()),
        ] {
            assert_clean(what, &json);
            assert!(!json.contains("cannot open"), "{what}: panic message leaked: {json}");
            let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
            let panics: Vec<_> = v["events"]
                .as_array()
                .expect("events array")
                .iter()
                .filter(|o| o["ev"] == "panic")
                .collect();
            assert!(!panics.is_empty(), "{what}: panic event missing from {json}");
            // Other tests in this process panic too, from other files.
            for p in &panics {
                assert!(p["file"].as_str().is_some_and(|f| f.ends_with(".rs")), "{what}: {p}");
                assert!(p["line"].is_u64());
                assert!(p.get("message").is_none(), "{what}: {p}");
            }
            assert!(
                panics.iter().any(|p| p["file"].as_str().is_some_and(|f| f.ends_with("lib.rs"))),
                "{what}: this test's own panic site (lib.rs) is missing: {json}"
            );
        }
    }

    /// `LuksError::forensic_code` is a copy of `error_code` living in core
    /// (core cannot see the jni constants). This is what keeps them equal.
    #[test]
    fn forensic_code_agrees_with_the_jni_wire_code() {
        let io = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let errors = [
            LuksError::WrongPassword,
            LuksError::BadMagic { found: [0; 6] },
            LuksError::NoPartitionTable,
            LuksError::UnsupportedVersion(9),
            LuksError::Luks1NotImplemented,
            LuksError::UnsupportedFsFeature("x".into()),
            LuksError::NotExt4(0),
            LuksError::UnknownFs,
            LuksError::AmbiguousFs,
            LuksError::ChecksumMismatch,
            LuksError::Truncated { needed: 2, got: 1 },
            LuksError::OutOfBounds,
            LuksError::CorruptFs("x"),
            LuksError::FsChecksumMismatch("x"),
            LuksError::BadInode(1),
            LuksError::NotFound("x".into()),
            LuksError::NotADirectory("x".into()),
            LuksError::IsADirectory("x".into()),
            LuksError::DirectoryNotEmpty("x".into()),
            LuksError::ScsiProtocol("x"),
            LuksError::ScsiCommandFailed { opcode: 0x2a, sense: None },
            LuksError::UsbTransfer("x".into()),
            LuksError::usb_errno(110, "x"),
            LuksError::FsNeedsRecovery,
            LuksError::Io { path: "x".into(), source: io() },
            LuksError::FilesystemFull,
            LuksError::NoFreeInodes,
            LuksError::WrongWriteTarget { path: "x".into(), expected: 1, detail: "x" },
            LuksError::UnverifiableWriteTarget { path: "x".into() },
            LuksError::AlreadyExists("x".into()),
            LuksError::WriterBusy,
            LuksError::BtrfsItemTooLarge { what: "x", item_bytes: 1, node_size: 1 },
            LuksError::SessionPoisoned,
            LuksError::Cancelled,
            LuksError::WriteSessionFenced("x".into()),
            LuksError::ReadOnlyVolume,
            LuksError::FileChanged,
            LuksError::KdfFailed("x".into()),
        ];
        for e in &errors {
            assert_eq!(e.forensic_code(), bridge::error_code(e), "{e:?}");
        }
    }
}
