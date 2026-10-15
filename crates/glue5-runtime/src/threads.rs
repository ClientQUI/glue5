//! Engine threads: the runtime ("main") thread that runs `main`, and pthreads.
//!
//! Each runs its own wasm instance over the shared memory, bootstrapped the
//! way Emscripten's worker does it.

use std::collections::HashSet;
use std::ptr;
use std::sync::Mutex;
use std::thread::{self, JoinHandle};

use anyhow::Result;
use glue5_game::{Env, Trap, exports};

use crate::rt;

/// Native stacks: wasm2c code recurses on the native stack.
const MAIN_STACK: usize = 64 << 20;
const PTHREAD_STACK: usize = 16 << 20;

/// Emscripten's default pthread stack size (passed to `_emscripten_thread_init`).
const DEFAULT_PTHREAD_STACK: u32 = 1 << 20;

#[derive(Default)]
pub(crate) struct Registry {
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    /// pthreads whose native thread has finished.
    finished: HashSet<u64>,
    /// pthreads whose data (stack, TLS, struct) can be freed once finished.
    cleanup: Vec<u64>,
}

impl Registry {
    /// Frees the data of exited threads; `env` is the calling thread's instance.
    fn reap(&self, env: *mut Env) {
        let ready: Vec<u64> = {
            let mut inner = self.inner.lock().unwrap();
            let RegistryInner { finished, cleanup } = &mut *inner;
            let ready = cleanup.iter().copied().filter(|t| finished.contains(t)).collect::<Vec<_>>();
            cleanup.retain(|t| !finished.contains(t));
            for t in &ready {
                finished.remove(t);
            }
            ready
        };
        for thread in ready {
            if let Err(trap) = unsafe { exports::_emscripten_thread_free_data(env, thread) } {
                log::error!("freeing thread {thread:#x}: {trap}");
            }
        }
    }
}

/// A heap-allocated environment for the current thread's instance.
fn new_env() -> *mut Env {
    Box::into_raw(Box::new(Env::new(rt().memory.desc, ptr::null_mut())))
}

pub(crate) fn spawn_main() -> Result<JoinHandle<i32>> {
    Ok(thread::Builder::new().name("engine main".into()).stack_size(MAIN_STACK).spawn(run_main)?)
}

fn run_main() -> i32 {
    crate::qos::apply_engine_thread(Some("engine main"));
    crate::diagnostics::note(format_args!("engine main: starting"));
    let env = new_env();
    let result = (|| -> Result<u32, Trap> {
        unsafe {
            glue5_game::instantiate(env)?;
            // Calls back into `_emscripten_init_main_thread_js`, among others.
            exports::__wasm_call_ctors(env)?;
            let args = rt().config.engine_args();
            log::info!("engine: {}", args.join(" "));
            let (argc, argv) = write_argv(env, &args)?;
            exports::__main_argc_argv(env, argc, argv)
        }
    })();
    match result {
        Ok(code) => {
            log::info!("engine main returned {code}");
            crate::diagnostics::note(format_args!("engine main: returned {code}"));
            code as i32
        }
        Err(trap) if trap == glue5_game::UNWIND => {
            log::info!("engine main unwound with a live runtime");
            crate::diagnostics::note(format_args!("engine main: unwound with live runtime"));
            loop {
                thread::park();
            }
        }
        Err(trap) => {
            crate::exit::fail(format!("the engine's main thread stopped ({trap})"));
            1
        }
    }
}

/// `callMain`: argv[0] and the arguments go on the wasm stack.
unsafe fn write_argv(env: *mut Env, args: &[String]) -> Result<(u32, u64), Trap> {
    let mem = unsafe { crate::memory::Mem::of(env) };
    let all: Vec<&str> = std::iter::once("./this.program").chain(args.iter().map(String::as_str)).collect();
    let argv = unsafe { exports::_emscripten_stack_alloc(env, (all.len() as u64 + 1) * 8)? };
    for (i, arg) in all.iter().enumerate() {
        let ptr = unsafe { exports::_emscripten_stack_alloc(env, arg.len() as u64 + 1)? };
        mem.write_str(ptr, arg, arg.len() + 1);
        mem.set_u64(argv + i as u64 * 8, ptr);
    }
    mem.set_u64(argv + all.len() as u64 * 8, 0);
    Ok((all.len() as u32, argv))
}

/// `_emscripten_init_main_thread_js`.
pub(crate) unsafe fn init_main_thread(env: *mut Env, pthread: u64) {
    // The browser build runs main in a worker: not the main browser thread,
    // but the runtime thread, and allowed to block.
    let init = unsafe {
        exports::_emscripten_thread_init(env, pthread, 0, 1, 1, DEFAULT_PTHREAD_STACK, 0).and_then(|()| init_tls(env))
    };
    if let Err(trap) = init {
        panic!("main thread init: {trap}");
    }
}

/// `PThread.threadInitTLS()`: game.js registers `_emscripten_tls_init` as the
/// (only) TLS initialiser, so every thread gets its TLS block and relocations.
unsafe fn init_tls(env: *mut Env) -> Result<(), Trap> {
    unsafe { exports::_emscripten_tls_init(env) }.map(|_| ())
}

/// `__pthread_create_js`: returns 0 or an errno.
pub(crate) unsafe fn create(env: *mut Env, pthread: u64, start: u64, arg: u64) -> u32 {
    rt().threads.reap(env);
    let name = engine_name(rt().memory.mem(), arg);
    let thread_name = name.clone();
    let spawned = thread::Builder::new()
        .name(name.unwrap_or_else(|| format!("pthread {pthread:#x}")))
        .stack_size(PTHREAD_STACK)
        .spawn(move || run_pthread(pthread, start, arg, thread_name));
    match spawned {
        Ok(_) => 0,
        Err(e) => {
            log::error!("cannot spawn a thread: {e}");
            6 // EAGAIN
        }
    }
}

/// The engine's name for a thread it starts: RAGE's thread entry takes its
/// parameters, with the name inline at +24 (the names the engine's log
/// lists as `engine thread n: ...`). None for threads started otherwise.
fn engine_name(mem: crate::memory::Mem, arg: u64) -> Option<String> {
    if arg == 0 {
        return None;
    }
    let mut name = Vec::new();
    for i in 0..48 {
        match mem.u8(arg + 24 + i) {
            0 => break,
            b if b == b' ' || b.is_ascii_graphic() => name.push(b),
            _ => return None,
        }
    }
    (name.len() >= 3 && name.len() < 48).then(|| String::from_utf8_lossy(&name).into_owned())
}

fn run_pthread(pthread: u64, start: u64, arg: u64, name: Option<String>) {
    // Hybrid starts this thread at user-interactive (a load) and drops it
    // once play is steady, unless it is the mixer, which stays interactive.
    // A load that left streaming below the frame used to stretch a
    // post-cutscene load from 1-2 s to 6-10 s on an iPhone 13 Pro.
    // Streaming is user-interactive during that boost and utility after.
    // The class is read again every frame on the game and render threads.
    crate::qos::apply_engine_thread(name.as_deref());
    let env = new_env();
    let mem = rt().memory.mem();
    let result = (|| -> Result<(), Trap> {
        unsafe {
            glue5_game::instantiate(env)?;
            exports::_emscripten_thread_init(env, pthread, 0, 0, 1, 0, 0)?;
            // establishStackSpace
            let high = mem.u64(pthread + 80);
            let size = mem.u64(pthread + 88);
            exports::emscripten_stack_set_limits(env, high, high - size)?;
            exports::_emscripten_stack_restore(env, high)?;
            init_tls(env)?;
            // invokeEntryPoint
            let value = glue5_game::call_ptr_ptr(env, start, arg)?;
            exports::_emscripten_thread_exit(env, value)
        }
    })();
    match result {
        Ok(()) => {}
        Err(trap) if trap == glue5_game::UNWIND => {
            // emscripten_exit_with_live_runtime: the thread stays around.
            loop {
                thread::park();
            }
        }
        Err(trap) => {
            log::error!("thread {pthread:#x}: {trap}");
            crate::exit::fail(format!("an engine thread stopped ({trap})"));
            let _ = unsafe { exports::_emscripten_thread_crashed(env) };
        }
    }
    unsafe {
        glue5_game::free_instance(env);
        drop(Box::from_raw(env));
    }
    rt().threads.inner.lock().unwrap().finished.insert(pthread);
}

/// `_emscripten_thread_cleanup`: frees the thread's data once it has finished.
pub(crate) unsafe fn cleanup(env: *mut Env, pthread: u64) {
    rt().threads.inner.lock().unwrap().cleanup.push(pthread);
    rt().threads.reap(env);
}
