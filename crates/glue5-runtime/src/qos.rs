//! Thread priorities. On Apple platforms they are quality-of-service classes.
//! The system chooses the cores: user-interactive prefers performance cores,
//! utility and background prefer efficiency cores. This process does not pin
//! cores and does not use the Neural Engine.
//!
//! Hybrid (the recommended mode) runs startup, loading, teleports and a
//! streaming backlog at user-interactive, then drops the game, render and
//! GPU threads to utility once play is steady. A missed frame steps those
//! threads back up, then down again. Full-E leaves them at utility. The
//! mixer, the playback pump and the device callback stay user-interactive
//! in every mode, and on Apple the callback also takes a time-constraint
//! schedule and joins an audio workgroup. On Android only user-interactive
//! threads join the performance hint session ([`frame`]).

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::sched::{self, Class, Demand, Mode, Pace, PaceSample, Role};

#[derive(Clone, Copy, Debug)]
pub enum Qos {
    /// A frame waits on it (the engine's main thread, the renderer, the GPU).
    Interactive,
    /// Work the game needs soon (engine workers, shaders being prepared).
    Initiated,
    /// Deferrable work that should prefer efficiency cores (streaming, the
    /// mixer, logs). Still scheduled; not held back the way background is.
    Utility,
    /// Work that may wait until the machine is idle (a dedicated save thread).
    Background,
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Hybrid as u8);
static DEMAND: AtomicU8 = AtomicU8::new(Demand::Boost as u8);
static FORCED: OnceLock<Option<Mode>> = OnceLock::new();
static PACE: Mutex<Pace> = Mutex::new(Pace::new());
static WARP: AtomicBool = AtomicBool::new(false);
static STREAM_BYTES: AtomicU64 = AtomicU64::new(0);
static STREAM_JOBS: AtomicU32 = AtomicU32::new(0);
/// Bumped when the class a thread should hold changes, so it can re-apply
/// without a syscall on the frames in between.
static PLACEMENT: AtomicU64 = AtomicU64::new(1);

fn mode_from(byte: u8) -> Mode {
    match byte {
        0 => Mode::Off,
        2 => Mode::FullE,
        _ => Mode::Hybrid,
    }
}

fn demand_from(byte: u8) -> Demand {
    match byte {
        1 => Demand::Hitch,
        2 => Demand::Ease,
        3 => Demand::Cruise,
        _ => Demand::Boost,
    }
}

/// `GLUE5_EFFICIENCY` is `off`, `hybrid`, or `full` / `full-e`. Otherwise
/// `GLUE5_EFFICIENCY_MODE=1` is full-E and `=0` is off, and
/// `GLUE5_EFFICIENCY_CORES=0` is off. Unset follows the menu.
pub fn forced_mode() -> Option<Mode> {
    *FORCED.get_or_init(|| {
        if let Some(mode) = std::env::var("GLUE5_EFFICIENCY").ok().as_deref().and_then(parse_mode) {
            return Some(mode);
        }
        match std::env::var("GLUE5_EFFICIENCY_MODE").ok().as_deref() {
            Some("1") => return Some(Mode::FullE),
            Some("0") => return Some(Mode::Off),
            _ => {}
        }
        if std::env::var("GLUE5_EFFICIENCY_CORES").ok().as_deref() == Some("0") {
            return Some(Mode::Off);
        }
        None
    })
}

fn parse_mode(text: &str) -> Option<Mode> {
    match text.trim().to_ascii_lowercase().as_str() {
        "0" | "off" => Some(Mode::Off),
        "hybrid" => Some(Mode::Hybrid),
        "1" | "full" | "full-e" | "fulle" => Some(Mode::FullE),
        _ => None,
    }
}

/// The menu's choice. An environment force still wins in [`mode`].
pub fn set_mode(mode: Mode) {
    let prev = MODE.swap(mode as u8, Ordering::Relaxed);
    let effective = forced_mode().unwrap_or(mode);
    if let Ok(mut pace) = PACE.lock() {
        pace.set_mode(effective);
        publish(pace.demand());
    }
    if prev != mode as u8 {
        PLACEMENT.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn mode() -> Mode {
    forced_mode().unwrap_or_else(|| mode_from(MODE.load(Ordering::Relaxed)))
}

pub fn demand() -> Demand {
    demand_from(DEMAND.load(Ordering::Relaxed))
}

/// Frames per second this mode paces at. The menu's own cap can only go lower.
pub fn pace_fps() -> Option<u32> {
    match mode() {
        Mode::Off => None,
        Mode::Hybrid => Some(24),
        Mode::FullE => Some(20),
    }
}

/// Playback does not start, and does not restart after an underrun, until
/// this many frames are buffered. Hybrid and full-E keep 250 ms so a stall
/// on the game thread does not reach the speakers. Off keeps 100 ms.
pub fn audio_prime_frames(rate: u32) -> usize {
    let ms = match mode() {
        Mode::Off => 100,
        Mode::Hybrid | Mode::FullE => 250,
    };
    rate as usize * ms / 1000
}

/// The game is starting. Threads already running pick the boost up on their
/// next refresh, which the game and render threads do every frame.
pub fn note_load() {
    WARP.store(true, Ordering::Relaxed);
    if let Ok(mut pace) = PACE.lock() {
        pace.note_load();
        publish(pace.demand());
    }
}

/// `CWarpManager::SetWarp`, or a camera that jumped further than a frame of
/// driving can explain.
pub fn note_warp() {
    note_load();
}

/// Texture decodes still waiting. A backlog boosts immediately.
pub fn note_stream(bytes: u64, jobs: u32) {
    STREAM_BYTES.store(bytes, Ordering::Relaxed);
    STREAM_JOBS.store(jobs, Ordering::Relaxed);
    if bytes >= Pace::STREAM_BYTES || jobs >= Pace::STREAM_JOBS {
        observe(PaceSample { frame_ms: 0.0, budget_ms: 1.0, stream_bytes: bytes, stream_jobs: jobs, warp: false });
    }
}

/// A presented frame, `took_ms` after the one before, against `budget_ms`.
pub fn note_frame(took_ms: f32, budget_ms: f32) {
    let warp = WARP.swap(false, Ordering::Relaxed);
    observe(PaceSample {
        frame_ms: took_ms,
        budget_ms,
        stream_bytes: STREAM_BYTES.load(Ordering::Relaxed),
        stream_jobs: STREAM_JOBS.load(Ordering::Relaxed),
        warp,
    });
}

fn observe(sample: PaceSample) {
    let Ok(mut pace) = PACE.lock() else { return };
    if pace.mode() != mode() {
        pace.set_mode(mode());
    }
    let demand = pace.observe(sample);
    publish(demand);
}

fn publish(demand: Demand) {
    if DEMAND.swap(demand as u8, Ordering::Relaxed) != demand as u8 {
        PLACEMENT.fetch_add(1, Ordering::Relaxed);
    }
}

/// The device callback. It only copies a filled buffer, and it is the
/// deadline, so it stays user-interactive in every mode. On Apple it also
/// takes a time-constraint schedule and joins an audio workgroup, so a
/// utility game thread cannot push it off the core.
pub fn set_audio_callback() {
    use std::cell::Cell;
    thread_local! {
        static DONE: Cell<bool> = const { Cell::new(false) };
    }
    DONE.with(|done| {
        if done.replace(true) {
            return;
        }
        set_rel(Qos::Interactive, 0);
        renice(-10);
        #[cfg(target_vendor = "apple")]
        realtime::promote();
    });
}

/// The playback pump and the game's mixer. They stay user-interactive in
/// every mode. The cushion is what covers a late mix; dropping them to
/// utility is what emptied it while the game was on efficiency cores.
pub fn set_audio() {
    set_rel(Qos::Interactive, 0);
    renice(-10);
}

#[derive(Clone, Copy)]
enum Bind {
    Unset,
    Engine(Role),
    Host(Qos),
}

thread_local! {
    static BIND: Cell<Bind> = const { Cell::new(Bind::Unset) };
    static APPLIED: Cell<u64> = const { Cell::new(0) };
}

/// Class for an engine pthread, from its name. [`refresh`] applies a later
/// change of the experimental mode on this thread.
pub fn apply_engine_thread(name: Option<&str>) {
    let role = name.map(sched::role).unwrap_or(Role::Worker);
    BIND.with(|bind| bind.set(Bind::Engine(role)));
    APPLIED.with(|applied| applied.set(0));
    refresh();
}

/// A host thread whose normal class is `normal` (the GPU command, submit,
/// encode and completion threads). Hybrid cruise and full-E use utility.
pub fn bind_host(normal: Qos) {
    BIND.with(|bind| bind.set(Bind::Host(normal)));
    APPLIED.with(|applied| applied.set(0));
    refresh();
}

/// Re-applies this thread's class when the menu flags have changed.
/// One atomic load until they do.
pub fn refresh() {
    let stamp = placement_stamp();
    let changed = APPLIED.with(|applied| {
        if applied.get() == stamp {
            false
        } else {
            applied.set(stamp);
            true
        }
    });
    if !changed {
        return;
    }
    match BIND.with(|bind| bind.get()) {
        Bind::Unset => {}
        Bind::Engine(role) => install_engine(role),
        Bind::Host(normal) => set(host_qos(normal)),
    }
}

/// Where a GPU thread sits. `normal` is its class with efficiency off.
pub fn host_qos(normal: Qos) -> Qos {
    match mode() {
        Mode::Off => normal,
        Mode::FullE => Qos::Utility,
        Mode::Hybrid => match demand() {
            Demand::Cruise => Qos::Utility,
            Demand::Ease => Qos::Initiated,
            Demand::Boost | Demand::Hitch => normal,
        },
    }
}

/// Shader compiles stay on the compile pool, never the game thread.
/// User-initiated during a load (still performance cores, below the frame),
/// utility once play is steady.
pub fn compile_qos() -> Qos {
    match mode() {
        Mode::Off => Qos::Initiated,
        Mode::FullE => Qos::Utility,
        Mode::Hybrid => match demand() {
            Demand::Boost | Demand::Hitch => Qos::Initiated,
            Demand::Ease | Demand::Cruise => Qos::Utility,
        },
    }
}

/// BC decompression stays on the decode pool. A streaming burst wants it
/// on performance cores; steady play wants efficiency cores.
pub fn decompress_qos() -> Qos {
    match mode() {
        Mode::Off => Qos::Initiated,
        Mode::FullE => Qos::Utility,
        Mode::Hybrid => match demand() {
            Demand::Boost => Qos::Interactive,
            Demand::Hitch | Demand::Ease | Demand::Cruise => Qos::Utility,
        },
    }
}

fn placement_stamp() -> u64 {
    let mode = mode() as u64;
    let demand = demand() as u64;
    PLACEMENT.load(Ordering::Relaxed) << 8 | mode << 4 | demand
}

fn install_engine(role: Role) {
    if role == Role::Audio {
        set_audio();
        return;
    }
    match sched::class_for(role, mode(), demand()) {
        Class::Interactive => set(Qos::Interactive),
        Class::Initiated => set(Qos::Initiated),
        Class::Utility => set(Qos::Utility),
        Class::Background => set(Qos::Background),
    }
}

/// Runs `body` at `qos`, then puts the previous class back. No change when
/// efficiency is off, or when the current class cannot be read (the calling
/// thread must not be left in the lower class).
pub fn lower<R>(qos: Qos, body: impl FnOnce() -> R) -> R {
    let _guard = if mode() != Mode::Off { push(qos) } else { None };
    body()
}

/// Sets the calling thread's class. Utility sits below the mixer inside
/// that class, so streaming does not queue ahead of audio.
pub fn set(qos: Qos) {
    let rel = match qos {
        Qos::Utility => -5,
        _ => 0,
    };
    set_rel(qos, rel);
    // Interactive is nice 0, so leaving the experimental utility test puts the
    // thread back. The audio callback sets its own niceness and does not use this.
    renice(match qos {
        Qos::Background => 10,
        Qos::Utility => 5,
        Qos::Interactive | Qos::Initiated => 0,
    });
}

fn set_rel(qos: Qos, relative: i32) {
    #[cfg(target_vendor = "apple")]
    unsafe {
        use libc::qos_class_t::*;
        let class = match qos {
            Qos::Interactive => QOS_CLASS_USER_INTERACTIVE,
            Qos::Initiated => QOS_CLASS_USER_INITIATED,
            Qos::Utility => QOS_CLASS_UTILITY,
            Qos::Background => QOS_CLASS_BACKGROUND,
        };
        libc::pthread_set_qos_class_self_np(class, relative);
    }
    #[cfg(target_os = "android")]
    {
        let _ = relative;
        if let Qos::Interactive = qos {
            hint::join();
        }
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "android")))]
    let _ = (qos, relative);
}

fn renice(nice: i32) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        if tid > 0 {
            let _ = libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = nice;
}

struct Guard {
    #[cfg(target_vendor = "apple")]
    class: libc::qos_class_t,
    #[cfg(target_vendor = "apple")]
    relative: i32,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    nice: i32,
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    _private: (),
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(target_vendor = "apple")]
        unsafe {
            libc::pthread_set_qos_class_self_np(self.class, self.relative);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        renice(self.nice);
    }
}

fn push(qos: Qos) -> Option<Guard> {
    #[cfg(target_vendor = "apple")]
    {
        let (class, relative) = current_apple()?;
        set(qos);
        return Some(Guard { class, relative });
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let nice = current_nice()?;
        set(qos);
        return Some(Guard { nice });
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    {
        let _ = qos;
        None
    }
}

#[cfg(target_vendor = "apple")]
fn current_apple() -> Option<(libc::qos_class_t, i32)> {
    unsafe {
        let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
        let mut relative = 0;
        if libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative) != 0 {
            return None;
        }
        // Unspecified cannot be restored: the setter rejects it, and the
        // thread would stay at the class we just applied. The threads that
        // reach a save are the UI thread and the engine, which are interactive.
        if matches!(class, libc::qos_class_t::QOS_CLASS_UNSPECIFIED) {
            class = libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE;
            relative = 0;
        }
        Some((class, relative))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn errno_ptr() -> *mut i32 {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::__errno_location()
    }
    #[cfg(target_os = "android")]
    unsafe {
        libc::__errno()
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn current_nice() -> Option<i32> {
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        if tid <= 0 {
            return None;
        }
        *errno_ptr() = 0;
        let nice = libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t);
        if nice == -1 && *errno_ptr() != 0 { None } else { Some(nice) }
    }
}

/// Apple: the audio callback is a real-time thread. QoS alone still loses
/// to a user-interactive game thread; a time-constraint policy does not, and
/// an audio workgroup tells the scheduler this thread feeds the device.
#[cfg(target_vendor = "apple")]
mod realtime {
    use std::os::raw::{c_char, c_int, c_void};
    use std::sync::OnceLock;

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    #[repr(C)]
    struct TimeConstraint {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: u32,
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> c_int;
    }

    type Create = unsafe extern "C" fn(*const c_char, *mut c_void) -> *mut c_void;
    type Join = unsafe extern "C" fn(*mut c_void, *mut u64) -> c_int;

    pub fn promote() {
        unsafe {
            let mut info = Timebase { numer: 0, denom: 0 };
            if mach_timebase_info(&mut info) != 0 || info.numer == 0 || info.denom == 0 {
                return;
            }
            // 256 frames at 48 kHz. The callback only copies that quantum.
            let ticks = |ns: u64| -> u32 {
                ns.saturating_mul(info.denom as u64).saturating_div(info.numer as u64).min(u32::MAX as u64) as u32
            };
            let period = ticks(5_333_333);
            let mut policy =
                TimeConstraint { period, computation: ticks(1_000_000), constraint: period, preemptible: 1 };
            let _ = libc::thread_policy_set(
                libc::pthread_mach_thread_np(libc::pthread_self()),
                2 as libc::thread_policy_flavor_t,
                &mut policy as *mut TimeConstraint as libc::thread_policy_t,
                4 as libc::mach_msg_type_number_t,
            );
            join_workgroup();
        }
    }

    fn join_workgroup() {
        static GROUP: OnceLock<usize> = OnceLock::new();
        let group = *GROUP.get_or_init(|| unsafe {
            let name = c"os_workgroup_create".as_ptr();
            let create = libc::dlsym(libc::RTLD_DEFAULT, name);
            if create.is_null() {
                return 0;
            }
            let create: Create = std::mem::transmute(create);
            create(c"glue5.audio".as_ptr(), std::ptr::null_mut()) as usize
        });
        if group == 0 {
            return;
        }
        unsafe {
            let join = libc::dlsym(libc::RTLD_DEFAULT, c"os_workgroup_join".as_ptr());
            if join.is_null() {
                return;
            }
            let join: Join = std::mem::transmute(join);
            // The token has to outlive the join. 16 u64s is larger than the
            // 64-byte token the header publishes.
            thread_local! {
                static TOKEN: std::cell::UnsafeCell<[u64; 16]> = const { std::cell::UnsafeCell::new([0; 16]) };
            }
            TOKEN.with(|token| {
                let _ = join(group as *mut c_void, token.get().cast());
            });
        }
    }
}

/// A frame was shown `took` after the one before, for `target`.
pub fn frame(took: std::time::Duration, target: std::time::Duration) {
    #[cfg(target_os = "android")]
    hint::frame(took, target);
    #[cfg(not(target_os = "android"))]
    let _ = (took, target);
}

/// Android's performance hints (ADPF, Android 13+; looked up at run time).
/// Without them the system's defaults keep the big cores' clocks low for a
/// game that never fully loads one thread, as the engine's pipeline does.
#[cfg(target_os = "android")]
mod hint {
    use std::ffi::{CStr, c_void};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    struct Api {
        manager: *mut c_void,
        create: unsafe extern "C" fn(*mut c_void, *const i32, usize, i64) -> *mut c_void,
        close: unsafe extern "C" fn(*mut c_void),
        update_target: unsafe extern "C" fn(*mut c_void, i64) -> i32,
        report: unsafe extern "C" fn(*mut c_void, i64) -> i32,
        /// Android 14+; before, the session is made again with the new thread.
        set_threads: Option<unsafe extern "C" fn(*mut c_void, *const i32, usize) -> i32>,
    }
    // SAFETY: the manager is a process-wide singleton, usable from any thread.
    unsafe impl Send for Api {}
    unsafe impl Sync for Api {}

    struct Session {
        threads: Vec<i32>,
        session: *mut c_void,
        target: i64,
    }
    // SAFETY: the session is only used under the mutex.
    unsafe impl Send for Session {}

    static SESSION: Mutex<Session> =
        Mutex::new(Session { threads: Vec::new(), session: std::ptr::null_mut(), target: 0 });

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| unsafe {
            let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let sym = |name: &CStr| {
                let p = libc::dlsym(lib, name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            let get_manager: unsafe extern "C" fn() -> *mut c_void =
                std::mem::transmute(sym(c"APerformanceHint_getManager")?);
            let manager = get_manager();
            if manager.is_null() {
                return None;
            }
            Some(Api {
                manager,
                create: std::mem::transmute(sym(c"APerformanceHint_createSession")?),
                close: std::mem::transmute(sym(c"APerformanceHint_closeSession")?),
                update_target: std::mem::transmute(sym(c"APerformanceHint_updateTargetWorkDuration")?),
                report: std::mem::transmute(sym(c"APerformanceHint_reportActualWorkDuration")?),
                set_threads: sym(c"APerformanceHint_setThreads").map(|p| std::mem::transmute(p)),
            })
        })
        .as_ref()
    }

    /// Adds the calling thread to the session.
    pub fn join() {
        let Some(api) = api() else { return };
        let mut s = SESSION.lock().unwrap();
        s.threads.push(unsafe { libc::gettid() });
        if s.target == 0 {
            s.target = Duration::from_secs(1).as_nanos() as i64 / 60;
        }
        unsafe {
            if !s.session.is_null()
                && let Some(set_threads) = api.set_threads
            {
                set_threads(s.session, s.threads.as_ptr(), s.threads.len());
                return;
            }
            if !s.session.is_null() {
                (api.close)(s.session);
            }
            s.session = (api.create)(api.manager, s.threads.as_ptr(), s.threads.len(), s.target);
        }
        if s.session.is_null() {
            log::warn!("no performance hint session");
        }
    }

    pub fn frame(took: Duration, target: Duration) {
        let Some(api) = api() else { return };
        let mut s = SESSION.lock().unwrap();
        if s.session.is_null() {
            return;
        }
        let target = target.as_nanos() as i64;
        unsafe {
            if target > 0 && target != s.target {
                s.target = target;
                (api.update_target)(s.session, target);
            }
            (api.report)(s.session, took.as_nanos().max(1) as i64);
        }
    }
}
