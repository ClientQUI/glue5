//! Home and the floating settings window share the same responsive controls.
use super::widgets::{
    Control, action_button, choice, close_button, focus, segmented, setting, slider, text_field, toggle,
};
use super::{Action, Page, SettingsTab, Status, WARN, theme};
use crate::paths::MOBILE;
use crate::settings::{
    self, Corner, Efficiency, OverlayDetail, PerfOverlay, RENDER_SCALES, ResourceBudget, Settings, TextureDetail,
    TouchMode, Upscaling, WorldDetail, WorldPreset,
};
use egui::{Align, Align2, Id, Layout, RichText, vec2};

pub(super) fn lobby_controls(ui: &mut egui::Ui, settings: &mut Settings) {
    let scale = theme::density(ui);
    let lobbies = [(false, "PUBLIC"), (true, "PRIVATE")];
    if segmented(ui, &mut settings.online_private, &lobbies, 38.0 * scale)
        && settings.online_private
        && settings::lobby_key(&settings.online_lobby).is_empty()
    {
        settings.online_lobby = settings::new_lobby_code();
    }
    let what = if settings.online_private {
        "Only the players with the same code: share it with your friends."
    } else {
        "Anyone playing now, matched automatically."
    };
    ui.label(RichText::new(what).size(12.0 * scale).color(theme::MUTED));
    if settings.online_private {
        ui.horizontal(|ui| {
            let button = 92.0 * scale;
            let field = ui.available_width() - button - ui.spacing().item_spacing.x;
            ui.allocate_ui(vec2(field, 44.0 * scale), |ui| {
                text_field(ui, "Code", &mut settings.online_lobby, "K7Q-29F", 11)
            });
            let new = egui::Button::new(RichText::new("⟳  New").size(14.0 * scale)).corner_radius((12.0 * scale) as u8);
            if focus(ui.add_sized([button, 44.0 * scale], new)).on_hover_text("A new code").clicked() {
                settings.online_lobby = settings::new_lobby_code();
            }
        });
        settings.online_lobby.make_ascii_uppercase();
    }
    text_field(ui, "Name", &mut settings.online_name, "Player", 16);
}

pub(super) fn graphics_reset_note() -> RichText {
    RichText::new("Graphics reset. Backup: settings.xml.bak").small().color(theme::MUTED)
}

pub(super) fn settings_page(
    ctx: &egui::Context,
    settings: &mut Settings,
    status: &Status,
    note: Option<&str>,
    tab: &mut SettingsTab,
    scale_edit: &mut Option<f32>,
    actions: &mut Vec<Action>,
    go: &mut Option<Option<Page>>,
) -> bool {
    sheet(ctx, "settings-window", "SETTINGS", |ui| {
        let scale = theme::density(ui);
        let tabs = [
            (SettingsTab::Video, "VIDEO"),
            (SettingsTab::World, "WORLD"),
            (SettingsTab::Controls, "CONTROLS"),
            (SettingsTab::Game, "GAME"),
        ];
        segmented(ui, tab, &tabs, 40.0 * scale);
        ui.add_space(10.0 * scale);
        if let Some(note) = note {
            ui.label(RichText::new(note).small().color(WARN));
        }
        settings_items(ui, settings, status, *tab, scale_edit, actions, go);
    })
}

/// A window over the start page or the game, centred in the safe area: a
/// title and a close button, then what `add` adds, scrolling. True when
/// closed.
pub(super) fn sheet(ctx: &egui::Context, id: &str, title: &str, add: impl FnOnce(&mut egui::Ui)) -> bool {
    let area = ctx.content_rect();
    let scale = theme::layout_scale(area);
    let edge = 6.0 * scale;
    let padding = (20.0 * scale) as i8;
    let size = vec2(
        (area.width() - 2.0 * edge - 2.0).clamp(1.0, 718.0),
        (area.height() - 2.0 * edge - 2.0).clamp(1.0, if scale < 1.0 { f32::INFINITY } else { 558.0 }),
    );
    // Padding lives in the scroll content, so rows scroll to the window edge.
    let frame = egui::Frame::window(&ctx.global_style()).inner_margin(0).corner_radius((22.0 * scale) as u8);
    let mut close = false;
    egui::Window::new(title)
        .id(Id::new(id))
        .title_bar(false)
        .movable(false)
        .resizable(false)
        .collapsible(false)
        .pivot(Align2::CENTER_CENTER)
        .fixed_pos(area.center())
        .fixed_size(size)
        .frame(frame)
        .show(ctx, |ui| {
            theme::apply_density(ui, scale);
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: padding,
                    right: padding,
                    top: (12.0 * scale) as i8,
                    bottom: (8.0 * scale) as i8,
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(theme::heading(title, 28.0 * scale));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            close = close_button(ui).clicked();
                        });
                    });
                });
            let height = (size.y - ui.min_rect().height()).max(1.0);
            egui::ScrollArea::vertical()
                .content_margin(0)
                .id_salt((id, "scroll"))
                .max_height(height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 10.0 * scale;
                    egui::Frame::new()
                        .inner_margin(egui::Margin {
                            left: padding,
                            right: padding,
                            top: (4.0 * scale) as i8,
                            bottom: (12.0 * scale) as i8,
                        })
                        .show(ui, add);
                });
        });
    close
}

fn upscaling_controls(ui: &mut egui::Ui, settings: &mut Settings, status: &Status) {
    if status.upscalers.len() > 1 {
        let options: Vec<_> = status
            .upscalers
            .iter()
            .map(|&u| {
                let name = match u {
                    Upscaling::Bicubic => "Bicubic",
                    Upscaling::MetalFx => "MetalFX",
                    Upscaling::MetalFxTemporal => "MetalFX Temporal",
                    Upscaling::Fsr => "FSR 2",
                };
                (u, name)
            })
            .collect();
        setting(ui, "Upscaling", Control::Choice, None, |ui| choice(ui, &mut settings.upscaler, &options));
    }
    if settings.upscaler == Upscaling::Fsr {
        setting(ui, "Sharpening", Control::Slider, None, |ui| {
            slider(ui, &mut settings.sharpness, 0.0..=1.0, 0.05, "%", 0)
        });
    }
    if status.frame_generation && settings.upscaler == Upscaling::MetalFxTemporal {
        setting(ui, "Frame generation", Control::Toggle, None, |ui| toggle(ui, &mut settings.frame_generation));
    }
}

fn settings_items(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    status: &Status,
    tab: SettingsTab,
    scale_edit: &mut Option<f32>,
    actions: &mut Vec<Action>,
    go: &mut Option<Option<Page>>,
) {
    let started = status.started;
    match tab {
        SettingsTab::Video => {
            ui.label(RichText::new("Quick profiles · world detail applies at next start").small().color(theme::MUTED));
            ui.horizontal_wrapped(|ui| {
                for (profile, label) in [(0, "Eco · 30 FPS"), (1, "Balanced · 30 FPS"), (2, "Detail · 60 FPS")] {
                    if focus(ui.add(egui::Button::new(label).min_size(vec2(0.0, 40.0)))).clicked() {
                        settings.performance_profile(profile);
                        *scale_edit = None;
                    }
                }
            });
            let mut preset = settings.scaling_preset();
            let mut pairs: Vec<_> = settings::SCALE_PAIRS.iter().enumerate().map(|(i, p)| (i, p.2)).collect();
            pairs.push((settings::SCALE_PAIRS.len(), "Custom / %"));
            let response = setting(ui, "Scaling preset", Control::Choice, Some("Aspect follows your screen"), |ui| {
                choice(ui, &mut preset, &pairs)
            });
            if response.changed() {
                settings.set_scaling_preset(preset);
                *scale_edit = None;
            }
            let mut fixed = settings.render_lines.is_some();
            let r = setting(ui, "Input resolution", Control::Choice, None, |ui| {
                choice(ui, &mut fixed, &[(false, "Display %"), (true, "Custom height")])
            });
            if r.changed() {
                settings.render_lines = fixed.then_some(settings.input_size(status.window).1);
            }
            if let Some(lines) = settings.render_lines {
                let mut h = lines as f32;
                if setting(ui, "Render height", Control::Slider, Some("Game renders at this height"), |ui| {
                    slider(ui, &mut h, 240.0..=2160.0, 2.0, "p", 0)
                })
                .changed()
                {
                    settings.render_lines = Some(h as u32);
                }
            } else {
                let mut scale = scale_edit.unwrap_or(settings.render_scale());
                let (rw, rh) = settings::render_size(status.window, scale, settings.low_memory);
                let subtitle = format!("{rw} × {rh}");
                let r = setting(ui, "Resolution", Control::Slider, Some(&subtitle), |ui| {
                    slider(ui, &mut scale, RENDER_SCALES, 0.05, "%", 0)
                });
                if r.dragged() {
                    *scale_edit = Some(scale);
                } else if r.changed() || r.drag_stopped() {
                    settings.render_scale = Some(settings::round_scale(scale));
                    *scale_edit = None;
                }
            }
            let mut native = settings.upscale_lines.is_none();
            if setting(ui, "MetalFX target", Control::Choice, Some("Then fit to display"), |ui| {
                choice(ui, &mut native, &[(true, "Display"), (false, "Custom height")])
            })
            .changed()
            {
                settings.upscale_lines = (!native).then_some(720);
            }
            if let Some(lines) = settings.upscale_lines {
                let mut h = lines as f32;
                if setting(ui, "Target height", Control::Slider, Some("Capped to display size"), |ui| {
                    slider(ui, &mut h, 240.0..=2160.0, 2.0, "p", 0)
                })
                .changed()
                {
                    settings.upscale_lines = Some(h as u32);
                }
            }
            let input = settings.input_size(status.window);
            let output = settings.output_size(status.window);
            if let Some((w, h)) = status.actual_input {
                ui.label(RichText::new(format!("Active input request: {w}×{h}")).small().color(theme::MUTED));
            }
            if settings.upscaler == Upscaling::Bicubic {
                ui.label(
                    RichText::new("Bicubic scales directly to the display; the intermediate target needs an upscaler.")
                        .small()
                        .color(theme::MUTED),
                );
            }
            ui.label(
                RichText::new(format!(
                    "Requested: {}×{} → {}×{} → display {}×{}",
                    input.0, input.1, output.0, output.1, status.window.0, status.window.1
                ))
                .small()
                .color(theme::MUTED),
            );
            upscaling_controls(ui, settings, status);
            if MOBILE {
                setting(ui, "Adaptive quality", Control::Toggle, None, |ui| {
                    toggle(ui, &mut settings.adaptive_graphics)
                });
            }
            let state = ["Nominal", "Fair", "Serious", "Critical"][status.thermal_state.min(3) as usize];
            let subtitle = format!(
                "{state} · active limit {} FPS",
                if status.effective_fps > 0 { status.effective_fps } else { 60 }
            );
            setting(ui, "Thermal protection", Control::Toggle, Some(&subtitle), |ui| {
                toggle(ui, &mut settings.thermal_protection)
            });
            let efficiency = settings.efficiency_now();
            let efficiency_note = match crate::settings::efficiency_forced() {
                Some(Efficiency::Off) => "Forced off",
                Some(Efficiency::Hybrid) => "Forced to Hybrid",
                Some(Efficiency::FullE) => "Forced to Full-E",
                None => match efficiency {
                    Efficiency::Off => "Performance cores",
                    Efficiency::Hybrid => "Recommended. E-cores in play, P-cores while loading",
                    Efficiency::FullE => "E-cores the whole time. About 20 FPS",
                },
            };
            setting(ui, "Efficiency", Control::Choice, Some(efficiency_note), |ui| {
                choice(ui, &mut settings.efficiency, &[
                    (Efficiency::Off, "Off"),
                    (Efficiency::Hybrid, "Hybrid"),
                    (Efficiency::FullE, "Full-E"),
                ])
            });
            ui.label(
                RichText::new(
                    "Hybrid is recommended. Loading runs uncapped on performance cores, including streaming and shader compiles, then play prefers efficiency cores and paces at 24 FPS. A missed frame briefly raises the game and render threads. Audio stays immediate, with 250 ms buffered, on its own thread. Full-E stays on efficiency cores and paces at 20 FPS, including the load. Off keeps the frame, streaming and compile threads interactive; the frame-rate cap still waits until the load's reads go quiet.",
                )
                .small()
                .color(theme::MUTED),
            );
            let unlimited = if settings.vsync { "Display" } else { "Unlimited" };
            let mut rates = vec![(0, unlimited), (60, "60")];
            if status.refresh_rate >= 119.0 {
                rates.push((40, "40"));
            }
            rates.push((30, "30"));
            rates.push((20, "20"));
            setting(
                ui,
                "Frame rate",
                Control::Choice,
                next_start(
                    started.is_some_and(|s| s.max_fps > 0 && (settings.max_fps == 0 || settings.max_fps > s.max_fps)),
                ),
                |ui| choice(ui, &mut settings.max_fps, &rates),
            );
            setting(ui, "VSync", Control::Toggle, None, |ui| toggle(ui, &mut settings.vsync));
            distance_options(ui, settings, status);
            let mut overlays = vec![(PerfOverlay::Off, "Off")];
            if cfg!(target_vendor = "apple") {
                overlays.push((PerfOverlay::MetalHud, "Metal HUD"));
            }
            overlays.push((PerfOverlay::Glue5, "Glue 5"));
            setting(
                ui,
                "Performance overlay",
                Control::Choice,
                (settings.overlay == PerfOverlay::MetalHud && !status.metal_hud).then_some("Next launch"),
                |ui| choice(ui, &mut settings.overlay, &overlays),
            );
            if settings.overlay == PerfOverlay::Glue5 {
                setting(ui, "Overlay detail", Control::Choice, None, |ui| {
                    choice(ui, &mut settings.overlay_detail, &[
                        (OverlayDetail::Minimal, "Minimal"),
                        (OverlayDetail::Standard, "Standard"),
                        (OverlayDetail::Full, "Full"),
                    ])
                });
                setting(ui, "Overlay position", Control::Choice, None, |ui| {
                    choice(ui, &mut settings.overlay_corner, &[
                        (Corner::TopLeft, "Top left"),
                        (Corner::TopRight, "Top right"),
                        (Corner::BottomLeft, "Bottom left"),
                        (Corner::BottomRight, "Bottom right"),
                    ])
                });
            }
        }
        SettingsTab::World => {
            setting(
                ui,
                "World textures",
                Control::Choice,
                Some(if started.is_some_and(|s| s.texture_detail != settings.texture_detail) {
                    "Next start"
                } else {
                    "GPU memory / longest edge"
                }),
                |ui| {
                    choice(ui, &mut settings.texture_detail, &[
                        (TextureDetail::Original, "Original"),
                        (TextureDetail::Balanced, "1024"),
                        (TextureDetail::Compact, "512"),
                        (TextureDetail::Minimal, "256"),
                    ])
                },
            );
            ui.label(RichText::new("Lower limits save Metal memory at the cost of sharpness. Sampled BC mipmaps only; unmipmapped UI and render targets stay intact. Next game start.").small().color(theme::MUTED));
            let desired = (settings.world_preset != WorldPreset::Game).then_some(settings.world);
            let hint = if started.is_some_and(|s| s.world != desired) {
                "Next start"
            } else {
                "Applies when starting the game"
            };
            let r = setting(ui, "LOD preset", Control::Choice, Some(hint), |ui| {
                choice(ui, &mut settings.world_preset, &[
                    (WorldPreset::Game, "Game settings"),
                    (WorldPreset::Eco, "Eco"),
                    (WorldPreset::Balanced, "Balanced"),
                    (WorldPreset::Detail, "Detail"),
                    (WorldPreset::Custom, "Custom"),
                ])
            });
            if r.changed() && !matches!(settings.world_preset, WorldPreset::Game | WorldPreset::Custom) {
                settings.world = WorldDetail::preset(settings.world_preset);
            }
            if settings.world_preset != WorldPreset::Game {
                for (label, value) in [
                    ("World draw distance", &mut settings.world.lod),
                    ("Ped LOD", &mut settings.world.ped_lod),
                    ("Vehicle LOD", &mut settings.world.vehicle_lod),
                    ("Population density", &mut settings.world.density),
                    ("Ped variety", &mut settings.world.ped_variety),
                    ("Vehicle variety", &mut settings.world.vehicle_variety),
                ] {
                    if setting(ui, label, Control::Slider, None, |ui| slider(ui, value, 0.0..=1.0, 0.05, "%", 0))
                        .changed()
                    {
                        settings.world_preset = WorldPreset::Custom;
                    }
                }
            }
            ui.label(RichText::new("Lower distances, density and variety reduce world/streaming work. Game settings leaves the game's XML choices alone; selecting it does not undo previously applied values.").small().color(theme::MUTED));
        }
        SettingsTab::Controls => {
            setting(ui, "Touch controls", Control::Choice, None, |ui| {
                choice(ui, &mut settings.touch, &[
                    (TouchMode::Auto, "Auto"),
                    (TouchMode::Always, "On"),
                    (TouchMode::Off, "Off"),
                ])
            })
            .on_hover_text("Auto: shown on touch screens; hidden while using a controller or keyboard");
            setting(ui, "Size", Control::Slider, None, |ui| {
                slider(ui, &mut settings.touch_size, 0.7..=1.5, 0.1, "×", 1)
            });
            setting(ui, "Opacity", Control::Slider, None, |ui| {
                slider(ui, &mut settings.touch_opacity, 0.2..=1.0, 0.01, "%", 0)
            });
            if crate::haptics::SUPPORTED {
                setting(ui, "Haptics", Control::Toggle, None, |ui| toggle(ui, &mut settings.haptics));
            }
        }
        SettingsTab::Game => {
            setting(
                ui,
                "Streaming budget",
                Control::Choice,
                Some(if started.is_some_and(|s| s.resource_budget != settings.resource_budget) {
                    "Next start"
                } else {
                    "Independent of graphics quality"
                }),
                |ui| {
                    choice(ui, &mut settings.resource_budget, &[
                        (ResourceBudget::Auto, "Auto"),
                        (ResourceBudget::Balanced, "1536 MiB"),
                        (ResourceBudget::Compact, "1280 MiB"),
                        (ResourceBudget::Compatibility, "2048 MiB"),
                    ])
                },
            );
            ui.label(RichText::new("1536 MiB is a reduced-budget test, not a total RAM cap. Auto uses 2048 MiB on mobile, or 768 with legacy Low memory. Too little can leave world assets missing. Restart to apply.").small().color(theme::MUTED));
            setting(
                ui,
                "Verbose engine logs",
                Control::Toggle,
                Some("Off keeps errors and important warnings"),
                |ui| toggle(ui, &mut settings.verbose_logs),
            );
            let r = setting(
                ui,
                "Low memory",
                Control::Toggle,
                next_start(started.is_some_and(|s| s.low_memory != settings.low_memory)),
                |ui| toggle(ui, &mut settings.low_memory),
            )
            .on_hover_text("Smaller engine pools; enabling this also lowers graphics settings once");
            if r.changed() && settings.low_memory {
                settings.low_preset_written = false;
            }
            let dir = if started.is_some() { settings.game_dir.as_deref() } else { status.game_dir };
            let mut shown = dir.map_or_else(|| "Not found".to_owned(), |d| d.display().to_string());
            if started.is_some_and(|s| Some(s.game_dir.as_path()) != settings.game_dir.as_deref()) {
                shown.push_str(" · next start");
            }
            setting(ui, "Game folder", Control::Action, Some(&shown), |ui| {
                if action_button(ui, if MOBILE { "Find files" } else { "Choose…" }).clicked() {
                    actions.push(if MOBILE { Action::FindGame } else { Action::ChooseGame });
                }
            });
            if started.is_none() {
                setting(
                    ui,
                    "Graphics",
                    Control::Action,
                    if status.graphics_reset { Some("Reset to defaults") } else { None },
                    |ui| {
                        ui.add_enabled_ui(!status.graphics_reset && status.game_dir.is_some(), |ui| {
                            if action_button(ui, "Reset graphics").clicked() {
                                actions.push(Action::ResetGraphics);
                            }
                        });
                    },
                );
            }
            if started.is_some() && !MOBILE {
                setting(ui, "Session", Control::Action, None, |ui| {
                    if action_button(ui, "Quit game").clicked() {
                        actions.push(Action::Quit);
                    }
                });
            }
            let version = concat!("Glue 5 ", env!("CARGO_PKG_VERSION"), " and the projects it builds on");
            setting(ui, "Credits", Control::Action, Some(version), |ui| {
                if action_button(ui, "View").clicked() {
                    *go = Some(Some(Page::Credits));
                }
            });
        }
    }
}

fn distance_options(ui: &mut egui::Ui, settings: &mut Settings, status: &Status) {
    ui.label(
        RichText::new("Distance · applies immediately. Near the camera, surfaces stay fully textured.")
            .small()
            .color(theme::MUTED),
    );
    setting(ui, "Fog", Control::Toggle, Some("Never tints the road in front of you"), |ui| {
        toggle(ui, &mut settings.fog)
    });
    let fog_note = if settings.fog_distance <= 0.0 {
        "Same as view distance".to_string()
    } else {
        format!("{:.0} m", settings.fog_distance)
    };
    setting(ui, "Fog distance", Control::Slider, Some(&fog_note), |ui| {
        slider(ui, &mut settings.fog_distance, 0.0..=1200.0, 10.0, "m", 0)
    });
    metres(ui, "View distance", &mut settings.view_distance, 0.0..=1200.0, "Off");
    metres(ui, "Detail distance", &mut settings.detail_distance, 0.0..=400.0, "Off");
    metres(ui, "Tree distance", &mut settings.tree_distance, 0.0..=800.0, "Off");
    metres(ui, "Shadow distance", &mut settings.shadow_distance, 0.0..=600.0, "Off");
    setting(ui, "Render boost", Control::Slider, Some("While cool. 1 keeps your resolution"), |ui| {
        slider(ui, &mut settings.render_boost, 1.0..=1.5, 0.05, "×", 2)
    });
    ui.label(
        RichText::new(
            "0 turns a distance off. Detail is clutter, decals and bushes. Tree distance is cards, not the trees next to you. Boost above 1 uses more memory and can stop near textures loading.",
        )
        .small()
        .color(theme::MUTED),
    );
    setting(ui, "Defaults", Control::Action, Some("Fog, distances, scale, boost, thermal, efficiency"), |ui| {
        if action_button(ui, "Reset").clicked() {
            settings.reset_distance_options(status.window.1);
        }
    });
}

fn metres(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, off: &str) {
    let note = if *value <= 0.0 { off.to_string() } else { format!("{:.0} m", *value) };
    setting(ui, label, Control::Slider, Some(&note), |ui| slider(ui, value, range, 10.0, "m", 0));
}

fn next_start(differs: bool) -> Option<&'static str> {
    differs.then_some("Next start")
}

#[cfg(test)]
mod tests {
    use super::{SettingsTab, Status, settings_page, theme};
    use crate::settings::{PerfOverlay, Settings, Upscaling};
    use crate::ui::{Mode, home::start_page};
    use egui::{Id, Pos2, Rect, SafeAreaInsets, vec2};
    use std::path::Path;

    fn status<'a>() -> Status<'a> {
        Status {
            game_dir: Some(Path::new("game")),
            problem: None,
            started: None,
            last_failure: None,
            graphics_reset: false,
            window: (1440, 900),
            back_buffer: None,
            upscalers: &[Upscaling::Bicubic, Upscaling::MetalFx, Upscaling::MetalFxTemporal, Upscaling::Fsr],
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

    #[test]
    fn start_page_icons_are_in_the_font() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        ctx.begin_pass(Default::default());
        ctx.end_pass().textures_delta.clear();
        assert!(ctx.fonts_mut(|f| f.has_glyphs(&egui::FontId::proportional(14.0), "⚙🗁🗙★›")));
    }

    #[test]
    fn settings_fit_the_safe_area_at_all_sizes() {
        for size in
            [vec2(1440.0, 900.0), vec2(768.0, 1024.0), vec2(390.0, 844.0), vec2(320.0, 480.0), vec2(844.0, 390.0)]
        {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            let mut settings = Settings::default();
            for tab in [SettingsTab::Video, SettingsTab::World, SettingsTab::Controls, SettingsTab::Game] {
                if tab == SettingsTab::World {
                    settings.world_preset = crate::settings::WorldPreset::Custom;
                }
                let mut tab = tab;
                for _ in 0..4 {
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(screen),
                            safe_area_insets: Some(SafeAreaInsets(egui::epaint::MarginF32 {
                                top: 24.0,
                                bottom: 16.0,
                                left: 8.0,
                                right: 8.0,
                            })),
                            ..Default::default()
                        },
                        |root| {
                            let ctx = root.ctx();
                            settings_page(
                                ctx,
                                &mut settings,
                                &status(),
                                None,
                                &mut tab,
                                &mut None,
                                &mut Vec::new(),
                                &mut None,
                            );
                        },
                    );
                    output.textures_delta.clear();
                }
                let window = ctx.memory(|m| m.area_rect(Id::new("settings-window"))).unwrap();
                let safe = ctx.content_rect();
                assert!(safe.contains_rect(window), "{size:?}: {window:?} outside {safe:?}");
                assert!(window.center().distance(safe.center()) < 2.0, "window not centered: {window:?}, {safe:?}");
            }
        }
    }
    #[test]
    fn phone_settings_use_the_height_and_keep_video_options_reachable() {
        for (overlay, last) in [(PerfOverlay::Off, "Performance overlay"), (PerfOverlay::Glue5, "Overlay position")] {
            let mut settings = Settings::default();
            settings.overlay = overlay;
            phone_video_options(settings, last);
        }
    }

    /// The Video tab on a phone, `last` its last option.
    fn phone_video_options(mut settings: Settings, last: &str) {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut tab = SettingsTab::Video;
        for pass in 0..8 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(874.0, 402.0))),
                    time: Some(pass as f64),
                    events: if pass == 4 {
                        vec![egui::Event::PointerMoved(Pos2::new(430.0, 300.0)), egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: vec2(0.0, -10000.0),
                            phase: egui::TouchPhase::Move,
                            modifiers: egui::Modifiers::default(),
                        }]
                    } else {
                        Vec::new()
                    },
                    safe_area_insets: Some(SafeAreaInsets(egui::epaint::MarginF32 {
                        left: 62.0,
                        right: 62.0,
                        top: 0.0,
                        bottom: 21.0,
                    })),
                    ..Default::default()
                },
                |root| {
                    settings_page(
                        root.ctx(),
                        &mut settings,
                        &status(),
                        None,
                        &mut tab,
                        &mut None,
                        &mut Vec::new(),
                        &mut None,
                    );
                },
            );
            output.textures_delta.clear();
            if pass == 3 || pass == 7 {
                let rows: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|s| match &s.shape {
                        egui::Shape::Rect(r) if r.fill == egui::Color32::from_gray(28) => Some((r.rect, s.clip_rect)),
                        _ => None,
                    })
                    .collect();
                // The video page is taller than the phone safe area. The first
                // row shows before scrolling, and the last option after it.
                assert!(!rows.is_empty());
                let visible = if pass == 3 { &rows[..1] } else { &rows[rows.len() - 1..] };
                for (rect, clip) in visible {
                    assert!(clip.contains_rect(*rect), "clipped option: {rect:?} in {clip:?}");
                }
                if pass == 7 {
                    let label = |s: &egui::epaint::ClippedShape| match &s.shape {
                        egui::Shape::Text(t) if t.galley.text() == last => Some(t.visual_bounding_rect()),
                        _ => None,
                    };
                    let rect = output.shapes.iter().find_map(label).expect("the last option is drawn");
                    assert!(visible[0].0.contains_rect(rect), "{last} is not the last row shown");
                }
            }
        }
        let window = ctx.memory(|m| m.area_rect(Id::new("settings-window"))).unwrap();
        let safe = ctx.content_rect();
        assert!(safe.contains_rect(window));
        assert!(safe.height() - window.height() <= 12.0, "wasted height: {window:?} in {safe:?}");
    }

    #[test]
    fn online_lobby_controls_fit_the_safe_area() {
        for private in [false, true] {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let mut settings = Settings::default();
            settings.online_private = private;
            settings.online_lobby = crate::settings::new_lobby_code();
            for _ in 0..4 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(390.0, 844.0))),
                        ..Default::default()
                    },
                    |root| {
                        super::sheet(root.ctx(), "online-window", "GTA ONLINE", |ui| {
                            super::lobby_controls(ui, &mut settings);
                        });
                    },
                );
                assert!(!output.shapes.is_empty());
                output.textures_delta.clear();
            }
            let rect = ctx.memory(|m| m.area_rect(Id::new("online-window"))).unwrap();
            assert!(ctx.content_rect().contains_rect(rect));
            assert_eq!(settings.lobby().is_empty(), !private);
        }
    }

    #[test]
    fn credits_fit_the_safe_area() {
        for size in [vec2(1440.0, 900.0), vec2(390.0, 844.0), vec2(844.0, 390.0), vec2(320.0, 480.0)] {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            for _ in 0..4 {
                let raw =
                    egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
                ctx.run_ui(raw, |root| {
                    assert!(!crate::ui::credits_page(root.ctx()));
                })
                .textures_delta
                .clear();
            }
            let window = ctx.memory(|m| m.area_rect(Id::new("credits-window"))).unwrap();
            assert!(ctx.content_rect().contains_rect(window), "{size:?}: {window:?}");
        }
    }

    #[test]
    fn home_reflows_when_the_same_window_is_resized() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        for size in
            [vec2(1440.0, 900.0), vec2(390.0, 844.0), vec2(320.0, 480.0), vec2(900.0, 500.0), vec2(1440.0, 900.0)]
        {
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            for _ in 0..4 {
                let mut output =
                    ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |root| {
                        start_page(root.ctx(), &Settings::default(), &status(), None, &mut Vec::new(), &mut None);
                    });
                output.textures_delta.clear();
            }
            let home = ctx.memory(|m| m.area_rect(Id::new("start"))).unwrap();
            assert!(screen.contains_rect(home), "{size:?}: {home:?}");
        }
    }
}
