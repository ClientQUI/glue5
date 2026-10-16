//! Where things are, per platform.
//!
//! - The user directory (settings, saves, caches) is SDL's per-user
//!   application directory. The org passed to `SDL_GetPrefPath` is empty and
//!   the app name is `glue5`, so the folder is `Application Support/glue5`
//!   (iOS and iPadOS share this; both are `target_os = "ios"`):
//!   - iOS and iPadOS: `<sandbox>/Library/Application Support/glue5/`,
//!     where the sandbox is
//!     `/var/mobile/Containers/Data/Application/<UUID>/`.
//!   - macOS, not sandboxed: `~/Library/Application Support/glue5/`.
//!     A sandboxed Mac app would use
//!     `~/Library/Containers/com.lennertwilhelm.gta5offline/Data/Library/Application Support/glue5/`
//!     instead. This build does not enable the App Sandbox.
//!   - Linux: `~/.local/share/glue5`. Windows: `%APPDATA%\glue5`.
//!     Android: the app's sandbox.
//! - The game (`b/<build>/` and `data/`, ~21 GB) is never part of the app: it
//!   is wherever the user put it. Mobile defaults are user-visible places
//!   (iOS: the app's Documents, through Finder or Files; Android: the app's
//!   external files directory).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const MOBILE: bool = cfg!(any(target_os = "ios", target_os = "android"));

pub fn user_dir() -> Result<PathBuf> {
    sdl3::filesystem::get_pref_path("", "glue5").map_err(|e| anyhow::anyhow!("no user directory: {e}"))
}

/// A place the user can retrieve after a freeze. On iOS this is deliberately
/// in Documents rather than the private preference directory: with file
/// sharing enabled, Files/Finder exposes it without Xcode or a crash report.
pub fn diagnostics_dir(user_dir: &Path) -> PathBuf {
    #[cfg(target_os = "ios")]
    if let Ok(documents) = sdl3::filesystem::get_user_folder(sdl3::filesystem::Folder::DOCUMENTS) {
        return documents.join("Glue5 Diagnostics");
    }
    user_dir.join("Glue5 Diagnostics")
}

/// Where the game is looked for when nothing says otherwise.
pub fn default_game_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "ios") {
        if let Ok(docs) = sdl3::filesystem::get_user_folder(sdl3::filesystem::Folder::DOCUMENTS) {
            dirs.push(docs.join("game"));
        }
    } else if cfg!(target_os = "android") {
        #[cfg(target_os = "android")]
        if let Some(ext) = android_external_dir() {
            dirs.push(ext.join("game"));
        }
    } else {
        // A distributed Mac app can sit beside an external game directory.
        // SDL's base path is inside the bundle, not beside the .app.
        #[cfg(target_os = "macos")]
        if let Ok(exe) = std::env::current_exe()
            && let Some(bundle) = exe.parent().and_then(Path::parent).and_then(Path::parent)
            && bundle.extension().is_some_and(|ext| ext == "app")
            && let Some(parent) = bundle.parent()
        {
            dirs.push(parent.join("game"));
        }
        // Next to the executable, the repository's ./game (target/<profile>/glue5),
        // and the working directory.
        if let Ok(base) = sdl3::filesystem::get_base_path() {
            dirs.push(base.join("game"));
            dirs.push(base.join("../../game"));
        }
        dirs.push(PathBuf::from("game"));
    }
    dirs
}

#[cfg(target_os = "android")]
fn android_external_dir() -> Option<PathBuf> {
    let p = unsafe { sdl3::sys::system::SDL_GetAndroidExternalStoragePath() };
    (!p.is_null()).then(|| PathBuf::from(unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()))
}

/// Where the game's files go, for the start page when they are missing.
pub fn where_the_game_goes() -> &'static str {
    if cfg!(target_os = "ios") {
        "Copy the game folder into the app's files, named game: Finder (iPhone › Files › Glue 5) \
         or the Files app (On My iPhone › Glue 5). This page looks again when the app comes back."
    } else if cfg!(target_os = "android") {
        "Copy the game folder, named game, into Android/data/<app>/files/ on the device."
    } else {
        "Choose the game folder, or start glue5 with its path."
    }
}

/// What the game folder holds.
pub fn game_layout() -> String {
    format!("game/\n  b/{}/shaders/\n  data/", glue5_runtime::GAME_BUILD)
}

pub fn canonical(dir: &Path) -> Result<PathBuf> {
    dir.canonicalize().with_context(|| format!("{}", dir.display()))
}
