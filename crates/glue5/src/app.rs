//! The application: window, renderer, engine and the event loop.
//!
//! It opens on the start page with the renderer already running: the
//! game's files, Play, Settings. What applies when the engine starts (frame
//! rate, memory profile, the game folder, a phone's resolution) can be
//! changed there before it loads, and a game that fails while starting is
//! not started again by itself. Play starts the engine, which then owns the
//! frames; the application's UI goes over them. `glue5 --play` (a restart
//! the game asks for) starts the game without the start page.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use glue5_gpu::{Presentation, Screen, Upscaler};
use glue5_runtime::exit::Exit;
use glue5_runtime::input::{Gamepad, InputBlock, mouse};
use glue5_runtime::layout::is_game_dir;
use sdl3::event::{Event, WindowEvent};
use sdl3::keyboard::Scancode;
use sdl3::mouse::MouseButton;

use crate::haptics;
use crate::paths::{self, MOBILE};
use crate::settings::{self, PerfOverlay, Settings, TouchMode, Upscaling};
use crate::touch::Touch;
use crate::ui::{self, Action, Mode, Page, Started, Status, Ui};
use crate::{audio, benchmark, keymap, pads};

/// The settings button's opacity over the game while the touch controls
/// are hidden (a controller in use, or touch off): there, but faint.
const GEAR_ALONE: f32 = 0.35;

/// In the user directory: why the last game stopped, for the start page
/// (gone once the game is started again).
const FAILURE_FILE: &str = "last-failure.txt";

#[cfg(test)]
mod performance_tests {
    #[test]
    fn thermal_caps_never_raise_a_user_limit() {
        use super::{hot_fps_cap, limited_fps};
        assert_eq!(hot_fps_cap(0), None);
        assert_eq!(limited_fps(60, hot_fps_cap(1)), 40);
        assert_eq!(limited_fps(60, hot_fps_cap(2)), 30);
        assert_eq!(limited_fps(30, hot_fps_cap(3)), 20);
        assert_eq!(limited_fps(20, hot_fps_cap(1)), 20);
        assert_eq!(limited_fps(0, hot_fps_cap(2)), 30);
        assert_eq!(limited_fps(0, None), 0);
    }
}

fn hot_fps_cap(state: u8) -> Option<u32> {
    match state {
        1 => Some(40),
        2 => Some(30),
        3.. => Some(20),
        _ => None,
    }
}

fn limited_fps(requested: u32, cap: Option<u32>) -> u32 {
    match cap {
        Some(cap) if requested == 0 => cap,
        Some(cap) => requested.min(cap),
        None => requested,
    }
}

pub fn run() -> Result<()> {
    // SDL's event wait on Android pumps events, which queues the poll
    // sentinel, which wakes the wait: it spins for its whole timeout (a big
    // core at 100%). Without the sentinel it sleeps until an event; polling
    // then drains the queue, which is what this loop does anyway.
    #[cfg(target_os = "android")]
    sdl3::hint::set("SDL_POLL_SENTINEL", "0");
    let sdl = sdl3::init().map_err(|e| anyhow!("SDL: {e}"))?;
    let video = sdl.video().map_err(|e| anyhow!("SDL video: {e}"))?;
    if MOBILE {
        sdl3::hint::set("SDL_ORIENTATIONS", "LandscapeLeft LandscapeRight");
        // Touches drive the touch controls, not a pretend mouse.
        sdl3::hint::set("SDL_TOUCH_MOUSE_EVENTS", "0");
    }
    // The renderer sizes the window's Metal layer when it configures it for
    // the window. SDL resizing the layer as well (macOS), from inside the
    // system's resize loop, had frames drawn for one size shown at another.
    sdl3::hint::set("SDL_VIDEO_METAL_AUTO_RESIZE_DRAWABLE", "0");
    let user_dir = paths::user_dir()?;
    let diagnostics_dir = paths::diagnostics_dir(&user_dir);
    match glue5_runtime::diagnostics::start(&diagnostics_dir) {
        Ok(path) => {
            log::info!("diagnostics: {}", path.display());
            glue5_runtime::diagnostics::note(format_args!(
                "app start: os={} mobile={} diagnostics={}",
                std::env::consts::OS,
                MOBILE,
                path.display(),
            ));
            #[cfg(target_os = "ios")]
            glue5_runtime::diagnostics::note(format_args!(
                "render: iOS uses wgpu surface presentation; normal resource heap budget is 2048MiB",
            ));
        }
        Err(e) => log::warn!("diagnostics: cannot start in {}: {e}", diagnostics_dir.display()),
    }
    let mut settings = Settings::load(&user_dir);
    glue5_runtime::qos::set_mode(settings.runtime_mode());
    // Before the GPU opens; turned on later, it shows from the next launch.
    let metal_hud = cfg!(target_vendor = "apple") && settings.overlay == PerfOverlay::MetalHud;
    if metal_hud {
        // SAFETY: Apple's libc locks the environment for getenv and setenv
        // (elsewhere this does nothing).
        unsafe { glue5_gpu::enable_metal_hud() };
    }
    let args = Args::parse();

    let mut builder = video.window("Glue 5", 1280, 720);
    builder.resizable().high_pixel_density();
    if MOBILE {
        builder.fullscreen();
    }
    if cfg!(target_vendor = "apple") {
        builder.metal_view();
    }
    let window = builder.build()?;
    let pixels = window.size_in_pixels();
    settings.for_screen(pixels.1);

    let presentation = Presentation::new(pixels, settings.vsync, upscaler(&settings));
    presentation.set_metal_hud(settings.overlay == PerfOverlay::MetalHud);
    let overlay = glue5_gpu::Overlay::default();
    let instance = glue5_gpu::instance();
    #[cfg(target_os = "android")]
    let surfaces = instance.clone();
    // The window outlives the renderer: the process exits with it.
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_display_and_window(&window, &window)?)?
    };
    let screen = Screen::open(glue5_gpu::Window {
        instance,
        surface,
        presentation: presentation.clone(),
        overlay: overlay.clone(),
    })?;
    // While the system resizes the window in a loop of its own (macOS,
    // Windows), events wait for it to end, and so does this loop; SDL still
    // calls watches, and from there the renderer follows the window. The
    // game's resolution changes once, when this loop runs again. SDL sends
    // the application's lifecycle only to watches too, from the call that
    // learns of it (not through the queue): the loop follows it from there.
    let watched = presentation.clone();
    let lifecycle = Arc::new(Lifecycle::new(user_dir.clone(), &settings));
    let life = lifecycle.clone();
    let _watch =
        sdl.event().map_err(|e| anyhow!("SDL events: {e}"))?.add_event_watch(move |event: Event| match event {
            Event::Window { win_event: WindowEvent::PixelSizeChanged(w, h), .. } => {
                watched.set_size(w.max(0) as u32, h.max(0) as u32)
            }
            // iOS: applicationWillResignActive, applicationDidEnterBackground,
            // applicationWillTerminate. SDL delivers the first as will-enter-background.
            // The process can be killed as soon as this returns, so the save is
            // flushed here rather than on the next turn of the loop.
            Event::AppWillEnterBackground { .. }
            | Event::AppDidEnterBackground { .. }
            | Event::AppTerminating { .. } => {
                life.background.store(true, Ordering::Relaxed);
                life.flush();
            }
            Event::AppDidEnterForeground { .. } => {
                life.background.store(false, Ordering::Relaxed);
                life.came_back.store(true, Ordering::Relaxed);
            }
            _ => {}
        });

    // Text input runs only while the game's text box is open (see `update`).
    let text_input = video.text_input();
    let refresh_rate = window_refresh_rate(&window);
    let mode = match () {
        _ if settings.online => Mode::Online,
        _ if settings.sandbox => Mode::Sandbox,
        _ => Mode::Story,
    };
    let last_benchmark = benchmark::last(&user_dir);
    let mut app = App {
        pads: pads::Pads::new(sdl.gamepad().map_err(|e| anyhow!("SDL gamepad: {e}"))?),
        mouse: sdl.mouse(),
        events: sdl.event_pump().map_err(|e| anyhow!("SDL events: {e}"))?,
        sdl,
        window,
        text_input,
        typing: false,
        keyboard_away: false,
        presentation,
        #[cfg(target_os = "android")]
        instance: surfaces,
        user_dir,
        settings,
        ui: Ui::new(overlay),
        touch: Touch::new(),
        touch_input: MOBILE,
        touch_shown: false,
        lifecycle,
        folder: None,
        game_dir: None,
        problem: None,
        last_failure: None,
        graphics_reset: false,
        upscalers: UPSCALERS.iter().copied().filter(|u| screen.has_upscaler(u.upscaler())).collect(),
        frame_generation: screen.has_frame_generation(),
        metal_hud,
        refresh_rate,
        screen: Some(screen),
        play_requested: false,
        mode,
        benchmark: last_benchmark,
        game: None,
        adaptive_slow_since: None,
        adaptive_fast_since: None,
        adaptive_last_change: None,
        active_render_scale: None,
        thermal_state: 0,
        thermal_last_poll: Instant::now(),
        thermal_cool_since: None,
        thermal_cap: None,
        last_ui_frame: Instant::now(),
    };
    app.last_failure = std::fs::read_to_string(app.user_dir.join(FAILURE_FILE)).ok();
    app.find_game(args.game_dir.as_deref());
    if args.play && app.game_dir.is_some() {
        app.play()?;
    }
    app.run()
}

/// The command line (desktop): `glue5 [--play] [game folder]`; the start
/// page is skipped only with `--play` (a folder alone is where the game is:
/// a game that fails is not started again by itself).
struct Args {
    /// Start the game without the start page.
    play: bool,
    game_dir: Option<PathBuf>,
}

impl Args {
    fn parse() -> Args {
        let mut args = Args { play: false, game_dir: None };
        // Phones pass arguments of their own.
        for arg in std::env::args_os().skip(1).filter(|_| !MOBILE) {
            if arg == "--play" {
                args.play = true;
            } else {
                args.game_dir = Some(arg.into());
            }
        }
        // Measurements on a phone, whose screen nobody taps (`mise run ios:run`
        // passes GLUE5_* variables on).
        args.play |= std::env::var_os("GLUE5_PLAY").is_some();
        args
    }
}

/// The application's lifecycle, as SDL tells the event watch.
struct Lifecycle {
    /// In the background (phones: no GPU work there).
    background: AtomicBool,
    /// Back in the foreground since the loop last looked.
    came_back: AtomicBool,
    user_dir: PathBuf,
    /// Latest settings, so the watch can flush them without borrowing `App`.
    settings_json: Mutex<Vec<u8>>,
}

impl Lifecycle {
    fn new(user_dir: PathBuf, settings: &Settings) -> Self {
        Self {
            background: AtomicBool::new(false),
            came_back: AtomicBool::new(false),
            user_dir,
            settings_json: Mutex::new(settings.snapshot()),
        }
    }

    fn remember(&self, settings: &Settings) {
        if let Ok(mut json) = self.settings_json.lock() {
            *json = settings.snapshot();
        }
    }

    /// Settings, then the engine's stats and save files. Bounded, so a stuck
    /// engine cannot hold the process past the point iOS suspends it.
    fn flush(&self) {
        let json = self.settings_json.lock().map(|json| json.clone()).unwrap_or_default();
        Settings::save_snapshot(&self.user_dir, &json);
        glue5_runtime::flush_on_suspend();
    }
}

struct App {
    sdl: sdl3::Sdl,
    window: sdl3::video::Window,
    events: sdl3::EventPump,
    mouse: sdl3::mouse::MouseUtil,
    text_input: sdl3::keyboard::TextInputUtil,
    /// Typed text is on: while the game's text box is open.
    typing: bool,
    /// The on-screen keyboard, which hides the touch controls, was put away
    /// with a tap above it: the game's text box takes A or B from them.
    keyboard_away: bool,
    pads: pads::Pads,
    presentation: Presentation,
    /// For the window's surfaces after the first.
    #[cfg(target_os = "android")]
    instance: wgpu::Instance,
    user_dir: PathBuf,
    settings: Settings,
    ui: Ui,
    touch: Touch,
    /// Touch is the input in use: the touch controls follow it (touching
    /// shows them, a controller or keyboard hides them; being connected is
    /// not enough, phones often have a paired controller lying around).
    touch_input: bool,
    touch_shown: bool,
    lifecycle: Arc<Lifecycle>,
    folder: Option<FolderPick>,
    /// The game's folder, once found.
    game_dir: Option<PathBuf>,
    /// Why it was not, or a folder chosen that is not one.
    problem: Option<String>,
    /// Why the last game stopped, if it did.
    last_failure: Option<String>,
    /// The game's graphics settings went back to their defaults.
    graphics_reset: bool,
    /// The upscalers this GPU has.
    upscalers: Vec<Upscaling>,
    /// It can generate frames.
    frame_generation: bool,
    /// Apple's Metal HUD can show: it was on at launch.
    metal_hud: bool,
    /// The display's refresh rate at most, in Hz.
    refresh_rate: f32,
    /// The renderer, until the game takes it over.
    screen: Option<Screen>,
    /// Play was chosen: the game starts once its pipelines are prepared.
    play_requested: bool,
    /// What Play starts.
    mode: Mode,
    /// The last benchmark's results.
    benchmark: Option<benchmark::Results>,
    game: Option<Running>,
    /// Hysteresis timers for adaptive mobile rendering. The timers make the
    /// quality react to sustained pressure, not one transient frame.
    adaptive_slow_since: Option<Instant>,
    adaptive_fast_since: Option<Instant>,
    adaptive_last_change: Option<Instant>,
    active_render_scale: Option<f32>,
    thermal_state: u8,
    thermal_last_poll: Instant,
    thermal_cool_since: Option<Instant>,
    thermal_cap: Option<u32>,
    last_ui_frame: Instant,
}

/// The game, once started.
struct Running {
    game: glue5_runtime::Game,
    /// The engine's input block, once it has published it.
    input: Option<InputBlock>,
    started: Started,
    /// The back buffer size last asked of the engine.
    back_buffer: (u32, u32),
    _audio: Option<audio::Audio>,
}

impl App {
    fn run(mut self) -> Result<()> {
        loop {
            self.lifecycle.remember(&self.settings);
            if let Some(exit) = glue5_runtime::exit::requested() {
                self.lifecycle.flush();
                if exit == Exit::Restart {
                    if MOBILE {
                        // A phone app cannot start itself again: says so before it ends.
                        let text = "The new settings apply when Glue 5 is opened again.";
                        let flag = sdl3::messagebox::MessageBoxFlag::INFORMATION;
                        let _ = sdl3::messagebox::show_simple_message_box(flag, "Glue 5", text, &self.window);
                    }
                    relaunch(true);
                } else if self.mode == Mode::Benchmark {
                    self.benchmark_done();
                }
                return Ok(());
            }
            if let Some(why) = glue5_runtime::exit::failed() {
                self.lifecycle.flush();
                self.stopped(why);
                return Ok(());
            }
            // This is intentionally on the UI/event thread. If it freezes,
            // the last durable heartbeat says when it stopped; if only an
            // engine thread stops, heartbeats continue but report its rings.
            if self.game.is_some() {
                glue5_runtime::diagnostics::heartbeat();
            }
            if let Some(game) = &mut self.game
                && game.input.is_none()
            {
                game.input = game.game.input();
                if let Some(i) = &game.input {
                    i.focus(true);
                }
            }
            let page = self.ui.page();
            // Before the game the loop sleeps until there is input or the UI
            // animates; with it, it wakes on the frame cap (events still
            // return early). In the background it wakes rarely: the renderer
            // is held to a few frames a second for the same reason.
            let background = self.lifecycle.background.load(Ordering::Relaxed);
            self.presentation.set_background(background);
            let wait = if background {
                Duration::from_millis(250)
            } else if self.game.is_some() {
                let fps = self.effective_fps();
                Duration::from_millis(if fps == 0 { 16 } else { (1000 / fps.clamp(20, 60)) as u64 })
            } else {
                self.ui.idle_for()
            };
            let first = wait_event(&mut self.events, wait);
            let events: Vec<Event> = first.into_iter().chain(self.events.poll_iter()).collect();
            for event in events {
                if self.event(event) {
                    self.lifecycle.remember(&self.settings);
                    self.lifecycle.flush();
                    return Ok(());
                }
            }
            if self.lifecycle.came_back.swap(false, Ordering::Relaxed) {
                self.came_back();
            }
            if self.update(page)? {
                self.lifecycle.remember(&self.settings);
                self.lifecycle.flush();
                return Ok(());
            }
        }
    }

    /// One event; true to quit.
    fn event(&mut self, event: Event) -> bool {
        match &event {
            Event::Quit { .. } => return true,
            Event::GamepadAdded { which, .. } => self.pads.added(*which),
            Event::GamepadRemoved { which, .. } => self.pads.removed(*which),
            Event::KeyDown { .. } | Event::MouseButtonDown { .. } | Event::GamepadButtonDown { .. } => {
                self.touch_input = false
            }
            Event::GamepadAxisMotion { value, .. } if value.unsigned_abs() > 16000 => self.touch_input = false,
            Event::Window { win_event: WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..), .. } => {
                let (w, h) = self.window.size_in_pixels();
                self.presentation.set_size(w, h);
                self.ui.repaint();
            }
            Event::Window { win_event: WindowEvent::Exposed, .. } => self.ui.repaint(),
            _ => {}
        }
        if let Event::FingerDown { x, y, .. } = event {
            // The settings button takes its finger before the touch controls
            // see it (or they would show, taking pad 0 from a controller).
            if !self.ui.is_open() && self.ui.gear_hit(x, y, &self.window) {
                self.ui.toggle_settings();
                return false;
            }
            self.touch_input = true;
            // A tap above the on-screen keyboard puts it away (it covers the
            // touch controls, and a phone's has no key to hide it).
            if MOBILE && self.typing && !self.ui.is_open() {
                self.keyboard_away = true;
                return false;
            }
        }
        if let Event::KeyDown { scancode: Some(Scancode::F1), repeat: false, .. } = event {
            self.ui.toggle_settings();
            return false;
        }
        if self.ui.is_open() {
            if let Event::KeyDown { scancode: Some(Scancode::Escape), .. } = event {
                self.ui.back();
            } else {
                self.ui.event(&event, &self.window);
            }
            return false;
        }
        match event {
            Event::FingerDown { finger_id, x, y, .. } => self.touch.finger_down(finger_id, x, y),
            Event::FingerMotion { finger_id, x, y, .. } => self.touch.finger_motion(finger_id, x, y),
            Event::FingerUp { finger_id, .. } => self.touch.finger_up(finger_id),
            _ => {}
        }
        if let Some(game) = &self.game
            && let Some(input) = &game.input
        {
            game_input(input, &event, &self.window, &self.mouse, game.back_buffer);
        }
        false
    }

    /// Everything after the events: touch controls, pads, the UI and what
    /// it asks for. True to quit.
    fn update(&mut self, page_before: Option<Page>) -> Result<bool> {
        let in_game = self.game.is_some();
        // Touch controls: a virtual pad in slot 0 unless a controller has it.
        let show = in_game
            && !self.ui.is_open()
            && match self.settings.touch {
                TouchMode::Always => true,
                TouchMode::Off => false,
                TouchMode::Auto => self.touch_input,
            };
        if show != self.touch_shown {
            self.touch_shown = show;
            self.touch.clear();
            glue5_runtime::input::set_gamepad(0, Gamepad::default());
        }
        if show {
            self.touch.haptics = self.settings.haptics;
            self.touch.layout(ui::screen_size(&self.window), ui::safe_rect(&self.window), self.settings.touch_size);
            glue5_runtime::input::set_gamepad(0, self.touch.gamepad());
            if self.settings.haptics {
                haptics::rumble(glue5_runtime::input::rumble(0));
            }
        }

        // While the touch controls are pad 0, a controller in slot 0 waits.
        let report = self.pads.update(self.ui.is_open(), show);
        if report.menu {
            self.ui.toggle_settings();
        }
        for nav in report.nav {
            self.ui.nav(nav);
        }

        // What applies at once (the engine's own frame limiter at its next start).
        self.presentation.set_vsync(self.settings.vsync);
        self.presentation.set_metal_hud(self.settings.overlay == PerfOverlay::MetalHud);
        glue5_gpu::perf::set_on(in_game && self.settings.overlay == PerfOverlay::Glue5);
        self.update_thermal();
        self.presentation.set_frame_rate(self.effective_fps());
        self.presentation.set_upscaler(upscaler(&self.settings));
        self.presentation.set_sharpness(self.settings.sharpness);
        self.presentation.set_frame_generation(self.settings.frame_generation);
        self.presentation.set_upscale_height(if self.settings.upscale_lines.is_some() {
            self.settings.output_size(self.window.size_in_pixels()).1
        } else {
            0
        });
        glue5_runtime::diagnostics::set_verbose_logs(self.settings.verbose_logs);
        glue5_runtime::qos::set_mode(self.settings.runtime_mode());
        if self.game.is_some()
            && let Some(ms) = self.presentation.frame_interval_ms()
        {
            let budget = 1000.0 / self.effective_fps().max(1) as f32;
            glue5_runtime::qos::note_frame(ms, budget);
        }
        glue5_gpu::set_distance_policy(self.settings.distance_policy());
        self.adapt_graphics();
        self.resize_game();

        // The UI: over the game at every turn, before it when something changed.
        let mut actions = Vec::new();
        let ui_budget = Duration::from_secs_f64(1.0 / self.effective_fps().clamp(30, 60) as f64);
        let ui_due = self.ui.wants_frame() || self.last_ui_frame.elapsed() >= ui_budget;
        if !self.lifecycle.background.load(Ordering::Relaxed) && ((in_game && ui_due) || self.ui.wants_frame()) {
            self.last_ui_frame = Instant::now();
            let status = Status {
                game_dir: self.game_dir.as_deref(),
                problem: self.problem.as_deref(),
                started: self.game.as_ref().map(|g| &g.started),
                last_failure: self.last_failure.as_deref(),
                graphics_reset: self.graphics_reset,
                window: self.window.size_in_pixels(),
                back_buffer: self.game.as_ref().map(|g| g.back_buffer),
                upscalers: &self.upscalers,
                frame_generation: self.frame_generation,
                metal_hud: self.metal_hud,
                refresh_rate: self.refresh_rate,
                preparing: self.play_requested.then(|| self.screen.as_ref().and_then(Screen::progress)).flatten(),
                mode: self.mode,
                benchmark: self.benchmark.as_ref(),
                thermal_state: self.thermal_state,
                effective_fps: self.effective_fps(),
                actual_input: self.game.as_ref().map(|g| g.back_buffer),
            };
            // The settings button: on touch screens always, so that it stays
            // with a controller in use or the touch controls off.
            let gear =
                (in_game && (MOBILE || show)).then_some(if show { self.settings.touch_opacity } else { GEAR_ALONE });
            actions = self.ui.frame(&self.window, &mut self.settings, &status, show.then_some(&self.touch), gear);
            if let Some(screen) = &mut self.screen
                && !screen.show_ui()
            {
                // Not shown (the window not visible yet, ...): again soon.
                self.ui.repaint_in(Duration::from_millis(100));
            }
        }
        for action in actions {
            match action {
                Action::Quit => return Ok(true),
                Action::Play(mode) => {
                    self.mode = mode;
                    if mode != Mode::Benchmark {
                        self.settings.sandbox = mode == Mode::Sandbox;
                        self.settings.online = mode == Mode::Online;
                    }
                    self.play()?;
                }
                Action::ChooseGame => self.folder = Some(FolderPick::start(&self.window)),
                Action::FindGame => self.find_game(None),
                Action::ResetGraphics => self.reset_graphics(),
            }
        }
        if self.play_requested {
            self.play()?;
        }
        if let Some(choice) = self.folder.as_ref().and_then(FolderPick::poll) {
            self.folder = None;
            self.chosen(choice);
        }

        let page = self.ui.page();
        if page != page_before {
            if let Some(input) = self.game.as_ref().and_then(|g| g.input.as_ref()) {
                // Nothing stays pressed in the game behind a page.
                input.release_all();
                if page.is_some() {
                    self.mouse.set_relative_mouse_mode(&self.window, false);
                    input.pointer_locked(false);
                }
            }
            if page_before == Some(Page::Settings) {
                self.settings.save(&self.user_dir);
            }
        }

        // Text input only while the game wants text: on touch devices it
        // brings up the system keyboard; on desktop it hands every key to
        // the system's input methods, whose accent menu (a held key, macOS),
        // dead keys and IME windows would otherwise get in the way of play.
        // Keys reach the game and the UI by scancode either way.
        let text_box = glue5_runtime::input::text_box_open();
        self.keyboard_away &= text_box;
        let want_typing = (text_box && !self.keyboard_away) || self.ui.wants_text();
        if want_typing != self.typing {
            self.typing = want_typing;
            if want_typing {
                self.text_input.start(&self.window);
            } else {
                self.text_input.stop(&self.window);
            }
        }
        Ok(false)
    }

    /// The game's back buffer follows the window (desktop), and on phones it
    /// can also follow adaptive quality. The engine waits for a size to hold
    /// for 20 of its frames before rebuilding its render targets, and a
    /// window being resized by the system does not reach this loop until the
    /// end: one rebuild per resize.
    fn resize_game(&mut self) {
        let Some(game) = &mut self.game else { return };
        let Some(input) = &game.input else { return };
        let (w, h) = self.window.size_in_pixels();
        if w == 0 || h == 0 {
            return;
        }
        let wanted = self.settings.input_size((w, h));
        let scale = self.active_render_scale.unwrap_or(wanted.1 as f32 / h as f32);
        let size = settings::render_size((w, h), scale, game.started.low_memory);
        if size != game.back_buffer {
            log::info!("back buffer {}x{}", size.0, size.1);
            input.resize(size.0, size.1);
            game.back_buffer = size;
            if MOBILE {
                game.started.render_scale = scale;
            }
        }
    }

    /// Adjusts only the engine back buffer, leaving the game's texture and
    /// world streaming settings alone. It needs sustained evidence before a
    /// change and a cooldown afterwards, so the picture does not oscillate.
    fn adapt_graphics(&mut self) {
        let pixels = self.window.size_in_pixels();
        let wanted = self.settings.input_size(pixels).1 as f32 / pixels.1.max(1) as f32;
        // Nominal spends the distant-draw savings on pixels. Heat gives them back.
        let thermal = if self.settings.thermal_protection { self.thermal_state } else { 0 };
        let factor = glue5_gpu::resolution_factor(thermal, glue5_gpu::render_boost());
        let ceiling = settings::round_scale((wanted * factor).min(1.0));
        if !MOBILE || !self.settings.adaptive_graphics || self.game.is_none() {
            self.adaptive_slow_since = None;
            self.adaptive_fast_since = None;
            self.active_render_scale = Some(ceiling);
            return;
        }
        self.active_render_scale = Some(self.active_render_scale.unwrap_or(ceiling).min(ceiling));
        let Some(interval_ms) = self.presentation.frame_interval_ms() else { return };
        let fps = self.effective_fps();
        let target_ms = 1000.0 / if fps > 0 { fps } else { 60 } as f32;
        let now = Instant::now();
        let overloaded = interval_ms > target_ms * 1.18;
        let headroom = thermal < 2 && self.thermal_state < 2 && interval_ms <= target_ms * 1.04;
        if overloaded {
            self.adaptive_slow_since.get_or_insert(now);
            self.adaptive_fast_since = None;
        } else if headroom {
            self.adaptive_fast_since.get_or_insert(now);
            self.adaptive_slow_since = None;
        } else {
            self.adaptive_slow_since = None;
            self.adaptive_fast_since = None;
        }
        let cooldown = self.adaptive_last_change.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(2));
        let lower =
            cooldown && self.adaptive_slow_since.is_some_and(|t| now.duration_since(t) >= Duration::from_millis(1200));
        let raise =
            cooldown && self.adaptive_fast_since.is_some_and(|t| now.duration_since(t) >= Duration::from_secs(8));
        let delta = if lower {
            -0.05
        } else if raise {
            0.05
        } else {
            return;
        };
        let old = self.active_render_scale.unwrap_or(ceiling);
        let floor = 240.0 / pixels.1.max(1) as f32;
        let new = (old + delta).max(floor.min(ceiling)).min(ceiling);
        self.adaptive_slow_since = None;
        self.adaptive_fast_since = None;
        if new == old {
            return;
        }
        self.active_render_scale = Some(new);
        if let Some(game) = &mut self.game {
            game.started.render_scale = new;
        }
        self.adaptive_last_change = Some(now);
        log::info!("adaptive graphics: frame interval {interval_ms:.1} ms, render scale {old:.2} -> {new:.2}");
        glue5_runtime::diagnostics::note(format_args!(
            "adaptive graphics: frame_interval={interval_ms:.1}ms render_scale={old:.2}->{new:.2}",
        ));
    }

    fn effective_fps(&self) -> u32 {
        let engine_cap = self.game.as_ref().and_then(|g| (g.started.max_fps > 0).then_some(g.started.max_fps));
        let requested = limited_fps(self.settings.max_fps, engine_cap);
        let thermal = limited_fps(requested, self.thermal_cap.filter(|_| self.settings.thermal_protection));
        limited_fps(thermal, glue5_runtime::qos::pace_fps())
    }

    fn update_thermal(&mut self) {
        // Polled on every platform. Off Apple the state stays nominal, so a
        // desktop build that leaves thermal protection off is unchanged.
        if self.thermal_last_poll.elapsed() >= Duration::from_secs(1) {
            self.thermal_last_poll = Instant::now();
            let state = glue5_gpu::thermal_state();
            if state != self.thermal_state {
                glue5_runtime::diagnostics::note(format_args!("thermal state: {} -> {state}", self.thermal_state));
                self.thermal_state = state;
            }
            if !self.settings.thermal_protection {
                self.thermal_cap = None;
                self.thermal_cool_since = None;
            } else if let Some(cap) = hot_fps_cap(state).filter(|_| MOBILE) {
                self.thermal_cap = Some(self.thermal_cap.map_or(cap, |old| old.min(cap)));
                self.thermal_cool_since = None;
            } else if self.thermal_cap.is_some() {
                let since = self.thermal_cool_since.get_or_insert(Instant::now());
                if since.elapsed() >= Duration::from_secs(30) {
                    self.thermal_cap = None;
                    self.thermal_cool_since = None;
                }
            }
        }
        let state = if self.settings.thermal_protection { self.thermal_state } else { 0 };
        self.presentation.set_thermal(state);
    }

    /// Starts the game once the pipelines known for it are prepared (the
    /// start page shows how far; called again until then); the renderer
    /// takes the screen over.
    fn play(&mut self) -> Result<()> {
        let Some(game_dir) = self.game_dir.clone() else { return Ok(()) };
        let mut gpu_settings = glue5_gpu::Settings::for_game(&game_dir);
        gpu_settings.cache_dir = Some(self.user_dir.join("cache"));
        gpu_settings.max_texture_dimension = self.settings.texture_detail.max_dimension();
        let Some(screen) = self.screen.as_mut() else { return Ok(()) };
        self.play_requested = !screen.prepare(&gpu_settings);
        if self.play_requested {
            return Ok(());
        }
        let Some(screen) = self.screen.take() else { return Ok(()) };
        // `play` is called once per UI frame while pipeline preparation is
        // running. Open the visible mirror only once that preparation has
        // completed, otherwise each attempt replaces its file descriptor.
        let visible_diagnostics = game_dir.join("Glue5 Diagnostics");
        match glue5_runtime::diagnostics::mirror_to(&visible_diagnostics) {
            Ok(path) => glue5_runtime::diagnostics::note(format_args!("diagnostics mirror: {}", path.display())),
            Err(e) => log::warn!("diagnostics: cannot mirror to {}: {e}", visible_diagnostics.display()),
        }
        // Graphics quality is the game's own (its menu); the low-memory
        // profile only lowers it once, after which the menu has the last word.
        if self.settings.low_memory && !self.settings.low_preset_written {
            match glue5_runtime::game_settings::apply_low_preset(&self.user_dir) {
                Ok(()) => {
                    self.settings.low_preset_written = true;
                    log::info!("the game's graphics settings: low-memory preset written");
                }
                Err(e) => log::warn!("cannot write the low-memory preset: {e}"),
            }
        }
        if self.mode == Mode::Online && self.settings.online_name.trim().is_empty() {
            self.settings.online_name = settings::default_online_name();
        }
        if self.settings.world_preset != settings::WorldPreset::Game {
            glue5_runtime::game_settings::apply(&self.user_dir, &self.settings.world.xml_values())?;
        }
        glue5_runtime::diagnostics::set_verbose_logs(self.settings.verbose_logs);
        self.settings.save(&self.user_dir);
        // A new attempt: the last one's failure is history.
        let _ = std::fs::remove_file(self.user_dir.join(FAILURE_FILE));
        log::info!("game: {}, user data: {}", game_dir.display(), self.user_dir.display());
        let s = &self.settings;
        let back_buffer = s.input_size(self.window.size_in_pixels());
        glue5_runtime::diagnostics::note(format_args!(
            "game launch requested: game={} resolution={}x{} render_scale={:.2} low_memory={} max_fps={} mode={:?}",
            game_dir.display(),
            back_buffer.0,
            back_buffer.1,
            s.render_scale(),
            s.low_memory,
            s.max_fps,
            self.mode,
        ));
        let game = glue5_runtime::start(
            glue5_runtime::Config {
                game_dir: game_dir.clone(),
                user_dir: self.user_dir.clone(),
                width: back_buffer.0,
                height: back_buffer.1,
                // The browser page's start choices: 1 the story, 2 the sandbox.
                start_mode: if self.mode == Mode::Sandbox { 2 } else { 1 },
                benchmark: self.mode == Mode::Benchmark,
                low_memory: s.low_memory,
                resource_mb: s.resource_budget.mb(),
                max_fps: s.max_fps,
                online: (self.mode == Mode::Online)
                    .then(|| glue5_runtime::Online { name: s.online_name.trim().into(), lobby: s.lobby() }),
                language: preferred_language(),
            },
            glue5_gpu::backend(gpu_settings, Some(screen)),
        )?;
        let audio = match self.sdl.audio().map_err(|e| anyhow!("{e}")).and_then(|a| audio::start(&a, game.audio())) {
            Ok(audio) => Some(audio),
            Err(e) => {
                log::warn!("no audio: {e}");
                None
            }
        };
        let started = Started {
            game_dir,
            max_fps: s.max_fps,
            low_memory: s.low_memory,
            render_scale: s.render_scale(),
            world: (s.world_preset != settings::WorldPreset::Game).then_some(s.world),
            texture_detail: s.texture_detail,
            resource_budget: s.resource_budget,
        };
        self.game = Some(Running { game, input: None, started, back_buffer, _audio: audio });
        glue5_runtime::qos::note_load();
        self.ui.game_started();
        glue5_runtime::diagnostics::note(format_args!("game launch complete: waiting for engine input block"));
        Ok(())
    }

    /// The benchmark is over (the engine asked to quit): on desktop the app
    /// opens again on its start page, which shows the results; a phone app
    /// cannot, so it shows them before it ends.
    fn benchmark_done(&mut self) {
        if !MOBILE {
            relaunch(false);
            return;
        }
        let text = match benchmark::last(&self.user_dir) {
            Some(r) => format!(
                "Benchmark: {:.0} fps on average, {:.0} at least; {:.0}% of the frames in under 33 ms.",
                r.avg_fps,
                r.min_fps,
                r.smooth * 100.0
            ),
            None => "The benchmark is over.".into(),
        };
        let flag = sdl3::messagebox::MessageBoxFlag::INFORMATION;
        let _ = sdl3::messagebox::show_simple_message_box(flag, "Glue 5", &text, &self.window);
    }

    /// The game has stopped working (a thread of the engine's failed): the
    /// window shows its last frame for good. Says so, and records it for the
    /// start page, which offers the way out of a graphics setting the engine
    /// cannot run with; on desktop it opens again on that page.
    fn stopped(&mut self, why: &str) {
        glue5_runtime::diagnostics::fault(format_args!("app stopped: {why}"));
        if let Err(e) = std::fs::write(self.user_dir.join(FAILURE_FILE), why) {
            log::warn!("cannot record the failure: {e}");
        }
        self.settings.save(&self.user_dir);
        self.mouse.set_relative_mouse_mode(&self.window, false);
        let page = if MOBILE { "when the app is opened again, its start page" } else { "the start page" };
        let text = format!(
            "The game has stopped: {why}.\n\nIf it stops again at the same point, one of its graphics settings \
             may be the cause: {page} can put them back to their defaults."
        );
        use sdl3::messagebox::{ButtonData, ClickedButton, MessageBoxButtonFlag as Button, MessageBoxFlag};
        let buttons: &[ButtonData] = if MOBILE {
            &[ButtonData { flags: Button::RETURNKEY_DEFAULT, button_id: 0, text: "OK" }]
        } else {
            &[
                ButtonData { flags: Button::ESCAPEKEY_DEFAULT, button_id: 0, text: "Quit" },
                ButtonData { flags: Button::RETURNKEY_DEFAULT, button_id: 1, text: "Start page" },
            ]
        };
        let shown =
            sdl3::messagebox::show_message_box(MessageBoxFlag::ERROR, buttons, "Glue 5", &text, &self.window, None);
        if let Ok(ClickedButton::CustomButton(ButtonData { button_id: 1, .. })) = shown {
            relaunch(false);
        }
    }

    /// The game's graphics settings back to the engine's defaults (asked for
    /// on the start page).
    fn reset_graphics(&mut self) {
        match glue5_runtime::game_settings::reset_graphics(&self.user_dir) {
            Ok(()) => {
                log::info!("the game's graphics settings: back to the defaults");
                self.graphics_reset = true;
                // The low-memory profile lowers them again at the next start.
                self.settings.low_preset_written = false;
                self.settings.save(&self.user_dir);
            }
            Err(e) => self.ui.note(format!("Cannot reset the game's graphics settings: {e}")),
        }
        self.ui.repaint();
    }

    /// Back in the foreground.
    fn came_back(&mut self) {
        // Android gave the window a new native surface (SDL has updated the
        // window's handle to it already).
        #[cfg(target_os = "android")]
        self.replace_surface();
        self.ui.repaint();
        // The game's files may have been copied in meanwhile (Finder, the
        // Files app, adb).
        if self.game.is_none() && self.game_dir.is_none() {
            self.find_game(None);
        }
    }

    /// A surface for the window's current native surface, for the renderer
    /// to present into from its next frame.
    #[cfg(target_os = "android")]
    fn replace_surface(&mut self) {
        // The window outlives the renderer: the process exits with it.
        let surface = unsafe {
            wgpu::SurfaceTargetUnsafe::from_display_and_window(&self.window, &self.window)
                .map_err(anyhow::Error::from)
                .and_then(|target| Ok(self.instance.create_surface_unsafe(target)?))
        };
        match surface {
            Ok(surface) => self.presentation.replace_surface(surface),
            Err(e) => log::error!("no surface for the window: {e:#}"),
        }
    }

    /// Looks for the game: the folder named on the command line or by
    /// `GLUE5_GAME_DIR`, the last one used, the platform's default places.
    fn find_game(&mut self, explicit: Option<&Path>) {
        self.game_dir = None;
        self.problem = None;
        self.ui.repaint();
        let explicit = explicit.map(PathBuf::from).or_else(|| std::env::var_os("GLUE5_GAME_DIR").map(PathBuf::from));
        if let Some(dir) = explicit {
            if is_game_dir(&dir) {
                self.game_dir = paths::canonical(&dir).ok();
                return;
            }
            self.problem = Some(format!("{} is not a game folder.", dir.display()));
        }
        let candidates = self.settings.game_dir.iter().cloned().chain(paths::default_game_dirs());
        if let Some(dir) = candidates.filter(|d| is_game_dir(d)).find_map(|d| paths::canonical(&d).ok()) {
            self.settings.game_dir = Some(dir.clone());
            self.game_dir = Some(dir);
        }
    }

    /// The folder picker's answer.
    fn chosen(&mut self, choice: Option<PathBuf>) {
        let Some(dir) = choice else { return };
        let Ok(dir) = paths::canonical(&dir) else { return };
        if !is_game_dir(&dir) {
            let problem = format!("{} is not a game folder.", dir.display());
            self.ui.note(problem.clone());
            if self.game.is_none() {
                self.problem = Some(problem);
            }
            return;
        }
        self.settings.game_dir = Some(dir.clone());
        self.settings.save(&self.user_dir);
        if self.game.is_none() {
            self.game_dir = Some(dir);
            self.problem = None;
        }
        self.ui.repaint();
    }
}

/// The next event, waiting up to `timeout` for one.
#[cfg(not(target_os = "ios"))]
fn wait_event(events: &mut sdl3::EventPump, timeout: Duration) -> Option<Event> {
    events.wait_event_timeout(timeout)
}

/// On iOS SDL cannot sleep until an event: its wait runs the run loop for
/// a moment every millisecond (a sixth of a core, all the time). The run
/// loop itself sleeps until a source fires (a touch, a controller, a timer)
/// or the time is up; SDL then has the events it queued.
#[cfg(target_os = "ios")]
fn wait_event(events: &mut sdl3::EventPump, timeout: Duration) -> Option<Event> {
    use std::ffi::c_void;
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: u8) -> i32;
    }
    if let Some(event) = events.poll_event() {
        return Some(event);
    }
    unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, timeout.as_secs_f64(), 1) };
    events.poll_event()
}

/// The refresh rate of the window's display (60 if unknown).
fn window_refresh_rate(window: &sdl3::video::Window) -> f32 {
    let rate = window.get_display().ok().and_then(|d| d.get_mode().ok()).map_or(0.0, |m| m.refresh_rate);
    if rate > 0.0 { rate } else { 60.0 }
}

/// The system's preferred language as a tag like `it-IT`, for the game
/// (phones have no `$LANG`).
fn preferred_language() -> Option<String> {
    use std::ffi::CStr;
    let mut count = 0;
    let locales = unsafe { sdl3::sys::locale::SDL_GetPreferredLocales(&mut count) };
    if locales.is_null() {
        return None;
    }
    let first = unsafe { (count > 0).then(|| &**locales) };
    let tag = first.filter(|l| !l.language.is_null()).map(|l| {
        let language = unsafe { CStr::from_ptr(l.language) }.to_string_lossy();
        match l.country.is_null() {
            true => language.into_owned(),
            false => format!("{language}-{}", unsafe { CStr::from_ptr(l.country) }.to_string_lossy()),
        }
    });
    unsafe { sdl3::sys::stdinc::SDL_free(locales.cast()) };
    tag
}

fn upscaler(settings: &Settings) -> Upscaler {
    settings.upscaler.upscaler()
}

/// The upscalers, as the settings offer them.
const UPSCALERS: &[Upscaling] = &[Upscaling::Bicubic, Upscaling::MetalFx, Upscaling::MetalFxTemporal, Upscaling::Fsr];

/// Keyboard and mouse into the game's input block.
fn game_input(
    input: &InputBlock,
    event: &Event,
    window: &sdl3::video::Window,
    mouse_util: &sdl3::mouse::MouseUtil,
    back_buffer: (u32, u32),
) {
    let (bw, bh) = (back_buffer.0 as f32, back_buffer.1 as f32);
    match *event {
        Event::KeyDown { scancode: Some(sc), repeat, .. } => {
            if sc == Scancode::Escape && mouse_util.relative_mouse_mode(window) {
                mouse_util.set_relative_mouse_mode(window, false);
                input.pointer_locked(false);
            }
            if !repeat {
                key(input, sc, true);
            }
            match sc {
                Scancode::Return | Scancode::KpEnter => input.text("\r"),
                Scancode::Backspace => input.text("\u{8}"),
                _ => {}
            }
        }
        Event::KeyUp { scancode: Some(sc), .. } => key(input, sc, false),
        Event::TextInput { ref text, .. } => input.text(text),
        Event::MouseMotion { x, y, xrel, yrel, .. } => {
            let (ww, wh) = window.size();
            input.mouse_position((x * bw / ww as f32) as i32, (y * bh / wh as f32) as i32);
            input.mouse_motion(xrel.round() as i32, yrel.round() as i32);
        }
        Event::MouseButtonDown { mouse_btn, .. } => {
            // Like the page: a click captures the pointer.
            if !mouse_util.relative_mouse_mode(window) {
                mouse_util.set_relative_mouse_mode(window, true);
                input.pointer_locked(true);
            }
            input.mouse_button(mouse_bit(mouse_btn), true);
        }
        Event::MouseButtonUp { mouse_btn, .. } => input.mouse_button(mouse_bit(mouse_btn), false),
        Event::MouseWheel { y, .. } => input.mouse_wheel(y.round() as i32),
        Event::Window { win_event: WindowEvent::FocusGained, .. } => input.focus(true),
        Event::Window { win_event: WindowEvent::FocusLost, .. } => {
            mouse_util.set_relative_mouse_mode(window, false);
            input.pointer_locked(false);
            input.focus(false);
        }
        _ => {}
    }
}

/// Starts the application again, straight into the game with `play`, else on
/// its start page (desktop; a phone app cannot, and the player reopens it).
fn relaunch(play: bool) {
    if MOBILE {
        return;
    }
    let args: Vec<_> = std::env::args_os().skip(1).filter(|a| a != "--play").collect();
    let started = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        if play {
            command.arg("--play");
        }
        command.args(args).spawn()
    });
    if let Err(e) = started {
        log::warn!("cannot start again: {e}");
    }
}

fn key(input: &InputBlock, sc: sdl3::keyboard::Scancode, down: bool) {
    if let Some(vk) = keymap::vk(sc) {
        input.key(vk, down);
        if let Some(generic) = keymap::generic(vk) {
            input.key(generic, down);
        }
    }
    if let Some(alias) = keymap::numpad_alias(sc) {
        input.key(alias, down);
    }
}

fn mouse_bit(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => mouse::LEFT,
        MouseButton::Right => mouse::RIGHT,
        MouseButton::Middle => mouse::MIDDLE,
        MouseButton::X1 => mouse::X1,
        MouseButton::X2 => mouse::X2,
        _ => 0,
    }
}

/// The system's folder picker, answered asynchronously (SDL calls back on
/// the main thread while events are pumped).
struct FolderPick(Arc<Mutex<Option<Option<PathBuf>>>>);

impl FolderPick {
    fn start(window: &sdl3::video::Window) -> FolderPick {
        let picked: Arc<Mutex<Option<Option<PathBuf>>>> = Default::default();
        let slot = picked.clone();
        sdl3::dialog::show_open_folder_dialog(
            None::<&str>,
            false,
            window,
            Box::new(move |result, _| *slot.lock().unwrap() = Some(result.ok().and_then(|mut v| v.pop()))),
        );
        FolderPick(picked)
    }

    /// `Some(choice)` once answered (`Some(None)`: cancelled).
    fn poll(&self) -> Option<Option<PathBuf>> {
        self.0.lock().unwrap().take()
    }
}
