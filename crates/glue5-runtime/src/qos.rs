//! Thread priorities. On Apple platforms they are quality-of-service classes.
//! The system chooses the cores: user-interactive prefers performance cores,
//! utility and background prefer efficiency cores. This process does not pin
//! cores and does not use the Neural Engine.
//!
//! The main, game, render and GPU threads stay user-interactive. With
//! efficiency placement on, the mixer and asset streaming run at utility
//! (the mixer at the top of that class) and a thread that only saves runs
//! at background. The audio device callback stays user-interactive and plays
//! a buffer the pump already filled. On Android only user-interactive threads
//! join the performance hint session ([`frame`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::sched::{self, Class, Role};

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

static EFFICIENCY: AtomicBool = AtomicBool::new(true);
static KILL: OnceLock<bool> = OnceLock::new();

/// `GLUE5_EFFICIENCY_CORES=0` forces placement off. Anything else follows
/// [`set_efficiency_cores`], which the graphics menu updates.
pub fn set_efficiency_cores(on: bool) {
    EFFICIENCY.store(on, Ordering::Relaxed);
}

pub fn efficiency_cores() -> bool {
    if *KILL.get_or_init(|| std::env::var("GLUE5_EFFICIENCY_CORES").ok().as_deref() == Some("0")) {
        return false;
    }
    EFFICIENCY.load(Ordering::Relaxed)
}

/// The device callback. It only copies a filled buffer, and it is the
/// deadline, so it stays user-interactive even when the mixer does not.
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
    });
}

/// The playback pump and the game's mixer. Utility (highest priority inside
/// that class) when efficiency placement is on, so they prefer efficiency
/// cores without being deferred as background. The 100 ms cushion covers a
/// late wake. When placement is off they stay interactive, which is the
/// previous behaviour: a frame is not scheduled ahead of the mix.
pub fn set_audio() {
    if efficiency_cores() {
        // Top of the utility class. Streaming uses a lower relative priority
        // and a higher nice, so a decompress does not sit ahead of the mix.
        // The device callback is nicer still (user-interactive, nice -10).
        set_rel(Qos::Utility, 0);
        renice(1);
    } else {
        set_rel(Qos::Interactive, 0);
        renice(-10);
    }
}

/// Class for an engine pthread, from its name. Threads already running keep
/// the class they were given here until the process starts them again.
pub fn apply_engine_thread(name: Option<&str>) {
    let role = name.map(sched::role).unwrap_or(Role::Worker);
    if role == Role::Audio {
        set_audio();
        return;
    }
    match sched::class_for(role, efficiency_cores()) {
        Class::Interactive => set(Qos::Interactive),
        Class::Initiated => set(Qos::Initiated),
        Class::Utility => set(Qos::Utility),
        Class::Background => set(Qos::Background),
    }
}

/// Runs `body` at `qos`, then puts the previous class back. No change when
/// efficiency placement is off, or when the current class cannot be read
/// (the calling thread must not be left in the lower class).
pub fn lower<R>(qos: Qos, body: impl FnOnce() -> R) -> R {
    let _guard = if efficiency_cores() { push(qos) } else { None };
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
    if !matches!(qos, Qos::Interactive) {
        renice(match qos {
            Qos::Background => 10,
            Qos::Utility => 5,
            _ => 0,
        });
    }
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
    use std::ffi::{c_void, CStr};
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
