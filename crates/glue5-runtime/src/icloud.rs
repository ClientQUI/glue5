//! iCloud Documents for saves and settings, on iPhone, iPad and Mac.
//!
//! The ubiquity container is `iCloud.com.lennertwilhelm.gta5offline`
//! (team `9WKKC8XF38`, bundle `com.lennertwilhelm.gta5offline`). Copies go
//! through `NSFileCoordinator`. When iCloud is not signed in, or the App ID
//! has no container, every call leaves the local files where they are.
//!
//! Newest file wins. The older one is kept beside it as
//! `name.glue5-conflict-<seconds>-local.bak` or `-remote.bak`. `glue5.json`
//! is included, but each device keeps its own `game_dir`: the cloud copy
//! stores that field as null. The renderer cache, the vfs scratch tree,
//! logs and benchmarks are not uploaded.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Container id. The entitlements and the Info.plists name this same string.
pub const CONTAINER: &str = "iCloud.com.lennertwilhelm.gta5offline";

const DOWNLOAD_BUDGET: Duration = Duration::from_secs(5);

const STATUS_IDLE: u8 = 0;
const STATUS_UNAVAILABLE: u8 = 1;
const STATUS_SYNCED: u8 = 2;

static STATUS: AtomicU8 = AtomicU8::new(STATUS_IDLE);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static STATE: Mutex<Option<State>> = Mutex::new(None);
static SYNC: Mutex<()> = Mutex::new(());
static DIRTY: Mutex<bool> = Mutex::new(false);
static WAKE: Condvar = Condvar::new();
static WORKER: OnceLock<()> = OnceLock::new();
#[cfg(target_vendor = "apple")]
static CONTAINER_DIR: Mutex<Container> = Mutex::new(Container::Unknown);

struct State {
    user_dir: PathBuf,
    enabled: bool,
}

#[cfg(target_vendor = "apple")]
enum Container {
    Unknown,
    Missing,
    Ready(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    /// Launch, and the start page: a newer cloud file replaces the local one.
    Both,
    /// A running session: upload local changes, never replace an open save.
    Push,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub mtime_ns: u128,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Same,
    /// Cloud copy replaces the local file.
    Remote,
    /// Local file replaces the cloud copy.
    Local,
}

/// What to do with two copies. `same_bytes` is only meaningful when both exist.
pub(crate) fn plan(local: Option<FileStamp>, remote: Option<FileStamp>, same_bytes: bool) -> Plan {
    match (local, remote) {
        (None, None) => Plan::Same,
        (Some(_), None) => Plan::Local,
        (None, Some(_)) => Plan::Remote,
        (Some(_), Some(_)) if same_bytes => Plan::Same,
        (Some(local), Some(remote)) if remote.mtime_ns > local.mtime_ns => Plan::Remote,
        (Some(local), Some(remote)) if local.mtime_ns > remote.mtime_ns => Plan::Local,
        // Same moment, different bytes: this device just wrote it.
        _ => Plan::Local,
    }
}

/// A path under the user directory that is a save or a setting.
pub(crate) fn synced(rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    if rel.is_empty() {
        return false;
    }
    for part in rel.split('/') {
        if part.is_empty() || part.starts_with('.') {
            return false;
        }
        if matches!(part, "cache" | "fsroot" | "Benchmarks" | "Glue5 Diagnostics" | "game.log" | "last-failure.txt") {
            return false;
        }
        if part.contains("glue5-conflict") || part.ends_with(".icloud") || part.ends_with(".glue5-tmp") {
            return false;
        }
    }
    true
}

/// The line under the iCloud toggle.
pub fn status_note() -> &'static str {
    if !cfg!(target_vendor = "apple") {
        return "iCloud is available on iPhone, iPad and Mac.";
    }
    match STATUS.load(Ordering::Relaxed) {
        STATUS_UNAVAILABLE => "iCloud isn't available. Saves stay on this device.",
        STATUS_SYNCED => "On. The newest save wins, and the other copy is kept beside it.",
        _ => "Saves and these settings follow your iPhone, iPad and Mac. The game folder stays on this device.",
    }
}

/// Bumps after each sync, so the start page can reload settings.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Remembers where saves are and whether the toggle (and `GLUE5_ICLOUD`) want sync.
pub fn configure(user_dir: &Path, enabled: bool) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    *state = Some(State { user_dir: user_dir.to_path_buf(), enabled });
    if !enabled {
        STATUS.store(STATUS_IDLE, Ordering::Relaxed);
    }
}

fn is_enabled() -> bool {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|s| s.enabled)
}

fn configured_dir() -> Option<PathBuf> {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|s| s.enabled).map(|s| s.user_dir.clone())
}

/// `glue5.json` was just written. Enables or disables sync from that file
/// and queues an upload when it is on.
pub fn on_settings_saved(user_dir: &Path, json: &[u8]) {
    configure(user_dir, enabled_from_json(json));
    if is_enabled() {
        note_write();
    }
}

/// A `/userdata/...` file was closed. Queues an upload when that path is a save.
pub fn note_userdata(engine_path: &str) {
    let Some(rel) = engine_path.strip_prefix("/userdata/") else { return };
    if synced(rel) {
        note_write();
    }
}

/// Queues an upload. A running game uses this so a cloud file cannot replace
/// a save the engine still has open; the next launch is what applies a newer
/// copy from another device.
pub fn note_write() {
    if !is_enabled() {
        return;
    }
    WORKER.get_or_init(|| {
        let _ = std::thread::Builder::new().name("icloud".into()).spawn(worker);
    });
    let mut dirty = DIRTY.lock().unwrap_or_else(|e| e.into_inner());
    *dirty = true;
    WAKE.notify_one();
}

fn worker() {
    loop {
        let mut dirty = DIRTY.lock().unwrap_or_else(|e| e.into_inner());
        while !*dirty {
            dirty = WAKE.wait(dirty).unwrap_or_else(|e| e.into_inner());
        }
        *dirty = false;
        drop(dirty);
        std::thread::sleep(Duration::from_millis(800));
        if let Ok(mut dirty) = DIRTY.lock() {
            *dirty = false;
        }
        if let Some(dir) = configured_dir() {
            push_now(&dir);
        }
    }
}

/// Launch and the start page: newest wins in either direction.
pub fn sync_now(user_dir: &Path) {
    sync(user_dir, Way::Both, Duration::from_secs(4), DOWNLOAD_BUDGET);
}

/// Background, and a save during play: upload, and leave local files in place.
/// A short wait, so a suspend is not held on a container lookup.
pub fn push_now(user_dir: &Path) {
    sync(user_dir, Way::Push, Duration::from_millis(750), Duration::from_millis(200));
}

fn sync(user_dir: &Path, way: Way, container_wait: Duration, download: Duration) {
    if !is_enabled() {
        return;
    }
    let _guard = SYNC.lock().unwrap_or_else(|e| e.into_inner());
    let Some(remote_root) = ubiquity_dir(container_wait) else {
        if container_missing() {
            STATUS.store(STATUS_UNAVAILABLE, Ordering::Relaxed);
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                log::info!("icloud: no ubiquity container. Saves stay in {}", user_dir.display());
            }
        } else {
            log::debug!("icloud: the container is not ready yet. Saves stay in {}", user_dir.display());
        }
        return;
    };
    let started = Instant::now();
    let deadline = started + download;
    let counts = reconcile(user_dir, &remote_root, way, deadline);
    STATUS.store(STATUS_SYNCED, Ordering::Relaxed);
    GENERATION.fetch_add(1, Ordering::Release);
    if counts.up + counts.down + counts.backups + counts.skipped > 0 {
        log::info!(
            "icloud: {} up, {} down, {} backups, {} waiting on a download ({:.1}s)",
            counts.up,
            counts.down,
            counts.backups,
            counts.skipped,
            started.elapsed().as_secs_f32()
        );
    }
}

struct Counts {
    up: u32,
    down: u32,
    backups: u32,
    skipped: u32,
}

fn reconcile(local_root: &Path, remote_root: &Path, way: Way, deadline: Instant) -> Counts {
    let _ = fs::create_dir_all(remote_root);
    let mut rels = Vec::new();
    collect(local_root, local_root, &mut rels);
    collect(remote_root, remote_root, &mut rels);
    rels.sort();
    rels.dedup();
    let mut counts = Counts { up: 0, down: 0, backups: 0, skipped: 0 };
    for rel in rels {
        let local = local_root.join(&rel);
        let remote = remote_root.join(&rel);
        match one(&local, &remote, &rel, way, deadline) {
            One::Up => counts.up += 1,
            One::Down => counts.down += 1,
            One::Skipped => counts.skipped += 1,
            One::Same => {}
        }
        if backup_written() {
            counts.backups += 1;
        }
    }
    counts
}

enum One {
    Same,
    Up,
    Down,
    Skipped,
}

/// Set by [`backup`] for the counter. One file at a time, under `SYNC`.
static DID_BACKUP: AtomicBool = AtomicBool::new(false);

fn backup_written() -> bool {
    DID_BACKUP.swap(false, Ordering::Relaxed)
}

fn one(local: &Path, remote: &Path, rel: &Path, way: Way, deadline: Instant) -> One {
    DID_BACKUP.store(false, Ordering::Relaxed);
    let remote_ready = if remote.is_file() { prepare_remote(remote, deadline) } else { false };
    if remote.exists() && !remote_ready {
        log::info!("icloud: left {} on this device; its iCloud copy is not downloaded yet", rel.display());
        return One::Skipped;
    }
    let local_stamp = stamp(local);
    let remote_stamp = if remote_ready { stamp(remote) } else { None };
    let settings = rel == Path::new("glue5.json");
    let same = match (local_stamp, remote_stamp) {
        (Some(_), Some(_)) if settings => settings_equal(&read_file(local), &read_file(remote)),
        (Some(_), Some(_)) => same_file(local, remote),
        _ => false,
    };
    let decided = plan(local_stamp, remote_stamp, same);
    if settings && decided == Plan::Same && remote_ready && remote_has_game_dir(&read_file(remote)) {
        let bytes = cloud_settings_bytes(&read_file(local));
        if transfer_bytes(&bytes, remote).is_ok() {
            return One::Up;
        }
    }
    match decided {
        Plan::Same => One::Same,
        Plan::Remote if way == Way::Push => One::Same,
        Plan::Remote => {
            if local_stamp.is_some() {
                backup(local, "local");
            }
            let wrote = if settings {
                let bytes = local_settings_bytes(&read_file(remote), &read_file(local));
                transfer_bytes(&bytes, local).is_ok()
            } else {
                transfer(remote, local).is_ok()
            };
            if wrote { One::Down } else { One::Skipped }
        }
        Plan::Local => {
            if remote_stamp.is_some() {
                backup(remote, "remote");
            }
            let wrote = if settings {
                let bytes = cloud_settings_bytes(&read_file(local));
                transfer_bytes(&bytes, remote).is_ok()
            } else {
                transfer(local, remote).is_ok()
            };
            if wrote { One::Up } else { One::Skipped }
        }
    }
}

fn collect(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let rel = entry.path().strip_prefix(root).unwrap_or(Path::new(&*name)).to_string_lossy().replace('\\', "/");
        if !synced(&rel) {
            continue;
        }
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect(&entry.path(), root, out);
        } else if kind.is_file() {
            out.push(PathBuf::from(rel));
        }
    }
}

fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime_ns = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some(FileStamp { mtime_ns, len: meta.len() })
}

fn same_file(a: &Path, b: &Path) -> bool {
    let Ok(mut fa) = File::open(a) else { return false };
    let Ok(mut fb) = File::open(b) else { return false };
    let mut ba = [0u8; 64 * 1024];
    let mut bb = [0u8; 64 * 1024];
    loop {
        let Ok(na) = fa.read(&mut ba) else { return false };
        let Ok(nb) = fb.read(&mut bb) else { return false };
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}

fn read_file(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_default()
}

fn backup(path: &Path, which: &str) {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let dest = path.with_file_name(format!("{name}.glue5-conflict-{stamp}-{which}.bak"));
    if transfer(path, &dest).is_ok() {
        DID_BACKUP.store(true, Ordering::Relaxed);
        log::info!("icloud: kept the older {} as {}", path.display(), dest.display());
    }
}

fn transfer(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        apple::coordinated_copy(from, to)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        plain_copy(from, to)
    }
}

fn transfer_bytes(bytes: &[u8], to: &Path) -> io::Result<()> {
    let tmp = spill_bytes(bytes)?;
    let result = transfer(&tmp, to);
    let _ = fs::remove_file(&tmp);
    result
}

fn plain_copy(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to)?;
    if let Ok(file) = File::open(to) {
        let _ = file.sync_all();
    }
    Ok(())
}

fn spill_bytes(bytes: &[u8]) -> io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path =
        std::env::temp_dir().join(format!("glue5-icloud-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut file = File::create(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(path)
}

fn prepare_remote(path: &Path, deadline: Instant) -> bool {
    #[cfg(target_vendor = "apple")]
    {
        apple::prepare(path, deadline)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = deadline;
        path.is_file()
    }
}

fn ubiquity_dir(wait: Duration) -> Option<PathBuf> {
    #[cfg(target_vendor = "apple")]
    {
        apple::ubiquity_dir(wait)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = wait;
        None
    }
}

fn container_missing() -> bool {
    #[cfg(target_vendor = "apple")]
    {
        apple::container_missing()
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        false
    }
}

/// `GLUE5_ICLOUD` wins. A file with no `icloud` field is on, on Apple.
fn enabled_from_json(json: &[u8]) -> bool {
    if !cfg!(target_vendor = "apple") {
        return false;
    }
    match std::env::var("GLUE5_ICLOUD").ok().as_deref().map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("0" | "off" | "false") => return false,
        Some("1" | "on" | "true") => return true,
        _ => {}
    }
    match serde_json::from_slice::<Value>(json) {
        Ok(v) => v.get("icloud").and_then(Value::as_bool).unwrap_or(true),
        Err(_) => true,
    }
}

fn normalized_settings(bytes: &[u8]) -> Option<String> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    value.as_object_mut()?.insert("game_dir".into(), Value::Null);
    Some(value.to_string())
}

fn settings_equal(local: &[u8], remote: &[u8]) -> bool {
    match (normalized_settings(local), normalized_settings(remote)) {
        (Some(local), Some(remote)) => local == remote,
        _ => local == remote,
    }
}

fn remote_has_game_dir(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes).ok().and_then(|v| v.get("game_dir").cloned()).is_some_and(|v| !v.is_null())
}

/// Cloud copy: the same settings, without this device's game folder.
fn cloud_settings_bytes(local: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<Value>(local) else { return local.to_vec() };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("game_dir".into(), Value::Null);
    }
    serde_json::to_vec_pretty(&value).unwrap_or_else(|_| local.to_vec())
}

/// Local copy after a newer cloud file: cloud settings, this device's game folder.
fn local_settings_bytes(remote: &[u8], previous_local: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<Value>(remote) else { return remote.to_vec() };
    let keep = serde_json::from_slice::<Value>(previous_local).ok().and_then(|v| v.get("game_dir").cloned());
    if let Some(obj) = value.as_object_mut()
        && let Some(dir) = keep
    {
        obj.insert("game_dir".into(), dir);
    }
    serde_json::to_vec_pretty(&value).unwrap_or_else(|_| remote.to_vec())
}

#[cfg(target_vendor = "apple")]
mod apple {
    use std::ffi::CStr;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, rc::autoreleasepool};
    use objc2_foundation::{
        NSError, NSFileCoordinator, NSFileCoordinatorReadingOptions, NSFileCoordinatorWritingOptions, NSFileManager,
        NSString, NSURL, NSURLUbiquitousItemDownloadingStatusCurrent, NSURLUbiquitousItemDownloadingStatusDownloaded,
        NSURLUbiquitousItemDownloadingStatusKey, NSURLUbiquitousItemDownloadingStatusNotDownloaded,
    };

    use super::{CONTAINER, CONTAINER_DIR, Container};

    static LOOKUP: std::sync::OnceLock<()> = std::sync::OnceLock::new();

    pub fn container_missing() -> bool {
        matches!(*CONTAINER_DIR.lock().unwrap_or_else(|e| e.into_inner()), Container::Missing)
    }

    /// One lookup thread. A timeout does not record "missing": the next sync
    /// waits on the same thread. Nil from the system does.
    pub fn ubiquity_dir(wait: Duration) -> Option<PathBuf> {
        LOOKUP.get_or_init(|| {
            // Apple asks that this not run on the main thread. It can block
            // while the container is created.
            let _ = std::thread::Builder::new().name("icloud-container".into()).spawn(|| {
                let path = autoreleasepool(|_| container_path());
                let mut slot = CONTAINER_DIR.lock().unwrap_or_else(|e| e.into_inner());
                *slot = match path {
                    Some(path) => Container::Ready(path),
                    None => Container::Missing,
                };
            });
        });
        let start = Instant::now();
        loop {
            let slot = CONTAINER_DIR.lock().unwrap_or_else(|e| e.into_inner());
            match &*slot {
                Container::Ready(path) => return Some(path.clone()),
                Container::Missing => return None,
                Container::Unknown if start.elapsed() >= wait => return None,
                Container::Unknown => {}
            }
            drop(slot);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn container_path() -> Option<PathBuf> {
        let fm = NSFileManager::defaultManager();
        let id = NSString::from_str(CONTAINER);
        let url = fm.URLForUbiquityContainerIdentifier(Some(&id))?;
        let docs = url.URLByAppendingPathComponent_isDirectory(&NSString::from_str("Documents"), true)?;
        let root = docs.URLByAppendingPathComponent_isDirectory(&NSString::from_str("glue5"), true)?;
        let path = url_path(&root)?;
        let _ = fs::create_dir_all(&path);
        Some(path)
    }

    pub fn prepare(path: &Path, deadline: Instant) -> bool {
        loop {
            match download_status(path) {
                Status::Ready => return true,
                Status::Missing => return path.is_file(),
                Status::Wait => {
                    let _ = start_download(path);
                    if Instant::now() >= deadline {
                        return matches!(download_status(path), Status::Ready) || path.is_file();
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    enum Status {
        Ready,
        Wait,
        Missing,
    }

    fn download_status(path: &Path) -> Status {
        let Some(url) = file_url(path) else { return Status::Missing };
        let mut value: Option<Retained<AnyObject>> = None;
        let key = unsafe { NSURLUbiquitousItemDownloadingStatusKey };
        if unsafe { url.getResourceValue_forKey_error(&mut value, key) }.is_err() {
            return if path.is_file() { Status::Ready } else { Status::Missing };
        }
        let Some(value) = value else { return Status::Ready };
        let current = unsafe { NSURLUbiquitousItemDownloadingStatusCurrent };
        let downloaded = unsafe { NSURLUbiquitousItemDownloadingStatusDownloaded };
        let waiting = unsafe { NSURLUbiquitousItemDownloadingStatusNotDownloaded };
        let is = |other: &NSString| -> bool { unsafe { msg_send![&*value, isEqualToString: other] } };
        if is(current) || is(downloaded) {
            Status::Ready
        } else if is(waiting) {
            Status::Wait
        } else {
            Status::Ready
        }
    }

    fn start_download(path: &Path) -> bool {
        let Some(url) = file_url(path) else { return false };
        NSFileManager::defaultManager().startDownloadingUbiquitousItemAtURL_error(&url).is_ok()
    }

    pub fn coordinated_copy(from: &Path, to: &Path) -> io::Result<()> {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = spill_copy(from)?;
        let result = write_coordinated(to, &tmp);
        let _ = fs::remove_file(&tmp);
        result
    }

    fn spill_copy(from: &Path) -> io::Result<PathBuf> {
        let mut out = None;
        let slot = &mut out as *mut Option<io::Result<PathBuf>>;
        read_coordinated(from, slot);
        out.unwrap_or_else(|| Err(io::Error::other("icloud read was not coordinated")))
    }

    fn read_coordinated(from: &Path, slot: *mut Option<io::Result<PathBuf>>) {
        let Some(url) = file_url(from) else {
            unsafe { *slot = Some(Err(io::Error::new(io::ErrorKind::NotFound, "icloud path"))) };
            return;
        };
        let coordinator = NSFileCoordinator::new();
        coordinator.setPurposeIdentifier(&NSString::from_str("glue5.icloud"));
        let mut error: Option<Retained<NSError>> = None;
        let block = RcBlock::new(move |item: NonNull<NSURL>| {
            let path = url_path(unsafe { item.as_ref() });
            let result = path.ok_or_else(|| io::Error::other("icloud read url")).and_then(|path| {
                let tmp = super::spill_bytes(&fs::read(&path)?)?;
                Ok(tmp)
            });
            unsafe { *slot = Some(result) };
        });
        coordinator.coordinateReadingItemAtURL_options_error_byAccessor(
            &url,
            NSFileCoordinatorReadingOptions::empty(),
            Some(&mut error),
            &block,
        );
        if let Some(error) = error {
            log::warn!("icloud: read {}: {}", from.display(), error.code());
        }
    }

    fn write_coordinated(to: &Path, tmp: &Path) -> io::Result<()> {
        let Some(url) = file_url(to) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "icloud path"));
        };
        let coordinator = NSFileCoordinator::new();
        coordinator.setPurposeIdentifier(&NSString::from_str("glue5.icloud"));
        let mut error: Option<Retained<NSError>> = None;
        let mut out = None;
        let slot = &mut out as *mut Option<io::Result<()>>;
        let tmp = tmp.to_path_buf();
        let block = RcBlock::new(move |item: NonNull<NSURL>| {
            let result = url_path(unsafe { item.as_ref() })
                .ok_or_else(|| io::Error::other("icloud write url"))
                .and_then(|path| super::plain_copy(&tmp, &path));
            unsafe { *slot = Some(result) };
        });
        coordinator.coordinateWritingItemAtURL_options_error_byAccessor(
            &url,
            NSFileCoordinatorWritingOptions::ForReplacing,
            Some(&mut error),
            &block,
        );
        if let Some(error) = error {
            log::warn!("icloud: write {}: {}", to.display(), error.code());
            return Err(io::Error::other("icloud write"));
        }
        out.unwrap_or_else(|| Err(io::Error::other("icloud write was not coordinated")))
    }

    fn file_url(path: &Path) -> Option<Retained<NSURL>> {
        let text = path.to_str()?;
        Some(NSURL::fileURLWithPath(&NSString::from_str(text)))
    }

    fn url_path(url: &NSURL) -> Option<PathBuf> {
        let ptr = url.fileSystemRepresentation();
        let text = unsafe { CStr::from_ptr(ptr.as_ptr()) }.to_str().ok()?;
        Some(PathBuf::from(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_wins_and_equal_bytes_do_not() {
        let older = FileStamp { mtime_ns: 10, len: 4 };
        let newer = FileStamp { mtime_ns: 20, len: 4 };
        assert_eq!(plan(Some(older), Some(newer), false), Plan::Remote);
        assert_eq!(plan(Some(newer), Some(older), false), Plan::Local);
        assert_eq!(plan(Some(older), Some(newer), true), Plan::Same);
        assert_eq!(plan(Some(older), None, false), Plan::Local);
        assert_eq!(plan(None, Some(newer), false), Plan::Remote);
        assert_eq!(plan(Some(older), Some(FileStamp { mtime_ns: 10, len: 9 }), false), Plan::Local);
    }

    #[test]
    fn cache_logs_and_conflict_copies_stay_local() {
        assert!(synced("glue5.json"));
        assert!(synced("online-id"));
        assert!(synced("cloud/extra-stats.txt"));
        assert!(synced("cloud/mpstats_char0001.save"));
        assert!(synced("Documents/Rockstar Games/GTA V/settings.xml"));
        assert!(synced("Documents/Rockstar Games/GTA V/Profiles/1/SGTA50015"));
        assert!(!synced("cache/pipelines.bin"));
        assert!(!synced("fsroot/tmp"));
        assert!(!synced("game.log"));
        assert!(!synced("last-failure.txt"));
        assert!(!synced("Documents/Rockstar Games/GTA V/Benchmarks/Benchmark-1.txt"));
        assert!(!synced("Glue5 Diagnostics/diagnostics.log"));
        assert!(!synced("cloud/note.glue5-conflict-1-local.bak"));
    }

    #[test]
    fn settings_keep_the_local_game_folder() {
        let local = br#"{"game_dir":"/iphone/Documents/game","icloud":true,"fog":true}"#;
        let remote = br#"{"game_dir":"/mac/game","icloud":true,"fog":false}"#;
        assert!(!settings_equal(local, remote));
        let merged = local_settings_bytes(remote, local);
        let parsed: Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(parsed["game_dir"], "/iphone/Documents/game");
        assert_eq!(parsed["fog"], false);
        let cloud = cloud_settings_bytes(local);
        let parsed: Value = serde_json::from_slice(&cloud).unwrap();
        assert!(parsed["game_dir"].is_null());
        assert_eq!(parsed["fog"], true);
        let again = cloud_settings_bytes(&merged);
        assert!(settings_equal(&merged, &again));
    }

    #[test]
    fn reconcile_backs_up_the_older_copy() {
        let root = std::env::temp_dir().join(format!("glue5-icloud-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let local = root.join("local");
        let remote = root.join("remote");
        fs::create_dir_all(local.join("cloud")).unwrap();
        fs::create_dir_all(remote.join("cloud")).unwrap();
        fs::create_dir_all(local.join("cache")).unwrap();
        fs::write(local.join("cache").join("skip.bin"), b"no").unwrap();
        let save = Path::new("cloud/extra-stats.txt");
        fs::write(local.join(save), b"local-old").unwrap();
        fs::write(remote.join(save), b"remote-new").unwrap();
        set_mtime(&local.join(save), 1_700_000_000);
        set_mtime(&remote.join(save), 1_700_000_100);
        let counts = reconcile(&local, &remote, Way::Both, Instant::now() + DOWNLOAD_BUDGET);
        assert_eq!(fs::read(local.join(save)).unwrap(), b"remote-new");
        assert_eq!(fs::read(remote.join(save)).unwrap(), b"remote-new");
        assert!(counts.down >= 1);
        assert!(counts.backups >= 1);
        let backup = fs::read_dir(local.join("cloud")).unwrap().flatten().any(|e| {
            e.file_name().to_string_lossy().contains("glue5-conflict")
                && e.file_name().to_string_lossy().contains("local")
        });
        assert!(backup);
        assert!(!remote.join("cache").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    fn set_mtime(path: &Path, secs: u64) {
        let t = libc::timespec { tv_sec: secs as _, tv_nsec: 0 };
        let times = [t, t];
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        unsafe {
            libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
        }
    }
}
