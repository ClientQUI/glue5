//! Crash-surviving diagnostic breadcrumbs.
//!
//! The regular `log` output is useful while a device is connected, but a
//! frozen phone has to be force-quit before it can be inspected. This module
//! keeps a small, durable log in the app's user-visible Documents folder:
//! normal entries are flushed immediately and synced every few seconds;
//! traps, panics and engine fatal errors are synced straight away. A POSIX
//! signal handler can only append a fixed marker, which is deliberately all
//! it does: allocating, locking or formatting after a memory fault would make
//! the crash less reliable, not more useful.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;

const DIAGNOSTICS_FILE: &str = "diagnostics.log";
const ENGINE_FILE: &str = "engine.log";
const MAX_DIAGNOSTICS_BYTES: u64 = 8 << 20;
const MAX_ENGINE_BYTES: u64 = 32 << 20;
const SYNC_INTERVAL: Duration = Duration::from_secs(15);
static VERBOSE_LOGS: AtomicBool = AtomicBool::new(false);

pub fn set_verbose_logs(on: bool) {
    VERBOSE_LOGS.store(on, Ordering::Relaxed);
    crate::gamelog::set_verbose(on);
}

pub(crate) fn verbose_logs() -> bool {
    VERBOSE_LOGS.load(Ordering::Relaxed)
}

struct Writer {
    file: File,
    /// An optional, user-visible copy. On iOS this is normally inside the
    /// game's folder, which also makes it available when an app is running
    /// inside a container whose Documents directory Files does not expose.
    mirror: Option<File>,
    last_sync: Instant,
}

struct Diagnostics {
    dir: PathBuf,
    started: Instant,
    writer: Mutex<Writer>,
    mirror_dir: Mutex<Option<PathBuf>>,
    last_heartbeat_ms: AtomicU64,
}

static DIAGNOSTICS: OnceLock<Diagnostics> = OnceLock::new();
static LAST_FRAME_MS: AtomicU64 = AtomicU64::new(0);
static GPU_COMMAND: AtomicU64 = AtomicU64::new(0);
static GPU_SUBMIT_STAGE: AtomicU64 = AtomicU64::new(0);
static GPU_COMPLETION_STAGE: AtomicU64 = AtomicU64::new(0);
static MAIN_CLOCK_MS: AtomicU64 = AtomicU64::new(0);
static RENDER_CLOCK_MS: AtomicU64 = AtomicU64::new(0);

/// Clock calls are progress breadcrumbs, not stack samples or proof of frames.
/// Cache the native thread name so the hot clock import needs no allocation.
pub(crate) fn engine_clock(now: u64) {
    thread_local! {
        static KIND: u8 = {
            let thread = std::thread::current();
            match thread.name() {
                Some("engine main") => 1,
                Some(name) if name.contains("RenderThread") => 2,
                _ => 0,
            }
        };
    }
    KIND.with(|kind| match kind {
        1 => {
            MAIN_CLOCK_MS.store(now, Ordering::Relaxed);
            crate::wait_diagnostics::clock(now);
        }
        2 => {
            RENDER_CLOCK_MS.store(now, Ordering::Relaxed);
            crate::wait_diagnostics::clock(now);
        }
        _ => {}
    });
}

/// Actual presentation progress, rather than a registered GPU ring.
pub fn frame_presented() {
    if let Some(d) = DIAGNOSTICS.get() {
        LAST_FRAME_MS.store(d.started.elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
    }
}

/// Last GPU command entered (it may still be executing).
pub fn gpu_command(op: u32) {
    GPU_COMMAND.store(op as u64, Ordering::Relaxed);
}

/// 0: idle, 1: encoding wait, 2: drawable, 3: submit,
/// 4: present, 5: resource release, 6: frame pacing.
pub fn gpu_submit_stage(stage: u64) {
    GPU_SUBMIT_STAGE.store(stage, Ordering::Relaxed);
}

/// 0: idle, 1: submission wait, 2: readbacks/callbacks.
pub fn gpu_completion_stage(stage: u64) {
    GPU_COMPLETION_STAGE.store(stage, Ordering::Relaxed);
}

fn watch_frames() {
    crate::qos::set(crate::qos::Qos::Utility);
    let mut reported = false;
    let mut last_report = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        heartbeat();
        let d = DIAGNOSTICS.get().expect("watchdog starts after diagnostics");
        let last = LAST_FRAME_MS.load(Ordering::Relaxed);
        // Do not classify startup before the first presented frame as a freeze.
        let age = (d.started.elapsed().as_millis() as u64 + 1).saturating_sub(last);
        if last != 0 && age >= 5_000 {
            if !reported || last_report.elapsed() >= Duration::from_secs(30) {
                let submit = match GPU_SUBMIT_STAGE.load(Ordering::Relaxed) {
                    1 => "encoding wait",
                    2 => "drawable acquisition",
                    3 => "queue submit",
                    4 => "presentation",
                    5 => "resource release",
                    6 => "frame pacing",
                    _ => "idle",
                };
                let completion = match GPU_COMPLETION_STAGE.load(Ordering::Relaxed) {
                    1 => "submission wait",
                    2 => "readbacks/callbacks",
                    _ => "idle",
                };
                let now = crate::time::date_now() as u64;
                let clock_age = |clock: &AtomicU64| {
                    let last = clock.load(Ordering::Relaxed);
                    if last == 0 { "unknown".to_owned() } else { format!("{}ms", now.saturating_sub(last)) }
                };
                fault(format_args!(
                    "frame watchdog: no presentation for {age}ms; last_gpu_command={} submit={submit} completion={completion} main_clock_age={} render_clock_age={} (clock calls are not frames; background/suspension may also stop presentation)",
                    GPU_COMMAND.load(Ordering::Relaxed),
                    clock_age(&MAIN_CLOCK_MS),
                    clock_age(&RENDER_CLOCK_MS)
                ));
                crate::wait_diagnostics::dump();
                reported = true;
                last_report = Instant::now();
            }
        } else if reported {
            note(format_args!("frame watchdog: presentation resumed"));
            reported = false;
        }
    }
}

/// Starts the diagnostics log. Calling this again in the same process keeps
/// the original, already-open log, which is useful to the headless runner.
pub fn start(dir: &Path) -> io::Result<PathBuf> {
    if let Some(diagnostics) = DIAGNOSTICS.get() {
        return Ok(diagnostics.path(DIAGNOSTICS_FILE));
    }

    fs::create_dir_all(dir)?;
    let path = dir.join(DIAGNOSTICS_FILE);
    rotate(&path, MAX_DIAGNOSTICS_BYTES)?;
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    #[cfg(unix)]
    let fd = file.as_raw_fd();
    let diagnostics = Diagnostics {
        dir: dir.to_path_buf(),
        started: Instant::now(),
        writer: Mutex::new(Writer { file, mirror: None, last_sync: Instant::now() }),
        mirror_dir: Mutex::new(None),
        last_heartbeat_ms: AtomicU64::new(0),
    };
    if DIAGNOSTICS.set(diagnostics).is_err() {
        return Ok(DIAGNOSTICS.get().expect("diagnostics just initialised").path(DIAGNOSTICS_FILE));
    }
    #[cfg(unix)]
    install_signal_handlers(fd);
    write("note", format_args!("session start: pid={}, build={}", std::process::id(), crate::GAME_BUILD), true);
    std::thread::Builder::new().name("frame watchdog".into()).spawn(watch_frames)?;
    Ok(path)
}

/// Adds a second durable copy of both logs. This is deliberately configured
/// only after the game folder is known: on iOS that folder is commonly the
/// only part of a nested application's storage that a container exposes to
/// Files.
pub fn mirror_to(dir: &Path) -> io::Result<PathBuf> {
    let Some(diagnostics) = DIAGNOSTICS.get() else {
        return Err(io::Error::other("diagnostics have not started"));
    };
    fs::create_dir_all(dir)?;
    let path = dir.join(DIAGNOSTICS_FILE);
    rotate(&path, MAX_DIAGNOSTICS_BYTES)?;
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    #[cfg(unix)]
    let fd = file.as_raw_fd();
    {
        let mut writer = diagnostics.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        writer.mirror = Some(file);
    }
    *diagnostics.mirror_dir.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(dir.to_path_buf());
    // A native crash can only append to one descriptor. Prefer the visible
    // copy once it exists.
    #[cfg(unix)]
    SIGNAL_FD.store(fd, Ordering::Release);
    Ok(path)
}

/// A normal diagnostic breadcrumb. It is flushed immediately, and persisted
/// to disk at least every [`SYNC_INTERVAL`].
pub fn note(args: fmt::Arguments<'_>) {
    write("note", args, false);
}

/// A failure breadcrumb. It is synced before this function returns whenever
/// the filesystem permits it.
pub fn fault(args: fmt::Arguments<'_>) {
    write("fault", args, true);
}

/// The runtime's current state, sampled by the independent watchdog and app's event loop. If the OS
/// kills the app without delivering a catchable signal (Jetsam), the final
/// heartbeat is still enough to distinguish memory pressure from a stopped
/// engine thread.
pub fn heartbeat() {
    let Some(diagnostics) = DIAGNOSTICS.get() else { return };
    let elapsed_ms = diagnostics.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let previous = diagnostics.last_heartbeat_ms.load(Ordering::Relaxed);
    let heartbeat_interval_ms = if verbose_logs() { 1_000 } else { 5_000 };
    if elapsed_ms.saturating_sub(previous) < heartbeat_interval_ms
        || diagnostics
            .last_heartbeat_ms
            .compare_exchange(previous, elapsed_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }

    let (footprint, available) = crate::memory::footprint();
    let (resource_budget, resource_high_water) = crate::memory::resource_heap_stats();
    let (scenario_workaround, scenario_full_lists) = crate::scenario::diagnostics_status();
    if let Some(runtime) = crate::RUNTIME.get() {
        let last_wasm_clock = runtime.wasm_clock_ms.load(Ordering::Relaxed);
        let wasm_liveness = if last_wasm_clock == 0 {
            "not-yet-called".to_owned()
        } else {
            format!("{}ms ago", (crate::time::date_now() as u64).saturating_sub(last_wasm_clock))
        };
        note(format_args!(
            "heartbeat: uptime={}ms footprint={}MiB available={}MiB wasm={}MiB resource={}MiB/{}MiB wasm_now={} input={} audio={} gpu={} scenario_workaround={} full_overlap_lists={}",
            elapsed_ms,
            footprint.unwrap_or(0) >> 20,
            available.unwrap_or(0) >> 20,
            runtime.memory.size() >> 20,
            resource_high_water >> 20,
            resource_budget >> 20,
            wasm_liveness,
            runtime.input_block.load(Ordering::Relaxed) != 0,
            runtime.audio_ring.load(Ordering::Relaxed) != 0,
            runtime.gpu_ring.load(Ordering::Relaxed) != 0,
            scenario_workaround,
            scenario_full_lists,
        ));
    } else {
        note(format_args!("heartbeat: uptime={}ms runtime=not-started", elapsed_ms));
    }
}

/// GPU state is sampled by its command thread every statistics interval.
pub fn gpu_snapshot(
    fps: f64,
    textures: u64,
    buffers: u64,
    uniforms: u64,
    objects: usize,
    pipelines: u32,
    failed_pipelines: u32,
    errors: u64,
) {
    note(format_args!(
        "gpu: fps={fps:.1} textures={}MiB buffers={}MiB uniforms={}MiB objects={objects} pipelines={pipelines} failed_pipelines={failed_pipelines} errors={errors}",
        textures >> 20,
        buffers >> 20,
        uniforms >> 20,
    ));
}

/// Engine-log destinations, primary first and the visible mirror second.
/// Returning `None` keeps the headless runtime usable without the platform
/// app having set up diagnostics first.
pub(crate) fn engine_log_paths() -> io::Result<Option<Vec<PathBuf>>> {
    let Some(diagnostics) = DIAGNOSTICS.get() else { return Ok(None) };
    let mut paths = vec![diagnostics.path(ENGINE_FILE)];
    if let Some(dir) = diagnostics.mirror_dir.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone() {
        paths.push(dir.join(ENGINE_FILE));
    }
    for path in &paths {
        rotate(path, MAX_ENGINE_BYTES)?;
    }
    Ok(Some(paths))
}

fn write(kind: &str, args: fmt::Arguments<'_>, force_sync: bool) {
    let Some(diagnostics) = DIAGNOSTICS.get() else { return };
    let elapsed_ms = diagnostics.started.elapsed().as_millis();
    let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed");
    let mut writer = diagnostics.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = writeln!(writer.file, "{unix_ms} +{elapsed_ms}ms [{thread}] {kind}: {args}");
    if let Some(mirror) = &mut writer.mirror {
        let _ = writeln!(mirror, "{unix_ms} +{elapsed_ms}ms [{thread}] {kind}: {args}");
    }
    let _ = writer.file.flush();
    if let Some(mirror) = &mut writer.mirror {
        let _ = mirror.flush();
    }
    if force_sync || writer.last_sync.elapsed() >= SYNC_INTERVAL {
        let _ = writer.file.sync_data();
        if let Some(mirror) = &mut writer.mirror {
            let _ = mirror.sync_data();
        }
        writer.last_sync = Instant::now();
    }
}

fn rotate(path: &Path, limit: u64) -> io::Result<()> {
    if fs::metadata(path).is_ok_and(|meta| meta.len() > limit) {
        let previous = path.with_extension("previous.log");
        let _ = fs::remove_file(&previous);
        fs::rename(path, previous)?;
    }
    Ok(())
}

impl Diagnostics {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

// A Jetsam termination uses SIGKILL, which cannot be caught. The heartbeat
// above is the evidence in that case. The signals below are the common native
// crash paths and only use `write(2)`, which is async-signal-safe.
#[cfg(unix)]
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(unix)]
fn install_signal_handlers(fd: i32) {
    SIGNAL_FD.store(fd, Ordering::Release);
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = fatal_signal as *const () as usize;
        action.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGABRT, libc::SIGBUS, libc::SIGFPE, libc::SIGILL, libc::SIGSEGV] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                log::warn!("diagnostics: cannot install signal handler {signal}");
            }
        }
    }
}

#[cfg(unix)]
unsafe extern "C" fn fatal_signal(signal: libc::c_int) {
    let message: &[u8] = match signal {
        libc::SIGABRT => b"[signal] SIGABRT\n",
        libc::SIGBUS => b"[signal] SIGBUS\n",
        libc::SIGFPE => b"[signal] SIGFPE\n",
        libc::SIGILL => b"[signal] SIGILL\n",
        libc::SIGSEGV => b"[signal] SIGSEGV\n",
        _ => b"[signal] unknown fatal signal\n",
    };
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let _ = unsafe { libc::write(fd, message.as_ptr().cast(), message.len()) };
    }
}
