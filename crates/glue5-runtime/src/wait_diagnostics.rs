//! Passive wait/notify breadcrumbs. No diagnostic callback changes a word,
//! timeout or wake result. Each thread owns its record; the watchdog snapshots
//! it with try_lock and never holds a record lock while writing to disk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use glue5_game::futex::{Event, Wait};

const NOTIFY_KEYS_PER_THREAD: usize = 128;
static RECORDS: Mutex<Vec<Arc<Mutex<Record>>>> = Mutex::new(Vec::new());

/// Full wait tracing is expensive on frequent scheduler wakes. The frame
/// watchdog stays enabled independently; stack tracing is opt-in.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GLUE5_WAIT_DIAGNOSTICS").as_deref() == Ok("1"))
}

fn now() -> Duration {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

#[derive(Clone)]
struct Waiting {
    key: usize,
    bits: u8,
    expected: u64,
    observed: u64,
    timeout: Option<Duration>,
    since: Duration,
    active: bool,
    result: Option<Wait>,
}

#[derive(Clone)]
struct Stack {
    key: usize,
    captured: Duration,
    frames: Vec<String>,
}

#[derive(Clone)]
struct Notification {
    at: Duration,
    requested: u32,
    woken: u32,
    calls: u64,
}

#[derive(Clone)]
struct Record {
    name: String,
    waiting: Option<Waiting>,
    stack: Option<Stack>,
    clock_stack: Option<(Duration, Vec<String>)>,
    waits: u64,
    not_equal: u64,
    timeouts: u64,
    wakes: u64,
    notifications: HashMap<usize, Notification>,
}

impl Record {
    fn new(name: String) -> Self {
        Self {
            name,
            waiting: None,
            stack: None,
            clock_stack: None,
            waits: 0,
            not_equal: 0,
            timeouts: 0,
            wakes: 0,
            notifications: HashMap::new(),
        }
    }

    fn notify(&mut self, key: usize, at: Duration, requested: u32, woken: u32) {
        if !self.notifications.contains_key(&key) && self.notifications.len() >= NOTIFY_KEYS_PER_THREAD {
            if let Some(oldest) = self.notifications.iter().min_by_key(|(_, n)| n.at).map(|(&key, _)| key) {
                self.notifications.remove(&oldest);
            }
        }
        let n = self.notifications.entry(key).or_insert(Notification { at, requested, woken, calls: 0 });
        n.at = at;
        n.requested = requested;
        n.woken = woken;
        n.calls += 1;
    }
}

thread_local! {
    static LOCAL: Arc<Mutex<Record>> = {
        let thread = std::thread::current();
        let name = format!("{} {:?}", thread.name().unwrap_or("unnamed"), thread.id());
        let record = Arc::new(Mutex::new(Record::new(name)));
        RECORDS.lock().unwrap_or_else(|p| p.into_inner()).push(record.clone());
        record
    };
}

pub(crate) fn record(event: Event) {
    LOCAL.with(|local| {
        let mut record = local.lock().unwrap_or_else(|p| p.into_inner());
        let at = now();
        match event {
            Event::Begin { key, bits, expected, observed, timeout } => {
                record.waits += 1;
                record.waiting =
                    Some(Waiting { key, bits, expected, observed, timeout, since: at, active: true, result: None });
                // Infinite scheduler waits are often woken thousands of times
                // a second too. All waits must obey the sampling interval.
                let interval = if record.name.contains("engine main") || record.name.contains("RenderThread") {
                    Duration::from_millis(250)
                } else {
                    Duration::from_secs(1)
                };
                if observed == expected
                    && record.stack.as_ref().is_none_or(|s| at.saturating_sub(s.captured) >= interval)
                {
                    record.stack = Some(Stack { key, captured: at, frames: crate::trap_site::frames() });
                }
            }
            Event::End { key, result } => {
                if let Some(wait) = &mut record.waiting {
                    if wait.key == key {
                        wait.active = false;
                        wait.result = Some(result);
                    }
                }
                match result {
                    Wait::Woken => record.wakes += 1,
                    Wait::NotEqual => record.not_equal += 1,
                    Wait::TimedOut => record.timeouts += 1,
                }
            }
            Event::Notify { key, requested, woken } => record.notify(key, at, requested, woken),
        }
    });
}

/// The clock import can still be called in a spin/sleep loop that never
/// parks. Sample its caller too; rate-limit before acquiring any record lock.
pub(crate) fn clock(now_ms: u64) {
    if !enabled() {
        return;
    }
    thread_local! { static LAST: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
    LAST.with(|last| {
        if now_ms.saturating_sub(last.get()) < 1_000 {
            return;
        }
        last.set(now_ms);
        let stack = (now(), crate::trap_site::frames());
        LOCAL.with(|local| local.lock().unwrap_or_else(|p| p.into_inner()).clock_stack = Some(stack));
    });
}

fn address(key: usize) -> String {
    if let Some(runtime) = crate::RUNTIME.get() {
        let base = runtime.memory.mem().ptr(0) as usize;
        if let Some(offset) = key.checked_sub(base) {
            if (offset as u64) < runtime.memory.size() {
                return format!("wasm+{offset:#x}");
            }
        }
    }
    format!("host+{key:#x}")
}

pub(crate) fn dump() {
    if !enabled() {
        return;
    }
    let records = RECORDS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let mut skipped = 0;
    let snapshots: Vec<Record> = records
        .iter()
        .filter_map(|r| match r.try_lock() {
            Ok(r) => Some(r.clone()),
            Err(_) => {
                skipped += 1;
                None
            }
        })
        .collect();
    let at = now();
    crate::diagnostics::fault(format_args!(
        "wait snapshot: threads={} busy_records={skipped}; notify history bounded to {NOTIFY_KEYS_PER_THREAD} keys/thread",
        snapshots.len()
    ));
    for r in &snapshots {
        if let Some((captured, frames)) = &r.clock_stack {
            crate::diagnostics::fault(format_args!(
                "engine clock stack sample: thread={} age={}ms native_frames={}",
                r.name,
                at.saturating_sub(*captured).as_millis(),
                frames.join(" ")
            ));
        }
        let Some(w) = &r.waiting else { continue };
        let addr = address(w.key);
        let timeout = w.timeout.map_or_else(|| "infinite".to_owned(), |t| format!("{}ns", t.as_nanos()));
        crate::diagnostics::fault(format_args!(
            "thread wait: thread={} active={} address={addr} bits={} expected={:#x} observed_before_wait={:#x} timeout={timeout} since={}ms result={:?} totals[waits={} woken={} not_equal={} timed_out={}]",
            r.name,
            w.active,
            w.bits,
            w.expected,
            w.observed,
            at.saturating_sub(w.since).as_millis(),
            w.result,
            r.waits,
            r.wakes,
            r.not_equal,
            r.timeouts
        ));
        if let Some(stack) = &r.stack {
            crate::diagnostics::fault(format_args!(
                "wait stack sample: thread={} address={} age={}ms native_frames={}",
                r.name,
                address(stack.key),
                at.saturating_sub(stack.captured).as_millis(),
                stack.frames.join(" ")
            ));
        }
        for notifier in &snapshots {
            if let Some(n) = notifier.notifications.get(&w.key) {
                crate::diagnostics::fault(format_args!(
                    "wait notify: address={addr} notifier={} age={}ms requested={} woken={} retained_calls={}",
                    notifier.name,
                    at.saturating_sub(n.at).as_millis(),
                    n.requested,
                    n.woken,
                    n.calls
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_infinite_waits_do_not_capture_every_entry() {
        std::thread::spawn(|| {
            let begin = Event::Begin { key: 1, bits: 32, expected: 0, observed: 0, timeout: None };
            record(begin);
            let captured = LOCAL.with(|l| l.lock().unwrap().stack.as_ref().unwrap().captured);
            for _ in 0..100 {
                record(begin);
            }
            LOCAL.with(|l| {
                let r = l.lock().unwrap();
                assert_eq!(r.stack.as_ref().unwrap().captured, captured);
                assert_eq!(r.waits, 101);
            });
        })
        .join()
        .unwrap();
    }

    #[test]
    fn real_waits_and_notifications_reach_the_thread_record() {
        glue5_game::futex::observe(record);
        std::thread::spawn(|| {
            let word = std::sync::atomic::AtomicU64::new(1);
            let key = &word as *const _ as usize;
            assert_eq!(glue5_game::futex::wait64(&word, 2, None), Wait::NotEqual);
            assert_eq!(glue5_game::futex::wait64(&word, 1, Some(Duration::ZERO)), Wait::TimedOut);
            assert_eq!(glue5_game::futex::notify(&word, 1), 0);
            LOCAL.with(|local| {
                let r = local.lock().unwrap();
                let w = r.waiting.as_ref().unwrap();
                assert_eq!(w.key, key);
                assert_eq!(w.bits, 64);
                assert_eq!(w.expected, 1);
                assert!(!w.active);
                assert_eq!(w.result, Some(Wait::TimedOut));
                assert_eq!((r.waits, r.not_equal, r.timeouts), (2, 1, 1));
                assert_eq!(r.notifications[&key].woken, 0);
                assert!(r.stack.is_some());
            });
        })
        .join()
        .unwrap();
    }

    #[test]
    fn notification_history_keeps_latest_result_and_stays_bounded() {
        let mut r = Record::new("test".into());
        r.notify(1, Duration::ZERO, 1, 0);
        r.notify(1, Duration::from_millis(1), 2, 1);
        assert_eq!(r.notifications[&1].calls, 2);
        assert_eq!(r.notifications[&1].woken, 1);
        for key in 2..=NOTIFY_KEYS_PER_THREAD + 1 {
            r.notify(key, Duration::from_millis(key as u64), 1, 0);
        }
        assert_eq!(r.notifications.len(), NOTIFY_KEYS_PER_THREAD);
        assert!(!r.notifications.contains_key(&1));
        assert!(r.notifications.contains_key(&(NOTIFY_KEYS_PER_THREAD + 1)));
    }
}
