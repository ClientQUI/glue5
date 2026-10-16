//! Google Drive sync for saves and settings, on iPhone, iPad and Mac.
//!
//! Files go in the hidden `appDataFolder` (`drive.appdata`), not the user's
//! Drive listing. Sign-in is OAuth 2.0 for native apps with PKCE: a loopback
//! redirect on the Mac, `ASWebAuthenticationSession` on iPhone and iPad. The
//! refresh token is stored in the Keychain. When Drive is off, signed out, or
//! unreachable, every call leaves the local files where they are.
//!
//! Newest file wins. The older one is kept beside it as
//! `name.glue5-conflict-<seconds>-local.bak` or `-remote.bak`. `glue5.json`
//! is included, but each device keeps its own game folder and OAuth client
//! ids. The renderer cache, the vfs scratch tree, logs and benchmarks are
//! not uploaded.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const SCOPE: &str = "https://www.googleapis.com/auth/drive.appdata";
/// Built-in iOS OAuth client. Public. iPhone and iPad use this unless the
/// settings field or `GLUE5_GOOGLE_CLIENT_ID` replaces it.
pub const DEFAULT_IOS_CLIENT_ID: &str = "566965852987-ep0g7185pbbhn83idg5mk5ikjij8q3fu.apps.googleusercontent.com";
/// Built-in desktop OAuth client. Public. Mac uses this unless the settings
/// field or `GLUE5_GOOGLE_CLIENT_ID` replaces it.
pub const DEFAULT_DESKTOP_CLIENT_ID: &str = "566965852987-md6g70j3j5qep867r642mg5u05furtfe.apps.googleusercontent.com";

/// The client id this device uses when the saved field is empty.
pub fn default_client_id() -> &'static str {
    if cfg!(target_os = "ios") { DEFAULT_IOS_CLIENT_ID } else { DEFAULT_DESKTOP_CLIENT_ID }
}
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Keychain item. The bundle id is `com.lennertwilhelm.gta5offline`.
#[cfg(target_vendor = "apple")]
const KEYCHAIN_SERVICE: &str = "com.lennertwilhelm.gta5offline";
#[cfg(target_vendor = "apple")]
const KEYCHAIN_ACCOUNT: &str = "google-drive";
/// Not synced: `synced` rejects dotfiles. Apple builds keep the token in the Keychain.
#[cfg(not(target_vendor = "apple"))]
const TOKEN_FILE: &str = ".google-token";
/// A slash cannot be a Drive file name. This stands in for `/`.
const SLASH: char = '\u{2215}';
const MAX_FILE: u64 = 32 * 1024 * 1024;

const STATUS_IDLE: u8 = 0;
const STATUS_OFF: u8 = 1;
const STATUS_NEED_ID: u8 = 2;
const STATUS_NEED_SIGNIN: u8 = 3;
const STATUS_WAITING: u8 = 4;
const STATUS_UNAVAILABLE: u8 = 5;
const STATUS_SYNCED: u8 = 6;

static STATUS: AtomicU8 = AtomicU8::new(STATUS_IDLE);
static DETAIL: Mutex<String> = Mutex::new(String::new());
static GENERATION: AtomicU64 = AtomicU64::new(0);
static STATE: Mutex<Option<State>> = Mutex::new(None);
static SYNC: Mutex<()> = Mutex::new(());
static DIRTY: Mutex<bool> = Mutex::new(false);
static WAKE: Condvar = Condvar::new();
static WORKER: OnceLock<()> = OnceLock::new();
static TOKEN: Mutex<Option<Token>> = Mutex::new(None);
static TOKEN_LOADED: AtomicBool = AtomicBool::new(false);
static SIGNING_IN: AtomicBool = AtomicBool::new(false);

struct State {
    user_dir: PathBuf,
    enabled: bool,
    client_id: String,
    /// Desktop OAuth client secret. Empty on iOS. Never uploaded.
    client_secret: String,
}

#[derive(Clone)]
struct Token {
    access: String,
    refresh: String,
    expires_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    /// Launch, and coming back with no session: a newer cloud file replaces the local one.
    Both,
    /// A running session, or a suspend: upload local changes, never replace an open save.
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
        if part.contains("glue5-conflict")
            || part.ends_with(".icloud")
            || part.ends_with(".glue5-tmp")
            || part.ends_with(".bak")
        {
            return false;
        }
    }
    true
}

fn set_status(code: u8, detail: &str) {
    if !detail.is_empty() {
        *DETAIL.lock().unwrap_or_else(|e| e.into_inner()) = detail.to_owned();
    }
    STATUS.store(code, Ordering::Relaxed);
}

/// The line under the Google Drive toggle.
pub fn status_note() -> String {
    if !cfg!(target_vendor = "apple") {
        return "Google Drive sync is for iPhone, iPad and Mac.".into();
    }
    match STATUS.load(Ordering::Relaxed) {
        STATUS_OFF => "Off. Saves stay on this device.".into(),
        STATUS_NEED_ID => "Add the OAuth client ID, then sign in.".into(),
        STATUS_NEED_SIGNIN => "Sign in to copy saves. Until then they stay on this device.".into(),
        STATUS_WAITING => "Waiting for the browser…".into(),
        STATUS_UNAVAILABLE => {
            let detail = DETAIL.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if detail.is_empty() { "Google Drive isn't reachable. Saves stay on this device.".into() } else { detail }
        }
        STATUS_SYNCED => "On. The newest save wins, and the other copy is kept beside it.".into(),
        _ => "Saves and these settings can follow your iPhone, iPad and Mac. The game folder stays on this device."
            .into(),
    }
}

/// Whether a refresh token is stored.
pub fn signed_in() -> bool {
    ensure_token().is_some_and(|t| !t.refresh.is_empty())
}

/// Bumps after each sync, so the start page can reload settings.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Remembers where saves are, whether the toggle wants sync, and the client id.
/// `client_secret` is the desktop secret, or empty. iOS ignores it.
pub fn configure(user_dir: &Path, enabled: bool, client_id: &str, client_secret: &str) {
    let client_id = client_id.trim().to_owned();
    let client_secret = if cfg!(target_os = "ios") { String::new() } else { client_secret.trim().to_owned() };
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    *state = Some(State { user_dir: user_dir.to_path_buf(), enabled, client_id, client_secret });
    if !enabled {
        set_status(STATUS_OFF, "");
    }
}

fn state_snapshot() -> Option<State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| State {
        user_dir: s.user_dir.clone(),
        enabled: s.enabled,
        client_id: s.client_id.clone(),
        client_secret: s.client_secret.clone(),
    })
}

fn is_enabled() -> bool {
    state_snapshot().is_some_and(|s| s.enabled)
}

/// `glue5.json` was just written. Enables or disables sync from that file
/// and queues an upload when it is on.
pub fn on_settings_saved(user_dir: &Path, json: &[u8]) {
    let (enabled, client_id, client_secret) = enabled_from_json(json);
    configure(user_dir, enabled, &client_id, &client_secret);
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
        let _ = std::thread::Builder::new().name("drive".into()).spawn(worker);
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
        if let Some(state) = state_snapshot().filter(|s| s.enabled) {
            sync(&state, Way::Push, Duration::from_secs(20));
        }
    }
}

/// Launch and coming back to the start page: newest wins in either direction.
pub fn sync_now(user_dir: &Path) {
    let Some(state) = state_snapshot().filter(|s| s.enabled && s.user_dir == user_dir) else { return };
    sync(&state, Way::Both, Duration::from_secs(12));
}

/// Background, and a save during play: upload, and leave local files in place.
/// Short, so a suspend is not held on the network.
pub fn push_now(user_dir: &Path) {
    let Some(state) = state_snapshot().filter(|s| s.enabled && s.user_dir == user_dir) else { return };
    sync(&state, Way::Push, Duration::from_secs(3));
}

/// Opens the Google sign-in page. The button's thread is the main thread on
/// iPhone and iPad, which is what `ASWebAuthenticationSession` requires.
pub fn sign_in(client_id: &str, client_secret: &str) {
    let client_id = client_id.trim().to_owned();
    let client_secret = client_secret.trim().to_owned();
    if let Some(state) = state_snapshot() {
        configure(&state.user_dir, true, &client_id, &client_secret);
    }
    if client_id.is_empty() {
        set_status(STATUS_NEED_ID, "");
        return;
    }
    if SIGNING_IN.swap(true, Ordering::Relaxed) {
        return;
    }
    set_status(STATUS_WAITING, "");
    let (verifier, challenge) = pkce();
    let state = random_token();
    #[cfg(target_os = "ios")]
    {
        let Some(scheme) = ios_scheme(&client_id) else {
            SIGNING_IN.store(false, Ordering::Relaxed);
            set_status(STATUS_UNAVAILABLE, "That client ID is not an iOS OAuth client (….apps.googleusercontent.com).");
            return;
        };
        if !bundle_has_scheme(&scheme) {
            SIGNING_IN.store(false, Ordering::Relaxed);
            set_status(
                STATUS_UNAVAILABLE,
                &format!(
                    "Rebuild the iOS app with the URL scheme {scheme} (platform/ios/project.yml, CFBundleURLSchemes)."
                ),
            );
            return;
        }
        let redirect = format!("{scheme}:/oauth2redirect");
        let url = auth_url(&client_id, &redirect, &challenge, &state);
        ios::start(&url, &scheme, move |callback| finish_sign_in(callback, &client_id, &verifier, &redirect, &state));
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = std::thread::Builder::new().name("drive-auth".into()).spawn(move || {
            let result = loopback_sign_in(&client_id, &client_secret, &verifier, &challenge, &state);
            SIGNING_IN.store(false, Ordering::Relaxed);
            match result {
                Ok(()) => {
                    if is_enabled()
                        && let Some(state) = state_snapshot()
                    {
                        sync(&state, Way::Both, Duration::from_secs(20));
                    } else {
                        set_status(STATUS_SYNCED, "");
                    }
                }
                Err(error) => {
                    log::info!("drive: sign-in: {error}");
                    set_status(STATUS_UNAVAILABLE, &error);
                }
            }
        });
    }
}

#[cfg(target_os = "ios")]
fn finish_sign_in(callback: Result<String, String>, client_id: &str, verifier: &str, redirect: &str, state: &str) {
    let result = callback.and_then(|url| exchange_from_redirect(&url, client_id, "", verifier, redirect, state));
    SIGNING_IN.store(false, Ordering::Relaxed);
    match result {
        Ok(()) => {
            if is_enabled()
                && let Some(state) = state_snapshot()
            {
                sync(&state, Way::Both, Duration::from_secs(20));
            } else {
                set_status(STATUS_SYNCED, "");
            }
        }
        Err(error) => {
            log::info!("drive: sign-in: {error}");
            set_status(STATUS_UNAVAILABLE, &error);
        }
    }
}

/// Drops the Keychain item. Files already in Drive stay there.
pub fn sign_out() {
    *TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    TOKEN_LOADED.store(true, Ordering::Relaxed);
    forget_token();
    if is_enabled() {
        set_status(STATUS_NEED_SIGNIN, "");
    }
}

fn sync(state: &State, way: Way, budget: Duration) {
    if !state.enabled {
        return;
    }
    if state.client_id.is_empty() {
        set_status(STATUS_NEED_ID, "");
        return;
    }
    if !signed_in() {
        set_status(STATUS_NEED_SIGNIN, "");
        return;
    }
    let _guard = SYNC.lock().unwrap_or_else(|e| e.into_inner());
    let started = Instant::now();
    let deadline = started + budget;
    match reconcile_drive(state, way, deadline) {
        Ok(counts) => {
            set_status(STATUS_SYNCED, "");
            GENERATION.fetch_add(1, Ordering::Release);
            if counts.up + counts.down + counts.backups + counts.skipped > 0 {
                log::info!(
                    "drive: {} up, {} down, {} backups, {} skipped ({:.1}s)",
                    counts.up,
                    counts.down,
                    counts.backups,
                    counts.skipped,
                    started.elapsed().as_secs_f32()
                );
            }
        }
        Err(error) => {
            log::info!("drive: {error}");
            let shown = if error.contains("reachable") || error.contains("timed out") || error.contains("Transport") {
                "Google Drive isn't reachable. Saves stay on this device.".to_owned()
            } else {
                error
            };
            set_status(STATUS_UNAVAILABLE, &shown);
        }
    }
}

struct Counts {
    up: u32,
    down: u32,
    backups: u32,
    skipped: u32,
}

struct RemoteFile {
    id: String,
    md5: String,
    mtime_ns: u128,
}

fn reconcile_drive(state: &State, way: Way, deadline: Instant) -> Result<Counts, String> {
    let access = fresh_access(&state.client_id, &state.client_secret)?;
    let remote = list_files(&access)?;
    let mirror = std::env::temp_dir().join(format!("glue5-drive-{}", std::process::id()));
    let _ = fs::remove_dir_all(&mirror);
    fs::create_dir_all(&mirror).map_err(|e| e.to_string())?;
    let mut hold = Vec::new();
    let mut known: std::collections::HashMap<String, RemoteFile> = std::collections::HashMap::new();
    for (name, file) in remote {
        let Some(rel) = decode_name(&name) else { continue };
        if !synced(&rel) {
            continue;
        }
        if Instant::now() >= deadline {
            hold.push(PathBuf::from(&rel));
            continue;
        }
        match download(&access, &file.id) {
            Ok(bytes) => {
                let path = mirror.join(&rel);
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if fs::write(&path, &bytes).is_err() {
                    hold.push(PathBuf::from(&rel));
                    continue;
                }
                set_mtime(&path, file.mtime_ns);
                known.insert(rel, file);
            }
            Err(error) => {
                log::info!("drive: left {rel} on this device ({error})");
                hold.push(PathBuf::from(rel));
            }
        }
    }
    let mut counts = reconcile(&state.user_dir, &mirror, way, &hold);
    // `up` from the mirror copy is not a network upload. Count those below.
    counts.up = 0;
    let access = fresh_access(&state.client_id, &state.client_secret)?;
    let mut rels = Vec::new();
    collect(&mirror, &mirror, &mut rels);
    for rel in rels {
        if hold.iter().any(|h| h == &rel) {
            continue;
        }
        if Instant::now() >= deadline {
            counts.skipped += 1;
            continue;
        }
        let path = mirror.join(&rel);
        let Ok(bytes) = fs::read(&path) else { continue };
        if bytes.len() as u64 > MAX_FILE {
            log::info!("drive: skipped {} (larger than 32 MiB)", rel.display());
            counts.skipped += 1;
            continue;
        }
        let name = encode_name(&rel.to_string_lossy());
        let md5 = md5_hex(&bytes);
        let mtime_ns = stamp(&path).map(|s| s.mtime_ns).unwrap_or_else(now_ns);
        let rel_key = rel.to_string_lossy().into_owned();
        let outcome = if let Some(prev) = known.get(&rel_key) {
            if prev.md5 == md5 {
                Ok(false)
            } else {
                update_file(&access, &prev.id, &name, &bytes, mtime_ns).map(|()| true)
            }
        } else {
            create_file(&access, &name, &bytes, mtime_ns).map(|()| true)
        };
        match outcome {
            Ok(true) => counts.up += 1,
            Ok(false) => {}
            Err(error) => {
                log::info!("drive: upload {}: {error}", rel.display());
                counts.skipped += 1;
            }
        }
    }
    let _ = fs::remove_dir_all(&mirror);
    Ok(counts)
}

fn reconcile(local_root: &Path, remote_root: &Path, way: Way, hold: &[PathBuf]) -> Counts {
    let _ = fs::create_dir_all(remote_root);
    let mut rels = Vec::new();
    collect(local_root, local_root, &mut rels);
    collect(remote_root, remote_root, &mut rels);
    rels.sort();
    rels.dedup();
    let mut counts = Counts { up: 0, down: 0, backups: 0, skipped: 0 };
    for rel in rels {
        if hold.iter().any(|h| h == &rel) {
            counts.skipped += 1;
            continue;
        }
        let local = local_root.join(&rel);
        let remote = remote_root.join(&rel);
        match one(&local, &remote, &rel, way) {
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

fn one(local: &Path, remote: &Path, rel: &Path, way: Way) -> One {
    DID_BACKUP.store(false, Ordering::Relaxed);
    let local_stamp = stamp(local);
    let remote_stamp = stamp(remote);
    let settings = rel == Path::new("glue5.json");
    let same = match (local_stamp, remote_stamp) {
        (Some(_), Some(_)) if settings => settings_equal(&read_file(local), &read_file(remote)),
        (Some(_), Some(_)) => same_file(local, remote),
        _ => false,
    };
    let decided = plan(local_stamp, remote_stamp, same);
    if settings && decided == Plan::Same && remote_stamp.is_some() && remote_has_device_fields(&read_file(remote)) {
        let bytes = cloud_settings_bytes(&read_file(local));
        if write_bytes(&bytes, remote, local_stamp.map(|s| s.mtime_ns).unwrap_or_else(now_ns)).is_ok() {
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
            let mtime = remote_stamp.map(|s| s.mtime_ns).unwrap_or_else(now_ns);
            let wrote = if settings {
                let bytes = local_settings_bytes(&read_file(remote), &read_file(local));
                write_bytes(&bytes, local, mtime).is_ok()
            } else {
                copy_mtime(remote, local, mtime).is_ok()
            };
            if wrote { One::Down } else { One::Skipped }
        }
        Plan::Local => {
            if remote_stamp.is_some() {
                backup(remote, "remote");
            }
            let mtime = local_stamp.map(|s| s.mtime_ns).unwrap_or_else(now_ns);
            let wrote = if settings {
                let bytes = cloud_settings_bytes(&read_file(local));
                write_bytes(&bytes, remote, mtime).is_ok()
            } else {
                copy_mtime(local, remote, mtime).is_ok()
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

fn now_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
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
    if plain_copy(path, &dest).is_ok() {
        DID_BACKUP.store(true, Ordering::Relaxed);
        log::info!("drive: kept the older {} as {}", path.display(), dest.display());
    }
}

fn copy_mtime(from: &Path, to: &Path, mtime_ns: u128) -> io::Result<()> {
    plain_copy(from, to)?;
    set_mtime(to, mtime_ns);
    Ok(())
}

fn write_bytes(bytes: &[u8], to: &Path, mtime_ns: u128) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(to)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_mtime(to, mtime_ns);
    Ok(())
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

fn set_mtime(path: &Path, mtime_ns: u128) {
    let secs = (mtime_ns / 1_000_000_000) as u64;
    let nanos = (mtime_ns % 1_000_000_000) as u32;
    let when = UNIX_EPOCH + Duration::new(secs, nanos);
    if let Ok(file) = File::options().write(true).open(path) {
        let _ = file.set_modified(when);
    }
}

/// `GLUE5_GOOGLE_DRIVE` wins. A file with no `google_drive` field is off.
/// `GLUE5_GOOGLE_CLIENT_ID` wins over the platform's saved client id.
fn enabled_from_json(json: &[u8]) -> (bool, String, String) {
    let value = serde_json::from_slice::<Value>(json).ok();
    let client_id = client_id_from(value.as_ref());
    let client_secret = client_secret_from(value.as_ref());
    if !cfg!(target_vendor = "apple") {
        return (false, client_id, client_secret);
    }
    let enabled = match std::env::var("GLUE5_GOOGLE_DRIVE").ok().as_deref().map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("0" | "off" | "false") => false,
        Some("1" | "on" | "true") => true,
        _ => value.as_ref().and_then(|v| v.get("google_drive")).and_then(Value::as_bool).unwrap_or(false),
    };
    (enabled, client_id, client_secret)
}

/// Desktop clients send this on the token request when it is set. iOS clients
/// have no secret. An empty string omits the parameter.
fn with_secret(form: String, secret: &str) -> String {
    let secret = secret.trim();
    if secret.is_empty() || cfg!(target_os = "ios") {
        form
    } else {
        format!("{form}&client_secret={}", form_encode(secret))
    }
}

fn client_id_from(value: Option<&Value>) -> String {
    if let Ok(env) = std::env::var("GLUE5_GOOGLE_CLIENT_ID") {
        let env = env.trim();
        if !env.is_empty() {
            return env.to_owned();
        }
    }
    let key = if cfg!(target_os = "ios") { "google_client_id_ios" } else { "google_client_id_desktop" };
    let id =
        value.and_then(|v| v.get(key)).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("");
    if id.is_empty() { default_client_id().to_owned() } else { id.to_owned() }
}

fn client_secret_from(value: Option<&Value>) -> String {
    if cfg!(target_os = "ios") {
        return String::new();
    }
    if let Ok(env) = std::env::var("GLUE5_GOOGLE_CLIENT_SECRET") {
        let env = env.trim();
        if !env.is_empty() {
            return env.to_owned();
        }
    }
    value
        .and_then(|v| v.get("google_client_secret"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn device_keys() -> &'static [&'static str] {
    &["game_dir", "google_client_id_ios", "google_client_id_desktop", "google_client_secret"]
}

fn normalized_settings(bytes: &[u8]) -> Option<String> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    value.as_object()?;
    strip_device_fields(&mut value);
    Some(value.to_string())
}

fn settings_equal(local: &[u8], remote: &[u8]) -> bool {
    match (normalized_settings(local), normalized_settings(remote)) {
        (Some(local), Some(remote)) => local == remote,
        _ => local == remote,
    }
}

fn remote_has_device_fields(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else { return false };
    if value.get("game_dir").is_some_and(|v| !v.is_null()) {
        return true;
    }
    device_keys().iter().skip(1).any(|key| value.get(*key).and_then(Value::as_str).is_some_and(|s| !s.is_empty()))
}

/// Cloud copy: the same settings, without this device's game folder or client ids.
fn cloud_settings_bytes(local: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<Value>(local) else { return local.to_vec() };
    strip_device_fields(&mut value);
    serde_json::to_vec_pretty(&value).unwrap_or_else(|_| local.to_vec())
}

fn strip_device_fields(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else { return };
    obj.insert("game_dir".into(), Value::Null);
    for key in device_keys().iter().skip(1) {
        obj.insert((*key).into(), Value::String(String::new()));
    }
}

/// Local copy after a newer cloud file: cloud settings, this device's folder and client ids.
fn local_settings_bytes(remote: &[u8], previous_local: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<Value>(remote) else { return remote.to_vec() };
    let previous = serde_json::from_slice::<Value>(previous_local).ok();
    if let Some(obj) = value.as_object_mut()
        && let Some(previous) = previous.as_ref()
    {
        for key in device_keys() {
            if let Some(kept) = previous.get(*key) {
                obj.insert((*key).to_owned(), kept.clone());
            }
        }
    }
    serde_json::to_vec_pretty(&value).unwrap_or_else(|_| remote.to_vec())
}

fn encode_name(rel: &str) -> String {
    rel.replace('\\', "/").replace('/', &SLASH.to_string())
}

fn decode_name(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(name.replace(SLASH, "/"))
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    format!("{:x}", Md5::digest(bytes))
}

fn pkce() -> (String, String) {
    let mut buf = [0u8; 32];
    let _ = getrandom::fill(&mut buf);
    let verifier = base64url(&buf);
    use sha2::{Digest, Sha256};
    let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn random_token() -> String {
    let mut buf = [0u8; 16];
    let _ = getrandom::fill(&mut buf);
    base64url(&buf)
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | bytes[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    if i < bytes.len() {
        let mut n = (bytes[i] as u32) << 16;
        if i + 1 < bytes.len() {
            n |= (bytes[i + 1] as u32) << 8;
        }
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        }
    }
    out
}

fn form_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1.split('#').next()?;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(k) == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn auth_url(client_id: &str, redirect: &str, challenge: &str, state: &str) -> String {
    format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
        form_encode(client_id),
        form_encode(redirect),
        form_encode(SCOPE),
        form_encode(challenge),
        form_encode(state),
    )
}

#[cfg(any(test, target_os = "ios"))]
fn ios_scheme(client_id: &str) -> Option<String> {
    let prefix = client_id.strip_suffix(".apps.googleusercontent.com")?;
    if prefix.is_empty() || prefix.contains('/') {
        return None;
    }
    Some(format!("com.googleusercontent.apps.{prefix}"))
}

#[cfg(target_os = "ios")]
fn bundle_has_scheme(scheme: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let Some(plist) = exe.parent().map(|dir| dir.join("Info.plist")) else { return false };
    fs::read_to_string(plist).is_ok_and(|text| text.contains(scheme))
}

#[cfg(not(target_os = "ios"))]
fn loopback_sign_in(
    client_id: &str,
    client_secret: &str,
    verifier: &str,
    challenge: &str,
    state: &str,
) -> Result<(), String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("cannot listen for the sign-in redirect: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let url = auth_url(client_id, &redirect, challenge, state);
    open_browser(&url)?;
    listener.set_nonblocking(false).map_err(|e| e.to_string())?;
    let (mut stream, _) = listener.accept().map_err(|e| format!("the browser did not return: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(180))).ok();
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let first = request.lines().next().unwrap_or("");
    let target = first.split_whitespace().nth(1).unwrap_or("");
    let body = "Signed in. You can close this window and go back to Glue 5.";
    let html = format!("<!DOCTYPE html><title>Glue 5</title><p>{body}</p>");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes());
    exchange_from_redirect(target, client_id, client_secret, verifier, &redirect, state)
}

fn exchange_from_redirect(
    url: &str,
    client_id: &str,
    client_secret: &str,
    verifier: &str,
    redirect: &str,
    expect_state: &str,
) -> Result<(), String> {
    if let Some(error) = query_param(url, "error") {
        if error == "access_denied" {
            return Err("Sign-in was cancelled.".into());
        }
        return Err(format!("Google refused sign-in ({error})."));
    }
    let got = query_param(url, "state").unwrap_or_default();
    if got != expect_state {
        return Err("Sign-in came back with the wrong state. Try again.".into());
    }
    let code = query_param(url, "code").ok_or_else(|| "Sign-in did not return a code.".to_owned())?;
    let form = with_secret(
        format!(
            "client_id={}&code={}&code_verifier={}&grant_type=authorization_code&redirect_uri={}",
            form_encode(client_id),
            form_encode(&code),
            form_encode(verifier),
            form_encode(redirect),
        ),
        client_secret,
    );
    let value = post_form(TOKEN_URL, &form, None)?;
    store_token_response(&value, None).map(|_| ())
}

#[cfg(not(target_os = "ios"))]
fn open_browser(url: &str) -> Result<(), String> {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    result.map(|_| ()).map_err(|e| format!("cannot open the browser: {e}"))
}

fn fresh_access(client_id: &str, client_secret: &str) -> Result<String, String> {
    let Some(token) = ensure_token() else {
        return Err("Sign in to copy saves. Until then they stay on this device.".into());
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if now + 30 < token.expires_at && !token.access.is_empty() {
        return Ok(token.access);
    }
    if token.refresh.is_empty() {
        return Err("Sign in again. Google did not return a refresh token.".into());
    }
    let form = with_secret(
        format!(
            "client_id={}&grant_type=refresh_token&refresh_token={}",
            form_encode(client_id),
            form_encode(&token.refresh),
        ),
        client_secret,
    );
    let value = post_form(TOKEN_URL, &form, None)?;
    let stored = store_token_response(&value, Some(&token.refresh))?;
    Ok(stored.access)
}

fn store_token_response(value: &Value, previous_refresh: Option<&str>) -> Result<Token, String> {
    let access = value.get("access_token").and_then(Value::as_str).unwrap_or("").to_owned();
    if access.is_empty() {
        let message = value.get("error_description").and_then(Value::as_str).unwrap_or("Google did not return a token");
        return Err(message.to_owned());
    }
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| previous_refresh.map(str::to_owned))
        .unwrap_or_default();
    let expires_in = value.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let token = Token { access, refresh, expires_at: now + expires_in };
    save_token(&token)?;
    *TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.clone());
    TOKEN_LOADED.store(true, Ordering::Relaxed);
    Ok(token)
}

fn ensure_token() -> Option<Token> {
    if !TOKEN_LOADED.load(Ordering::Relaxed) {
        let loaded = load_token();
        *TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = loaded;
        TOKEN_LOADED.store(true, Ordering::Relaxed);
    }
    TOKEN.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn token_json(token: &Token) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "access_token": token.access,
        "refresh_token": token.refresh,
        "expires_at": token.expires_at,
    }))
    .unwrap_or_default()
}

fn token_from_bytes(bytes: &[u8]) -> Option<Token> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    Some(Token {
        access: value.get("access_token").and_then(Value::as_str).unwrap_or("").to_owned(),
        refresh: value.get("refresh_token").and_then(Value::as_str).unwrap_or("").to_owned(),
        expires_at: value.get("expires_at").and_then(Value::as_u64).unwrap_or(0),
    })
}

#[cfg(target_vendor = "apple")]
fn save_token(token: &Token) -> Result<(), String> {
    use security_framework::passwords::{delete_generic_password, set_generic_password};
    let _ = delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
    set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &token_json(token)).map_err(|e| format!("Keychain: {e}"))
}

#[cfg(not(target_vendor = "apple"))]
fn save_token(token: &Token) -> Result<(), String> {
    let Some(dir) = state_snapshot().map(|s| s.user_dir) else {
        return Err("the user directory is not set".into());
    };
    let path = dir.join(TOKEN_FILE);
    let mut file = File::create(&path).map_err(|e| e.to_string())?;
    file.write_all(&token_json(token)).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn load_token() -> Option<Token> {
    let bytes = security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).ok()?;
    token_from_bytes(&bytes)
}

#[cfg(not(target_vendor = "apple"))]
fn load_token() -> Option<Token> {
    let dir = state_snapshot().map(|s| s.user_dir)?;
    token_from_bytes(&fs::read(dir.join(TOKEN_FILE)).ok()?)
}

#[cfg(target_vendor = "apple")]
fn forget_token() {
    let _ = security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
}

#[cfg(not(target_vendor = "apple"))]
fn forget_token() {
    if let Some(dir) = state_snapshot().map(|s| s.user_dir) {
        let _ = fs::remove_file(dir.join(TOKEN_FILE));
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(timeout).timeout_read(timeout).timeout_write(timeout).build()
}

fn post_form(url: &str, form: &str, bearer: Option<&str>) -> Result<Value, String> {
    let mut req = agent(Duration::from_secs(20)).post(url).set("Content-Type", "application/x-www-form-urlencoded");
    if let Some(bearer) = bearer {
        req = req.set("Authorization", &format!("Bearer {bearer}"));
    }
    let response = req.send_string(form).map_err(http_error)?;
    let text = response.into_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

fn http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| v.get("error_description").and_then(Value::as_str).map(str::to_owned))
                })
                .unwrap_or(body);
            let message: String = message.chars().take(180).collect();
            if code == 401 {
                format!("Sign in again ({message}).")
            } else {
                format!("Google Drive returned {code}: {message}")
            }
        }
        ureq::Error::Transport(transport) => {
            format!("Google Drive isn't reachable ({transport}). Saves stay on this device.")
        }
    }
}

fn list_files(access: &str) -> Result<Vec<(String, RemoteFile)>, String> {
    let mut out = Vec::new();
    let mut page = String::new();
    for _ in 0..20 {
        let mut url = "https://www.googleapis.com/drive/v3/files?spaces=appDataFolder&pageSize=1000&fields=nextPageToken,files(id,name,md5Checksum,modifiedTime,appProperties)&q=trashed%3Dfalse".to_owned();
        if !page.is_empty() {
            url.push_str("&pageToken=");
            url.push_str(&form_encode(&page));
        }
        let response = agent(Duration::from_secs(20))
            .get(&url)
            .set("Authorization", &format!("Bearer {access}"))
            .call()
            .map_err(http_error)?;
        let value: Value =
            serde_json::from_str(&response.into_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        if let Some(files) = value.get("files").and_then(Value::as_array) {
            for file in files {
                let Some(id) = file.get("id").and_then(Value::as_str) else { continue };
                let Some(name) = file.get("name").and_then(Value::as_str) else { continue };
                let md5 = file.get("md5Checksum").and_then(Value::as_str).unwrap_or("").to_owned();
                let mtime_ns = file
                    .pointer("/appProperties/mtime_ns")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok())
                    .or_else(|| file.get("modifiedTime").and_then(Value::as_str).and_then(rfc3339_ns))
                    .unwrap_or(0);
                out.push((name.to_owned(), RemoteFile { id: id.to_owned(), md5, mtime_ns }));
            }
        }
        match value.get("nextPageToken").and_then(Value::as_str) {
            Some(token) if !token.is_empty() => page = token.to_owned(),
            _ => break,
        }
    }
    Ok(out)
}

fn rfc3339_ns(text: &str) -> Option<u128> {
    let t: jiff::Timestamp = text.parse().ok()?;
    let secs = t.as_second();
    if secs < 0 {
        return None;
    }
    Some((secs as u128) * 1_000_000_000 + t.subsec_nanosecond().max(0) as u128)
}

fn download(access: &str, id: &str) -> Result<Vec<u8>, String> {
    let url = format!("https://www.googleapis.com/drive/v3/files/{id}?alt=media");
    let response = agent(Duration::from_secs(20))
        .get(&url)
        .set("Authorization", &format!("Bearer {access}"))
        .call()
        .map_err(http_error)?;
    let mut bytes = Vec::new();
    response.into_reader().take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("larger than 32 MiB".into());
    }
    Ok(bytes)
}

fn create_file(access: &str, name: &str, bytes: &[u8], mtime_ns: u128) -> Result<(), String> {
    let meta = json!({
        "name": name,
        "parents": ["appDataFolder"],
        "appProperties": {"mtime_ns": mtime_ns.to_string()},
    });
    upload(access, "POST", "https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart", &meta, bytes)
}

fn update_file(access: &str, id: &str, name: &str, bytes: &[u8], mtime_ns: u128) -> Result<(), String> {
    let meta = json!({"name": name, "appProperties": {"mtime_ns": mtime_ns.to_string()}});
    let url = format!("https://www.googleapis.com/upload/drive/v3/files/{id}?uploadType=multipart");
    upload(access, "PATCH", &url, &meta, bytes)
}

fn upload(access: &str, method: &str, url: &str, meta: &Value, bytes: &[u8]) -> Result<(), String> {
    let boundary = format!("glue5{}", random_token());
    let meta = serde_json::to_string(meta).map_err(|e| e.to_string())?;
    let mut body = Vec::new();
    write!(
        body,
        "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    body.extend_from_slice(bytes);
    write!(body, "\r\n--{boundary}--\r\n").map_err(|e| e.to_string())?;
    agent(Duration::from_secs(20))
        .request(method, url)
        .set("Authorization", &format!("Bearer {access}"))
        .set("Content-Type", &format!("multipart/related; boundary={boundary}"))
        .send_bytes(&body)
        .map(|_| ())
        .map_err(http_error)
}

#[cfg(target_os = "ios")]
mod ios {
    use std::sync::mpsc;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
    use objc2::{class, msg_send};
    use objc2_foundation::{NSString, NSURL};

    #[link(name = "AuthenticationServices", kind = "framework")]
    #[link(name = "UIKit", kind = "framework")]
    unsafe extern "C" {}

    pub fn start(url: &str, scheme: &str, done: impl FnOnce(Result<String, String>) + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        match start_session(url, scheme, tx) {
            Ok(()) => {
                let _ = std::thread::Builder::new().name("drive-auth".into()).spawn(move || {
                    let callback = rx.recv().unwrap_or_else(|_| Err("Sign-in was cancelled.".into()));
                    done(callback);
                });
            }
            Err(error) => done(Err(error)),
        }
    }

    fn start_session(url: &str, scheme: &str, tx: mpsc::Sender<Result<String, String>>) -> Result<(), String> {
        // SAFETY: UIKit and AuthenticationServices, called from the main thread
        // (the settings button). The session is retained until the callback.
        unsafe { start_session_inner(url, scheme, tx) }
    }

    unsafe fn start_session_inner(
        url: &str,
        scheme: &str,
        tx: mpsc::Sender<Result<String, String>>,
    ) -> Result<(), String> {
        let Some(class) = AnyClass::get(c"ASWebAuthenticationSession") else {
            return Err("AuthenticationServices is not linked into this app.".into());
        };
        let ns_url =
            NSURL::URLWithString(&NSString::from_str(url)).ok_or_else(|| "the sign-in URL is not valid".to_owned())?;
        let scheme = NSString::from_str(scheme);
        let tx = std::sync::Mutex::new(Some(tx));
        let block = RcBlock::new(move |callback: *mut AnyObject, error: *mut AnyObject| {
            let result = unsafe { callback_result(callback, error) };
            if let Some(tx) = tx.lock().ok().and_then(|mut g| g.take()) {
                let _ = tx.send(result);
            }
        });
        let anchor = presentation_anchor()?;
        // alloc returns +1. init consumes that and returns +1. One Retained owns it.
        let session: *mut AnyObject = unsafe { msg_send![class, alloc] };
        let session: *mut AnyObject = unsafe {
            msg_send![session, initWithURL: &*ns_url, callbackURLScheme: &*scheme, completionHandler: &*block]
        };
        let Some(session) = (unsafe { Retained::from_raw(session) }) else {
            return Err("could not open the sign-in window".into());
        };
        let _: () = unsafe { msg_send![&*session, setPresentationContextProvider: &*anchor] };
        let _: () = unsafe { msg_send![&*session, setPrefersEphemeralWebBrowserSession: Bool::new(false)] };
        let started: Bool = unsafe { msg_send![&*session, start] };
        if started.as_bool() {
            // The session retains the block and the anchor. Leak our handles
            // so they outlive this function. Retained is not Send, so a static
            // cannot hold it. One leak per successful sign-in.
            std::mem::forget(block);
            std::mem::forget(anchor);
            std::mem::forget(session);
            Ok(())
        } else {
            Err("could not open the sign-in window".into())
        }
    }

    unsafe fn callback_result(callback: *mut AnyObject, error: *mut AnyObject) -> Result<String, String> {
        if !callback.is_null() {
            // absoluteString is autoreleased. Retained retains it for the copy.
            let text: Retained<NSString> = unsafe { msg_send![callback, absoluteString] };
            return Ok(text.to_string());
        }
        if error.is_null() {
            return Err("Sign-in was cancelled.".into());
        }
        let text: Retained<NSString> = unsafe { msg_send![error, localizedDescription] };
        let message = text.to_string();
        if message.to_ascii_lowercase().contains("cancel") {
            Err("Sign-in was cancelled.".into())
        } else {
            Err(message)
        }
    }

    fn presentation_anchor() -> Result<Retained<AnyObject>, String> {
        static READY: std::sync::Once = std::sync::Once::new();
        READY.call_once(|| {
            unsafe { register_anchor() };
        });
        let Some(class) = AnyClass::get(c"Glue5WebAuthAnchor") else {
            return Err("cannot present the sign-in window".into());
        };
        let object: *mut AnyObject = unsafe { msg_send![class, alloc] };
        let object: *mut AnyObject = unsafe { msg_send![object, init] };
        unsafe { Retained::from_raw(object) }.ok_or_else(|| "cannot present the sign-in window".into())
    }

    /// SAFETY: registers one NSObject subclass whose method matches
    /// `ASWebAuthenticationPresentationContextProviding`.
    unsafe fn register_anchor() {
        unsafe extern "C-unwind" fn anchor(
            _this: *mut AnyObject,
            _sel: Sel,
            _session: *mut AnyObject,
        ) -> *mut AnyObject {
            let app: *mut AnyObject = unsafe { msg_send![class!(UIApplication), sharedApplication] };
            if app.is_null() {
                return std::ptr::null_mut();
            }
            let window: *mut AnyObject = unsafe { msg_send![app, keyWindow] };
            if !window.is_null() {
                return window;
            }
            let windows: *mut AnyObject = unsafe { msg_send![app, windows] };
            if windows.is_null() {
                return std::ptr::null_mut();
            }
            unsafe { msg_send![windows, firstObject] }
        }
        let Some(superclass) = AnyClass::get(c"NSObject") else { return };
        let Some(mut builder) = ClassBuilder::new(c"Glue5WebAuthAnchor", superclass) else { return };
        let sel = Sel::register(c"presentationAnchorForWebAuthenticationSession:");
        let anchor: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> *mut AnyObject = anchor;
        unsafe { builder.add_method(sel, anchor) };
        if let Some(proto) = objc2::runtime::AnyProtocol::get(c"ASWebAuthenticationPresentationContextProviding") {
            let _ = builder.add_protocol(proto);
        }
        builder.register();
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
        assert!(!synced(".google-token"));
        assert!(!synced("Documents/Rockstar Games/GTA V/Benchmarks/Benchmark-1.txt"));
        assert!(!synced("Glue5 Diagnostics/diagnostics.log"));
        assert!(!synced("cloud/note.glue5-conflict-1-local.bak"));
        assert!(!synced("Documents/Rockstar Games/GTA V/settings.xml.bak"));
    }

    #[test]
    fn settings_keep_the_local_game_folder_and_client_ids() {
        let local = br#"{"game_dir":"/iphone/Documents/game","google_drive":true,"google_client_id_ios":"ios.apps.googleusercontent.com","google_client_id_desktop":"desktop","google_client_secret":"local-secret","fog":true}"#;
        let remote = br#"{"game_dir":"/mac/game","google_drive":true,"google_client_id_ios":"other","google_client_id_desktop":"mac","google_client_secret":"remote-secret","fog":false}"#;
        assert!(!settings_equal(local, remote));
        let merged = local_settings_bytes(remote, local);
        let parsed: Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(parsed["game_dir"], "/iphone/Documents/game");
        assert_eq!(parsed["fog"], false);
        assert_eq!(parsed["google_client_id_ios"], "ios.apps.googleusercontent.com");
        assert_eq!(parsed["google_client_id_desktop"], "desktop");
        assert_eq!(parsed["google_client_secret"], "local-secret");
        let cloud = cloud_settings_bytes(local);
        let parsed: Value = serde_json::from_slice(&cloud).unwrap();
        assert!(parsed["game_dir"].is_null());
        assert_eq!(parsed["google_client_id_ios"], "");
        assert_eq!(parsed["google_client_secret"], "");
        assert_eq!(parsed["fog"], true);
        let again = cloud_settings_bytes(&merged);
        assert!(settings_equal(&merged, &again));
    }

    #[test]
    fn names_round_trip_and_pkce_challenge_is_s256() {
        let rel = "Documents/Rockstar Games/GTA V/settings.xml";
        assert_eq!(decode_name(&encode_name(rel)).as_deref(), Some(rel));
        assert!(!encode_name(rel).contains('/'));
        let (verifier, challenge) = pkce();
        assert!(verifier.len() >= 43);
        use sha2::{Digest, Sha256};
        assert_eq!(challenge, base64url(&Sha256::digest(verifier.as_bytes())));
        assert_eq!(
            ios_scheme("123-abc.apps.googleusercontent.com").as_deref(),
            Some("com.googleusercontent.apps.123-abc")
        );
        assert_eq!(
            ios_scheme(DEFAULT_IOS_CLIENT_ID).as_deref(),
            Some("com.googleusercontent.apps.566965852987-ep0g7185pbbhn83idg5mk5ikjij8q3fu")
        );
        let redirect = "com.googleusercontent.apps.566965852987-ep0g7185pbbhn83idg5mk5ikjij8q3fu:/oauth2redirect";
        assert!(auth_url(DEFAULT_IOS_CLIENT_ID, redirect, "c", "s").contains(&form_encode(redirect)));
        if std::env::var_os("GLUE5_GOOGLE_CLIENT_ID").is_none() {
            assert_eq!(client_id_from(None), default_client_id());
            assert_eq!(client_id_from(Some(&serde_json::json!({"google_client_id_desktop": ""}))), default_client_id());
        }
        assert!(ios_scheme("desktop-client.apps.googleusercontent.com").is_some());
        assert!(ios_scheme("not-a-google-client").is_none());
    }

    #[test]
    fn reconcile_backs_up_the_older_copy() {
        let root = std::env::temp_dir().join(format!("glue5-drive-test-{}", std::process::id()));
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
        set_mtime(&local.join(save), 1_700_000_000_000_000_000);
        set_mtime(&remote.join(save), 1_700_000_100_000_000_000);
        let counts = reconcile(&local, &remote, Way::Both, &[]);
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
}
