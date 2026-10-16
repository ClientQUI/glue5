//! GTA Online's stats that the game's data does not define. The scripts are
//! a later version than the data (and its DLC packs): they read and write
//! thousands of stats the engine has no entry for, which it drops (and
//! reads as 0). Some matter: a character whose "version" stats read 0 is an
//! old one and goes back to the creator, its hair is gone...
//!
//! The host keeps them instead, by stat hash or by packed stat (index,
//! character slot), and saves them with the character's files (cloud.rs).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Mutex;

use glue5_game::{Env, orig};

use crate::memory::Mem;

thread_local! {
    /// The engine's last stat lookup found nothing.
    static MISSING: Cell<bool> = const { Cell::new(false) };
    /// The stat hash a command get/set is servicing. Lookups of other stats
    /// during that call must not flip [`MISSING`]: a miss used to make
    /// `engine_or` throw away the engine's value and answer 0.
    static WANTED: Cell<u32> = const { Cell::new(0) };
    static WANTED_ON: Cell<bool> = const { Cell::new(false) };
}

/// Stat values by key: `h<hash>` (int, float bits, or bool), `p<slot>:<index>`
/// (packed: a bool or a byte).
struct Store {
    values: BTreeMap<String, i64>,
    dirty: bool,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn file() -> std::path::PathBuf {
    crate::rt().config.user_dir.join("cloud").join("extra-stats.txt")
}

fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut store = STORE.lock().unwrap();
    let store = store.get_or_insert_with(|| {
        let values = std::fs::read_to_string(file())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once(' ')?;
                Some((k.to_string(), v.parse().ok()?))
            })
            .collect();
        Store { values, dirty: false }
    });
    f(store)
}

fn get(key: &str) -> i64 {
    stored(key).unwrap_or(0)
}

fn stored(key: &str) -> Option<i64> {
    with_store(|s| s.values.get(key).copied())
}

fn set(key: String, value: i64) {
    with_store(|s| {
        if s.values.get(&key) != Some(&value) {
            s.values.insert(key, value);
            s.dirty = true;
        }
    })
}

/// Writes the stats down; with each save of the character's files.
pub(crate) fn save() {
    with_store(|s| {
        if !s.dirty {
            return;
        }
        let text: String = s.values.iter().map(|(k, v)| format!("{k} {v}\n")).collect();
        let path = file();
        let saved = crate::vfs::durable_write(&path, text.as_bytes());
        match saved {
            Ok(()) => {
                s.dirty = false;
                crate::drive::note_write();
            }
            Err(e) => log::error!("stats: saving {}: {e}", path.display()),
        }
    })
}

/// The engine's stat operation, or the host's when the engine has no such
/// stat. [`MISSING`] is only the lookup of the hash [`with_stat`] is
/// servicing, so a nested miss does not replace a value the engine wrote.
fn engine_or<R>(engine: impl FnOnce() -> R, host: impl FnOnce() -> R) -> R {
    MISSING.set(false);
    let r = engine();
    if MISSING.get() { host() } else { r }
}

fn with_stat<R>(hash: u32, f: impl FnOnce() -> R) -> R {
    let prev_on = WANTED_ON.get();
    let prev = WANTED.get();
    let prev_missing = MISSING.get();
    WANTED_ON.set(true);
    WANTED.set(hash);
    MISSING.set(false);
    let result = f();
    WANTED_ON.set(prev_on);
    WANTED.set(prev);
    MISSING.set(prev_missing);
    result
}

/// `found == 0` means the engine's map has no entry. Outside a command
/// get/set, any miss counts. Inside one, only a miss of that stat's hash
/// counts: `StatId`'s first field is the joaat.
fn lookup_missing(wanted_on: bool, wanted: u32, id_hash: u32, found: u32, missing: bool) -> bool {
    if !wanted_on || id_hash == wanted { found == 0 } else { missing }
}

/// Hooks for `CStatsDataMgr::StatIterFind(StatId const&, Iterator&)` and
/// `packed_stats::GetStatKey(StatId&, int, int)`: the engine's lookups.
pub(crate) unsafe extern "C" fn stat_find(env: *mut Env, id: u64, iter: u64) -> u32 {
    let found = unsafe { orig::stat_find(env, id, iter) };
    let id_hash = unsafe { Mem::of(env) }.u32(id);
    MISSING.set(lookup_missing(WANTED_ON.get(), WANTED.get(), id_hash, found, MISSING.get()));
    found
}

pub(crate) unsafe extern "C" fn packed_key(env: *mut Env, id: u64, index: u32, slot: u32) -> u32 {
    let m = unsafe { Mem::of(env) };
    let map = stats_map(m);
    let missing = with_misses(map, |keys| keys.get(&(index, slot)).copied());
    let found = match missing {
        Some(hash) => {
            m.set_u32(id, hash);
            0
        }
        None => {
            let found = unsafe { orig::packed_key(env, id, index, slot) };
            if found == 0 {
                with_misses(map, |keys| keys.insert((index, slot), m.u32(id)));
            }
            found
        }
    };
    if !WANTED_ON.get() {
        MISSING.set(found == 0);
    }
    found
}

/// The packed stats `GetStatKey` found missing: (index, slot) → the key's
/// hash. The scripts ask for the same few dozen every frame (and for every
/// packed stat once while the character loads, ~27000), and each miss
/// costs the engine a name (snprintf), its hash and an error in the log
/// (~1 GB an hour). The answer depends only on (index, slot) and on the
/// stats map, so it holds while the map is the same: no stat registered or
/// cleared since.
struct Misses {
    map: (u64, u16, u8),
    keys: BTreeMap<(u32, u32), u32>,
}

static MISSES: Mutex<Misses> = Mutex::new(Misses { map: (0, 0, 0), keys: BTreeMap::new() });

/// `CStatsDataMgr`'s stats map (an atBinaryMap: entries, count, sorted).
fn stats_map(m: Mem) -> (u64, u16, u8) {
    const ENTRIES: u64 = 25461184;
    const COUNT: u64 = 25461192;
    const SORTED: u64 = 25461176;
    (m.u64(ENTRIES), m.u16(COUNT), m.u8(SORTED))
}

fn with_misses<R>(map: (u64, u16, u8), f: impl FnOnce(&mut BTreeMap<(u32, u32), u32>) -> R) -> R {
    let mut misses = MISSES.lock().unwrap();
    if misses.map != map {
        misses.map = map;
        misses.keys.clear();
    }
    f(&mut misses.keys)
}

fn hash_key(hash: u32) -> String {
    format!("h{hash:08x}")
}

fn packed_key_name(index: u32, slot: u32) -> String {
    format!("p{}:{index}", slot as i32)
}

/// Hooks for `packed_stats::GetPackedValue(int&, int, int)` and
/// `SetPackedValue(int, int, int)`.
pub(crate) unsafe extern "C" fn packed_get(env: *mut Env, out: u64, index: u32, slot: u32) -> u32 {
    engine_or(
        || unsafe { orig::packed_get(env, out, index, slot) },
        || write(env, out, get(&packed_key_name(index, slot))),
    )
}

pub(crate) unsafe extern "C" fn packed_set(env: *mut Env, value: u32, index: u32, slot: u32) {
    engine_or(
        || unsafe { orig::packed_set(env, value, index, slot) },
        || set(packed_key_name(index, slot), i64::from(value as i32)),
    )
}

/// Hooks for `stats_commands::CommandStatGetInt(int, int&, int)`, its
/// float, bool and masked int variants, and the setters. Floats are kept as
/// their bits.
pub(crate) unsafe extern "C" fn get_int(env: *mut Env, hash: u32, out: u64, player: u32) -> u32 {
    if is_cash_hash(hash)
        && let Some(value) = stored(&hash_key(hash))
    {
        // The earn script adds to whatever this returns, then the HUD reads
        // it back. The engine's copy of these stats stays 0 (a member-file
        // load, or a set the engine does not keep); the green "+$25" is the
        // change popup and does not read the balance.
        return write(env, out, value);
    }
    let result = with_stat(hash, || {
        engine_or(|| unsafe { orig::stat_get_int(env, hash, out, player) }, || write(env, out, get(&hash_key(hash))))
    });
    if is_cash_hash(hash) && result != 0 {
        let engine = i64::from(unsafe { Mem::of(env) }.u32(out) as i32);
        let (_value, seed) = resolve_cash(None, Some(engine));
        if let Some(seed) = seed {
            set(hash_key(hash), seed);
            save();
        }
    }
    result
}

pub(crate) unsafe extern "C" fn get_float(env: *mut Env, hash: u32, out: u64, player: u32) -> u32 {
    with_stat(hash, || {
        engine_or(|| unsafe { orig::stat_get_float(env, hash, out, player) }, || write(env, out, get(&hash_key(hash))))
    })
}

pub(crate) unsafe extern "C" fn get_bool(env: *mut Env, hash: u32, out: u64, player: u32) -> u32 {
    with_stat(hash, || {
        engine_or(
            || unsafe { orig::stat_get_bool(env, hash, out, player) },
            || write(env, out, i64::from(get(&hash_key(hash)) != 0)),
        )
    })
}

pub(crate) unsafe extern "C" fn get_masked_int(
    env: *mut Env,
    hash: u32,
    out: u64,
    start: u32,
    bits: u32,
    player: u32,
) -> u32 {
    with_stat(hash, || {
        engine_or(
            || unsafe { orig::stat_get_masked_int(env, hash, out, start, bits, player) },
            || write(env, out, ((get(&hash_key(hash)) as u64 >> (start & 63)) & mask(bits)) as i64),
        )
    })
}

pub(crate) unsafe extern "C" fn set_int(env: *mut Env, hash: u32, value: u32, save_flag: u32) -> u32 {
    let result = with_stat(hash, || {
        engine_or(|| unsafe { orig::stat_set_int(env, hash, value, save_flag) }, || keep(hash, i64::from(value as i32)))
    });
    if is_cash_hash(hash) {
        // Always remember the balance the script just wrote, even when the
        // engine also has an entry and leaves it at 0. `save` puts it in
        // `cloud/extra-stats.txt`, which Drive syncs.
        keep(hash, i64::from(value as i32));
        save();
    }
    result
}

pub(crate) unsafe extern "C" fn set_float(env: *mut Env, hash: u32, value: f32, save_flag: u32) -> u32 {
    with_stat(hash, || {
        engine_or(
            || unsafe { orig::stat_set_float(env, hash, value, save_flag) },
            || keep(hash, i64::from(value.to_bits())),
        )
    })
}

pub(crate) unsafe extern "C" fn set_bool(env: *mut Env, hash: u32, value: u32, save_flag: u32) -> u32 {
    with_stat(hash, || {
        engine_or(|| unsafe { orig::stat_set_bool(env, hash, value, save_flag) }, || keep(hash, i64::from(value != 0)))
    })
}

pub(crate) unsafe extern "C" fn set_masked_int(
    env: *mut Env,
    hash: u32,
    value: u32,
    start: u32,
    bits: u32,
    save: u32,
) -> u32 {
    with_stat(hash, || {
        engine_or(
            || unsafe { orig::stat_set_masked_int(env, hash, value, start, bits, save) },
            || {
                let field = mask(bits) << (start & 63);
                let old = get(&hash_key(hash)) as u64;
                keep(hash, ((old & !field) | ((u64::from(value) << (start & 63)) & field)) as i64)
            },
        )
    })
}

/// The host's answer to a get: the value in `out`, success.
fn write(env: *mut Env, out: u64, value: i64) -> u32 {
    unsafe { Mem::of(env) }.set_u32(out, value as u32);
    1
}

/// The host's answer to a set: kept, success.
fn keep(hash: u32, value: i64) -> u32 {
    set(hash_key(hash), value);
    1
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else { (1 << bits) - 1 }
}

/// Jenkins one-at-a-time, the hash the game uses for stat names.
const fn joaat(name: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    let mut i = 0;
    while i < name.len() {
        let c = name[i];
        let c = if c.is_ascii_uppercase() { c + 32 } else { c };
        hash = hash.wrapping_add(c as u32);
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
        i += 1;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash.wrapping_add(hash << 15)
}

/// Story cash (`SPn_TOTAL_CASH`) and the online wallet and bank. The HUD
/// reads these; the "+$" / "-$" line is a separate change popup.
const CASH_STATS: &[u32] = &[
    joaat(b"SP0_TOTAL_CASH"),
    joaat(b"SP1_TOTAL_CASH"),
    joaat(b"SP2_TOTAL_CASH"),
    joaat(b"MP0_WALLET_BALANCE"),
    joaat(b"MP1_WALLET_BALANCE"),
    joaat(b"MP2_WALLET_BALANCE"),
    joaat(b"MP3_WALLET_BALANCE"),
    joaat(b"MP4_WALLET_BALANCE"),
    joaat(b"MP0_BANK_BALANCE"),
    joaat(b"MP1_BANK_BALANCE"),
    joaat(b"MP2_BANK_BALANCE"),
    joaat(b"MP3_BANK_BALANCE"),
    joaat(b"MP4_BANK_BALANCE"),
    joaat(b"BANK_BALANCE"),
];

fn is_cash_hash(hash: u32) -> bool {
    CASH_STATS.contains(&hash)
}

/// What a cash read should return, and a value to store when the host has
/// none yet. The host wins once a set (or a non-zero engine read) has been
/// remembered, so a later engine read of 0 does not wipe the balance.
fn resolve_cash(stored: Option<i64>, engine: Option<i64>) -> (i64, Option<i64>) {
    if let Some(value) = stored {
        return (value, None);
    }
    match engine {
        Some(value) if value != 0 => (value, Some(value)),
        Some(value) => (value, None),
        None => (0, None),
    }
}

/// Hook for `stats_commands::CommandStatDeleteSlot(int)`: a deleted
/// character's packed stats go too (its stats by hash cannot be told from
/// the others').
pub(crate) unsafe extern "C" fn delete_slot(env: *mut Env, slot: u32) {
    unsafe { orig::stat_delete_slot(env, slot) };
    let prefix = format!("p{}:", slot as i32);
    with_store(|s| {
        let before = s.values.len();
        s.values.retain(|k, _| !k.starts_with(&prefix));
        s.dirty |= s.values.len() != before;
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn masks() {
        assert_eq!(super::mask(8), 0xff);
        assert_eq!(super::mask(64), u64::MAX);
    }

    #[test]
    fn cash_stats_are_the_story_and_online_balances() {
        assert_eq!(super::joaat(b"SP0_TOTAL_CASH"), 0x0324_c31d);
        assert_eq!(super::joaat(b"SP1_TOTAL_CASH"), 0x44bd_6982);
        assert_eq!(super::joaat(b"SP2_TOTAL_CASH"), 0x8d75_047d);
        assert!(super::is_cash_hash(super::joaat(b"MP0_WALLET_BALANCE")));
        assert!(super::is_cash_hash(super::joaat(b"MP1_BANK_BALANCE")));
        assert!(super::is_cash_hash(super::joaat(b"BANK_BALANCE")));
        assert!(!super::is_cash_hash(super::joaat(b"SP0_TOTAL_CASH_EARNED")));
        assert_eq!(super::hash_key(super::joaat(b"SP0_TOTAL_CASH")), "h0324c31d");
    }

    #[test]
    fn a_miss_of_another_stat_does_not_hide_cash() {
        let cash = super::joaat(b"SP0_TOTAL_CASH");
        let mut missing = false;
        missing = super::lookup_missing(true, cash, 0x1111_1111, 0, missing);
        assert!(!missing);
        missing = super::lookup_missing(true, cash, cash, 0, missing);
        assert!(missing);
        missing = super::lookup_missing(true, cash, cash, 1, missing);
        assert!(!missing);
    }

    #[test]
    fn earn_and_spend_stick_when_the_engine_still_reads_zero() {
        assert_eq!(super::resolve_cash(None, None), (0, None));
        assert_eq!(super::resolve_cash(None, Some(0)), (0, None));
        // A real balance already in the engine is remembered.
        assert_eq!(super::resolve_cash(None, Some(500)), (500, Some(500)));
        // +$25 was stored. The engine (and a cloud reload) still say 0.
        assert_eq!(super::resolve_cash(Some(25), Some(0)), (25, None));
        // Spending replaces that balance rather than adding to the engine's 0.
        assert_eq!(super::resolve_cash(Some(60), Some(100)), (60, None));
    }
}

/// This module's hooks, online.
pub(crate) fn hooks(hooks: &mut glue5_game::Hooks) {
    hooks.stat_find = Some(stat_find);
    hooks.packed_key = Some(packed_key);
    hooks.packed_get = Some(packed_get);
    hooks.packed_set = Some(packed_set);
    hooks.stat_get_int = Some(get_int);
    hooks.stat_get_float = Some(get_float);
    hooks.stat_get_bool = Some(get_bool);
    hooks.stat_get_masked_int = Some(get_masked_int);
    hooks.stat_set_int = Some(set_int);
    hooks.stat_set_float = Some(set_float);
    hooks.stat_set_bool = Some(set_bool);
    hooks.stat_set_masked_int = Some(set_masked_int);
    hooks.stat_delete_slot = Some(delete_slot);
}
