//! Peds placed at scenario points no longer read past a full entity list.
//!
//! `CScenarioManager::PerformCollisionChecksForScenarioPoint` asks
//! `CPedPlacement::IsPositionClearForPed` for up to 6 overlapping entities
//! into a 6-slot array on its stack, then walks the array until the first
//! null. With six overlaps the array has no null: the walk goes on into the
//! stack slot after it, the position's x and y (two floats, read as a
//! pointer), and the engine thread faults on the first field it reads
//! through it (a crash after minutes of play, around a crowded scenario
//! point). On the original hardware the same read found whatever lay there.
//!
//! The list is therefore asked for one entity fewer than the caller's size:
//! the last slot stays null. The caller only uses the list to skip a few
//! known entities and cares whether anything else overlaps, which a list of
//! five tells it as well as one of six.
//!
//! This is a workaround, not a fix of the caller's loop. If peds misbehave
//! (spawning overlapping something, scenario points refused or accepted
//! oddly), look here first: with six overlaps the caller now sees five, and
//! if those are all entities it ignores, the sixth goes unnoticed. The cap
//! applies to every caller of `IsPositionClearForPed`, not just this one.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use glue5_game::{Env, orig};

/// The engine's own default for a count of -1.
const DEFAULT_MAX: u32 = 2;
/// Counts the six-or-more-entry cases implicated in the out-of-bounds walker.
/// The heartbeat makes it possible to tell whether the affected placement
/// path was active just before a mission freeze without logging every ped.
static FULL_LIST_WORKAROUND_HITS: AtomicU64 = AtomicU64::new(0);

/// The workaround is enabled by default, but can be disabled for an A/B run
/// when diagnosing a mission-specific crash. The variable is read once so it
/// is not consulted on every ped placement query.
fn workaround_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GLUE5_DISABLE_SCENARIO_WORKAROUND").as_deref() != Ok("1"))
}

pub(crate) fn diagnostics_status() -> (bool, u64) {
    (workaround_enabled(), FULL_LIST_WORKAROUND_HITS.load(Ordering::Relaxed))
}

/// Hook for `CPedPlacement::IsPositionClearForPed`: the entity count is an
/// array size, and the list must keep a terminating null.
pub(crate) unsafe extern "C" fn is_position_clear_for_ped(
    env: *mut Env,
    pos: u64,
    radius: f32,
    max: u32,
    list: u64,
) -> u32 {
    if !workaround_enabled() {
        return unsafe { orig::is_position_clear_for_ped(env, pos, radius, max, list) };
    }
    let size = if max == u32::MAX { DEFAULT_MAX } else { max };
    if size >= 6 {
        let hits = FULL_LIST_WORKAROUND_HITS.fetch_add(1, Ordering::Relaxed) + 1;
        if hits <= 3 || hits % 1_000 == 0 {
            crate::diagnostics::note(format_args!(
                "scenario placement: capped a {size}-entry overlap list (full-list hits={hits})"
            ));
        }
    }
    let max = if size > 1 { size - 1 } else { max };
    unsafe { orig::is_position_clear_for_ped(env, pos, radius, max, list) }
}

/// This module's hooks.
pub(crate) fn hooks(hooks: &mut glue5_game::Hooks) {
    hooks.is_position_clear_for_ped = Some(is_position_clear_for_ped);
}
