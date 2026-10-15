//! Which work may leave the performance cores.
//!
//! The operating system places threads. A user-interactive class prefers
//! performance cores; utility and background prefer efficiency cores. This
//! module only classifies names and models the audio cushion. It does not
//! pin cores and it does not use the Neural Engine.

/// What an engine thread is for, from the name it publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Main, game, and render. A frame waits on these.
    Frame,
    /// Mixer. Utility while efficiency placement is on; the device callback
    /// stays user-interactive.
    Audio,
    /// Asset streaming and decompression.
    Stream,
    /// A thread that exists to save. File sync on a frame thread is utility
    /// for the duration of the write instead, so a suspend flush is not
    /// deferred.
    Save,
    /// Physics, scripts, and anything else a frame may wait on.
    Worker,
}

/// The class [`Role`] maps to. Apple's QoS uses the same names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Interactive,
    Initiated,
    Utility,
    Background,
}

/// `name` is the engine's thread name (`RenderThread`, `MixThread`, …).
pub fn role(name: &str) -> Role {
    let words = words(name);
    if words.iter().any(|w| starts(w, "render")) {
        return Role::Frame;
    }
    if words.iter().any(|w| starts(w, "audio") || starts(w, "mix") || starts(w, "sound")) {
        return Role::Audio;
    }
    if words.iter().any(|w| starts(w, "save") || starts(w, "cloud") || starts(w, "stats")) {
        return Role::Save;
    }
    if words.iter().any(|w| {
        starts(w, "stream")
            || starts(w, "decompress")
            || starts(w, "inflate")
            || starts(w, "deflate")
            || starts(w, "zstd")
            || starts(w, "zlib")
            || starts(w, "lz4")
            || starts(w, "resource")
    }) {
        return Role::Stream;
    }
    if words.iter().any(|w| starts(w, "game") || starts(w, "main")) {
        return Role::Frame;
    }
    Role::Worker
}

fn starts(word: &str, needle: &str) -> bool {
    word.len() >= needle.len() && word.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

/// CamelCase and separators, so "street" is not "stream" and "RenderThread"
/// is "render" + "thread".
fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let boundary = !c.is_ascii_alphanumeric()
            || (!cur.is_empty()
                && c.is_ascii_uppercase()
                && (chars[i - 1].is_ascii_lowercase()
                    || (cur.len() > 1 && chars.get(i + 1).copied().is_some_and(|n| n.is_ascii_lowercase()))));
        if boundary {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if c.is_ascii_alphanumeric() {
                cur.push(c);
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Class for `role` when efficiency-core placement is `efficiency`.
/// Unknown workers stay user-initiated either way.
pub fn class_for(role: Role, efficiency: bool) -> Class {
    match role {
        Role::Frame => Class::Interactive,
        Role::Audio => {
            if efficiency {
                Class::Utility
            } else {
                Class::Interactive
            }
        }
        Role::Stream => {
            if efficiency {
                Class::Utility
            } else {
                Class::Initiated
            }
        }
        Role::Save => {
            if efficiency {
                Class::Background
            } else {
                Class::Initiated
            }
        }
        Role::Worker => Class::Initiated,
    }
}

/// One run of the playback pump against the device callback.
///
/// The device plays only while `live`. A callback that finds an empty buffer
/// clears `live` and stays silent until the pump has stored `prime` frames
/// again. A callback that gets some but not all of its period is a short
/// (a gap inside one quantum). This is the cushion the app's audio thread
/// relies on: 100 ms primed, 256-frame quanta, a pump every 4 ms.
#[derive(Clone, Copy, Debug)]
pub struct PlaybackSpec {
    pub prime: usize,
    pub cap: usize,
    pub device_frames: usize,
    pub device_every_us: u64,
    pub pump_frames: usize,
    pub pump_every_us: u64,
    /// No pump samples until this time. Zero pumps at the start.
    pub pump_delay_us: u64,
    pub until_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackMeasure {
    pub callbacks: u32,
    pub silent: u32,
    pub short: u32,
}

/// The app's device quantum and cushion, in microseconds.
pub fn app_playback(pump_delay_us: u64, until_us: u64) -> PlaybackSpec {
    const RATE: u64 = 48_000;
    const QUANTUM: usize = 256;
    PlaybackSpec {
        prime: (RATE as usize) * 100 / 1000,
        cap: 16384,
        device_frames: QUANTUM,
        device_every_us: QUANTUM as u64 * 1_000_000 / RATE,
        pump_frames: QUANTUM,
        pump_every_us: 4_000,
        pump_delay_us,
        until_us,
    }
}

pub fn measure_playback(spec: PlaybackSpec) -> PlaybackMeasure {
    let mut buffered = spec.prime.min(spec.cap);
    let mut live = buffered >= spec.prime;
    let mut next_pump = spec.pump_delay_us;
    let mut next_dev = spec.device_every_us;
    let mut silent = 0u32;
    let mut short = 0u32;
    let mut callbacks = 0u32;
    while next_dev <= spec.until_us || next_pump <= spec.until_us {
        let t = next_dev.min(next_pump);
        if t > spec.until_us {
            break;
        }
        // The callback runs first when both are due, so a pump that lands on
        // the same instant cannot hide an empty read.
        if next_dev == t {
            callbacks += 1;
            if !live || buffered == 0 {
                live = false;
                silent += 1;
            } else {
                let take = spec.device_frames.min(buffered);
                if take < spec.device_frames {
                    short += 1;
                }
                buffered -= take;
            }
            next_dev = next_dev.saturating_add(spec.device_every_us);
        }
        if next_pump == t {
            if buffered < spec.cap {
                let room = spec.cap - buffered;
                buffered += spec.pump_frames.min(room);
            }
            if buffered >= spec.prime {
                live = true;
            }
            next_pump = next_pump.saturating_add(spec.pump_every_us);
        }
    }
    PlaybackMeasure { callbacks, silent, short }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_go_to_the_class_the_frame_can_spare() {
        assert_eq!(role("RenderThread"), Role::Frame);
        assert_eq!(role("engine main"), Role::Frame);
        assert_eq!(role("GameThread"), Role::Frame);
        assert_eq!(role("audMixerDeviceWasmDiscard::MixThread"), Role::Audio);
        assert_eq!(role("Audio Mixer"), Role::Audio);
        assert_eq!(role("SoundEngine"), Role::Audio);
        assert_eq!(role("Streaming"), Role::Stream);
        assert_eq!(role("ResourceStreaming"), Role::Stream);
        assert_eq!(role("zlibInflate"), Role::Stream);
        assert_eq!(role("SaveGame"), Role::Save);
        assert_eq!(role("CloudSave"), Role::Save);
        assert_eq!(role("StatsWriter"), Role::Save);
        assert_eq!(role("Physics"), Role::Worker);
        assert_eq!(role("street"), Role::Worker);
        assert_eq!(role("AsyncBink"), Role::Worker);

        assert_eq!(class_for(Role::Frame, true), Class::Interactive);
        assert_eq!(class_for(Role::Audio, true), Class::Utility);
        assert_eq!(class_for(Role::Stream, true), Class::Utility);
        assert_eq!(class_for(Role::Save, true), Class::Background);
        assert_eq!(class_for(Role::Worker, true), Class::Initiated);
        assert_eq!(class_for(Role::Audio, false), Class::Interactive);
        assert_eq!(class_for(Role::Stream, false), Class::Initiated);
        assert_ne!(class_for(Role::Audio, true), Class::Background);
    }

    #[test]
    fn a_steady_pump_does_not_starve_the_device() {
        let m = measure_playback(app_playback(0, 2_000_000));
        assert!(m.callbacks > 100, "{m:?}");
        assert_eq!(m.silent, 0, "{m:?}");
        assert_eq!(m.short, 0, "{m:?}");
    }

    #[test]
    fn a_stall_inside_the_cushion_does_not_drop_or_short() {
        // 18 full quanta drain 4608 of the 4800 primed frames and take 96 ms.
        // A 90 ms stall on the pump leaves a whole quantum, so the callback
        // neither underruns nor plays a short period.
        let m = measure_playback(app_playback(90_000, 500_000));
        assert_eq!(m.silent, 0, "{m:?}");
        assert_eq!(m.short, 0, "{m:?}");
    }

    #[test]
    fn a_stall_past_the_cushion_is_counted_and_then_reprimes() {
        let m = measure_playback(app_playback(200_000, 800_000));
        assert!(m.silent > 0, "{m:?}");
        // The first empty read is after the primed audio has played out.
        let early = measure_playback(app_playback(200_000, 96_000));
        assert_eq!(early.silent, 0, "{early:?}");
        assert_eq!(early.short, 0, "{early:?}");
    }
}
