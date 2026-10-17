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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    /// Inside [`apply_stored_cash`], so its own stat lookup is not logged as
    /// the HUD's.
    static APPLYING: Cell<bool> = const { Cell::new(false) };
}

/// Stat values by key: `h<hash>` (int, float bits, or bool), `p<slot>:<index>`
/// (packed: a bool or a byte). `cash_zero` is a cash hash whose stored 0 is a
/// real empty wallet (a spend), and the engine balance that 0 replaced, so a
/// later read of that same engine number does not bring the money back.
struct Store {
    values: BTreeMap<String, i64>,
    cash_zero: BTreeMap<u32, i64>,
    dirty: bool,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn file() -> std::path::PathBuf {
    crate::rt().config.user_dir.join("cloud").join("extra-stats.txt")
}

fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut store = STORE.lock().unwrap();
    let store = store.get_or_insert_with(|| {
        let (values, cash_zero) = parse_store(&std::fs::read_to_string(file()).unwrap_or_default());
        Store { values, cash_zero, dirty: false }
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
        let mut text: String = s.values.iter().map(|(k, v)| format!("{k} {v}\n")).collect();
        for (hash, against) in &s.cash_zero {
            text.push_str(&format!("z{hash:08x} {against}\n"));
        }
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
    let id_hash = unsafe { Mem::of(env) }.u32(id);
    // A cash lookup that is not the script command's own. The corner HUD
    // reads the stat in this map; it does not call `CommandStatGetInt`.
    if is_cash_hash(id_hash) && !APPLYING.get() && (!WANTED_ON.get() || WANTED.get() != id_hash) {
        note_cash_lookup(id_hash, false);
    }
    let found = unsafe { orig::stat_find(env, id, iter) };
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

/// `h<hash> <value>` and, for a cash spend down to 0, `z<hash> <against>`.
fn parse_store(text: &str) -> (BTreeMap<String, i64>, BTreeMap<u32, i64>) {
    let mut values = BTreeMap::new();
    let mut cash_zero = BTreeMap::new();
    for line in text.lines() {
        let Some((key, raw)) = line.split_once(' ') else { continue };
        let Ok(value) = raw.parse::<i64>() else { continue };
        if let Some(hex) = key.strip_prefix('z') {
            if let Ok(hash) = u32::from_str_radix(hex, 16) {
                cash_zero.insert(hash, value);
            }
            continue;
        }
        values.insert(key.to_string(), value);
    }
    (values, cash_zero)
}

fn cash_pin(hash: u32) -> Option<i64> {
    with_store(|s| s.cash_zero.get(&hash).copied())
}

fn pin_cash(hash: u32, against: i64) {
    with_store(|s| {
        if s.cash_zero.get(&hash) != Some(&against) {
            s.cash_zero.insert(hash, against);
            s.dirty = true;
        }
    });
}

fn unpin_cash(hash: u32) {
    with_store(|s| {
        if s.cash_zero.remove(&hash).is_some() {
            s.dirty = true;
        }
    });
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
    if is_cash_hash(hash) {
        return unsafe { cash_get(env, hash, out, player) };
    }
    with_stat(hash, || {
        engine_or(|| unsafe { orig::stat_get_int(env, hash, out, player) }, || write(env, out, get(&hash_key(hash))))
    })
}

/// Story cash for scripts (`STAT_GET_INT`). The corner HUD does not call
/// this. It reads the int in `CStatsDataMgr`, which [`apply_stored_cash`]
/// keeps equal to the stored balance. The green "+$" line is only the
/// change. A stored 0 must not hide a balance the story save already loaded.
/// A balance this session has set still wins when the engine reads 0.
unsafe fn cash_get(env: *mut Env, hash: u32, out: u64, player: u32) -> u32 {
    note_cash_lookup(hash, true);
    let stored_v = stored(&hash_key(hash));
    let pinned = cash_pin(hash);
    if pinned.is_none() && stored_v.is_some_and(|value| value != 0) {
        return write(env, out, stored_v.unwrap_or(0));
    }
    let result = with_stat(hash, || {
        engine_or(|| unsafe { orig::stat_get_int(env, hash, out, player) }, || write(env, out, get(&hash_key(hash))))
    });
    let engine = (result != 0).then(|| i64::from(unsafe { Mem::of(env) }.u32(out) as i32));
    let (value, seed) = resolve_cash_get(stored_v, engine, pinned);
    if let Some(seed) = seed {
        log::info!("stats: h{hash:08x} cash is {seed} from the engine (host was {stored_v:?})");
        set(hash_key(hash), seed);
        unpin_cash(hash);
        save();
    }
    write(env, out, value)
}

pub(crate) unsafe extern "C" fn get_float(env: *mut Env, hash: u32, out: u64, player: u32) -> u32 {
    if crate::online::story_market() && is_bawsaq_price(hash) {
        return unsafe { bawsaq_get_float(env, hash, out, player) };
    }
    with_stat(hash, || {
        engine_or(|| unsafe { orig::stat_get_float(env, hash, out, player) }, || write(env, out, get(&hash_key(hash))))
    })
}

/// `STAT_GET_FLOAT` of a BAWSAQ price. The script copies this into the
/// stock's price only while it believes Rockstar's service answered. A 0
/// from the engine is that service missing, not a real quote: the local
/// price stands. A positive value (a stat the game did set) is kept.
unsafe fn bawsaq_get_float(env: *mut Env, hash: u32, out: u64, player: u32) -> u32 {
    let local = hosted_price(hash);
    let result = with_stat(hash, || engine_or(|| unsafe { orig::stat_get_float(env, hash, out, player) }, || 0));
    let engine = if result != 0 { f32::from_bits(unsafe { Mem::of(env) }.u32(out)) } else { 0.0 };
    let value = bawsaq_choose(engine, local);
    if engine > 0.0 {
        set(hash_key(hash), i64::from(engine.to_bits()));
    }
    write(env, out, i64::from(value.to_bits()))
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
    if is_cash_hash(hash) {
        return unsafe { cash_set(env, hash, value, save_flag) };
    }
    with_stat(hash, || {
        engine_or(|| unsafe { orig::stat_set_int(env, hash, value, save_flag) }, || keep(hash, i64::from(value as i32)))
    })
}

/// Remember a story or online cash write. A set of 0 before any balance
/// exists is the startup sync, not a spend: forwarding it would wipe a
/// story-save load or a pickup the engine already counted.
unsafe fn cash_set(env: *mut Env, hash: u32, value: u32, save_flag: u32) -> u32 {
    let value_i = i64::from(value as i32);
    let decision = accept_cash_set(stored(&hash_key(hash)), value_i);
    let result = if decision.forward {
        log::info!("stats: h{hash:08x} set cash to {value_i}");
        with_stat(hash, || engine_or(|| unsafe { orig::stat_set_int(env, hash, value, save_flag) }, || 1))
    } else {
        log::info!("stats: h{hash:08x} ignored cash set of 0");
        1
    };
    if let Some(keep_v) = decision.keep {
        keep(hash, keep_v);
        if let Some(against) = decision.pin_against {
            pin_cash(hash, against);
        } else {
            unpin_cash(hash);
        }
        save();
    }
    if decision.keep.is_some() { 1 } else { result }
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

/// Story cash (`SPn_TOTAL_CASH`) and the online wallet and bank. The corner
/// HUD and the phone's cash copy both follow these ints in the stats map.
/// The "+$" / "-$" line is a separate change popup. Share counts
/// (`AMU_OW0` and the rest) are not here: a 0 written when a BAWSAQ price
/// is missing must not be pinned like a wallet.
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

fn cash_index(hash: u32) -> Option<usize> {
    CASH_STATS.iter().position(|&candidate| candidate == hash)
}

/// First sight of a cash hash, per path. Fourteen stats, two paths, so the
/// bit is `index` or `index + 16`.
static CASH_SEEN: AtomicU32 = AtomicU32::new(0);
static CASH_WAITING: AtomicU32 = AtomicU32::new(0);

fn note_cash_lookup(hash: u32, script_command: bool) {
    let Some(index) = cash_index(hash) else { return };
    debug_assert!(index < 16);
    let bit = 1u32 << (index + if script_command { 16 } else { 0 });
    if CASH_SEEN.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        return;
    }
    if script_command {
        log::info!("stats: h{hash:08x} read by StatGetInt");
    } else {
        log::info!("stats: h{hash:08x} read from the stats map, outside StatGetInt");
    }
}

fn note_cash_waiting(hash: u32) {
    let Some(index) = cash_index(hash) else { return };
    let bit = 1u32 << index;
    if CASH_WAITING.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        return;
    }
    log::info!("stats: h{hash:08x} is in extra-stats but not in the stats map yet");
}

fn log_cash_write(hash: u32, current: i64, desired: i64) {
    static LAST: Mutex<[i64; 16]> = Mutex::new([i64::MIN; 16]);
    let Some(index) = cash_index(hash) else { return };
    let mut last = LAST.lock().unwrap_or_else(|err| err.into_inner());
    if last[index] == desired {
        return;
    }
    last[index] = desired;
    log::info!("stats: h{hash:08x} engine cash is {current}; the HUD reads that stat, writing {desired}");
}

/// `seed` is an engine balance to remember. `write` is the value to put into
/// the engine stat the HUD reads. `None` leaves that stat alone: it already
/// matches, or it is not in the map yet (`engine` is `None`).
fn cash_publish(stored: Option<i64>, engine: Option<i64>, pinned: Option<i64>) -> (Option<i64>, Option<i64>) {
    let (value, seed) = resolve_cash_get(stored, engine, pinned);
    let write = match engine {
        Some(current) if current != value => Some(value),
        _ => None,
    };
    (seed, write)
}

/// A word in the game heap for `CommandStatGetInt`'s out parameter.
fn cash_word(env: *mut Env) -> u64 {
    static SLOT: AtomicU64 = AtomicU64::new(0);
    let existing = SLOT.load(Ordering::Relaxed);
    if existing != 0 {
        return existing;
    }
    let allocated = unsafe { orig::builtin_malloc(env, 4) };
    if allocated != 0 {
        let _ = SLOT.compare_exchange(0, allocated, Ordering::Relaxed, Ordering::Relaxed);
    }
    SLOT.load(Ordering::Relaxed)
}

struct ClearApply;

impl Drop for ClearApply {
    fn drop(&mut self) {
        APPLYING.set(false);
    }
}

/// Writes stored cash into the stats-map ints the corner HUD reads.
///
/// `CommandStatGetInt` is the script wrapper. Returning the stored balance
/// from it changes what a script receives and does not change the int the
/// HUD already has, so a hand-edited `extra-stats.txt` stayed invisible and
/// the file was never rewritten. The phone's cash is a script copy of this
/// same int (`Global_61482`), refreshed from it while signed in. Shares,
/// the price paid and which stock are the story save's script block, not a
/// stat.
pub(crate) unsafe fn apply_stored_cash(env: *mut Env) {
    if APPLYING.get() {
        return;
    }
    APPLYING.set(true);
    let _guard = ClearApply;
    let out = cash_word(env);
    if out == 0 {
        return;
    }
    let mem = unsafe { Mem::of(env) };
    for &hash in CASH_STATS {
        let stored_v = stored(&hash_key(hash));
        let pinned = cash_pin(hash);
        if stored_v.is_none() && pinned.is_none() {
            continue;
        }
        let read = unsafe { orig::stat_get_int(env, hash, out, u32::MAX) };
        let engine = (read != 0).then(|| i64::from(mem.u32(out) as i32));
        if engine.is_none() && stored_v.is_some_and(|value| value != 0) {
            note_cash_waiting(hash);
        }
        let (seed, write) = cash_publish(stored_v, engine, pinned);
        if let Some(seed) = seed {
            log::info!("stats: h{hash:08x} cash is {seed} from the engine (host was {stored_v:?})");
            set(hash_key(hash), seed);
            unpin_cash(hash);
            save();
        }
        if let Some(value) = write {
            log_cash_write(hash, engine.unwrap_or(0), value);
            // Save flag 0: the host file is the copy that survives a restart.
            // Setting it every frame a load resets the int would queue a save
            // every frame. The in-memory int is what the HUD reads.
            unsafe { orig::stat_set_int(env, hash, value as i32 as u32, 0) };
        }
    }
}

/// What a cash read should return, and a value to store when the engine's
/// number should replace the host's. A remembered earn wins over an engine
/// that still reads 0. A stored 0 does not, unless `pinned_against` says
/// that 0 was a spend of that engine balance.
fn resolve_cash_get(stored: Option<i64>, engine: Option<i64>, pinned_against: Option<i64>) -> (i64, Option<i64>) {
    if let Some(against) = pinned_against {
        return match engine {
            Some(value) if value != 0 && value != against => (value, Some(value)),
            _ => (0, None),
        };
    }
    if let Some(value) = stored.filter(|value| *value != 0) {
        return (value, None);
    }
    match engine {
        Some(value) if value != 0 => (value, Some(value)),
        _ => (0, None),
    }
}

/// A cash set. `forward` calls the engine. `keep` is what to store. `pin_against`
/// is the previous balance a new 0 must keep hiding.
struct CashSet {
    forward: bool,
    keep: Option<i64>,
    pin_against: Option<i64>,
}

fn accept_cash_set(stored: Option<i64>, value: i64) -> CashSet {
    if value != 0 {
        CashSet { forward: true, keep: Some(value), pin_against: None }
    } else if let Some(prev) = stored.filter(|prev| *prev != 0) {
        CashSet { forward: true, keep: Some(0), pin_against: Some(prev) }
    } else {
        CashSet { forward: false, keep: None, pin_against: None }
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

/// BAWSAQ price stats, in the order `stock_controller` writes into
/// `Global_59488`. Slot 32 is `SM_PRICE_MOL`, which is Betta Pharmaceuticals
/// (BET, stock 43): Lester's missions multiply that price in the script.
/// LCN does not read these. Share counts (`MOL_OW0` and the rest) are not
/// in this list and are not cash.
const BAWSAQ_PRICES: &[u32] = &[
    joaat(b"SM_PRICE_AMU"),
    joaat(b"SM_PRICE_BDG"),
    joaat(b"SM_PRICE_BFA"),
    joaat(b"SM_PRICE_BIN"),
    joaat(b"SM_PRICE_BTR"),
    joaat(b"SM_PRICE_BLE"),
    joaat(b"SM_PRICE_BRU"),
    joaat(b"SM_PRICE_CNT"),
    joaat(b"SM_PRICE_CRE"),
    joaat(b"SM_PRICE_DGP"),
    joaat(b"SM_PRICE_WAP"),
    joaat(b"SM_PRICE_FAC"),
    joaat(b"SM_PRICE_FRT"),
    joaat(b"SM_PRICE_LSC"),
    joaat(b"SM_PRICE_LST"),
    joaat(b"SM_PRICE_LTD"),
    joaat(b"SM_PRICE_MAI"),
    joaat(b"SM_PRICE_PKW"),
    joaat(b"SM_PRICE_PIS"),
    joaat(b"SM_PRICE_PON"),
    joaat(b"SM_PRICE_RON"),
    joaat(b"SM_PRICE_SHT"),
    joaat(b"SM_PRICE_SPU"),
    joaat(b"SM_PRICE_TNK"),
    joaat(b"SM_PRICE_WIW"),
    joaat(b"SM_PRICE_UMA"),
    joaat(b"SM_PRICE_VAP"),
    joaat(b"SM_PRICE_VOM"),
    joaat(b"SM_PRICE_WZL"),
    joaat(b"SM_PRICE_WIZ"),
    joaat(b"SM_PRICE_ZIT"),
    joaat(b"SM_PRICE_SHK"),
    joaat(b"SM_PRICE_MOL"),
    joaat(b"SM_PRICE_PMP"),
    joaat(b"SM_PRICE_GOT"),
    joaat(b"SM_PRICE_EYE"),
    joaat(b"SM_PRICE_HVY"),
    joaat(b"SM_PRICE_SHR"),
    joaat(b"SM_PRICE_HAL"),
];

fn is_bawsaq_price(hash: u32) -> bool {
    BAWSAQ_PRICES.contains(&hash)
}

/// A positive engine quote wins. Zero is the offline service, so the local
/// price is what the phone trades at. The script multiplies that price by
/// the mission factor itself (BET after Lester); this does not.
fn bawsaq_choose(engine: f32, local: f32) -> f32 {
    if engine > 0.0 { engine } else { local }
}

/// Deterministic opening price, 15.00 to 289.99, the band LCN starts in.
fn bawsaq_seed(hash: u32) -> f32 {
    let n = hash.wrapping_mul(0x9E37_79B1);
    let cents = (n % 27_500) + 1_500;
    (cents as f32) / 100.0
}

/// One step of the local walk, kept inside the band the script clamps to.
fn bawsaq_step_price(hash: u32, price: f32, step: u32) -> f32 {
    let n = hash.wrapping_mul(0x85EB_CA6B).wrapping_add(step.wrapping_mul(0xC2B2_AE35));
    let factor = 0.97 + ((n % 601) as f32) / 10_000.0;
    let next = (price * factor).clamp(3.5, 290.0);
    (next * 100.0).floor() / 100.0
}

fn hosted_price(hash: u32) -> f32 {
    tick_bawsaq();
    with_store(|s| {
        let key = hash_key(hash);
        if let Some(&bits) = s.values.get(&key) {
            let price = f32::from_bits(bits as u32);
            if price > 0.0 && price.is_finite() {
                return price;
            }
        }
        let price = bawsaq_seed(hash);
        s.values.insert(key, i64::from(price.to_bits()));
        s.dirty = true;
        price
    })
}

/// Moves every saved BAWSAQ price at most once per two minutes, which is
/// how often the phone asks the service. The first call only starts the
/// clock, so a launch does not jump the quote before it has been shown.
fn tick_bawsaq() {
    static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
    let mut next = NEXT.lock().unwrap_or_else(|err| err.into_inner());
    let now = Instant::now();
    if next.is_none() {
        *next = Some(now + Duration::from_secs(120));
        return;
    }
    if now < next.unwrap() {
        return;
    }
    *next = Some(now + Duration::from_secs(120));
    drop(next);
    with_store(|s| {
        let step = s.values.get("bawsaq_step").copied().unwrap_or(0).wrapping_add(1);
        let mut any = false;
        for &hash in BAWSAQ_PRICES {
            let key = hash_key(hash);
            let Some(&bits) = s.values.get(&key) else { continue };
            let price = f32::from_bits(bits as u32);
            if !(price > 0.0 && price.is_finite()) {
                continue;
            }
            let stepped = bawsaq_step_price(hash, price, step as u32);
            s.values.insert(key, i64::from(stepped.to_bits()));
            any = true;
        }
        if any {
            s.values.insert("bawsaq_step".into(), step);
            s.dirty = true;
        }
    });
}

/// `Global_56607`, 80 stocks, 36 ints each. `f_8` is the BAWSAQ flag.
const STOCK_GLOBAL: u64 = 56_607;
const STOCK_COUNT: u64 = 80;
const STOCK_STRIDE: u64 = 36 * 4;
const FIELD_EXCHANGE: u64 = 8 * 4;
const FIELD_PRICE: u64 = 9 * 4;
const FIELD_CHART: u64 = 14 * 4;
const FIELD_HIGH: u64 = 31 * 4;
const FIELD_LOW: u64 = 32 * 4;
const FIELD_SLOT: u64 = 35 * 4;
/// Set while the phone thinks BAWSAQ failed to download.
const BAWSAQ_DOWN: u64 = 64_293;

static STOCK_BASE: AtomicU64 = AtomicU64::new(0);
static STOCK_FRAME: AtomicU32 = AtomicU32::new(0);
static STOCK_LOGGED: AtomicBool = AtomicBool::new(false);

fn read_u32_at(mem: &[u8], at: usize) -> Option<u32> {
    let bytes = mem.get(at..at + 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// The live stock table: the names are 144 bytes apart, BET is index 43,
/// and the exchange flag matches the script (LCN is 0, BAWSAQ is 1).
fn find_stock_table(mem: &[u8]) -> Option<u64> {
    let bet = b"BSS_BSTR_85\0";
    let next = b"BSS_BSTR_87\0";
    let first = b"BSS_BSTR_0\0";
    let ticker = b"BSS_BSTR_86\0";
    let bet_at = 43 * STOCK_STRIDE as usize;
    let mut i = 0;
    while i + bet.len() <= mem.len() {
        if mem[i..].starts_with(bet) {
            let Some(base) = i.checked_sub(bet_at) else {
                i += 4;
                continue;
            };
            let next_at = base + 44 * STOCK_STRIDE as usize;
            let tick_at = base + bet_at + 16;
            let names_ok = mem.get(base..base + first.len()) == Some(first.as_slice())
                && mem.get(next_at..next_at + next.len()) == Some(next.as_slice())
                && mem.get(tick_at..tick_at + ticker.len()) == Some(ticker.as_slice());
            let bawsaq = read_u32_at(mem, base + bet_at + FIELD_EXCHANGE as usize) == Some(1);
            let lcn = read_u32_at(mem, base + 2 * STOCK_STRIDE as usize + FIELD_EXCHANGE as usize) == Some(0);
            if names_ok && bawsaq && lcn {
                return Some(base as u64);
            }
        }
        i += 4;
    }
    None
}

fn stock_table() -> Option<u64> {
    let known = STOCK_BASE.load(Ordering::Relaxed);
    if known != 0 {
        return Some(known);
    }
    let frame = STOCK_FRAME.fetch_add(1, Ordering::Relaxed);
    // A full heap walk is not a per-frame cost. These updates cover the
    // stock script starting after the first frames.
    const AT: [u32; 7] = [0, 120, 300, 600, 1_200, 2_400, 4_800];
    if !AT.contains(&frame) {
        return None;
    }
    let mem = crate::rt().memory.mem();
    let size = crate::rt().memory.size();
    let found = find_stock_table(mem.bytes(0, size as usize));
    if let Some(base) = found {
        STOCK_BASE.store(base, Ordering::Relaxed);
        if !STOCK_LOGGED.swap(true, Ordering::Relaxed) {
            log::info!("bawsaq: story stock table at {base:#x}; prices are local");
        }
    } else if frame == 4_800 {
        log::warn!("bawsaq: the story stock table was not found; prices stay with the script");
    }
    found
}

fn put_f32(mem: Mem, addr: u64, value: f32) {
    let bits = value.to_bits();
    if mem.u32(addr) != bits {
        mem.set_u32(addr, bits);
    }
}

/// Writes the local BAWSAQ quote into the script stock the phone reads.
/// LCN (`f_8 == 0`) is left alone: its price is the script's own walk and
/// is already stored in the story save. Holdings (which stock, shares,
/// dollars paid) are that same save, on iOS and macOS, and Drive syncs it.
///
/// The phone shows and trades `price * mission multiplier`. The multiplier
/// (BET after an assassination) lives in the script, so this only has to
/// keep a positive base price. The "BAWSAQ is down" flag is cleared with
/// it: a failed download sets the flag and nothing in the script clears it
/// again when the download never happened.
pub(crate) unsafe fn publish_bawsaq(env: *mut Env) {
    if !crate::online::story_market() {
        return;
    }
    let Some(base) = stock_table() else {
        return;
    };
    let mem = unsafe { Mem::of(env) };
    let size = crate::rt().memory.size();
    let table_end = base + STOCK_COUNT * STOCK_STRIDE;
    if table_end > size {
        return;
    }
    for i in 0..STOCK_COUNT {
        let stock = base + i * STOCK_STRIDE;
        if mem.u32(stock + FIELD_EXCHANGE) == 0 {
            continue;
        }
        let slot = mem.u32(stock + FIELD_SLOT) as usize;
        let Some(&hash) = BAWSAQ_PRICES.get(slot) else { continue };
        let price = hosted_price(hash);
        put_f32(mem, stock + FIELD_PRICE, price);
        for n in 0..16u64 {
            let at = stock + FIELD_CHART + n * 4;
            if mem.u32(at) == 0 {
                put_f32(mem, at, price);
            }
        }
        let high = f32::from_bits(mem.u32(stock + FIELD_HIGH));
        if !(high > 0.0) || high < price {
            put_f32(mem, stock + FIELD_HIGH, price);
        }
        let low = f32::from_bits(mem.u32(stock + FIELD_LOW));
        if !(low > 0.0) || low > price {
            put_f32(mem, stock + FIELD_LOW, price);
        }
    }
    if let Some(globals) = base.checked_sub(STOCK_GLOBAL * 4) {
        let flag = globals + BAWSAQ_DOWN * 4;
        // Only a 0/1 flag. A different script layout must not have some
        // other global cleared just because this index moved.
        if flag + 4 <= size && mem.u32(flag) == 1 {
            mem.set_u32(flag, 0);
        }
    }
    save();
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
        assert_eq!(super::resolve_cash_get(None, None, None), (0, None));
        assert_eq!(super::resolve_cash_get(None, Some(0), None), (0, None));
        // A real balance already in the engine (the story save) is remembered.
        assert_eq!(super::resolve_cash_get(None, Some(500), None), (500, Some(500)));
        // +$25 was stored. The engine (and a cloud reload) still say 0.
        assert_eq!(super::resolve_cash_get(Some(25), Some(0), None), (25, None));
        // Spending to a lower total replaces the balance. The engine's old
        // number does not come back.
        assert_eq!(super::resolve_cash_get(Some(60), Some(100), None), (60, None));
    }

    #[test]
    fn a_stored_zero_does_not_hide_franklin_earning() {
        // The story session's extra-stats.txt. SP1_TOTAL_CASH is h44bd6982.
        // The two fives are not cash stats.
        let text = "h0324c31d 0\nh29d38e8a 5\nh44bd6982 0\nh5f89566d 5\nh8d75047d 0\n";
        let (values, pins) = super::parse_store(text);
        assert!(pins.is_empty());
        assert_eq!(values["h44bd6982"], 0);
        assert!(!super::is_cash_hash(0x29d3_8e8a));
        assert!(!super::is_cash_hash(0x5f89_566d));
        let franklin = 0x44bd_6982;
        assert_eq!(super::joaat(b"SP1_TOTAL_CASH"), franklin);
        // That stored 0 yields to money the engine already has.
        assert_eq!(super::resolve_cash_get(Some(0), Some(200), None), (200, Some(200)));
        // A startup set of 0 is not stored and is not written through,
        // so it cannot wipe the earn.
        let startup = super::accept_cash_set(None, 0);
        assert!(!startup.forward);
        assert_eq!(startup.keep, None);
        let again = super::accept_cash_set(Some(0), 0);
        assert!(!again.forward);
        // The earn script adds, then sets the new total.
        let earn = super::accept_cash_set(Some(0), 200);
        assert!(earn.forward);
        assert_eq!(earn.keep, Some(200));
        assert_eq!(earn.pin_against, None);
        assert_eq!(super::resolve_cash_get(Some(200), Some(0), None), (200, None));
        // Spending the last dollar stays empty even if the engine still
        // reads the old total, until the engine moves to a different number.
        let spent = super::accept_cash_set(Some(200), 0);
        assert!(spent.forward);
        assert_eq!(spent.keep, Some(0));
        assert_eq!(spent.pin_against, Some(200));
        assert_eq!(super::resolve_cash_get(Some(0), Some(200), Some(200)), (0, None));
        assert_eq!(super::resolve_cash_get(Some(0), Some(40), Some(200)), (40, Some(40)));
    }

    #[test]
    fn a_stock_trade_changes_cash_and_share_counts_are_not_cash() {
        // The phone spends `SPn_TOTAL_CASH`: $500, buy $120, then sell for $200.
        let bought = super::accept_cash_set(Some(500), 380);
        assert!(bought.forward);
        assert_eq!(bought.keep, Some(380));
        assert_eq!(bought.pin_against, None);
        let sold = super::accept_cash_set(Some(380), 580);
        assert!(sold.forward);
        assert_eq!(sold.keep, Some(580));
        // Shares owned are `AMU_OW0` and the rest, one stat per stock per
        // character. They mirror the story save; a 0 written when a BAWSAQ
        // price is missing must not be treated as spending the wallet.
        assert_eq!(super::joaat(b"AMU_OW0"), 0x8248_4e3b);
        assert_eq!(super::joaat(b"AMU_OW1"), 0x7109_abbe);
        assert_eq!(super::joaat(b"AMU_OW2"), 0x1e94_06d4);
        assert_eq!(super::joaat(b"SM_PRICE_AMU"), 0xaf1b_8cbd);
        assert!(!super::is_cash_hash(super::joaat(b"AMU_OW0")));
        assert!(!super::is_cash_hash(super::joaat(b"HAL_OW2")));
        assert!(!super::is_cash_hash(super::joaat(b"SM_PRICE_AMU")));
    }

    #[test]
    fn stored_cash_is_written_into_the_engine_stat_the_hud_reads() {
        // The file from the macOS run: each SPn_TOTAL_CASH is 100000, and
        // the HUD stayed at $0 because that file is not what it reads.
        assert!(super::CASH_STATS.len() <= 16);
        assert_eq!(super::cash_publish(Some(100_000), Some(0), None), (None, Some(100_000)));
        assert_eq!(super::cash_publish(Some(100_000), Some(100_000), None), (None, None));
        // Not in the map yet: leave it, and try again next update.
        assert_eq!(super::cash_publish(Some(100_000), None, None), (None, None));
        // A stored 0 still yields to a balance the story save loaded.
        assert_eq!(super::cash_publish(Some(0), Some(200), None), (Some(200), None));
        // Spending the last dollar has to clear the engine stat too.
        assert_eq!(super::cash_publish(Some(0), Some(200), Some(200)), (None, Some(0)));
        // Franklin, the character that run loaded.
        assert_eq!(super::joaat(b"SP1_TOTAL_CASH"), 0x44bd_6982);
        assert!(!super::is_cash_hash(super::joaat(b"AMU_OW1")));
    }

    #[test]
    fn bawsaq_bet_has_a_local_price_and_shares_stay_shares() {
        // BET's community price is SM_PRICE_MOL (slot 32), not a cash stat
        // and not the share mirror. LCN never reads this list.
        let bet = super::joaat(b"SM_PRICE_MOL");
        assert_eq!(super::BAWSAQ_PRICES.len(), 39);
        assert_eq!(super::BAWSAQ_PRICES[32], bet);
        assert!(super::is_bawsaq_price(bet));
        assert!(super::is_bawsaq_price(super::joaat(b"SM_PRICE_AMU")));
        assert!(!super::is_bawsaq_price(super::joaat(b"MOL_OW0")));
        assert!(!super::is_bawsaq_price(super::joaat(b"MOL_OW2")));
        assert!(!super::is_cash_hash(bet));
        assert!(!super::is_bawsaq_price(super::joaat(b"SP1_TOTAL_CASH")));
        let seed = super::bawsaq_seed(bet);
        assert!((15.0..=290.0).contains(&seed));
        assert_eq!(super::bawsaq_seed(bet), seed);
        let mut price = seed;
        for step in 1..8 {
            price = super::bawsaq_step_price(bet, price, step);
            assert!((3.5..=290.0).contains(&price), "{price}");
        }
        // A quote the engine already stored wins. Zero is the missing service.
        assert_eq!(super::bawsaq_choose(80.0, seed), 80.0);
        assert_eq!(super::bawsaq_choose(0.0, seed), seed);
        assert_eq!(super::bawsaq_choose(-1.0, seed), seed);
        // extra-stats.txt keeps the float bits. Drive syncs that file.
        let key = super::hash_key(bet);
        let text = format!("{key} {}\nbawsaq_step 3\n", i64::from(seed.to_bits()));
        let (values, pins) = super::parse_store(&text);
        assert!(pins.is_empty());
        assert_eq!(f32::from_bits(values[&key] as u32), seed);
        assert_eq!(values["bawsaq_step"], 3);
    }

    #[test]
    fn the_stock_table_scanner_finds_bet_and_ignores_a_string_table() {
        let stride = super::STOCK_STRIDE as usize;
        let mut table = vec![0u8; stride * super::STOCK_COUNT as usize];
        let put = |table: &mut [u8], at: usize, text: &[u8]| {
            table[at..at + text.len()].copy_from_slice(text);
        };
        put(&mut table, 0, b"BSS_BSTR_0\0");
        put(&mut table, 43 * stride, b"BSS_BSTR_85\0");
        put(&mut table, 43 * stride + 16, b"BSS_BSTR_86\0");
        put(&mut table, 44 * stride, b"BSS_BSTR_87\0");
        // Stock 2 is LCN. Stock 43 (BET) is BAWSAQ.
        table[2 * stride + 32..2 * stride + 36].copy_from_slice(&0u32.to_le_bytes());
        table[43 * stride + 32..43 * stride + 36].copy_from_slice(&1u32.to_le_bytes());
        let mut hay = vec![0u8; 64];
        hay.extend_from_slice(&table);
        assert_eq!(super::find_stock_table(&hay), Some(64));
        // The script's own string literals sit next to each other, not one
        // stock apart, and they have no exchange flag.
        let packed = b"BSS_BSTR_0\0BSS_BSTR_85\0BSS_BSTR_86\0BSS_BSTR_87\0";
        assert_eq!(super::find_stock_table(packed), None);
        table[43 * stride + 32..43 * stride + 36].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(super::find_stock_table(&table), None);
    }
}

/// Stat commands for story and online. Story cash never reached these when
/// they were installed only with GTA Online.
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
