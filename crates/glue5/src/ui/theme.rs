//! Portable visual assets and the shared home backdrop.
use egui::*;

pub(super) const INK: Color32 = Color32::from_rgb(14, 15, 17);
pub(super) const MUTED: Color32 = Color32::from_rgb(157, 160, 166);
pub(super) const SUNSET: Color32 = Color32::from_rgb(239, 198, 143);
pub(super) const PAPER: Color32 = Color32::from_rgb(247, 242, 233);

pub(super) fn heading(text: &str, size: f32) -> RichText {
    RichText::new(text).font(FontId::new(size, FontFamily::Name("condensed".into())))
}

pub(super) fn install(ctx: &Context) {
    ctx.set_theme(Theme::Dark);
    let mut fonts = FontDefinitions::default();
    for (name, bytes) in [
        ("ui", include_bytes!("../../assets/ui/Barlow-SemiBold.ttf").as_slice()),
        ("condensed", include_bytes!("../../assets/ui/BarlowCondensed-Bold.ttf").as_slice()),
    ] {
        fonts.font_data.insert(name.into(), FontData::from_static(bytes).into());
    }
    fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "ui".into());
    let mut headings = vec!["condensed".into()];
    headings.extend(fonts.families[&FontFamily::Proportional].clone());
    fonts.families.insert(FontFamily::Name("condensed".into()), headings);
    ctx.set_fonts(fonts);
    ctx.all_styles_mut(|s| {
        s.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(14.0));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(11.0));
        s.spacing.item_spacing = vec2(10.0, 10.0);
        s.spacing.button_padding = vec2(12.0, 8.0);
        s.spacing.interact_size.y = 44.0;
        s.visuals.override_text_color = Some(Color32::WHITE);
        s.visuals.panel_fill = INK;
        s.visuals.window_fill = Color32::from_rgba_unmultiplied(12, 13, 15, 242);
        s.visuals.window_corner_radius = 22.into();
        s.visuals.window_stroke = Stroke::new(1.0, Color32::from_gray(76));
        s.visuals.selection.bg_fill = Color32::from_gray(90);
        s.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        for w in [&mut s.visuals.widgets.inactive, &mut s.visuals.widgets.hovered, &mut s.visuals.widgets.active] {
            w.corner_radius = 14.into();
            w.bg_fill = Color32::from_gray(39);
            w.weak_bg_fill = Color32::from_gray(34);
            w.bg_stroke = Stroke::NONE;
            w.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        }
        s.visuals.widgets.hovered.bg_fill = Color32::from_gray(60);
        s.visuals.widgets.active.bg_stroke = Stroke::new(2.0, Color32::WHITE);
    });
}

/// Density follows available space, including landscape phones and split views.
pub(super) fn layout_scale(rect: Rect) -> f32 {
    (rect.height() / 560.0).min(rect.width() / 780.0).clamp(0.75, 1.0)
}

pub(super) fn density(ui: &Ui) -> f32 {
    ui.style().text_styles[&TextStyle::Body].size / 14.0
}

pub(super) fn apply_density(ui: &mut Ui, scale: f32) {
    let style = ui.style_mut();
    for font in style.text_styles.values_mut() {
        font.size *= scale;
    }
    style.spacing.item_spacing *= scale;
    style.spacing.button_padding *= scale;
    style.spacing.interact_size *= scale;
}

pub(super) fn background(ctx: &Context) -> Option<TextureHandle> {
    let decode = || -> Result<ColorImage, png::DecodingError> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../../assets/ui/home.png")));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info()?;
        let mut pixels = vec![0; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut pixels)?;
        let size = [info.width as usize, info.height as usize];
        Ok(match info.color_type {
            png::ColorType::Rgb => ColorImage::from_rgb(size, &pixels[..info.buffer_size()]),
            png::ColorType::Rgba => ColorImage::from_rgba_unmultiplied(size, &pixels[..info.buffer_size()]),
            _ => return Err(png::DecodingError::IoError(std::io::Error::other("unsupported home image format"))),
        })
    };
    match decode() {
        Ok(image) => Some(ctx.load_texture("home-background", image, TextureOptions::LINEAR)),
        Err(error) => {
            log::warn!("Could not load home background: {error}");
            None
        }
    }
}

pub(super) fn backdrop(ctx: &Context, texture: Option<&TextureHandle>) {
    let r = ctx.viewport_rect();
    let mut p = ctx.layer_painter(LayerId::background());
    p.set_clip_rect(r);
    p.rect_filled(r, 0, INK);
    if let Some(texture) = texture {
        let image_aspect = texture.size_vec2().x / texture.size_vec2().y;
        let aspect = r.width() / r.height().max(1.0);
        let uv = if aspect > image_aspect {
            let h = image_aspect / aspect;
            Rect::from_min_max(pos2(0.0, (1.0 - h) * 0.5), pos2(1.0, (1.0 + h) * 0.5))
        } else {
            let w = aspect / image_aspect;
            Rect::from_min_max(pos2((1.0 - w) * 0.65, 0.0), pos2((1.0 - w) * 0.65 + w, 1.0))
        };
        p.image(texture.id(), r, uv, Color32::from_gray(235));
    }
    for alphas in [[242, 45, 45, 242], [0, 0, 248, 248]] {
        let mut mesh = Mesh::default();
        for (pos, a) in [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()].into_iter().zip(alphas) {
            mesh.colored_vertex(pos, Color32::from_rgba_unmultiplied(14, 15, 17, a));
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        p.add(mesh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backdrop_is_not_clipped_to_the_safe_area() {
        let ctx = Context::default();
        let viewport = Rect::from_min_size(Pos2::ZERO, vec2(874.0, 402.0));
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: Some(viewport),
                safe_area_insets: Some(SafeAreaInsets(egui::epaint::MarginF32 {
                    left: 62.0,
                    right: 62.0,
                    top: 0.0,
                    bottom: 21.0,
                })),
                ..Default::default()
            },
            |ui| backdrop(ui.ctx(), None),
        );
        output.textures_delta.clear();
        let background = output.shapes.iter().find(|s| matches!(&s.shape, Shape::Rect(r) if r.fill == INK)).unwrap();
        assert_eq!(background.clip_rect, viewport);
        output.textures_delta.clear();
    }
}
