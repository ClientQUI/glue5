//! Rockstar's cloud, where GTA Online keeps the player's characters: files
//! in the user directory instead (`cloud/`).
//!
//! The engine saves and loads the online stats only with Rockstar's
//! services: with `-lan` (online.rs) it skips both. In its stats saves
//! manager `-lan` counts as unset, and the member files they post and get
//! go to and come from the disk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use glue5_game::{Env, orig};

use crate::memory::Mem;

/// `PARAM_lan`'s value (non-null: set).
const PARAM_LAN: u64 = 13346752;
/// `fiDeviceGrowBuffer`, the device of files read into memory: its handles
/// are `datGrowBuffer`s.
const GROW_BUFFER_DEVICE: u64 = 8638336;
/// The stats' slots whose load failed, a bit each (the scripts and the
/// saves give up on such a slot).
const LOAD_FAILED: u64 = 25461316;
/// Slots without a file yet: a new player's (or a new character's),
/// loaded empty rather than failed.
static NEW_SLOTS: AtomicU32 = AtomicU32::new(0);
/// A suspend asked the engine thread to queue a stats save. The generation
/// is what was asked; `SAVE_DONE` is the newest generation `stats_update`
/// has already queued and then finished.
static SAVE_REQUEST: AtomicU64 = AtomicU64::new(0);
static SAVE_DONE: AtomicU64 = AtomicU64::new(0);

/// Asks the engine thread to queue a stats save on its next update.
/// The returned generation is done once [`stats_save_done`] is true.
pub(crate) fn request_stats_save() -> u64 {
    SAVE_REQUEST.fetch_add(1, Ordering::AcqRel) + 1
}

pub(crate) fn stats_save_done(generation: u64) -> bool {
    SAVE_DONE.load(Ordering::Acquire) >= generation
}
/// `netStatus`: its code, then a result code.
const NET_STATUS_FAILED: u32 = 2;
const NET_STATUS_SUCCEEDED: u32 = 3;

fn without_lan<R>(env: *mut Env, f: impl FnOnce() -> R) -> R {
    let m = unsafe { Mem::of(env) };
    let lan = m.u64(PARAM_LAN);
    m.set_u64(PARAM_LAN, 0);
    let r = f();
    m.set_u64(PARAM_LAN, lan);
    r
}

/// Hook for `rage::parElement::FindAttributeBoolValue(char const*, bool,
/// bool) const`: no stat's value comes from Rockstar's servers (the stats
/// files' "server authoritative" ones, the character's team among them, are
/// not in the character's files otherwise).
pub(crate) unsafe extern "C" fn xml_bool_attribute(env: *mut Env, element: u64, name: u64, default: u32) -> u32 {
    /// The literal "ServerAuthoritative", as `CStatsDataMgr::LoadDataXMLFile`
    /// asks for it.
    const SERVER_AUTHORITATIVE: u64 = 1596083;
    if name == SERVER_AUTHORITATIVE {
        return 0;
    }
    unsafe { orig::xml_bool_attribute(env, element, name, default) }
}

/// Hooks for the stats saves manager's `Init(unsigned int)`, `Save(...)`,
/// `Load(eStatsLoadType, int)`, `QueueSave(...)`, `QueueLoad(...)` and
/// `CStatsDataMgr::Update()` (which finishes the saves and loads): each
/// gives up with `-lan`.
pub(crate) unsafe extern "C" fn stats_init(env: *mut Env, mode: u32) {
    without_lan(env, || unsafe { orig::stats_init(env, mode) })
}

pub(crate) unsafe extern "C" fn stats_save(env: *mut Env, kind: u32, slot: u32, save_type: u32, reason: u32) -> u32 {
    without_lan(env, || unsafe { orig::stats_save(env, kind, slot, save_type, reason) })
}

pub(crate) unsafe extern "C" fn stats_load(env: *mut Env, kind: u32) -> u32 {
    without_lan(env, || unsafe { orig::stats_load(env, kind) })
}

pub(crate) unsafe extern "C" fn stats_queue_save(env: *mut Env) {
    without_lan(env, || unsafe { orig::stats_queue_save(env) })
}

pub(crate) unsafe extern "C" fn stats_queue_load(env: *mut Env) -> u32 {
    without_lan(env, || unsafe { orig::stats_queue_load(env) })
}

/// A slot without a file is a new one, not a failed one (`stats_load_failed`).
///
/// A suspend asks, from another thread, that this update queue a stats save
/// first. Money is a stat: `QueueSave` is the save the engine already has,
/// and this update is what finishes writing it.
pub(crate) unsafe extern "C" fn stats_update(env: *mut Env) {
    let pending = SAVE_REQUEST.load(Ordering::Acquire);
    let done = SAVE_DONE.load(Ordering::Acquire);
    if pending > done {
        log::info!("stats: saving on suspend");
        stats_queue_save(env);
    }
    without_lan(env, || unsafe { orig::stats_update(env) });
    if pending > done {
        // Cash lives in extra-stats.txt as well as the engine's member file.
        // The member file can load back as 0; this file is what Drive syncs.
        crate::stats::save();
        SAVE_DONE.fetch_max(pending, Ordering::Release);
    }
    let new = NEW_SLOTS.load(Ordering::Relaxed);
    if new != 0 {
        let m = unsafe { Mem::of(env) };
        m.set_u32(LOAD_FAILED, m.u32(LOAD_FAILED) & !new);
    }
}

/// Hook for `StatsInterface::CloudFileLoadFailed(int)`, what the scripts
/// ask: a slot without a file did not fail.
pub(crate) unsafe extern "C" fn stats_load_failed(env: *mut Env, slot: u32) -> u32 {
    let failed = unsafe { orig::stats_load_failed(env, slot) };
    let new = slot < 32 && NEW_SLOTS.load(Ordering::Relaxed) & (1 << slot) != 0;
    if new { 0 } else { failed }
}

/// Hook for `NetworkInterface::IsCloudAvailable(int)`, which the stats'
/// loads and saves ask first: the disk is.
pub(crate) unsafe extern "C" fn cloud_available(_env: *mut Env, _gamer: u32) -> u32 {
    1
}

thread_local! {
    /// In an online stats save or load.
    static IN_STATS_OPERATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn in_stats_operation<R>(f: impl FnOnce() -> R) -> R {
    IN_STATS_OPERATION.set(true);
    let r = f();
    IN_STATS_OPERATION.set(false);
    r
}

/// Hooks for `CSavegameQueuedOperation_MPStats_Load::Update()` and
/// `..._Save::Update()`: they need Rockstar's credentials (see
/// `ros_credentials_valid`).
pub(crate) unsafe extern "C" fn mpstats_load_update(env: *mut Env, op: u64) -> u32 {
    in_stats_operation(|| unsafe { orig::mpstats_load_update(env, op) })
}

pub(crate) unsafe extern "C" fn mpstats_save_update(env: *mut Env, op: u64) -> u32 {
    in_stats_operation(|| unsafe { orig::mpstats_save_update(env, op) })
}

/// Hook for `rage::rlRosCredentials::IsValid() const`: valid in the stats'
/// saves and loads, which go to the disk; elsewhere the engine would talk
/// to Rockstar's services with them.
pub(crate) unsafe extern "C" fn ros_credentials_valid(env: *mut Env, credentials: u64) -> u32 {
    if IN_STATS_OPERATION.get() {
        return 1;
    }
    unsafe { orig::ros_credentials_valid(env, credentials) }
}

/// Hooks for `CProfileStats::SynchronizedFailed(eSyncType, int&) const`
/// and `Synchronized(eSyncType) const`: the profile stats (kept on Rockstar's servers, there are none
/// here) count as synchronized.
pub(crate) unsafe extern "C" fn profile_stats_failed(_env: *mut Env, _code: u64) -> u32 {
    0
}

pub(crate) unsafe extern "C" fn profile_stats_synced(_env: *mut Env, _kind: u32) -> u32 {
    1
}

/// A stats file's slot: `..._default0000.save` is 0, `..._char0001.save` 1.
fn slot(path: &str) -> Option<u32> {
    let stem = path.strip_suffix(".save")?;
    let digits = stem.len() - stem.bytes().rev().take_while(u8::is_ascii_digit).count();
    stem[digits..].parse().ok().filter(|&s: &u32| s < 32)
}

/// Where a member file is kept.
fn file(path: &str) -> PathBuf {
    let name: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' })
        .collect();
    crate::rt().config.user_dir.join("cloud").join(name)
}

/// HTTP's "Not Found": no such file yet (a new player).
const NOT_FOUND: u32 = 404;

fn finish(m: Mem, status: u64, code: u32) {
    finish_with(m, status, code, 0);
}

fn finish_with(m: Mem, status: u64, code: u32, result: u32) {
    if status != 0 {
        m.set_u32(status + 4, result);
        m.set_u32(status, code);
    }
}

/// Hook for `rage::rlCloud::PostMemberFile(int, rlCloudOnlineService, char
/// const*, void const*, unsigned int, PostType, rlCloudFileInfo*,
/// sysMemAllocator*, netStatus*)` (the post type is not passed in this
/// build).
pub(crate) unsafe extern "C" fn post_member_file(
    env: *mut Env,
    _gamer: u32,
    _service: u32,
    path: u64,
    data: u64,
    size: u32,
    _info: u64,
    _allocator: u64,
    status: u64,
) -> u32 {
    let m = unsafe { Mem::of(env) };
    let name = m.str(path);
    let to = file(&name);
    let written = crate::vfs::durable_write(&to, m.bytes(data, size as usize));
    match written {
        Ok(()) => {
            log::info!("cloud: saved {name} ({size} bytes)");
            crate::stats::save();
            crate::drive::note_write();
            if let Some(slot) = slot(&name) {
                NEW_SLOTS.fetch_and(!(1 << slot), Ordering::Relaxed);
            }
            finish(m, status, NET_STATUS_SUCCEEDED);
        }
        Err(e) => {
            log::error!("cloud: saving {name} to {}: {e}", to.display());
            finish(m, status, NET_STATUS_FAILED);
        }
    }
    1
}

/// Hook for `rage::rlCloud::GetMemberFile(int, rlCloudMemberId const&,
/// rlCloudOnlineService, char const*, rlRosSecurityFlags, unsigned long,
/// netHttpOptions, fiDevice const*, fiHandle*, rlCloudFileInfo*,
/// sysMemAllocator*, netStatus*)`: files read into memory only (the stats').
pub(crate) unsafe extern "C" fn get_member_file(
    env: *mut Env,
    _gamer: u32,
    _member: u64,
    _service: u32,
    path: u64,
    _security: u32,
    _since: u64,
    _options: u32,
    device: u64,
    handle: u64,
    _info: u64,
    _allocator: u64,
    status: u64,
) -> u32 {
    let m = unsafe { Mem::of(env) };
    let name = m.str(path);
    let data = match std::fs::read(file(&name)) {
        Ok(data) if device == GROW_BUFFER_DEVICE => data,
        Ok(_) => {
            log::warn!("cloud: {name} asked for into device {device:#x}");
            finish(m, status, NET_STATUS_FAILED);
            return 1;
        }
        Err(_) => {
            log::info!("cloud: no {name} yet");
            if let Some(slot) = slot(&name) {
                NEW_SLOTS.fetch_or(1 << slot, Ordering::Relaxed);
            }
            finish_with(m, status, NET_STATUS_FAILED, NOT_FOUND);
            return 1;
        }
    };
    let at = unsafe { orig::builtin_malloc(env, data.len() as u64) };
    if at == 0 {
        finish(m, status, NET_STATUS_FAILED);
        return 1;
    }
    m.bytes_mut(at, data.len()).copy_from_slice(&data);
    let appended = unsafe { orig::grow_buffer_append(env, handle, at, data.len() as u32) };
    unsafe { orig::builtin_free(env, at) };
    log::info!("cloud: loaded {name} ({} bytes)", data.len());
    finish(m, status, if appended != 0 { NET_STATUS_SUCCEEDED } else { NET_STATUS_FAILED });
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn slots() {
        assert_eq!(super::slot("GTA5/saves/mpstats/save_default0000.save"), Some(0));
        assert_eq!(super::slot("GTA5/saves/mpstats/save_char0002.save"), Some(2));
        assert_eq!(super::slot("GTA5/saves/mpstats/save.save"), None);
    }

    #[test]
    fn a_suspend_save_stays_open_until_that_generation_finishes() {
        use std::sync::atomic::Ordering;
        let generation = super::request_stats_save();
        assert!(!super::stats_save_done(generation));
        super::SAVE_DONE.fetch_max(generation, Ordering::Release);
        assert!(super::stats_save_done(generation));
        let newer = super::request_stats_save();
        assert!(newer > generation);
        assert!(!super::stats_save_done(newer));
    }
}

/// Hook for `rage::rlUgc::QueryContent(int, rlUgcContentType, char const*,
/// char const*, int, int, int*, int*, atDelegate<...> const&,
/// sysMemAllocator*, netStatus*)`: the players' creations are on Rockstar's
/// servers, and a query fails to start, as the engine's does without
/// credentials, but without making its web request first. Freemode's
/// FM_maintain_cloud_header_data asks every frame, and each try logged five
/// errors. (A query that succeeds with nothing found does not do: the way
/// into GTA Online then stops at "Unable to connect to Rockstar game
/// services".)
pub(crate) unsafe extern "C" fn ugc_query_content(
    _env: *mut Env,
    _gamer: u32,
    _content_type: u32,
    _query: u64,
    _params: u64,
    _offset: u32,
    _count: u32,
    _found: u64,
    _total: u64,
    _delegate: u64,
    _allocator: u64,
    _status: u64,
) -> u32 {
    0
}

/// Hook for `rage::fiDevice::MakeGrowBufferFileName(char*, int,
/// datGrowBuffer*)`: "growbuffer:" and the buffer's address in hex, which
/// `fiDeviceGrowBuffer` reads back as the file's handle. The engine formats
/// it with `%p`, which in this build's C library begins with "0x": the
/// device read 0 (Windows' `%p` has no prefix), and files in memory had no
/// buffer.
pub(crate) unsafe extern "C" fn grow_buffer_file_name(env: *mut Env, name: u64, size: u32, buffer: u64) {
    let m = unsafe { Mem::of(env) };
    m.write_str(name, &format!("growbuffer:{buffer:x}"), size as usize);
}

/// This module's hooks, online.
pub(crate) fn hooks(hooks: &mut glue5_game::Hooks) {
    hooks.grow_buffer_file_name = Some(grow_buffer_file_name);
    hooks.xml_bool_attribute = Some(xml_bool_attribute);
    hooks.stats_init = Some(stats_init);
    hooks.stats_update = Some(stats_update);
    hooks.stats_queue_load = Some(stats_queue_load);
    hooks.stats_queue_save = Some(stats_queue_save);
    hooks.stats_load_failed = Some(stats_load_failed);
    hooks.stats_save = Some(stats_save);
    hooks.stats_load = Some(stats_load);
    hooks.cloud_available = Some(cloud_available);
    hooks.profile_stats_failed = Some(profile_stats_failed);
    hooks.profile_stats_synced = Some(profile_stats_synced);
    hooks.mpstats_load_update = Some(mpstats_load_update);
    hooks.mpstats_save_update = Some(mpstats_save_update);
    hooks.ros_credentials_valid = Some(ros_credentials_valid);
    hooks.post_member_file = Some(post_member_file);
    hooks.get_member_file = Some(get_member_file);
    hooks.ugc_query_content = Some(ugc_query_content);
}
