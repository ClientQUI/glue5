//! A quiet, static home screen. All setup readouts are saved preferences,
//! not live measurements or promises about the game's current memory use.
use super::widgets::{action_button, focus};
use super::{Action, Mode, Page, Status, WARN, theme};
use crate::paths::{self, MOBILE};
use crate::settings::{Settings, Upscaling, WorldPreset};
use egui::{Align2, Color32, FontId, Id, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};

const PANEL: Color32 = Color32::from_rgb(18, 22, 25);
const BORDER: Color32 = Color32::from_rgb(49, 52, 54);
const READY: Color32 = Color32::from_rgb(155, 204, 166);
const MODES: [(Mode, &str, &str); 4] = [
    (Mode::Story, "STORY MODE", "Back to the streets of Los Santos."),
    (Mode::Online, "GTA ONLINE", "Public freemode or a private lobby with friends."),
    (Mode::Sandbox, "SANDBOX", "All of the city. No missions. Your rules."),
    (Mode::Benchmark, "BENCHMARK", "A fly-through to check performance."),
];

/// One scroll surface on narrow screens; two independently scrolling columns
/// in landscape. No animations or timer-driven repaints while idle.
pub(super) fn start_page(
    ctx: &egui::Context,
    settings: &Settings,
    status: &Status,
    starting: Option<Mode>,
    actions: &mut Vec<Action>,
    go: &mut Option<Option<Page>>,
) -> Option<Mode> {
    start_page_for(ctx, settings, status, starting, actions, go, MOBILE)
}

fn start_page_for(
    ctx: &egui::Context,
    settings: &Settings,
    status: &Status,
    starting: Option<Mode>,
    actions: &mut Vec<Action>,
    go: &mut Option<Option<Page>>,
    mobile: bool,
) -> Option<Mode> {
    let safe = ctx.content_rect();
    let scale = theme::layout_scale(safe);
    let edge = 24.0 * scale;
    let size =
        vec2((safe.width() - edge * 2.0).clamp(1.0, 1160.0).floor(), (safe.height() - edge * 2.0).max(1.0).floor());
    let stacked = size.x < 600.0 * scale;
    let compact = size.y < 480.0;
    let mut chosen = None;
    let mut quit = false;
    let going = starting.or(status.preparing.map(|_| status.mode));
    egui::Area::new(Id::new("start"))
        .default_size(size)
        .fade_in(false)
        .constrain(false)
        .fixed_pos(pos2(safe.center().x - size.x * 0.5, safe.top() + edge))
        .show(ctx, |ui| {
            theme::apply_density(ui, scale);
            ui.set_width(size.x);
            ui.set_height(size.y);
            ui.horizontal(|ui| {
                ui.label(
                    theme::heading("GLUE 5", if compact { 38.0 * scale } else { 52.0 * scale }).color(theme::PAPER),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    badge(
                        ui,
                        if going.is_some() {
                            "PREPARING"
                        } else if status.game_dir.is_some() {
                            "READY TO PLAY"
                        } else {
                            "SETUP NEEDED"
                        },
                        if going.is_some() {
                            theme::SUNSET
                        } else if status.game_dir.is_some() {
                            READY
                        } else {
                            WARN
                        },
                    );
                });
            });
            ui.add_space(6.0 * scale);
            rule(ui);
            ui.add_space(if compact { 12.0 * scale } else { 28.0 * scale });
            let footer = 25.0 * scale;
            let remaining = (ui.max_rect().bottom() - ui.cursor().top() - footer).max(1.0).floor();
            let mut menu = |ui: &mut egui::Ui| {
                ui.label(RichText::new("LOS SANTOS / NATIVE").size(10.0 * scale).color(theme::SUNSET));
                if !compact {
                    ui.add_space(8.0 * scale);
                    ui.label(theme::heading("THE CITY\nIS YOURS.", 54.0 * scale).color(theme::PAPER));
                    ui.add_space(12.0 * scale);
                }
                ui.add_enabled_ui(status.game_dir.is_some() && going.is_none(), |ui| {
                    for (mode, title, subtitle) in MODES {
                        let label = match status.preparing {
                            _ if going != Some(mode) => title.to_owned(),
                            Some(done) => format!("PREPARING {:.0}%", done.clamp(0.0, 1.0) * 100.0),
                            None => "STARTING…".to_owned(),
                        };
                        if mode_button(ui, mode, &label, subtitle, status.mode == mode, compact).clicked() {
                            chosen = Some(mode);
                        }
                    }
                });
                if let Some(done) = status.preparing {
                    ui.add(egui::ProgressBar::new(done.clamp(0.0, 1.0)).fill(theme::SUNSET).animate(false));
                    ui.label(
                        RichText::new("Preparing shaders for this device. The first launch takes longer.")
                            .small()
                            .color(theme::MUTED),
                    );
                }
                ui.add_space(6.0 * scale);
                ui.columns(2, |columns| {
                    if utility_button(&mut columns[0], "⚙  Settings").clicked() {
                        *go = Some(Some(Page::Settings));
                    }
                    if utility_button(&mut columns[1], "★  Credits").clicked() {
                        *go = Some(Some(Page::Credits));
                    }
                });
                if !mobile && utility_button(ui, "🗙  Quit").clicked() {
                    quit = true;
                }
            };
            if stacked {
                egui::ScrollArea::vertical()
                    .id_salt("home-stacked")
                    .max_height(remaining)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        menu(ui);
                        ui.add_space(16.0 * scale);
                        details(ui, settings, status, actions, mobile);
                    });
            } else {
                let gap = 36.0 * scale;
                let left = (size.x * 0.48).min(440.0 * scale);
                let right = (size.x - left - gap).min(360.0 * scale);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    column(ui, "home-menu", left, remaining, menu);
                    // Leave some of the skyline visible on roomy displays.
                    let skyline = size.x - left - right - gap * 2.0;
                    if skyline > 0.0 {
                        ui.add_space(skyline);
                    }
                    column(ui, "home-details", right, remaining, |ui| details(ui, settings, status, actions, mobile));
                });
            }
            ui.add_space(8.0 * scale);
            ui.label(
                RichText::new(if mobile {
                    "Tap to play  ·  Controller ready  ·  Back + Start: settings"
                } else {
                    "Arrows / D-pad: navigate  ·  Enter / A: select  ·  F1: settings"
                })
                .size(10.0 * scale)
                .color(theme::MUTED),
            );
        });
    if quit {
        actions.push(Action::Quit);
    }
    chosen
}

fn column(ui: &mut egui::Ui, id: &str, width: f32, height: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(vec2(width, height), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_width(width);
        egui::ScrollArea::vertical().id_salt(id).max_height(height).auto_shrink([false, false]).show(ui, add);
    });
}

fn rule(ui: &mut egui::Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0, BORDER));
}

fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    let scale = theme::density(ui);
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric((9.0 * scale) as i8, (5.0 * scale) as i8))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(10.0 * scale).color(color));
        });
}

fn mode_button(
    ui: &mut egui::Ui,
    mode: Mode,
    title: &str,
    subtitle: &str,
    last: bool,
    compact: bool,
) -> egui::Response {
    ui.push_id(format!("home-{mode:?}"), |ui| {
        let scale = theme::density(ui);
        let primary = mode == Mode::Story;
        let (r, response) = ui.allocate_exact_size(
            vec2(
                ui.available_width(),
                (if compact {
                    if primary { 66.0 } else { 54.0 }
                } else if primary {
                    82.0
                } else {
                    62.0
                }) * scale,
            ),
            Sense::click(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), title));
        if response.clicked() {
            response.request_focus();
        }
        let highlighted = ui.is_enabled() && (response.hovered() || response.has_focus());
        let fill = if primary {
            if highlighted { theme::PAPER } else { theme::SUNSET }
        } else if highlighted {
            Color32::from_rgba_unmultiplied(42, 46, 48, 245)
        } else {
            PANEL
        };
        let ink = if primary { theme::INK } else { theme::PAPER };
        ui.painter().rect(
            r,
            (12.0 * scale) as u8,
            fill,
            Stroke::new(1.0, if highlighted { theme::SUNSET } else { BORDER }),
            StrokeKind::Inside,
        );
        if response.has_focus() {
            ui.painter().rect_stroke(
                r.shrink(3.0),
                9,
                Stroke::new(2.0, if primary { theme::INK } else { theme::SUNSET }),
                StrokeKind::Inside,
            );
        }
        let c = pos2(r.left() + 25.0 * scale, r.center().y);
        match mode {
            Mode::Story => {
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![c + vec2(-5.0, -9.0) * scale, c + vec2(9.0, 0.0) * scale, c + vec2(-5.0, 9.0) * scale],
                    ink,
                    Stroke::NONE,
                ));
            }
            Mode::Online | Mode::Sandbox => {
                ui.painter().circle_stroke(c, 10.0 * scale, Stroke::new(1.2, ink));
                ui.painter()
                    .line_segment([c + vec2(-5.0, 5.0) * scale, c + vec2(5.0, -5.0) * scale], Stroke::new(2.0, ink));
                ui.painter().circle_filled(c, 2.0 * scale, ink);
            }
            Mode::Benchmark => {
                for (x, h) in [(-7.0, 6.0), (0.0, 12.0), (7.0, 18.0)] {
                    ui.painter().rect_filled(
                        Rect::from_min_size(c + vec2(x - 2.0, 9.0 - h) * scale, vec2(4.0, h) * scale),
                        1,
                        ink,
                    );
                }
            }
        }
        let text =
            Rect::from_min_max(pos2(r.left() + 52.0 * scale, r.top()), pos2(r.right() - 27.0 * scale, r.bottom()));
        paint_text(ui, text, title, if primary { 25.0 * scale } else { 20.0 * scale }, -8.0 * scale, ink, true);
        paint_text(
            ui,
            text,
            subtitle,
            11.0 * scale,
            14.0 * scale,
            if primary { Color32::from_rgb(67, 58, 46) } else { theme::MUTED },
            false,
        );
        ui.painter().text(
            pos2(r.right() - 17.0 * scale, r.center().y),
            Align2::CENTER_CENTER,
            "›",
            FontId::proportional(24.0 * scale),
            ink,
        );
        if last {
            ui.painter().circle_filled(pos2(r.right() - 17.0 * scale, r.top() + 11.0 * scale), 2.0 * scale, ink);
        }
        focus(response).on_hover_text(format!("{title}: {subtitle}{}", if last { " Last selected mode." } else { "" }))
    })
    .inner
}

fn paint_text(ui: &egui::Ui, rect: Rect, text: &str, size: f32, offset: f32, color: Color32, condensed: bool) {
    let text = if condensed { theme::heading(text, size) } else { RichText::new(text).size(size) }.color(color);
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        rect.width(),
        FontId::proportional(size),
    );
    ui.painter().galley(pos2(rect.left(), rect.center().y + offset - galley.size().y * 0.5), galley, color);
}

fn utility_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let scale = theme::density(ui);
    focus(
        ui.add_sized(
            [ui.available_width(), (44.0 * scale).max(44.0)],
            egui::Button::new(RichText::new(label).size(14.0 * scale).color(theme::PAPER))
                .fill(Color32::from_black_alpha(95))
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(10),
        ),
    )
}

fn panel(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let scale = theme::density(ui);
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .inner_margin((16.0 * scale) as i8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn details(ui: &mut egui::Ui, settings: &Settings, status: &Status, actions: &mut Vec<Action>, mobile: bool) {
    // Setup guidance and its action must not hide below a performance card.
    if status.game_dir.is_none() {
        files_panel(ui, status, actions, mobile);
    }
    panel(ui, |ui| session_setup(ui, settings, status));
    if status.game_dir.is_some() {
        files_panel(ui, status, actions, mobile);
    }
    if let Some(results) = status.benchmark {
        panel(ui, |ui| {
            ui.label(RichText::new("LAST BENCHMARK").small().color(theme::MUTED));
            ui.label(
                theme::heading(&format!("{:.0} FPS", results.avg_fps), 30.0 * theme::density(ui)).color(theme::SUNSET),
            );
            ui.label(
                RichText::new(format!(
                    "{:.0} FPS minimum · {:.0}% under 33 ms",
                    results.min_fps,
                    results.smooth * 100.0
                ))
                .small()
                .color(theme::MUTED),
            );
        });
    }
    if let Some(why) = status.last_failure {
        panel(ui, |ui| {
            ui.label(RichText::new("LAST SESSION STOPPED").small().color(WARN));
            ui.label(RichText::new(why).small().color(theme::MUTED));
            if status.graphics_reset {
                ui.label(super::pages::graphics_reset_note());
            } else if action_button(ui, "Reset graphics").clicked() {
                actions.push(Action::ResetGraphics);
            }
        });
    }
}

fn files_panel(ui: &mut egui::Ui, status: &Status, actions: &mut Vec<Action>, mobile: bool) {
    panel(ui, |ui| {
        ui.label(RichText::new("GAME FILES").small().color(theme::MUTED));
        if let Some(dir) = status.game_dir {
            let text = dir.display().to_string();
            ui.add(egui::Label::new(RichText::new(&text).small().color(theme::PAPER)).truncate()).on_hover_text(text);
        } else {
            if let Some(problem) = status.problem {
                ui.label(RichText::new(problem).small().color(WARN));
            }
            ui.label(RichText::new(paths::where_the_game_goes()).small().color(theme::MUTED));
            ui.label(RichText::new(paths::game_layout()).monospace().small().color(theme::MUTED));
        }
        if utility_button(ui, if mobile { "🗁  Find files" } else { "🗁  Game folder" }).clicked() {
            actions.push(if mobile { Action::FindGame } else { Action::ChooseGame });
        }
    });
}

fn session_setup(ui: &mut egui::Ui, settings: &Settings, status: &Status) {
    ui.spacing_mut().interact_size.y = 0.0;
    ui.spacing_mut().item_spacing.y = 5.0 * theme::density(ui);
    ui.label(RichText::new("SESSION SETUP").small().color(theme::MUTED));
    let (name, about) = match settings.world_preset {
        WorldPreset::Eco => ("ECO", "Lower world detail. More room to play."),
        WorldPreset::Balanced => ("BALANCED", "A balance of world detail and performance."),
        WorldPreset::Detail => ("DETAIL", "More world detail. Higher render cost."),
        WorldPreset::Custom => ("CUSTOM", "Your own world-detail settings."),
        WorldPreset::Game => ("GAME DEFAULTS", "World detail follows the game's settings."),
    };
    ui.label(theme::heading(name, 30.0 * theme::density(ui)).color(theme::SUNSET));
    ui.label(RichText::new(about).small().color(theme::MUTED));
    rule(ui);
    let fps = if settings.max_fps == 0 {
        if settings.vsync { "Display limit" } else { "Uncapped" }.to_owned()
    } else {
        format!("{} FPS", settings.max_fps)
    };
    stat(ui, "Frame cap", &fps);
    let input = settings.input_size(status.window);
    let scaling = if settings.upscaler != Upscaling::Bicubic {
        format!("{}p > {}p", input.1, settings.output_size(status.window).1)
    } else {
        format!("{}p", input.1)
    };
    stat(ui, if settings.upscaler != Upscaling::Bicubic { "Upscaling" } else { "Render height" }, &scaling);
    stat(
        ui,
        "Resource heap",
        &if settings.resource_budget.mb() == 0 {
            "Auto".to_owned()
        } else {
            format!("{} MiB", settings.resource_budget.mb())
        },
    );
    stat(
        ui,
        "Texture limit",
        &if settings.texture_detail.max_dimension() == 0 {
            "Original".to_owned()
        } else {
            format!("{} px", settings.texture_detail.max_dimension())
        },
    );
    ui.label(RichText::new("Saved start settings · heap budget is not total RAM.").small().color(theme::MUTED));
    if settings.low_memory {
        ui.label(RichText::new("Legacy low-memory mode enabled.").small().color(WARN));
    }
}

fn stat(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).small().color(theme::MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).small().color(theme::PAPER));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ResourceBudget, TextureDetail};
    use egui::{Event, FullOutput, Key, Modifiers, Pos2, RawInput, SafeAreaInsets};
    use std::path::Path;

    fn status() -> Status<'static> {
        Status {
            game_dir: Some(Path::new("Documents/game")),
            problem: None,
            started: None,
            last_failure: None,
            graphics_reset: false,
            window: (2868, 1320),
            upscalers: &[Upscaling::Bicubic, Upscaling::MetalFx, Upscaling::MetalFxTemporal],
            frame_generation: true,
            metal_hud: false,
            refresh_rate: 120.0,
            preparing: None,
            mode: Mode::Story,
            benchmark: None,
            thermal_state: 0,
            effective_fps: 30,
            actual_input: None,
        }
    }

    fn settings() -> Settings {
        let mut settings = Settings::default();
        settings.low_memory = false;
        settings.resource_budget = ResourceBudget::Compact;
        settings.texture_detail = TextureDetail::Minimal;
        settings.performance_profile(0);
        settings
    }

    fn draw(
        ctx: &egui::Context,
        settings: &Settings,
        status: &Status,
        raw: RawInput,
    ) -> (FullOutput, Option<Mode>, Vec<Action>, Option<Option<Page>>) {
        let mut actions = Vec::new();
        let mut go = None;
        ctx.begin_pass(raw);
        let chosen = start_page(ctx, settings, status, None, &mut actions, &mut go);
        let mut output = ctx.end_pass();
        output.textures_delta.clear();
        (output, chosen, actions, go)
    }

    fn input(size: egui::Vec2, events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            focused: true,
            events,
            ..Default::default()
        }
    }

    fn text(output: &FullOutput, label: &str) -> Option<Rect> {
        output.shapes.iter().find_map(|s| match &s.shape {
            egui::Shape::Text(t) if t.galley.job.text == label => Some(Rect::from_min_size(t.pos, t.galley.size())),
            _ => None,
        })
    }

    #[test]
    fn home_keeps_safe_areas_and_saved_settings_at_all_sizes() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let settings = settings();
        let before = serde_json::to_vec(&settings).unwrap();
        for size in [
            vec2(1440.0, 900.0),
            vec2(874.0, 402.0),
            vec2(768.0, 1024.0),
            vec2(390.0, 844.0),
            vec2(320.0, 480.0),
            vec2(600.0, 400.0),
        ] {
            for missing in [false, true] {
                let mut status = status();
                if missing {
                    status.game_dir = None;
                    status.problem = Some("Game files not found. Choose the folder containing b/ and data/.");
                }
                for _ in 0..4 {
                    let mut raw = input(size, vec![]);
                    raw.safe_area_insets = Some(SafeAreaInsets(egui::epaint::MarginF32 {
                        left: if size.x > size.y { 62.0 } else { 8.0 },
                        right: if size.x > size.y { 62.0 } else { 8.0 },
                        top: 0.0,
                        bottom: 21.0,
                    }));
                    let (mut output, chosen, actions, go) = draw(&ctx, &settings, &status, raw);
                    assert!(chosen.is_none() && actions.is_empty() && go.is_none());
                    output.textures_delta.clear();
                }
                let area = ctx.memory(|m| m.area_rect(Id::new("start"))).unwrap();
                assert!(ctx.content_rect().contains_rect(area), "{size:?}: {area:?} outside {:?}", ctx.content_rect());
            }
        }
        assert_eq!(serde_json::to_vec(&settings).unwrap(), before, "home is strictly read-only");
    }

    #[test]
    fn phone_modes_and_settings_are_visible_without_scrolling() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        for pass in 0..4 {
            let mut raw = input(vec2(874.0, 402.0), vec![]);
            raw.safe_area_insets =
                Some(SafeAreaInsets(egui::epaint::MarginF32 { left: 62.0, right: 62.0, top: 0.0, bottom: 21.0 }));
            let (mut output, ..) = draw(&ctx, &settings(), &status(), raw);
            if pass < 2 {
                continue;
            }
            for label in ["STORY MODE", "SANDBOX", "BENCHMARK", "⚙  Settings"] {
                let rect = text(&output, label).unwrap();
                let clip = output
                    .shapes
                    .iter()
                    .find(|s| match &s.shape {
                        egui::Shape::Text(t) => t.galley.job.text == label,
                        _ => false,
                    })
                    .unwrap()
                    .clip_rect;
                assert!(clip.contains_rect(rect), "{label} clipped: {rect:?} in {clip:?}");
            }
            output.textures_delta.clear();
        }
    }

    #[test]
    fn idle_home_does_not_schedule_continuous_repaints() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        for pass in 0..8 {
            let mut raw = input(vec2(956.0, 440.0), vec![]);
            raw.time = Some(pass as f64);
            let (output, ..) = draw(&ctx, &settings(), &status(), raw);
            if pass == 7 {
                let delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
                assert!(delay >= std::time::Duration::from_secs(10), "idle repaint: {delay:?}");
            }
        }
    }

    #[test]
    fn pointer_and_controller_activation_respect_start_readiness() {
        for blocked in [0, 1, 2] {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let mut status = status();
            if blocked == 1 {
                status.game_dir = None;
            }
            if blocked == 2 {
                status.preparing = Some(0.4);
            }
            let mut at = Pos2::ZERO;
            for pass in 0..4 {
                let (mut output, ..) = draw(&ctx, &settings(), &status, input(vec2(1280.0, 720.0), vec![]));
                if pass >= 2 {
                    at = text(&output, if blocked == 2 { "PREPARING 40%" } else { "STORY MODE" }).unwrap().center();
                }
                output.textures_delta.clear();
            }
            for pressed in [true, false] {
                let (mut output, chosen, actions, ..) = draw(
                    &ctx,
                    &settings(),
                    &status,
                    input(
                        vec2(1280.0, 720.0),
                        vec![
                            Event::PointerMoved(at),
                            Event::PointerButton {
                                pos: at,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: Modifiers::NONE,
                            },
                        ],
                    ),
                );
                if !pressed {
                    assert_eq!(chosen, if blocked == 0 { Some(Mode::Story) } else { None });
                }
                assert!(actions.is_empty());
                output.textures_delta.clear();
            }
        }
        let ctx = egui::Context::default();
        theme::install(&ctx);
        for key in [None, None, Some(Key::Tab), Some(Key::Enter)] {
            let events = key
                .into_iter()
                .map(|key| Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                })
                .collect();
            let (mut output, chosen, ..) = draw(&ctx, &settings(), &status(), input(vec2(1280.0, 720.0), events));
            if key == Some(Key::Enter) {
                assert_eq!(chosen, Some(Mode::Story));
            }
            output.textures_delta.clear();
        }
    }

    /// Render actual egui output through the same renderer as the app, without
    /// opening a window or starting the engine. Example:
    /// GLUE5_UI_SHOTS=target/ui-preview cargo test -p glue5 render_home_preview -- --ignored
    #[test]
    #[ignore = "requires a GPU and GLUE5_UI_SHOTS output directory"]
    fn render_home_preview() {
        use std::time::Duration;
        let dir = std::path::PathBuf::from(std::env::var_os("GLUE5_UI_SHOTS").expect("set GLUE5_UI_SHOTS"));
        std::fs::create_dir_all(&dir).unwrap();
        let instance = glue5_gpu::instance();
        let adapter = pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions { apply_limit_buckets: false, ..Default::default() }),
        )
        .unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let mut renderer = egui_wgpu::Renderer::new(&device, wgpu::TextureFormat::Rgba8Unorm, Default::default());
        for (name, size, insets, missing, preparing) in [
            ("desktop", vec2(1280.0, 720.0), [0.0, 0.0, 0.0, 0.0], false, false),
            ("iphone", vec2(956.0, 440.0), [62.0, 62.0, 0.0, 21.0], false, false),
            ("portrait", vec2(390.0, 844.0), [8.0, 8.0, 24.0, 21.0], false, false),
            ("setup", vec2(956.0, 440.0), [62.0, 62.0, 0.0, 21.0], true, false),
            ("preparing", vec2(956.0, 440.0), [62.0, 62.0, 0.0, 21.0], false, true),
        ] {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let background = theme::background(&ctx);
            let mut status = status();
            if missing {
                status.game_dir = None;
                status.problem = Some("Game files not found.");
            }
            if preparing {
                status.preparing = Some(0.42);
            }
            let ppp = 2.0;
            let pixels = [(size.x * ppp) as u32, (size.y * ppp) as u32];
            for pass in 0..4 {
                let mut raw = input(size, vec![]);
                raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
                raw.safe_area_insets = Some(SafeAreaInsets(egui::epaint::MarginF32 {
                    left: insets[0],
                    right: insets[1],
                    top: insets[2],
                    bottom: insets[3],
                }));
                ctx.begin_pass(raw);
                theme::backdrop(&ctx, background.as_ref());
                start_page_for(&ctx, &settings(), &status, None, &mut Vec::new(), &mut None, name != "desktop");
                let mut output = ctx.end_pass();
                for (id, deltas) in &output.textures_delta.set {
                    for delta in deltas {
                        renderer.update_texture(&device, &queue, *id, delta);
                    }
                }
                let shapes = ctx.tessellate(output.shapes, ppp);
                let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: pixels, pixels_per_point: ppp };
                let target = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("home preview"),
                    size: wgpu::Extent3d { width: pixels[0], height: pixels[1], depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let mut encoder = device.create_command_encoder(&Default::default());
                let callbacks = renderer.update_buffers(&device, &queue, &mut encoder, &shapes, &screen);
                assert!(callbacks.is_empty());
                let view = target.create_view(&Default::default());
                {
                    let mut render = encoder
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                depth_slice: None,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            ..Default::default()
                        })
                        .forget_lifetime();
                    renderer.render(&mut render, &shapes, &screen);
                }
                let bpr = (pixels[0] * 4).next_multiple_of(256);
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: (bpr * pixels[1]) as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                encoder.copy_texture_to_buffer(
                    target.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(bpr),
                            rows_per_image: Some(pixels[1]),
                        },
                    },
                    target.size(),
                );
                let index = queue.submit([encoder.finish()]);
                let (tx, rx) = std::sync::mpsc::channel();
                buffer.map_async(wgpu::MapMode::Read, .., move |result| tx.send(result).unwrap());
                device
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(index),
                        timeout: Some(Duration::from_secs(10)),
                    })
                    .unwrap();
                rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
                if pass == 3 {
                    let bytes = buffer.get_mapped_range(..).unwrap();
                    let rgba: Vec<_> = bytes
                        .chunks(bpr as usize)
                        .flat_map(|row| row[..pixels[0] as usize * 4].iter().copied())
                        .collect();
                    let file = std::fs::File::create(dir.join(format!("{name}.png"))).unwrap();
                    let mut png = png::Encoder::new(file, pixels[0], pixels[1]);
                    png.set_color(png::ColorType::Rgba);
                    png.set_depth(png::BitDepth::Eight);
                    png.write_header().unwrap().write_image_data(&rgba).unwrap();
                }
                buffer.unmap();
                for id in &output.textures_delta.free {
                    renderer.free_texture(id);
                }
                output.textures_delta.clear();
            }
        }
    }
}
