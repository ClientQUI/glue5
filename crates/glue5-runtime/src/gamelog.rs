//! The engine's own output: its log (tens of thousands of lines while loading)
//! and whatever it prints to stdout/stderr.
//!
//! Engine threads only queue it; a background thread writes it to
//! `<user dir>/game.log`, and to the terminal too with `GLUE5_ENGINE_LOG=stdout`
//! (on Android, where stdout goes nowhere, to the log).
//! Printing synchronously from engine threads stalled the engine behind a
//! slow terminal: loading looked stuck.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use glue5_game::{Env, orig};

/// Queued writes before new ones are dropped (the writer only falls this far
/// behind on a terminal that cannot keep up).
const QUEUE: usize = 1 << 16;
/// Keep continuous engine output recoverable too: a busy queue otherwise
/// never reaches its receive timeout, so a buffered log could lose its most
/// useful tail when iOS terminates the app.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const SYNC_INTERVAL: Duration = Duration::from_secs(15);

static SINK: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
static VERBOSE: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

fn retain(bytes: &[u8]) -> bool {
    if VERBOSE.load(Ordering::Relaxed) {
        return true;
    }
    let has = |s: &[u8]| bytes.windows(s.len()).any(|w| w == s);
    // This warning appears for many cloth buffers every frame. It is a
    // known backend warning, not a fatal failure; verbose mode retains it.
    if has(b"Fatal Error:") || has(b"Error:") {
        return true;
    }
    if has(b"GPU version of buffer not updated") {
        return false;
    }
    has(b"Fatal Error:") || has(b"Error:") || has(b"Warning:") || has(b"[hang]") || has(b"[wasm] mem:")
}

/// Starts the writer; returns the log file's path.
pub(crate) fn start(user_dir: &Path) -> io::Result<PathBuf> {
    let mut paths = crate::diagnostics::engine_log_paths()?.unwrap_or_else(|| vec![user_dir.join("game.log")]);
    // Append, rather than replace: after a force quit, the next launch must
    // not erase the tail that explains why the last one stopped.
    let mut files = Vec::with_capacity(paths.len());
    for path in &paths {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "\n=== Glue 5 engine session pid={} build={} ===", std::process::id(), crate::GAME_BUILD)?;
        let _ = file.sync_data();
        files.push(file);
    }
    let to_stdout = std::env::var("GLUE5_ENGINE_LOG").is_ok_and(|v| v == "stdout");
    let (tx, rx) = sync_channel(QUEUE);
    if SINK.set(tx).is_ok() {
        thread::Builder::new().name("engine log".into()).spawn(move || run(rx, files, to_stdout))?;
    }
    Ok(paths.remove(0))
}

/// Queues engine output. Never blocks.
pub(crate) fn write(bytes: &[u8]) {
    if !retain(bytes) {
        return;
    }
    enqueue(bytes.to_vec());
}

fn enqueue(bytes: Vec<u8>) {
    // A full debug queue must never hide a fatal error from the application.
    if let Some(why) = fatal_error(&bytes) {
        crate::exit::fail(why);
    }
    let Some(sink) = SINK.get() else {
        let _ = io::stdout().write_all(&bytes);
        return;
    };
    if let Err(TrySendError::Full(_)) = sink.try_send(bytes) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Queues one line of engine output.
pub(crate) fn line(text: &str) {
    if !retain(text.as_bytes()) {
        return;
    }
    let mut bytes = Vec::with_capacity(text.len() + 1);
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(b'\n');
    enqueue(bytes);
}

/// A fatal error the engine reports ("Fatal Error: ..."): it then waits for
/// an OK that never comes ("Press Ok to abort the program"), the screen
/// frozen, so the game has stopped.
fn fatal_error(bytes: &[u8]) -> Option<String> {
    const MARK: &[u8] = b"Fatal Error: ";
    let at = bytes.windows(MARK.len()).position(|w| w == MARK)? + MARK.len();
    let rest = &bytes[at..];
    let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    Some(format!("the engine failed: {}", String::from_utf8_lossy(&rest[..end]).trim()))
}

fn run(rx: Receiver<Vec<u8>>, files: Vec<File>, to_stdout: bool) {
    crate::qos::set(crate::qos::Qos::Utility);
    let mut files: Vec<_> = files.into_iter().map(|file| BufWriter::with_capacity(1 << 20, file)).collect();
    let mut last_flush = Instant::now();
    let mut last_sync = Instant::now();
    #[cfg(not(target_os = "android"))]
    let mut stdout = to_stdout.then(io::stdout);
    #[cfg(target_os = "android")]
    let mut stdout = to_stdout.then(Logcat::default);
    let mut repeats = Repeats::default();
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(bytes) => {
                let fatal = fatal_error(&bytes);
                let bytes = match repeats.check(&bytes) {
                    Repeat::Write => bytes,
                    Repeat::Count(n) => format!("[glue5] {n} times so far: {}", String::from_utf8_lossy(&bytes)).into(),
                    Repeat::Skip => {
                        persist_if_due(&mut files, &mut last_flush, &mut last_sync);
                        continue;
                    }
                };
                for file in &mut files {
                    let _ = file.write_all(&bytes);
                }
                if let Some(out) = &mut stdout {
                    let _ = out.write_all(&bytes);
                }
                if let Some(why) = fatal {
                    crate::diagnostics::fault(format_args!("engine fatal error: {why}"));
                    crate::exit::fail(why);
                    for file in &mut files {
                        let _ = file.flush();
                        let _ = file.get_ref().sync_data();
                    }
                    last_flush = Instant::now();
                    last_sync = last_flush;
                } else {
                    persist_if_due(&mut files, &mut last_flush, &mut last_sync);
                }
            }
            // Idle: make the file current (for `tail -f`).
            Err(RecvTimeoutError::Timeout) => {
                let dropped = DROPPED.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    for file in &mut files {
                        let _ = writeln!(
                            file,
                            "[glue5] {dropped} engine output writes dropped (the log could not keep up)"
                        );
                    }
                }
                persist_if_due(&mut files, &mut last_flush, &mut last_sync);
                if let Some(out) = &mut stdout {
                    let _ = out.flush();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    for file in &mut files {
        let _ = file.flush();
        let _ = file.get_ref().sync_data();
    }
}

fn persist_if_due(files: &mut [BufWriter<File>], last_flush: &mut Instant, last_sync: &mut Instant) {
    let now = Instant::now();
    if now.duration_since(*last_flush) >= FLUSH_INTERVAL {
        for file in &mut *files {
            let _ = file.flush();
        }
        *last_flush = now;
    }
    if now.duration_since(*last_sync) >= SYNC_INTERVAL {
        // Sync only after flushing: `sync_data` does not persist bytes still
        // waiting in BufWriter's userspace buffer.
        for file in &mut *files {
            let _ = file.flush();
            let _ = file.get_ref().sync_data();
        }
        *last_flush = now;
        *last_sync = now;
    }
}

/// Lines the engine prints over and over (online, some messages come every
/// frame: millions of lines, a log of hundreds of MB) are written the first
/// [`Repeats::KEEP`] times, then only counted, the count written at each
/// power of two.
#[derive(Default)]
struct Repeats(HashMap<u64, u64>);

enum Repeat {
    Write,
    Count(u64),
    Skip,
}

impl Repeats {
    const KEEP: u64 = 10;
    /// Distinct lines remembered (lines with addresses or times in them are
    /// all distinct): past this the counts start over.
    const LINES: usize = 50_000;

    fn check(&mut self, bytes: &[u8]) -> Repeat {
        if self.0.len() >= Self::LINES {
            self.0.clear();
        }
        let n = self.0.entry(hash(bytes)).or_default();
        *n += 1;
        match *n {
            n if n <= Self::KEEP => Repeat::Write,
            n if n.is_power_of_two() => Repeat::Count(n),
            _ => Repeat::Skip,
        }
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Engine output to the log, line by line.
#[cfg(target_os = "android")]
#[derive(Default)]
struct Logcat(Vec<u8>);

#[cfg(target_os = "android")]
impl Write for Logcat {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(bytes);
        while let Some(end) = self.0.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.0.drain(..=end).collect();
            log::info!(target: "engine", "{}", String::from_utf8_lossy(&line).trim_end());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Hook for `rage::netLogImpl::Flush(bool)`. The network's logs go to their
/// files from a spool thread, one of the network's threads, which the
/// engine skips out of GTA Online (imports.rs, `keepThreads`). A flush that
/// waits for it (`wait`) waited forever once a log had queued a buffer, and
/// the session's shutdown flushes so: loading a save or starting a new game
/// from the pause menu hung there. Without the thread nothing waits.
pub(crate) unsafe extern "C" fn net_log_flush(env: *mut Env, log: u64, _wait: u32) {
    unsafe { orig::net_log_flush(env, log, 0) }
}

/// This module's hooks.
pub(crate) fn hooks(hooks: &mut glue5_game::Hooks) {
    if crate::rt().config.online.is_none() {
        hooks.net_log_flush = Some(net_log_flush);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_engine_s_fatal_errors() {
        let line = b"Fatal Error: <HDD Reader> Short read on /game/x.rpf - got -1\n";
        assert_eq!(
            fatal_error(line).as_deref(),
            Some("the engine failed: <HDD Reader> Short read on /game/x.rpf - got -1")
        );
        assert_eq!(fatal_error(b"Error: something else\n"), None);
    }

    #[test]
    fn repeated_lines_are_counted() {
        let mut repeats = Repeats::default();
        let written: Vec<u64> = (1..=40)
            .filter_map(|n| match repeats.check(b"Error: again\n") {
                Repeat::Write => Some(n),
                Repeat::Count(c) => Some(1000 + c),
                Repeat::Skip => None,
            })
            .collect();
        assert_eq!(written, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 1016, 1032]);
        assert!(matches!(repeats.check(b"Error: other\n"), Repeat::Write));
    }

    #[test]
    fn compact_logs_keep_failures_but_remove_frame_spam() {
        assert!(retain(b"[wasm] mem: malloc arena 3720 MB (in use 3678 MB)"));
        assert!(retain(b"Fatal Error: broken allocation"));
        assert!(retain(b"Fatal Error: GPU version of buffer not updated"));
        assert!(retain(b"Error: failed to read"));
        assert!(retain(b"Warning: missing asset"));
        assert!(!retain(b"Warning: [Graphics] GPU version of buffer not updated!"));
        assert!(!retain(b"[Audio] LoadBank request"));
        assert!(!retain(b"[modelinfo] car suspension bias"));
    }
}
