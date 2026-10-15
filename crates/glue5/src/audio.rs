//! The engine's mixer output to the default playback device (SDL).
//!
//! The device callback used to read the game's ring directly. That ring holds
//! about 64 ms, and a late callback turned the gap into silence. Playback now
//! has its own thread, at interactive priority. Every few milliseconds it
//! pulls whatever the mixer has written into a local buffer and keeps the
//! mixer's heartbeat alive, so a stalled frame or a background present does
//! not look like a device that went away. The callback only copies that
//! buffer. It starts once 100 ms is buffered, and waits for that again after
//! a real underrun instead of playing the gaps.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use glue5_runtime::audio::{AudioOut, CHANNELS, SAMPLE_RATE};
use glue5_runtime::qos;
use sdl3::audio::{AudioCallback, AudioFormat, AudioSpec, AudioStream, AudioStreamWithCallback};

/// Room if the mixer gets ahead of the device. Playback does not start until
/// [`PRIME_FRAMES`] are in, so a hitch shorter than that cushion does not
/// reach the speakers. Capacity is the most a burst can add on top.
const CAP_FRAMES: usize = 16384; // power of two, 341 ms at 48 kHz
/// Play only after this much is buffered, and again after an underrun.
const PRIME_FRAMES: usize = 48_000 * 100 / 1000;
/// The pump's period. Shorter than the device quantum, so the buffer refills
/// between callbacks.
const PUMP_PERIOD: Duration = Duration::from_millis(4);
const PULL_FRAMES: usize = 256;

/// Playing while alive.
pub struct Audio {
    stream: AudioStreamWithCallback<Pull>,
    stop: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

impl Drop for Audio {
    fn drop(&mut self) {
        // sdl3 frees the callback's state before it destroys the stream, and
        // SDL's audio thread can call it in between: at exit, a memset into
        // the freed buffer. Pausing takes the device's lock, so it waits for
        // a callback in progress, and no other comes. The pump is stopped
        // after that, so it is not writing the buffer the callback reads.
        let _ = self.stream.pause();
        self.stop.store(true, Ordering::Release);
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

pub fn start(subsystem: &sdl3::AudioSubsystem, out: AudioOut) -> Result<Audio> {
    let spec = AudioSpec {
        freq: Some(SAMPLE_RATE as i32),
        channels: Some(CHANNELS as i32),
        format: Some(AudioFormat::f32_sys()),
    };
    // A small device period. The cushion lives in [`Playback`], not in SDL's
    // own queue, so a callback stays a few milliseconds and never blocks.
    sdl3::hint::set("SDL_AUDIO_DEVICE_SAMPLE_FRAMES", "256");
    let shared = Arc::new(Shared::new());
    let stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let (shared, stop, out) = (Arc::clone(&shared), Arc::clone(&stop), out);
        thread::Builder::new()
            .name("audio pump".into())
            .spawn(move || pump(out, shared, stop))
            .map_err(|e| anyhow!("audio pump: {e}"))?
    };
    let pull = Pull { shared, buf: Vec::new(), frames: 0, mixed: 0, calls: 0, peak: 0.0, last_log: Instant::now() };
    let stream = subsystem.open_playback_stream(&spec, pull).map_err(|e| anyhow!("SDL audio: {e}"))?;
    stream.resume().map_err(|e| anyhow!("SDL audio: {e}"))?;
    log::info!(
        "audio: {SAMPLE_RATE} Hz stereo, {} ms ahead of the device, own thread ({})",
        PRIME_FRAMES * 1000 / SAMPLE_RATE as usize,
        subsystem.current_audio_driver()
    );
    Ok(Audio { stream, stop, pump: Some(pump) })
}

fn pump(out: AudioOut, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    qos::set_audio();
    let mut scratch = vec![0f32; PULL_FRAMES * CHANNELS];
    while !stop.load(Ordering::Acquire) {
        let free = shared.playback.free();
        if free == 0 {
            // The device is behind. Leave the game's 64 ms where it is and
            // only prove the consumer is still running.
            out.heartbeat();
        } else {
            let frames = free.min(PULL_FRAMES);
            let samples = frames * CHANNELS;
            let mixed = out.read(&mut scratch[..samples]);
            if mixed > 0 {
                shared.playback.write(&scratch[..mixed * CHANNELS]);
            }
            if shared.playback.filled() >= PRIME_FRAMES {
                shared.live.store(true, Ordering::Release);
            }
        }
        thread::sleep(PUMP_PERIOD);
    }
}

/// One producer (the pump) and one consumer (the device callback).
struct Playback {
    cap: usize,
    mask: usize,
    samples: UnsafeCell<Box<[f32]>>,
    write: AtomicUsize,
    read: AtomicUsize,
}

// The two sides never touch the same slot: `write` is published after the
// samples are stored, and `read` after they are copied out.
unsafe impl Sync for Playback {}

impl Playback {
    fn new(cap: usize) -> Self {
        assert!(cap.is_power_of_two() && cap > 1);
        Self {
            cap,
            mask: cap - 1,
            samples: UnsafeCell::new(vec![0f32; cap * CHANNELS].into_boxed_slice()),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }

    fn filled(&self) -> usize {
        self.write.load(Ordering::Acquire).wrapping_sub(self.read.load(Ordering::Acquire))
    }

    fn free(&self) -> usize {
        self.cap - self.filled()
    }

    /// Copies interleaved frames. Returns the frames stored.
    fn write(&self, frames: &[f32]) -> usize {
        let write = self.write.load(Ordering::Relaxed);
        let n = (frames.len() / CHANNELS).min(self.free());
        if n == 0 {
            return 0;
        }
        // SAFETY: the callback does not read slots past `read`, and this write
        // stays inside the free range published by `filled`.
        let samples = unsafe { &mut *self.samples.get() };
        let start = (write & self.mask) * CHANNELS;
        let first = n.min(self.cap - (write & self.mask));
        samples[start..start + first * CHANNELS].copy_from_slice(&frames[..first * CHANNELS]);
        if first < n {
            samples[..(n - first) * CHANNELS].copy_from_slice(&frames[first * CHANNELS..n * CHANNELS]);
        }
        self.write.store(write.wrapping_add(n), Ordering::Release);
        n
    }

    /// Fills `out` from the buffer and silences whatever is still missing.
    /// Returns the frames that were buffered.
    fn read(&self, out: &mut [f32]) -> usize {
        let read = self.read.load(Ordering::Relaxed);
        let n = (out.len() / CHANNELS).min(self.filled());
        if n > 0 {
            // SAFETY: the pump does not write slots before `write`, which this
            // read stays behind.
            let samples = unsafe { &*self.samples.get() };
            let start = (read & self.mask) * CHANNELS;
            let first = n.min(self.cap - (read & self.mask));
            out[..first * CHANNELS].copy_from_slice(&samples[start..start + first * CHANNELS]);
            if first < n {
                out[first * CHANNELS..n * CHANNELS].copy_from_slice(&samples[..(n - first) * CHANNELS]);
            }
            self.read.store(read.wrapping_add(n), Ordering::Release);
        }
        out[n * CHANNELS..].fill(0.0);
        n
    }
}

struct Shared {
    playback: Playback,
    /// Enough is buffered to start, or to resume after a gap.
    live: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self { playback: Playback::new(CAP_FRAMES), live: AtomicBool::new(false) }
    }
}

/// SDL's audio thread. It only copies the pump's buffer.
struct Pull {
    shared: Arc<Shared>,
    buf: Vec<f32>,
    frames: u64,
    mixed: u64,
    calls: u64,
    peak: f32,
    last_log: Instant,
}

impl AudioCallback<f32> for Pull {
    fn callback(&mut self, stream: &mut AudioStream, requested: i32) {
        let samples = (requested.max(0) as usize).next_multiple_of(CHANNELS);
        self.buf.resize(samples, 0.0);
        let frames = samples / CHANNELS;
        // Hold the start (and the moment after a gap) until the cushion is
        // back. Playing the trickle as it arrives is the click.
        let got = if self.shared.live.load(Ordering::Acquire) {
            let got = self.shared.playback.read(&mut self.buf);
            if got == 0 {
                self.shared.live.store(false, Ordering::Release);
            }
            got
        } else {
            self.buf.fill(0.0);
            0
        };
        self.mixed += got as u64;
        self.frames += frames as u64;
        self.calls += 1;
        let _ = stream.put_data_f32(&self.buf);

        if log::log_enabled!(log::Level::Debug) {
            self.peak = self.buf.iter().fold(self.peak, |p, s| p.max(s.abs()));
            if self.last_log.elapsed() >= Duration::from_secs(10) {
                self.last_log = Instant::now();
                let mixed = std::mem::take(&mut self.mixed);
                let played = std::mem::take(&mut self.frames);
                let calls = std::mem::take(&mut self.calls);
                log::debug!(
                    "audio: {} frames played ({} per request), {} silent (underrun or still priming), peak {:.3}",
                    played,
                    played / calls.max(1),
                    played.saturating_sub(mixed),
                    std::mem::take(&mut self.peak)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_wraps_and_silences_what_it_lacks() {
        let playback = Playback::new(8);
        assert_eq!(playback.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 3);
        let mut out = [0f32; 4 * CHANNELS];
        assert_eq!(playback.read(&mut out), 3);
        assert_eq!(&out[..6], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(&out[6..], &[0.0, 0.0]);

        // Six more frames cross the end of the eight-frame ring.
        let more: Vec<f32> = (0..6 * CHANNELS).map(|i| i as f32).collect();
        assert_eq!(playback.write(&more), 6);
        assert_eq!(playback.free(), 2);
        assert_eq!(playback.write(&[9.0, 9.0, 9.0, 9.0]), 2);
        assert_eq!(playback.write(&[8.0, 8.0]), 0, "full");
        let mut all = [0f32; 8 * CHANNELS];
        assert_eq!(playback.read(&mut all), 8);
        assert_eq!(&all[..more.len()], more.as_slice());
        assert_eq!(&all[more.len()..], &[9.0, 9.0, 9.0, 9.0]);
        assert_eq!(playback.filled(), 0);
    }

    #[test]
    fn priming_waits_for_the_cushion() {
        let shared = Shared::new();
        assert!(!shared.live.load(Ordering::Acquire));
        let tone: Vec<f32> = (0..PRIME_FRAMES * CHANNELS).map(|i| (i % 7) as f32).collect();
        assert_eq!(shared.playback.write(&tone), PRIME_FRAMES);
        assert!(shared.playback.filled() >= PRIME_FRAMES);
    }
}
