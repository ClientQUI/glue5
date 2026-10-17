//! Prerecorded Bink movies have no picture in this build.
//!
//! The wasm module has no Bink decoder. `AsyncBink` is a worker that fails
//! the request at once, and the engine then draws "not found". Retail `.bik`
//! bytes live in the game's RPF archives (`x64d.rpf/movies` and the update
//! packs). The web mirror this port plays from does not ship those bytes,
//! and Bink 2 (FourCC `KB2`) is a proprietary codec: ffmpeg decodes Bink 1
//! only. Wiring a decoder into the movie texture would also need engine
//! symbols this tree must not guess.
//!
//! What scripts do have is the movie clock. `GET_BINK_MOVIE_TIME` stays at 0
//! when the clip fails, so a mission that waits for a moment in the clip
//! (Friend Request, Lester's LifeInvader keynote) never opens its window.
//! When the script wrappers are in game.wasm, this module keeps a clock per
//! clip and reports progress as a percentage of the clip's duration, 0–100,
//! which is the value those natives use (`SetBinkMovieTime(id, 0)` seeks to
//! the start, and scripts wait until the value reaches 100). The picture
//! stays the engine's "not found" frame. A caption names the clip and the
//! percentage, so the wait is visible.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use glue5_game::{Env, orig};

use crate::memory::Mem;

/// `lester1b_1.bik`, the LifeInvader keynote, runs about 2:58.
const LIFEINVADER_SECS: f32 = 178.0;
/// A clip whose length we do not know. Long enough that a script waiting on
/// 100 does not fire in the same frame, short enough that a TV left on does
/// not sit at 0 forever.
const UNKNOWN_SECS: f32 = 30.0;
/// Ids handed out when the engine's own `SET_BINK_MOVIE` returns none.
const HOST_ID_BASE: i32 = 1_000_000;

struct Clip {
    name: String,
    duration: f32,
    /// Seconds already played, not counting the current run.
    offset: f32,
    /// When the current run started.
    started: Option<Instant>,
    playing: bool,
}

struct Movies {
    next_host_id: i32,
    clips: HashMap<i32, Clip>,
    /// The clip the caption follows.
    shown: Option<i32>,
    logged: Vec<String>,
}

impl Movies {
    fn new() -> Self {
        Movies { next_host_id: HOST_ID_BASE, clips: HashMap::new(), shown: None, logged: Vec::new() }
    }
}

fn movies() -> std::sync::MutexGuard<'static, Movies> {
    static MOVIES: std::sync::OnceLock<Mutex<Movies>> = std::sync::OnceLock::new();
    MOVIES.get_or_init(|| Mutex::new(Movies::new())).lock().unwrap_or_else(|e| e.into_inner())
}

/// Seconds of `name` (a path or a bare file name, any case).
fn duration_secs(name: &str) -> f32 {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let base = base.split('.').next().unwrap_or(base);
    match base.to_ascii_lowercase().as_str() {
        "lester1b_1" => LIFEINVADER_SECS,
        _ => UNKNOWN_SECS,
    }
}

fn played_seconds(offset: f32, playing: bool, since_start: f32) -> f32 {
    let extra = if playing { since_start.max(0.0) } else { 0.0 };
    (offset + extra).max(0.0)
}

/// `GET_BINK_MOVIE_TIME`: 0 at the start, 100 at the end.
fn percent_played(seconds: f32, duration: f32) -> f32 {
    if !seconds.is_finite() || !duration.is_finite() || duration <= 0.0 {
        return 0.0;
    }
    (seconds / duration * 100.0).clamp(0.0, 100.0)
}

fn caption_line(name: &str, percent: f32) -> String {
    format!("No video in this copy — {name}  {percent:.0}%")
}

impl Clip {
    fn seconds(&self, now: Instant) -> f32 {
        let since = self
            .started
            .filter(|_| self.playing)
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(0.0);
        played_seconds(self.offset, self.playing, since)
    }

    fn percent(&self, now: Instant) -> f32 {
        percent_played(self.seconds(now), self.duration)
    }
}

/// The line drawn over the game while a script movie is open, if one is.
pub fn caption() -> Option<String> {
    let now = Instant::now();
    let movies = movies();
    let clip = movies.clips.get(&movies.shown?)?;
    Some(caption_line(&clip.name, clip.percent(now)))
}

/// `rage::scrThread::Info` as this build's other script wrappers use it:
/// `+0` result pointer, `+8` argument count, `+16` pointer to 8-byte slots.
struct Call {
    result: u64,
    args: u64,
    argc: u32,
}

fn native_call(m: Mem, info: u64) -> Option<Call> {
    let size = crate::rt().memory.size();
    if info == 0 || info.saturating_add(24) > size {
        return None;
    }
    let result = m.u64(info);
    let argc = m.u32(info + 8);
    let args = m.u64(info + 16);
    if result != 0 && result.saturating_add(4) > size {
        return None;
    }
    let slots = argc as u64 * 8;
    if argc > 32 || (argc > 0 && (args == 0 || args.saturating_add(slots) > size)) {
        return None;
    }
    Some(Call { result, args, argc })
}

fn arg_i32(m: Mem, call: &Call, i: u32) -> Option<i32> {
    (i < call.argc).then(|| m.i32(call.args + i as u64 * 8))
}

fn arg_f32(m: Mem, call: &Call, i: u32) -> Option<f32> {
    (i < call.argc).then(|| f32::from_bits(m.u32(call.args + i as u64 * 8)))
}

fn arg_str(m: Mem, call: &Call, i: u32) -> Option<String> {
    if i >= call.argc {
        return None;
    }
    let addr = m.u64(call.args + i as u64 * 8);
    let size = crate::rt().memory.size();
    if addr == 0 || addr >= size {
        return None;
    }
    let mut raw = Vec::new();
    let end = size.min(addr.saturating_add(160));
    let mut at = addr;
    while at < end {
        let b = m.u8(at);
        if b == 0 {
            break;
        }
        raw.push(b);
        at += 1;
    }
    Some(String::from_utf8_lossy(&raw).into_owned())
}

fn register(id: i32, name: String) {
    let mut movies = movies();
    let duration = duration_secs(&name);
    if movies.logged.len() < 32 && !movies.logged.iter().any(|n| n == &name) {
        log::info!("bink: \"{name}\" has no picture; script time runs for {duration:.0}s");
        movies.logged.push(name.clone());
    }
    movies.clips.insert(id, Clip { name, duration, offset: 0.0, started: None, playing: false });
    movies.shown = Some(id);
}

fn with_clip(id: i32, f: impl FnOnce(&mut Clip)) {
    if let Some(clip) = movies().clips.get_mut(&id) {
        f(clip);
    }
}

/// `SET_BINK_MOVIE`. The engine still creates its (failing) movie. A 0 from
/// it would leave the script with nothing to play, so a host id stands in.
pub(crate) unsafe extern "C" fn bink_set_movie(env: *mut Env, info: u64) {
    unsafe { orig::bink_set_movie(env, info) };
    let m = unsafe { Mem::of(env) };
    let Some(call) = native_call(m, info) else { return };
    if call.result == 0 {
        return;
    }
    let name = arg_str(m, &call, 0).filter(|s| !s.is_empty()).unwrap_or_else(|| "video".into());
    let mut id = m.i32(call.result);
    if id <= 0 {
        let mut movies = movies();
        id = movies.next_host_id;
        movies.next_host_id = movies.next_host_id.saturating_add(1);
        drop(movies);
        m.set_i32(call.result, id);
    }
    register(id, name);
}

/// `PLAY_BINK_MOVIE`.
pub(crate) unsafe extern "C" fn bink_play_movie(env: *mut Env, info: u64) {
    unsafe { orig::bink_play_movie(env, info) };
    let m = unsafe { Mem::of(env) };
    let Some(id) = native_call(m, info).and_then(|c| arg_i32(m, &c, 0)) else { return };
    let now = Instant::now();
    with_clip(id, |clip| {
        if !clip.playing {
            clip.playing = true;
            clip.started = Some(now);
        }
    });
    movies().shown = Some(id);
}

/// `STOP_BINK_MOVIE`. The clock pauses; the caption stays until release.
pub(crate) unsafe extern "C" fn bink_stop_movie(env: *mut Env, info: u64) {
    unsafe { orig::bink_stop_movie(env, info) };
    let m = unsafe { Mem::of(env) };
    let Some(id) = native_call(m, info).and_then(|c| arg_i32(m, &c, 0)) else { return };
    let now = Instant::now();
    with_clip(id, |clip| {
        clip.offset = clip.seconds(now);
        clip.playing = false;
        clip.started = None;
    });
}

/// `RELEASE_BINK_MOVIE`.
pub(crate) unsafe extern "C" fn bink_release_movie(env: *mut Env, info: u64) {
    unsafe { orig::bink_release_movie(env, info) };
    let m = unsafe { Mem::of(env) };
    let Some(id) = native_call(m, info).and_then(|c| arg_i32(m, &c, 0)) else { return };
    let mut movies = movies();
    movies.clips.remove(&id);
    if movies.shown == Some(id) {
        movies.shown = None;
    }
}

/// `GET_BINK_MOVIE_TIME`. The engine's answer stays 0 once AsyncBink has
/// failed the clip, so a clip we opened reports our clock instead.
pub(crate) unsafe extern "C" fn bink_movie_time(env: *mut Env, info: u64) {
    unsafe { orig::bink_movie_time(env, info) };
    let m = unsafe { Mem::of(env) };
    let Some(call) = native_call(m, info) else { return };
    let Some(id) = arg_i32(m, &call, 0) else { return };
    if call.result == 0 {
        return;
    }
    let now = Instant::now();
    let Some(percent) = movies().clips.get(&id).map(|clip| clip.percent(now)) else { return };
    m.set_u32(call.result, percent.to_bits());
}

/// `SET_BINK_MOVIE_TIME`. The argument is a percentage, the same unit `GET` returns.
pub(crate) unsafe extern "C" fn bink_set_movie_time(env: *mut Env, info: u64) {
    unsafe { orig::bink_set_movie_time(env, info) };
    let m = unsafe { Mem::of(env) };
    let Some(call) = native_call(m, info) else { return };
    let Some(id) = arg_i32(m, &call, 0) else { return };
    let Some(percent) = arg_f32(m, &call, 1) else { return };
    let now = Instant::now();
    with_clip(id, |clip| {
        let percent = if percent.is_finite() { percent.clamp(0.0, 100.0) } else { 0.0 };
        clip.offset = percent / 100.0 * clip.duration;
        if clip.playing {
            clip.started = Some(now);
        }
    });
}

/// `DRAW_BINK_MOVIE`. The engine draws its "not found" frame; the caption
/// follows whichever clip the script is showing.
pub(crate) unsafe extern "C" fn bink_draw_movie(env: *mut Env, info: u64) {
    unsafe { orig::bink_draw_movie(env, info) };
    let m = unsafe { Mem::of(env) };
    let Some(id) = native_call(m, info).and_then(|c| arg_i32(m, &c, 0)) else { return };
    // The condition's guard would live for the whole `if` and deadlock on the
    // assignment below.
    let known = movies().clips.contains_key(&id);
    if known {
        movies().shown = Some(id);
    }
}

pub(crate) fn hooks(hooks: &mut glue5_game::Hooks) {
    hooks.bink_set_movie = Some(bink_set_movie);
    hooks.bink_play_movie = Some(bink_play_movie);
    hooks.bink_stop_movie = Some(bink_stop_movie);
    hooks.bink_release_movie = Some(bink_release_movie);
    hooks.bink_movie_time = Some(bink_movie_time);
    hooks.bink_set_movie_time = Some(bink_set_movie_time);
    hooks.bink_draw_movie = Some(bink_draw_movie);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifeinvader_keynote_is_just_under_three_minutes() {
        assert_eq!(duration_secs("lester1b_1"), 178.0);
        assert_eq!(duration_secs("MOVIES/lester1b_1.bik"), 178.0);
        assert_eq!(duration_secs(r"x64\movies\LESTER1B_1.BIK"), 178.0);
        assert_eq!(duration_secs("lifeinvader"), UNKNOWN_SECS, "only the keynote file has a known length");
    }

    #[test]
    fn movie_time_is_a_percentage_of_the_real_duration() {
        assert_eq!(percent_played(0.0, 178.0), 0.0);
        assert!((percent_played(89.0, 178.0) - 50.0).abs() < 0.01);
        assert_eq!(percent_played(178.0, 178.0), 100.0);
        assert_eq!(percent_played(400.0, 178.0), 100.0);
        assert_eq!(percent_played(10.0, 0.0), 0.0);
        assert_eq!(percent_played(f32::NAN, 178.0), 0.0);
    }

    #[test]
    fn a_paused_movie_does_not_advance() {
        assert_eq!(played_seconds(40.0, false, 100.0), 40.0);
        assert_eq!(played_seconds(40.0, true, 9.0), 49.0);
        // A seek to 25% of the keynote, then ten seconds of playback.
        let offset = 25.0 / 100.0 * 178.0;
        let seconds = played_seconds(offset, true, 10.0);
        assert!((percent_played(seconds, 178.0) - (54.5 / 178.0 * 100.0)).abs() < 0.02);
    }

    #[test]
    fn the_caption_names_the_clip_and_its_progress() {
        assert_eq!(caption_line("lester1b_1", 42.2), "No video in this copy — lester1b_1  42%");
    }

    #[test]
    fn movies_start_empty() {
        // The clock is process-wide. This only checks the constructor a test
        // can hold without touching that clock.
        let fresh = Movies::new();
        assert!(fresh.clips.is_empty());
        assert_eq!(fresh.next_host_id, HOST_ID_BASE);
    }
}
