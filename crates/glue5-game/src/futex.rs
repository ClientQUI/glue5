//! `memory.atomic.wait32/wait64/notify`, shared by the module and host threads.
//!
//! Waiters park on the host address of the watched word, so a host thread
//! (GPU, I/O, audio) waiting on a word in wasm memory and a wasm thread
//! notifying it (or the reverse) meet in the same queue.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot_core::{DEFAULT_PARK_TOKEN, DEFAULT_UNPARK_TOKEN, FilterOp, ParkResult};

use crate::Memory;

/// Result of a wait, with the values wasm uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Wait {
    Woken = 0,
    NotEqual = 1,
    TimedOut = 2,
}

/// Optional diagnostic observer. It must not change words or wake waiters.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    Begin { key: usize, bits: u8, expected: u64, observed: u64, timeout: Option<Duration> },
    End { key: usize, result: Wait },
    Notify { key: usize, requested: u32, woken: u32 },
}

static OBSERVER: OnceLock<fn(Event)> = OnceLock::new();

/// Install before starting engine threads. Unset observers cost one check.
pub fn observe(observer: fn(Event)) {
    let _ = OBSERVER.set(observer);
}

fn event(event: Event) {
    if let Some(observer) = OBSERVER.get() {
        observer(event);
    }
}

/// Waits while `*word == expected`, up to `timeout` (`None` = forever).
pub fn wait32(word: &AtomicU32, expected: u32, timeout: Option<Duration>) -> Wait {
    let key = word as *const _ as usize;
    if let Some(observer) = OBSERVER.get() {
        observer(Event::Begin {
            key,
            bits: 32,
            expected: expected as u64,
            observed: word.load(Ordering::SeqCst) as u64,
            timeout,
        });
    }
    let result = park(key, || word.load(Ordering::SeqCst) == expected, timeout);
    event(Event::End { key, result });
    result
}

/// Waits while `*word == expected`, up to `timeout` (`None` = forever).
pub fn wait64(word: &AtomicU64, expected: u64, timeout: Option<Duration>) -> Wait {
    let key = word as *const _ as usize;
    if let Some(observer) = OBSERVER.get() {
        observer(Event::Begin { key, bits: 64, expected, observed: word.load(Ordering::SeqCst), timeout });
    }
    let result = park(key, || word.load(Ordering::SeqCst) == expected, timeout);
    event(Event::End { key, result });
    result
}

/// Wakes up to `count` waiters on `word`; returns how many were woken.
pub fn notify<T>(word: &T, count: u32) -> u32 {
    let key = word as *const T as usize;
    let mut left = count;
    let result = unsafe {
        parking_lot_core::unpark_filter(
            key,
            |_| {
                if left == 0 {
                    FilterOp::Stop
                } else {
                    left -= 1;
                    FilterOp::Unpark
                }
            },
            |_| DEFAULT_UNPARK_TOKEN,
        )
    };
    let woken = result.unparked_threads as u32;
    event(Event::Notify { key, requested: count, woken });
    woken
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn changed_word_never_parks() {
        assert_eq!(wait32(&AtomicU32::new(2), 1, None), Wait::NotEqual);
        assert_eq!(wait64(&AtomicU64::new(2), 1, None), Wait::NotEqual);
    }

    #[test]
    fn unchanged_word_times_out() {
        assert_eq!(wait32(&AtomicU32::new(1), 1, Some(Duration::from_millis(2))), Wait::TimedOut);
        assert_eq!(wait64(&AtomicU64::new(1), 1, Some(Duration::from_millis(2))), Wait::TimedOut);
    }

    #[test]
    fn notify_wakes_a_parked_waiter() {
        let word = Arc::new(AtomicU32::new(1));
        let waiting = word.clone();
        let thread = std::thread::spawn(move || wait32(&waiting, 1, Some(Duration::from_secs(2))));
        let deadline = Instant::now() + Duration::from_secs(1);
        while notify(&*word, 1) == 0 {
            assert!(Instant::now() < deadline, "waiter never parked");
            std::thread::yield_now();
        }
        assert_eq!(thread.join().unwrap(), Wait::Woken);
    }
}

fn park(key: usize, validate: impl FnOnce() -> bool, timeout: Option<Duration>) -> Wait {
    let deadline = timeout.map(|t| Instant::now() + t);
    match unsafe { parking_lot_core::park(key, validate, || {}, |_, _| {}, DEFAULT_PARK_TOKEN, deadline) } {
        ParkResult::Unparked(_) => Wait::Woken,
        ParkResult::Invalid => Wait::NotEqual,
        ParkResult::TimedOut => Wait::TimedOut,
    }
}

fn timeout(ns: i64) -> Option<Duration> {
    (ns >= 0).then(|| Duration::from_nanos(ns as u64))
}

unsafe fn word<T>(memory: *mut Memory, addr: u64) -> &'static T {
    unsafe { &*((*memory).data.add(addr as usize) as *const T) }
}

// Called by the generated code (through glue5.c).
pub(crate) unsafe extern "C" fn wasm_wait32(memory: *mut Memory, addr: u64, expected: u32, ns: i64) -> u32 {
    wait32(unsafe { word(memory, addr) }, expected, timeout(ns)) as u32
}

pub(crate) unsafe extern "C" fn wasm_wait64(memory: *mut Memory, addr: u64, expected: u64, ns: i64) -> u32 {
    wait64(unsafe { word(memory, addr) }, expected, timeout(ns)) as u32
}

pub(crate) unsafe extern "C" fn wasm_notify(memory: *mut Memory, addr: u64, count: u32) -> u32 {
    notify::<AtomicU32>(unsafe { word(memory, addr) }, count)
}
