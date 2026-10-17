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
    /// Mixer. Always user-interactive, with the pump and the device callback.
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

/// How far efficiency placement goes. Hybrid is the one to leave on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every frame thread stays user-interactive.
    Off,
    /// Performance cores while loading or hitching, efficiency cores once
    /// play is steady. Audio stays user-interactive either way.
    Hybrid,
    /// Frame threads stay at utility the whole time. Audio does not.
    FullE,
}

/// Why a hybrid frame is or is not on efficiency cores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Demand {
    /// Startup, a load, a teleport, or a streaming backlog.
    Boost,
    /// The frame budget was missed. Game and render step up.
    Hitch,
    /// The hitch is over. One step down before cruise.
    Ease,
    /// Steady play.
    Cruise,
}

/// One observation of the frame the hybrid policy is pacing.
#[derive(Clone, Copy, Debug)]
pub struct PaceSample {
    pub frame_ms: f32,
    pub budget_ms: f32,
    /// Bytes of texture decode still waiting.
    pub stream_bytes: u64,
    /// Decode jobs still waiting.
    pub stream_jobs: u32,
    /// Game-file and shader bytes read since the previous sample.
    pub io_bytes: u64,
    /// A teleport, or the game just started.
    pub warp: bool,
}

/// Hybrid's memory: stay boosted through a load, then cruise, and step up
/// for a hitch. Full-E and Off do not use it.
pub struct Pace {
    mode: Mode,
    settled: bool,
    steady: u32,
    boost_left: u32,
    hitch_left: u32,
    ease_left: u32,
    miss_run: u32,
    demand: Demand,
    /// The process just started the game. Heavy reads keep this up so the
    /// frame cap stays off until the load's reads fall away. A later
    /// teleport boosts thread class without lifting the cap again.
    booting: bool,
    /// A heavy read has been seen since [`Self::note_load`]. Quiet frames
    /// before that do not end the load: the screen can be up before the
    /// archives start.
    seen_load: bool,
}

impl Pace {
    /// Good frames before cruise, at a 24 fps cap (about three seconds).
    pub const STEADY_FRAMES: u32 = 72;
    /// Minimum boosted frames after a load or a teleport (about two seconds).
    pub const BOOST_HOLD: u32 = 48;
    pub const HITCH_HOLD: u32 = 6;
    pub const EASE_HOLD: u32 = 6;
    /// This many missed budgets in a row is a load, not a hitch.
    pub const MISS_RUN: u32 = 6;
    /// Pending decode bytes that count as a streaming backlog.
    pub const STREAM_BYTES: u64 = 24 << 20;
    /// Pending decode jobs that count as a backlog. One large texture does not,
    /// and ordinary driving stays under this. The hard cap that stalls a frame
    /// is 64 jobs.
    pub const STREAM_JOBS: u32 = 24;
    /// Game-file or shader bytes in one sample that mean the load is still
    /// reading. Only consulted while [`Self::booting`] is set, so a burst
    /// during play does not lift the frame cap.
    pub const LOAD_IO: u64 = 128 << 10;

    pub const fn new() -> Self {
        Self {
            mode: Mode::Hybrid,
            settled: false,
            steady: 0,
            boost_left: 0,
            hitch_left: 0,
            ease_left: 0,
            miss_run: 0,
            demand: Demand::Boost,
            booting: false,
            seen_load: false,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn demand(&self) -> Demand {
        self.demand
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            self.begin_boost();
        }
    }

    /// The game is starting. The frame cap stays off until the load's reads
    /// have been quiet for the steady window.
    pub fn note_load(&mut self) {
        self.booting = true;
        self.seen_load = false;
        self.begin_boost();
    }

    /// The menu's frame cap is not applied yet. Full-E keeps its cap: that
    /// mode is the efficiency-core test, including during the load.
    pub fn uncapped(&self) -> bool {
        self.booting && self.mode != Mode::FullE
    }

    fn begin_boost(&mut self) {
        self.settled = false;
        self.steady = 0;
        self.boost_left = Self::BOOST_HOLD;
        self.hitch_left = 0;
        self.ease_left = 0;
        self.miss_run = 0;
        self.demand = Demand::Boost;
    }

    pub fn observe(&mut self, sample: PaceSample) -> Demand {
        if self.mode == Mode::FullE {
            self.booting = false;
            self.settled = true;
            self.demand = Demand::Cruise;
            return self.demand;
        }
        let backlog = sample.stream_bytes >= Self::STREAM_BYTES || sample.stream_jobs >= Self::STREAM_JOBS;
        // A smooth loading screen is not play. Archive and shader reads hold
        // the boost, and the uncap, until they drop. Off uses the same latch
        // so its menu cap does not slice the load; its thread class does not
        // follow demand.
        let loading_io = self.booting && sample.io_bytes >= Self::LOAD_IO;
        if loading_io {
            self.seen_load = true;
        }
        if sample.warp || backlog || loading_io {
            self.begin_boost();
            return self.demand;
        }
        // Frames before the first archive or shader burst are not "play".
        if self.booting && !self.seen_load {
            self.demand = Demand::Boost;
            return self.demand;
        }
        let over = sample.budget_ms > 0.0 && sample.frame_ms > sample.budget_ms * 1.35;
        let ok = sample.budget_ms > 0.0 && sample.frame_ms <= sample.budget_ms * 1.15;
        if !self.settled {
            if ok {
                self.steady = self.steady.saturating_add(1);
            } else {
                self.steady = 0;
            }
            if self.boost_left > 0 {
                self.boost_left -= 1;
            }
            if self.steady >= Self::STEADY_FRAMES && self.boost_left == 0 {
                self.settled = true;
                self.booting = false;
                self.demand = Demand::Cruise;
            } else {
                self.demand = Demand::Boost;
            }
            return self.demand;
        }
        if self.mode != Mode::Hybrid {
            self.demand = Demand::Cruise;
            return self.demand;
        }
        if over {
            self.miss_run = self.miss_run.saturating_add(1);
            self.steady = 0;
            if self.miss_run >= Self::MISS_RUN {
                self.begin_boost();
                return self.demand;
            }
            self.hitch_left = Self::HITCH_HOLD;
            self.ease_left = 0;
            self.demand = Demand::Hitch;
            return self.demand;
        }
        self.miss_run = 0;
        if self.hitch_left > 0 {
            self.hitch_left -= 1;
            if self.hitch_left == 0 {
                self.ease_left = Self::EASE_HOLD;
                self.demand = Demand::Ease;
            } else {
                self.demand = Demand::Hitch;
            }
            return self.demand;
        }
        if self.ease_left > 0 {
            self.ease_left -= 1;
            self.demand = if self.ease_left == 0 { Demand::Cruise } else { Demand::Ease };
            return self.demand;
        }
        self.demand = Demand::Cruise;
        self.demand
    }
}

/// Class for `role`. Audio is user-interactive in every mode: the mixer and
/// the pump fill a buffer the device callback plays, and a utility class
/// there is what let a stalled game empty the speakers. Full-E holds every
/// other role at utility. Hybrid uses [`Demand`].
pub fn class_for(role: Role, mode: Mode, demand: Demand) -> Class {
    if role == Role::Audio {
        return Class::Interactive;
    }
    // Streaming below the frame stretched a post-cutscene load from 1-2 s
    // to 6-10 s on an iPhone 13 Pro: the interactive game and render threads
    // kept the performance cores, and the load waited on the other class.
    // Off and Hybrid keep streaming with the frame. Full-E is the test that
    // does not.
    match mode {
        Mode::Off => match role {
            Role::Save => Class::Initiated,
            _ => Class::Interactive,
        },
        Mode::FullE => Class::Utility,
        Mode::Hybrid => match (role, demand) {
            (Role::Stream, _) => Class::Interactive,
            (Role::Save, _) => Class::Background,
            (Role::Frame, Demand::Cruise) => Class::Utility,
            (Role::Frame, Demand::Ease) => Class::Initiated,
            (Role::Frame, Demand::Hitch | Demand::Boost) => Class::Interactive,
            (Role::Worker, Demand::Cruise) => Class::Utility,
            (Role::Worker, Demand::Ease | Demand::Hitch) => Class::Initiated,
            (Role::Worker, Demand::Boost) => Class::Interactive,
            (Role::Audio, _) => Class::Interactive,
        },
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
/// `prime_ms` is 100 with efficiency off and 250 in hybrid and full-E.
pub fn app_playback(pump_delay_us: u64, until_us: u64) -> PlaybackSpec {
    playback_with_prime(100, pump_delay_us, until_us)
}

pub fn playback_with_prime(prime_ms: u64, pump_delay_us: u64, until_us: u64) -> PlaybackSpec {
    const RATE: u64 = 48_000;
    const QUANTUM: usize = 256;
    PlaybackSpec {
        prime: (RATE as usize) * prime_ms as usize / 1000,
        cap: 32768,
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

        let cruise = Demand::Cruise;
        assert_eq!(class_for(Role::Frame, Mode::Off, cruise), Class::Interactive);
        assert_eq!(class_for(Role::Audio, Mode::Off, cruise), Class::Interactive);
        assert_eq!(class_for(Role::Stream, Mode::Off, cruise), Class::Interactive);
        assert_eq!(class_for(Role::Worker, Mode::Off, cruise), Class::Interactive);
        assert_eq!(class_for(Role::Save, Mode::Off, cruise), Class::Initiated);
        assert_eq!(class_for(Role::Audio, Mode::Hybrid, cruise), Class::Interactive);
        assert_eq!(class_for(Role::Audio, Mode::FullE, Demand::Boost), Class::Interactive);
        assert_eq!(class_for(Role::Frame, Mode::Hybrid, cruise), Class::Utility);
        assert_eq!(class_for(Role::Stream, Mode::Hybrid, cruise), Class::Interactive);
        assert_eq!(class_for(Role::Stream, Mode::FullE, cruise), Class::Utility);
        assert_eq!(class_for(Role::Save, Mode::Hybrid, cruise), Class::Background);
        assert_eq!(class_for(Role::Frame, Mode::Hybrid, Demand::Boost), Class::Interactive);
        assert_eq!(class_for(Role::Stream, Mode::Hybrid, Demand::Boost), Class::Interactive);
        assert_eq!(class_for(Role::Frame, Mode::Hybrid, Demand::Hitch), Class::Interactive);
        assert_eq!(class_for(Role::Frame, Mode::Hybrid, Demand::Ease), Class::Initiated);
        assert_eq!(class_for(Role::Frame, Mode::FullE, cruise), Class::Utility);
        assert_eq!(class_for(Role::Worker, Mode::FullE, cruise), Class::Utility);
        assert_eq!(class_for(Role::Save, Mode::FullE, cruise), Class::Utility);
        assert_ne!(class_for(Role::Audio, Mode::FullE, cruise), Class::Utility);
    }

    fn good(pace: &mut Pace, n: u32) {
        let sample = PaceSample {
            frame_ms: 40.0,
            budget_ms: 1000.0 / 24.0,
            stream_bytes: 0,
            stream_jobs: 0,
            io_bytes: 0,
            warp: false,
        };
        for _ in 0..n {
            pace.observe(sample);
        }
    }

    #[test]
    fn hybrid_stays_boosted_until_play_is_steady() {
        let mut pace = Pace::new();
        pace.note_load();
        pace.observe(PaceSample {
            frame_ms: 40.0,
            budget_ms: 1000.0 / 24.0,
            stream_bytes: 0,
            stream_jobs: 0,
            io_bytes: Pace::LOAD_IO,
            warp: false,
        });
        good(&mut pace, Pace::BOOST_HOLD);
        assert_eq!(pace.demand(), Demand::Boost, "the hold is not cruise");
        good(&mut pace, Pace::STEADY_FRAMES - Pace::BOOST_HOLD - 1);
        assert_eq!(pace.demand(), Demand::Boost);
        good(&mut pace, 1);
        assert_eq!(pace.demand(), Demand::Cruise);
    }

    #[test]
    fn a_warp_or_a_backlog_returns_to_performance_cores() {
        let mut pace = Pace::new();
        good(&mut pace, Pace::STEADY_FRAMES);
        assert_eq!(pace.demand(), Demand::Cruise);
        pace.observe(PaceSample {
            frame_ms: 40.0,
            budget_ms: 1000.0 / 24.0,
            stream_bytes: 0,
            stream_jobs: 0,
            io_bytes: 0,
            warp: true,
        });
        assert_eq!(pace.demand(), Demand::Boost);
        good(&mut pace, Pace::STEADY_FRAMES);
        assert_eq!(pace.demand(), Demand::Cruise);
        pace.observe(PaceSample {
            frame_ms: 40.0,
            budget_ms: 1000.0 / 24.0,
            stream_bytes: 0,
            stream_jobs: Pace::STREAM_JOBS,
            io_bytes: 0,
            warp: false,
        });
        assert_eq!(pace.demand(), Demand::Boost);
    }

    #[test]
    fn a_spike_steps_up_and_then_back_and_a_run_of_them_reloads() {
        let mut pace = Pace::new();
        good(&mut pace, Pace::STEADY_FRAMES);
        let slow = PaceSample {
            frame_ms: 80.0,
            budget_ms: 1000.0 / 24.0,
            stream_bytes: 0,
            stream_jobs: 0,
            io_bytes: 0,
            warp: false,
        };
        assert_eq!(pace.observe(slow), Demand::Hitch);
        good(&mut pace, Pace::HITCH_HOLD - 1);
        assert_eq!(pace.demand(), Demand::Hitch);
        good(&mut pace, 1);
        assert_eq!(pace.demand(), Demand::Ease);
        good(&mut pace, Pace::EASE_HOLD);
        assert_eq!(pace.demand(), Demand::Cruise);
        for _ in 0..Pace::MISS_RUN - 1 {
            assert_eq!(pace.observe(slow), Demand::Hitch);
        }
        assert_eq!(pace.observe(slow), Demand::Boost);
    }

    #[test]
    fn loading_reads_hold_the_boost_until_they_go_quiet() {
        let mut pace = Pace::new();
        pace.note_load();
        assert!(pace.uncapped());
        let loading = PaceSample {
            frame_ms: 16.0,
            budget_ms: 16.7,
            stream_bytes: 0,
            stream_jobs: 0,
            io_bytes: Pace::LOAD_IO,
            warp: false,
        };
        for _ in 0..Pace::STEADY_FRAMES * 3 {
            assert_eq!(pace.observe(loading), Demand::Boost);
        }
        assert!(pace.uncapped(), "a loading screen that renders on time is still a load");
        good(&mut pace, Pace::STEADY_FRAMES);
        assert_eq!(pace.demand(), Demand::Cruise);
        assert!(!pace.uncapped());
        // Reads after the load has settled do not lift the cap or the boost.
        // A teleport still does, through the warp signal.
        pace.observe(loading);
        assert_eq!(pace.demand(), Demand::Cruise);
        assert!(!pace.uncapped());
    }

    #[test]
    fn off_uncaps_the_load_and_then_the_menu_cap_returns() {
        let mut pace = Pace::new();
        pace.set_mode(Mode::Off);
        pace.note_load();
        assert!(pace.uncapped());
        pace.observe(PaceSample {
            frame_ms: 16.0,
            budget_ms: 16.7,
            stream_bytes: 0,
            stream_jobs: 0,
            io_bytes: Pace::LOAD_IO,
            warp: false,
        });
        assert!(pace.uncapped());
        assert_eq!(class_for(Role::Stream, Mode::Off, pace.demand()), Class::Interactive);
        assert_eq!(class_for(Role::Worker, Mode::Off, pace.demand()), Class::Interactive);
        good(&mut pace, Pace::STEADY_FRAMES);
        assert!(!pace.uncapped());
        let mut full = Pace::new();
        full.set_mode(Mode::FullE);
        full.note_load();
        assert!(!full.uncapped(), "full-E keeps its cap through the load");
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

    #[test]
    fn the_efficiency_cushion_covers_a_stall_the_short_one_does_not() {
        let m = measure_playback(playback_with_prime(250, 200_000, 800_000));
        assert_eq!(m.silent, 0, "{m:?}");
        assert_eq!(m.short, 0, "{m:?}");
        let past = measure_playback(playback_with_prime(250, 400_000, 900_000));
        assert!(past.silent > 0, "{past:?}");
    }
}
