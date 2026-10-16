//! The application's own settings, in `glue5.json` in the user directory.
//! Graphics quality is the game's (its menu, Settings › Graphics, saved in
//! its `settings.xml`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::MOBILE;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The game directory, once found or chosen.
    pub game_dir: Option<PathBuf>,
    /// Smaller engine pools, and the game's graphics settings lowered once
    /// (the browser build's `?low=1`).
    pub low_memory: bool,
    /// Whether the low-memory preset is in the game's settings (written at
    /// the first start with `low_memory`; the game's menu changes it after).
    pub low_preset_written: bool,
    /// The game's resolution as a fraction of the window's, per side (the
    /// back buffer follows the window). Chosen for the screen at the first
    /// start ([`Settings::for_screen`]).
    pub render_scale: Option<f32>,
    /// Fixed input height, aspect follows the display. None keeps legacy % scaling.
    pub render_lines: Option<u32>,
    /// MetalFX output height before the final display blit. None: native display.
    pub upscale_lines: Option<u32>,
    pub scaling_custom: bool,
    pub world_preset: WorldPreset,
    pub world: WorldDetail,
    pub texture_detail: TextureDetail,
    pub resource_budget: ResourceBudget,
    pub thermal_protection: bool,
    pub verbose_logs: bool,
    /// On phones, adjust the back buffer gradually when frame pacing falls
    /// behind or has headroom. This changes render cost without touching the
    /// game's texture/streaming quality settings.
    pub adaptive_graphics: bool,
    pub vsync: bool,
    /// At most this many frames per second; 0: no limit of the engine's
    /// (the display's refresh rate with VSync).
    pub max_fps: u32,
    /// How the game's frames are scaled up to the window (what the GPU has
    /// of it; the bicubic filter otherwise).
    pub upscaler: Upscaling,
    /// How much FSR sharpens its image, 0 to 1.
    pub sharpness: f32,
    /// A frame made between each two of the game's (MetalFX Temporal, on
    /// macOS and iOS 26): twice the frames shown, half a frame more of
    /// latency.
    pub frame_generation: bool,
    /// The performance overlay over the game, and how the application's
    /// own shows.
    pub overlay: PerfOverlay,
    pub overlay_detail: OverlayDetail,
    pub overlay_corner: Corner,
    /// Distance fog over the world. Near surfaces are never tinted.
    pub fog: bool,
    /// Where fog is fully mixed, in metres. 0 uses the view distance.
    pub fog_distance: f32,
    /// Placed LOD stand-ins and trees stop at this distance. 0 disables the cutoff.
    pub view_distance: f32,
    /// Decals, debris and bushes stop at this distance. 0 disables that culling.
    pub detail_distance: f32,
    /// Tree and bush cards stop at this distance. 0 keeps the cards.
    pub tree_distance: f32,
    /// Shadow casters stop at this distance. 0 disables that culling.
    pub shadow_distance: f32,
    /// Extra render scale while the device is cool. 1 keeps the chosen scale.
    pub render_boost: f32,
    /// Where threads sit. Hybrid is the recommended one: performance cores
    /// while loading or hitching, efficiency cores once play is steady.
    /// Audio stays immediate in every choice. `GLUE5_EFFICIENCY` forces one.
    pub efficiency: Efficiency,
    pub touch: TouchMode,
    pub touch_opacity: f32,
    pub touch_size: f32,
    /// The Taptic Engine taps under pressed touch controls and plays the game's rumble (iPhone).
    pub haptics: bool,
    /// Copy saves and these settings through Google Drive on iPhone, iPad and
    /// Mac. The game folder stays on this device. Off, signed out, or offline
    /// leaves the files local.
    #[serde(default)]
    pub google_drive: bool,
    /// iOS OAuth client id (`….apps.googleusercontent.com`). Not a secret.
    /// Empty uses `GLUE5_GOOGLE_CLIENT_ID`.
    #[serde(default)]
    pub google_client_id_ios: String,
    /// Desktop OAuth client id, for the Mac. Not a secret. Empty uses
    /// `GLUE5_GOOGLE_CLIENT_ID`.
    #[serde(default)]
    pub google_client_id_desktop: String,
    /// The game starts in the sandbox (the map, no story) rather than the
    /// story: the start page's last choice.
    pub sandbox: bool,
    /// GTA Online was the start page's last choice.
    pub online: bool,
    /// The player's name online (made up when GTA Online is first opened).
    pub online_name: String,
    /// A private lobby rather than everyone's, and its code (kept while the
    /// public one is chosen).
    pub online_private: bool,
    pub online_lobby: String,
    /// Settings saved before the render scale: the back buffer's height.
    #[serde(skip_serializing)]
    render_height: Option<u32>,
    /// Settings saved before the choice of upscalers: MetalFX's spatial one
    /// on or off.
    #[serde(skip_serializing)]
    metalfx: Option<bool>,
    /// Settings saved before the choice of overlays: Apple's HUD on or off.
    #[serde(skip_serializing)]
    metal_hud: Option<bool>,
    /// Settings saved before the single Efficiency choice.
    #[serde(skip_serializing)]
    efficiency_cores: Option<bool>,
    #[serde(skip_serializing)]
    efficiency_mode: Option<bool>,
}

/// Where the engine's threads sit. Hybrid is the one to leave on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Efficiency {
    Off,
    /// Performance cores for loads and hitches, efficiency cores in steady play.
    Hybrid,
    /// Frame threads stay at utility. About 20 frames per second.
    #[serde(rename = "full-e", alias = "full")]
    FullE,
}

/// The performance overlay over the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PerfOverlay {
    Off,
    /// Apple's, drawn by Metal (shown from the launch it is on at).
    MetalHud,
    /// The application's (ui/perf.rs), on every platform.
    Glue5,
}

/// How much the application's overlay shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlayDetail {
    /// One line: frame rate, frame time, GPU time.
    Minimal,
    /// The frame times as a graph, and where they go.
    Standard,
    /// The busiest threads, the renderer's counts, resolution and memory too.
    Full,
}

/// A corner of the safe area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// How the game's frames are scaled up to the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Upscaling {
    Bicubic,
    /// MetalFX's spatial upscaler (Apple).
    #[serde(rename = "metalfx")]
    MetalFx,
    /// MetalFX's temporal upscaler (Apple).
    #[serde(rename = "metalfx-temporal")]
    MetalFxTemporal,
    /// AMD's FSR 2 upscaler (temporal, on GPUs with the features it needs).
    Fsr,
}

impl Upscaling {
    pub fn upscaler(self) -> glue5_gpu::Upscaler {
        match self {
            Upscaling::Bicubic => glue5_gpu::Upscaler::Bicubic,
            Upscaling::MetalFx => glue5_gpu::Upscaler::MetalFx,
            Upscaling::MetalFxTemporal => glue5_gpu::Upscaler::MetalFxTemporal,
            Upscaling::Fsr => glue5_gpu::Upscaler::Fsr,
        }
    }
}

/// When the on-screen controls show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TouchMode {
    /// On touch screens, while no controller (or keyboard) is in use.
    Auto,
    Always,
    Off,
}

/// Phones and tablets with less RAM start with the low-memory profile (the
/// game takes about 4 GB at the normal one).
const LOW_MEMORY_RAM_MB: i32 = 7000;

/// The render scale's range, in steps of 5%.
pub const RENDER_SCALES: std::ops::RangeInclusive<f32> = 0.2..=1.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorldPreset {
    #[default]
    Game,
    Eco,
    Balanced,
    Detail,
    Custom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextureDetail {
    #[default]
    Original,
    Balanced,
    Compact,
    Minimal,
}

impl TextureDetail {
    pub fn max_dimension(self) -> u32 {
        match self {
            Self::Original => 0,
            Self::Balanced => 1024,
            Self::Compact => 512,
            Self::Minimal => 256,
        }
    }
}

/// Keep this independent of resolution, texture tails and the bundled legacy
/// low-memory graphics preset. Reduced budgets require a game restart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceBudget {
    #[default]
    Auto,
    Balanced,
    Compact,
    Compatibility,
}

impl ResourceBudget {
    pub fn mb(self) -> u32 {
        match self {
            Self::Auto => 0,
            Self::Balanced => 1536,
            Self::Compact => 1280,
            Self::Compatibility => 2048,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldDetail {
    pub lod: f32,
    pub ped_lod: f32,
    pub vehicle_lod: f32,
    pub density: f32,
    pub ped_variety: f32,
    pub vehicle_variety: f32,
}

impl Default for WorldDetail {
    fn default() -> Self {
        Self::preset(WorldPreset::Balanced)
    }
}

impl WorldDetail {
    pub fn preset(p: WorldPreset) -> Self {
        let (lod, bias, density, variety) = match p {
            WorldPreset::Eco => (0.0, 0.0, 0.0, 0.15),
            WorldPreset::Detail => (0.5, 0.35, 0.5, 0.7),
            _ => (0.2, 0.1, 0.2, 0.35),
        };
        Self { lod, ped_lod: bias, vehicle_lod: bias, density, ped_variety: variety, vehicle_variety: variety }
    }

    pub fn xml_values(self) -> Vec<(&'static str, String)> {
        [
            ("LodScale", self.lod),
            ("PedLodBias", self.ped_lod),
            ("VehicleLodBias", self.vehicle_lod),
            ("CityDensity", self.density),
            ("PedVarietyMultiplier", self.ped_variety),
            ("VehicleVarietyMultiplier", self.vehicle_variety),
        ]
        .into_iter()
        .map(|(name, value)| (name, format!("{:.6}", finite_unit(value))))
        .collect()
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() { value.clamp(0.0, 1.0) } else { 0.0 }
}

/// Built-in distances, in metres. The same numbers as the renderer's defaults.
pub fn default_view_distance() -> f32 {
    if MOBILE { 260.0 } else { 420.0 }
}
pub fn default_detail_distance() -> f32 {
    if MOBILE { 50.0 } else { 80.0 }
}
pub fn default_tree_distance() -> f32 {
    if MOBILE { 130.0 } else { 200.0 }
}
pub fn default_shadow_distance() -> f32 {
    if MOBILE { 120.0 } else { 250.0 }
}

/// `0` disables the cutoff. A value on the built-in default stays `None`,
/// so heat can still shorten it. Anything else is used as written.
fn distance_override(value: f32, builtin: f32) -> Option<f32> {
    if !value.is_finite() || value <= 0.0 {
        Some(0.0)
    } else if (value - builtin).abs() < 0.5 {
        None
    } else {
        Some(value)
    }
}

/// The environment's efficiency choice, when one of the variables sets it.
pub fn efficiency_forced() -> Option<Efficiency> {
    glue5_runtime::qos::forced_mode().map(|mode| match mode {
        glue5_runtime::sched::Mode::Off => Efficiency::Off,
        glue5_runtime::sched::Mode::Hybrid => Efficiency::Hybrid,
        glue5_runtime::sched::Mode::FullE => Efficiency::FullE,
    })
}

fn durable_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    glue5_runtime::qos::lower(glue5_runtime::qos::Qos::Utility, || durable_write_at(path, bytes))
}

fn durable_write_at(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Heights, not fixed 16:9 widths: phone aspect ratios are preserved.
pub const SCALE_PAIRS: &[(u32, Option<u32>, &str)] = &[
    (360, Some(720), "360p → 720p"),
    (480, Some(720), "480p → 720p"),
    (540, Some(900), "540p → 900p"),
    (540, Some(1080), "540p → 1080p"),
    (720, Some(1080), "720p → 1080p"),
    (720, Some(1440), "720p → 1440p"),
    (720, None, "720p → Display"),
    (1080, None, "1080p → Display"),
];

impl Default for Settings {
    fn default() -> Self {
        Settings {
            game_dir: None,
            low_memory: MOBILE && sdl3::cpuinfo::system_ram() < LOW_MEMORY_RAM_MB,
            low_preset_written: false,
            render_scale: None,
            render_lines: None,
            upscale_lines: None,
            scaling_custom: false,
            world_preset: WorldPreset::Game,
            world: WorldDetail::default(),
            texture_detail: TextureDetail::Original,
            resource_budget: ResourceBudget::Auto,
            thermal_protection: MOBILE,
            verbose_logs: false,
            adaptive_graphics: MOBILE,
            vsync: true,
            // Conservative engine-frame budget; generated frames are extra.
            max_fps: if MOBILE { 30 } else { 0 },
            upscaler: Upscaling::MetalFx,
            sharpness: 0.5,
            frame_generation: false,
            overlay: PerfOverlay::Off,
            overlay_detail: OverlayDetail::Standard,
            overlay_corner: Corner::TopLeft,
            fog: true,
            fog_distance: 0.0,
            view_distance: default_view_distance(),
            detail_distance: default_detail_distance(),
            tree_distance: default_tree_distance(),
            shadow_distance: default_shadow_distance(),
            render_boost: 1.0,
            efficiency: Efficiency::Hybrid,
            touch: TouchMode::Auto,
            touch_opacity: 0.6,
            touch_size: 1.0,
            haptics: true,
            google_drive: false,
            google_client_id_ios: String::new(),
            google_client_id_desktop: String::new(),
            sandbox: false,
            online: false,
            online_name: String::new(),
            online_private: false,
            online_lobby: String::new(),
            render_height: None,
            metalfx: None,
            metal_hud: None,
            efficiency_cores: None,
            efficiency_mode: None,
        }
    }
}

/// A name for a new online player: "Player" and four digits.
pub fn default_online_name() -> String {
    format!("Player{:04}", random() % 10000)
}

/// A new private lobby's code: six letters and digits that do not look
/// alike, as "K7Q-29F".
pub fn new_lobby_code() -> String {
    const SYMBOLS: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut n = random();
    let mut code = String::new();
    for i in 0..6 {
        if i == 3 {
            code.push('-');
        }
        code.push(SYMBOLS[(n % SYMBOLS.len() as u64) as usize] as char);
        n /= SYMBOLS.len() as u64;
    }
    code
}

/// A lobby's code as players compare it: its letters and digits, upper
/// case ("k7q 29f" is "K7Q-29F").
pub fn lobby_key(code: &str) -> String {
    code.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect()
}

/// `GLUE5_GOOGLE_DRIVE=0` or `1` overrides the toggle. `None` means the toggle wins.
pub fn drive_forced() -> Option<bool> {
    match std::env::var("GLUE5_GOOGLE_DRIVE").ok().as_deref().map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("0" | "off" | "false") => Some(false),
        Some("1" | "on" | "true") => Some(true),
        _ => None,
    }
}

/// `GLUE5_GOOGLE_CLIENT_ID`, when it is set and not blank.
pub fn google_client_id_from_env() -> Option<String> {
    std::env::var("GLUE5_GOOGLE_CLIENT_ID").ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish()
}

impl Settings {
    /// The lobby GTA Online plays in: a private one's key, or empty for
    /// everyone's.
    pub fn lobby(&self) -> String {
        if self.online_private { lobby_key(&self.online_lobby) } else { String::new() }
    }

    fn file(user_dir: &Path) -> PathBuf {
        user_dir.join("glue5.json")
    }

    pub fn load(user_dir: &Path) -> Settings {
        let mut settings = match std::fs::read(Self::file(user_dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!("ignoring unreadable settings: {e}");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        if settings.metalfx.take() == Some(false) {
            settings.upscaler = Upscaling::Bicubic;
        }
        if settings.metal_hud.take() == Some(true) {
            settings.overlay = PerfOverlay::MetalHud;
        }
        let full = settings.efficiency_mode.take();
        let cores = settings.efficiency_cores.take();
        if full == Some(true) {
            settings.efficiency = Efficiency::FullE;
        } else if cores == Some(false) {
            settings.efficiency = Efficiency::Off;
        }
        settings
    }

    pub fn save(&self, user_dir: &Path) {
        Self::save_snapshot(user_dir, &self.snapshot());
    }

    /// Whether saves should be copied through Google Drive. Never off Apple:
    /// the sign-in window is the Mac browser or the iOS authentication session.
    pub fn drive_enabled(&self) -> bool {
        if !cfg!(target_vendor = "apple") {
            return false;
        }
        drive_forced().unwrap_or(self.google_drive)
    }

    /// The OAuth client id for this device. The environment variable wins.
    pub fn google_client_id(&self) -> String {
        if let Some(id) = google_client_id_from_env() {
            return id;
        }
        if cfg!(target_os = "ios") { self.google_client_id_ios.clone() } else { self.google_client_id_desktop.clone() }
    }

    /// JSON for the lifecycle watch, which cannot borrow `Settings`.
    pub fn snapshot(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).unwrap_or_default()
    }

    /// Writes and flushes settings. Used when the app backgrounds, where a
    /// later `save` would not run.
    pub fn save_snapshot(user_dir: &Path, json: &[u8]) {
        if json.is_empty() {
            return;
        }
        if let Err(e) = durable_write(&Self::file(user_dir), json) {
            log::warn!("cannot save settings: {e}");
        }
        glue5_runtime::drive::on_settings_saved(user_dir, json);
    }

    /// The distance, fog, resolution and thermal options the graphics menu owns.
    pub fn reset_distance_options(&mut self, window_height: u32) {
        let fresh = Settings::default();
        self.fog = fresh.fog;
        self.fog_distance = fresh.fog_distance;
        self.view_distance = fresh.view_distance;
        self.detail_distance = fresh.detail_distance;
        self.tree_distance = fresh.tree_distance;
        self.shadow_distance = fresh.shadow_distance;
        self.render_boost = fresh.render_boost;
        self.thermal_protection = fresh.thermal_protection;
        self.efficiency = fresh.efficiency;
        self.adaptive_graphics = fresh.adaptive_graphics;
        let lines = if MOBILE { 720.0 } else { 1080.0 };
        self.render_scale = Some(round_scale(lines / window_height.max(1) as f32));
        self.render_lines = None;
        self.scaling_custom = false;
    }

    /// The efficiency choice the process should use, after the environment.
    pub fn efficiency_now(&self) -> Efficiency {
        efficiency_forced().unwrap_or(self.efficiency)
    }

    pub fn runtime_mode(&self) -> glue5_runtime::sched::Mode {
        match self.efficiency_now() {
            Efficiency::Off => glue5_runtime::sched::Mode::Off,
            Efficiency::Hybrid => glue5_runtime::sched::Mode::Hybrid,
            Efficiency::FullE => glue5_runtime::sched::Mode::FullE,
        }
    }

    /// Policy for the renderer. Built-in distances still shorten while the
    /// device is hot; a number chosen in the menu is used as written. 0
    /// turns that cutoff off.
    pub fn distance_policy(&self) -> glue5_gpu::Policy {
        let mode = std::env::var("GLUE5_DISTANT_DETAIL").ok();
        let kill = mode.as_deref() == Some("0");
        let boost = if self.render_boost.is_finite() && self.render_boost >= 1.0 { self.render_boost } else { 1.0 };
        glue5_gpu::Policy {
            enabled: !kill,
            mobile: MOBILE,
            aggressive: mode.as_deref() == Some("2"),
            shadow_override: distance_override(self.shadow_distance, default_shadow_distance()),
            detail_override: distance_override(self.detail_distance, default_detail_distance()),
            tree_override: distance_override(self.tree_distance, default_tree_distance()),
            view_override: distance_override(self.view_distance, default_view_distance()),
            fog: self.fog && !kill,
            fog_override: (self.fog_distance.is_finite() && self.fog_distance > 0.0).then_some(self.fog_distance),
            render_boost: boost,
        }
    }

    /// Picks the render scale, the first time, for a window `height` pixels
    /// tall: the height an older settings file had, else about 1080 lines on
    /// desktop and 720 on phones and tablets (what the game ran at before:
    /// its GPU time grows with the pixels, and phone screens have many).
    pub fn for_screen(&mut self, height: u32) {
        if self.render_scale.is_none() {
            let lines = self.render_height.take().unwrap_or(if MOBILE { 720 } else { 1080 });
            self.render_scale = Some(round_scale(lines as f32 / height.max(1) as f32));
        }
    }

    pub fn render_scale(&self) -> f32 {
        self.render_scale.unwrap_or(1.0)
    }

    pub fn input_size(&self, pixels: (u32, u32)) -> (u32, u32) {
        let scale =
            self.render_lines.map_or(self.render_scale(), |h| h.clamp(240, 2160) as f32 / pixels.1.max(1) as f32);
        let output = self.output_size(pixels);
        render_size(pixels, scale.min(output.1 as f32 / pixels.1.max(1) as f32), self.low_memory)
    }

    pub fn output_size(&self, pixels: (u32, u32)) -> (u32, u32) {
        if self.upscale_lines.is_none() || self.upscaler == Upscaling::Bicubic {
            return pixels;
        }
        let scale =
            self.upscale_lines.map_or(1.0, |h| h.clamp(240, 2160).min(pixels.1.max(1)) as f32 / pixels.1.max(1) as f32);
        render_size(pixels, scale, self.low_memory)
    }

    pub fn scaling_preset(&self) -> usize {
        if self.scaling_custom || self.upscaler == Upscaling::Bicubic {
            return SCALE_PAIRS.len();
        }
        SCALE_PAIRS
            .iter()
            .position(|(h, out, _)| self.render_lines == Some(*h) && self.upscale_lines == *out)
            .unwrap_or(SCALE_PAIRS.len())
    }

    pub fn set_scaling_preset(&mut self, index: usize) {
        if let Some(&(input, output, _)) = SCALE_PAIRS.get(index) {
            self.scaling_custom = false;
            self.render_lines = Some(input);
            self.upscale_lines = output;
            // Resolution presets must not turn Temporal/FG back into Spatial.
            if self.upscaler == Upscaling::Bicubic {
                self.upscaler = Upscaling::MetalFx;
            }
        } else {
            self.scaling_custom = true;
        }
    }

    pub fn performance_profile(&mut self, profile: u8) {
        self.max_fps = if profile == 2 { 60 } else { 30 };
        self.thermal_protection = true;
        self.verbose_logs = false;
        self.overlay = PerfOverlay::Off;
        self.set_scaling_preset(match profile {
            0 => 1,
            2 => 4,
            _ => 2,
        });
        self.world_preset = match profile {
            0 => WorldPreset::Eco,
            2 => WorldPreset::Detail,
            _ => WorldPreset::Balanced,
        };
        self.world = WorldDetail::preset(self.world_preset);
    }
}

/// A render scale in the range, to its 5% step.
pub fn round_scale(scale: f32) -> f32 {
    ((scale * 20.0).round() / 20.0).clamp(*RENDER_SCALES.start(), *RENDER_SCALES.end())
}

/// The game's back buffer for a window of `pixels` at `scale`, as the
/// browser build sizes it: even sides (the engine halves them for its
/// post-processing), at least the 320x240 the engine takes, at most 4K
/// (1440p with low memory: render targets the size of the screen are most
/// of the GPU's memory).
pub fn render_size((w, h): (u32, u32), scale: f32, low_memory: bool) -> (u32, u32) {
    let cap = if low_memory { (2560.0, 1440.0) } else { (3840.0, 2160.0) };
    let (w, h) = (w as f32 * scale, h as f32 * scale);
    let k = (cap.0 / w).min(cap.1 / h).min(1.0);
    let side = |v: f32, min: u32| ((v * k / 2.0).round() as u32 * 2).max(min);
    (side(w, 320), side(h, 240))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lobby_codes() {
        let code = new_lobby_code();
        assert_eq!(code.len(), 7, "{code}");
        assert_eq!(lobby_key(&code).len(), 6);
        assert_eq!(lobby_key(" k7q 29-f "), "K7Q29F");
        let mut s = Settings { online_lobby: "k7q-29f".into(), ..Settings::default() };
        assert_eq!(s.lobby(), "");
        s.online_private = true;
        assert_eq!(s.lobby(), "K7Q29F");
    }

    #[test]
    fn render_sizes() {
        assert_eq!(render_size((2560, 1440), 0.75, false), (1920, 1080));
        assert_eq!(render_size((2532, 1170), 0.6, false), (1520, 702));
        assert_eq!(render_size((300, 100), 0.5, false), (320, 240));
        assert_eq!(render_size((6016, 3384), 1.0, false), (3840, 2160));
        assert_eq!(render_size((6016, 3384), 1.0, true), (2560, 1440));
    }

    #[test]
    fn upscaler_and_overlay_from_older_settings() {
        let dir = std::env::temp_dir().join(format!("glue5-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("glue5.json"), r#"{"metalfx": false}"#).unwrap();
        assert_eq!(Settings::load(&dir).upscaler, Upscaling::Bicubic);
        std::fs::write(dir.join("glue5.json"), r#"{"metalfx": true}"#).unwrap();
        assert_eq!(Settings::load(&dir).upscaler, Upscaling::MetalFx);
        std::fs::write(dir.join("glue5.json"), r#"{"upscaler": "metalfx-temporal"}"#).unwrap();
        assert_eq!(Settings::load(&dir).upscaler, Upscaling::MetalFxTemporal);
        std::fs::write(dir.join("glue5.json"), r#"{"metal_hud": true}"#).unwrap();
        assert_eq!(Settings::load(&dir).overlay, PerfOverlay::MetalHud);
        std::fs::write(dir.join("glue5.json"), r#"{"metal_hud": false}"#).unwrap();
        assert_eq!(Settings::load(&dir).overlay, PerfOverlay::Off);
        std::fs::write(dir.join("glue5.json"), r#"{"efficiency_mode": true}"#).unwrap();
        assert_eq!(Settings::load(&dir).efficiency, Efficiency::FullE);
        std::fs::write(dir.join("glue5.json"), r#"{"efficiency_cores": false}"#).unwrap();
        assert_eq!(Settings::load(&dir).efficiency, Efficiency::Off);
        std::fs::write(dir.join("glue5.json"), r#"{"efficiency_cores": true, "efficiency_mode": false}"#).unwrap();
        assert_eq!(Settings::load(&dir).efficiency, Efficiency::Hybrid);
        std::fs::write(dir.join("glue5.json"), r#"{"metalfx":true,"resource_budget":"Compact","texture_detail":"Minimal","render_lines":480,"upscale_lines":720,"world_preset":"Eco","max_fps":30}"#).unwrap();
        let migrated = Settings::load(&dir);
        assert_eq!(migrated.upscaler, Upscaling::MetalFx);
        assert_eq!(migrated.resource_budget.mb(), 1280);
        assert_eq!(migrated.texture_detail.max_dimension(), 256);
        assert_eq!(migrated.world_preset, WorldPreset::Eco);
        assert!(migrated.fog);
        assert_eq!(migrated.efficiency, Efficiency::Hybrid);
        assert_eq!(migrated.overlay, PerfOverlay::Off);
        assert_eq!(migrated.render_boost, 1.0);
        assert_eq!(migrated.distance_policy().render_boost, 1.0);
        assert_eq!(migrated.input_size((2868, 1320)), (1042, 480));
        assert_eq!(migrated.output_size((2868, 1320)), (1564, 720));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn profiles_preserve_temporal_frame_generation_and_memory_preferences() {
        let mut s = Settings::default();
        s.upscaler = Upscaling::MetalFxTemporal;
        s.frame_generation = true;
        s.resource_budget = ResourceBudget::Compact;
        s.texture_detail = TextureDetail::Minimal;
        for profile in 0..3 {
            s.performance_profile(profile);
            assert_eq!(s.upscaler, Upscaling::MetalFxTemporal);
            assert!(s.frame_generation);
            assert_eq!(s.resource_budget.mb(), 1280);
            assert_eq!(s.texture_detail.max_dimension(), 256);
        }
    }

    #[test]
    fn first_scale() {
        let mut s = Settings { render_height: Some(720), ..Settings::default() };
        s.for_screen(1440);
        assert_eq!(s.render_scale, Some(0.5));
        s.for_screen(720);
        assert_eq!(s.render_scale, Some(0.5), "chosen once");
        let mut s = Settings::default();
        s.for_screen(1170);
        assert_eq!(s.render_scale, Some(if MOBILE { 0.6 } else { 0.9 }));
    }

    #[test]
    fn independent_input_output_heights_and_display_caps() {
        let mut s = Settings { low_memory: false, ..Settings::default() };
        s.set_scaling_preset(1);
        assert_eq!(s.input_size((1920, 1080)), (854, 480));
        assert_eq!(s.output_size((1920, 1080)), (1280, 720));
        s.set_scaling_preset(SCALE_PAIRS.len());
        assert_eq!(s.scaling_preset(), SCALE_PAIRS.len());
        assert_eq!(s.input_size((1920, 1080)), (854, 480));
        assert_eq!(s.input_size((2868, 1320)), (1042, 480));
        assert_eq!(s.output_size((2868, 1320)), (1564, 720));
        s.render_lines = Some(900);
        s.upscale_lines = Some(480);
        assert_eq!(s.input_size((1920, 1080)).1, 480);
        s.upscale_lines = Some(2160);
        assert_eq!(s.output_size((1920, 1080)), (1920, 1080));
    }

    #[test]
    fn old_settings_keep_their_resolution_and_world_choices() {
        let mut s: Settings = serde_json::from_str(r#"{"render_scale":0.55,"max_fps":60}"#).unwrap();
        s.for_screen(1320);
        assert_eq!(s.render_scale(), 0.55);
        assert_eq!(s.max_fps, 60);
        assert_eq!(s.world_preset, WorldPreset::Game);
        assert_eq!(s.texture_detail, TextureDetail::Original);
        assert_eq!(s.resource_budget, ResourceBudget::Auto);
        assert!(s.render_lines.is_none());
        assert_eq!(s.efficiency, Efficiency::Hybrid);
        s.performance_profile(0);
        let restored: Settings = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(restored.render_lines, Some(480));
        assert_eq!(restored.upscale_lines, Some(720));
        assert_eq!(restored.world_preset, WorldPreset::Eco);
    }

    #[test]
    fn texture_limits_persist_independently_of_performance_profiles() {
        let mut s = Settings::default();
        s.texture_detail = TextureDetail::Compact;
        s.resource_budget = ResourceBudget::Balanced;
        s.performance_profile(0);
        let restored: Settings = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(restored.texture_detail.max_dimension(), 512);
        assert_eq!(restored.resource_budget.mb(), 1536);
        assert_eq!(TextureDetail::Original.max_dimension(), 0);
        assert_eq!(TextureDetail::Balanced.max_dimension(), 1024);
        assert_eq!(TextureDetail::Minimal.max_dimension(), 256);
    }

    #[test]
    fn custom_world_values_are_bounded_and_only_touch_world_fields() {
        let w = WorldDetail { lod: 3.0, density: f32::NAN, ..WorldDetail::default() };
        let values = w.xml_values();
        assert_eq!(values[0], ("LodScale", "1.000000".into()));
        assert_eq!(values[3], ("CityDensity", "0.000000".into()));
        assert!(values.iter().all(|(name, _)| !name.contains("Texture") && !name.contains("MSAA")));
    }

    #[test]
    fn google_drive_follows_the_file_and_stays_off_without_apple() {
        let dir = std::env::temp_dir().join(format!("glue5-drive-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("glue5.json"), r#"{"google_drive": false}"#).unwrap();
        assert!(!Settings::load(&dir).google_drive);
        std::fs::write(
            dir.join("glue5.json"),
            r#"{"google_drive": true, "google_client_id_desktop": "desktop.apps.googleusercontent.com"}"#,
        )
        .unwrap();
        let on = Settings::load(&dir);
        assert!(on.google_drive);
        assert!(!Settings::default().google_drive);
        if !cfg!(target_vendor = "apple") {
            assert!(!on.drive_enabled());
        }
        if !cfg!(target_os = "ios") {
            assert_eq!(on.google_client_id(), "desktop.apps.googleusercontent.com");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
