//! Distance detail for placed world draws.
//!
//! The command stream has no mesh bounds and no impostor assets, so a far
//! building cannot be swapped for a billboard here. What the renderer can
//! see is the object's world position (`rage_matrices` slot 1, the same
//! constants the camera comes from), the shader's effect name, and how many
//! triangles the draw has. Past a near band that position selects a coarser
//! mip floor. Trees and grass ramp that floor smoothly. Small clutter is
//! not drawn past a shorter distance, and nothing placed is drawn past the
//! view distance. Depth-only shadow draws stop past their own cutoff.
//!
//! Anything near the camera, attached to it, or without a world position
//! (the HUD, full-screen passes, weapons) keeps the samplers and shadows
//! the engine asked for.

use std::sync::OnceLock;

/// Quarter-mip steps in a sampler's coarseness cache (4 mips).
pub const LOD_QUANTA: u32 = 16;

/// Tunables read once. `GLUE5_DISTANT_DETAIL=0` disables the mip floor, the
/// shadow cutoff, clutter and view culling, and distance fog. `2` starts
/// them closer. Distances are metres; `0` disables that one cull.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    pub enabled: bool,
    pub mobile: bool,
    pub aggressive: bool,
    /// `Some` when the environment set a shadow distance, including 0.
    pub shadow_override: Option<f32>,
    pub detail_override: Option<f32>,
    pub tree_override: Option<f32>,
    pub view_override: Option<f32>,
    pub fog: bool,
    /// Multiplier on the user's render scale while the device is cool.
    pub render_boost: f32,
}

pub fn policy() -> Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    *POLICY.get_or_init(|| {
        let var = |k| std::env::var(k).ok();
        policy_from(
            var("GLUE5_DISTANT_DETAIL").as_deref(),
            var("GLUE5_SHADOW_DISTANCE").as_deref(),
            var("GLUE5_DETAIL_DISTANCE").as_deref(),
            var("GLUE5_TREE_DISTANCE").as_deref(),
            var("GLUE5_VIEW_DISTANCE").as_deref(),
            var("GLUE5_FOG").as_deref(),
            var("GLUE5_RENDER_BOOST").as_deref(),
            cfg!(any(target_os = "ios", target_os = "android")),
        )
    })
}

pub fn policy_from(
    detail: Option<&str>,
    shadow: Option<&str>,
    detail_distance: Option<&str>,
    tree_distance: Option<&str>,
    view_distance: Option<&str>,
    fog: Option<&str>,
    render_boost: Option<&str>,
    mobile: bool,
) -> Policy {
    let enabled = detail != Some("0");
    Policy {
        enabled,
        mobile,
        aggressive: detail == Some("2"),
        shadow_override: shadow.and_then(|v| v.parse().ok()),
        detail_override: detail_distance.and_then(|v| v.parse().ok()),
        tree_override: tree_distance.and_then(|v| v.parse().ok()),
        view_override: view_distance.and_then(|v| v.parse().ok()),
        fog: enabled && fog != Some("0"),
        render_boost: render_boost_from(render_boost, mobile, enabled),
    }
}

fn render_boost_from(value: Option<&str>, mobile: bool, detail_on: bool) -> f32 {
    match value {
        Some("0") | Some("1") | Some("1.0") => 1.0,
        Some(v) => v.parse::<f32>().ok().filter(|b| b.is_finite() && *b >= 1.0).unwrap_or(1.0),
        None if detail_on => {
            if mobile {
                1.25
            } else {
                1.15
            }
        }
        None => 1.0,
    }
}

/// How far above the user's render scale to go. Nominal spends the distant
/// work that was skipped. Fair returns to the user's scale. Serious and
/// Critical give resolution back.
pub fn resolution_factor(thermal: u8, boost: f32) -> f32 {
    let boost = if boost.is_finite() && boost >= 1.0 { boost } else { 1.0 };
    match thermal {
        0 => boost,
        1 => 1.0,
        2 => 0.75,
        _ => 0.6,
    }
}

/// Where each extra mip starts, in metres from the camera.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bands {
    pub near: f32,
    pub mid: f32,
    pub far: f32,
}

/// `thermal` is Apple's state: 0 nominal, 1 fair, 2 serious, 3 critical.
pub fn bands(mobile: bool, aggressive: bool, thermal: u8) -> Bands {
    let scale = if aggressive { 0.75f32 } else { 1.0 };
    let (near, mid, far) = if mobile { (60.0f32, 160.0, 350.0) } else { (100.0, 250.0, 500.0) };
    // Fair and hotter: coarsen a little sooner, but not a facade you can
    // walk up to. Serious/critical add a further mip in [`mip_floor`].
    let near_floor = if mobile { 45.0f32 } else { 70.0 };
    let near = if thermal >= 1 { (near * 0.85).max(near_floor) } else { near };
    Bands { near: near * scale, mid: mid * scale, far: far * scale }
}

/// Extra mips past the engine's own sampler range. 0 leaves the draw alone.
pub fn mip_floor(distance_m: f32, bands: Bands, thermal: u8) -> u32 {
    let base = if distance_m < bands.near {
        0
    } else if distance_m < bands.mid {
        1
    } else if distance_m < bands.far {
        2
    } else {
        3
    };
    let extra = u32::from(thermal >= 2 && distance_m >= bands.near);
    (base + extra).min(4)
}

/// Metres past which a placed object's depth-only shadow draw is skipped.
/// A non-positive override disables culling.
pub fn shadow_cutoff(mobile: bool, thermal: u8, override_m: Option<f32>) -> f32 {
    if let Some(metres) = override_m {
        return if metres <= 0.0 { f32::INFINITY } else { metres };
    }
    let mut cut = if mobile { 120.0 } else { 250.0 };
    if thermal >= 3 {
        cut *= 0.55;
    } else if thermal >= 2 {
        cut *= 0.7;
    }
    cut
}

/// Where clutter, grass, tree cards and the rest of the world stop being drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distances {
    pub clutter: f32,
    pub grass: f32,
    pub tree_lod: f32,
    pub view: f32,
}

/// An explicit non-positive distance disables that cull. Defaults shrink
/// when the mode is aggressive or the device is hot.
pub fn distances(policy: Policy, thermal: u8) -> Distances {
    let scale = if policy.aggressive { 0.75 } else { 1.0 };
    let heat = if thermal >= 3 { 0.65 } else if thermal >= 2 { 0.8 } else { 1.0 };
    let s = scale * heat;
    let (clutter, grass, tree, view) = if policy.mobile { (50.0, 40.0, 130.0, 260.0) } else { (80.0, 70.0, 200.0, 420.0) };
    Distances {
        clutter: metres(policy.detail_override, clutter * s),
        grass: metres(policy.detail_override, grass * s),
        tree_lod: metres(policy.tree_override, tree * s),
        view: metres(policy.view_override, view * s),
    }
}

fn metres(override_m: Option<f32>, default_m: f32) -> f32 {
    match override_m {
        Some(m) if m <= 0.0 => f32::INFINITY,
        Some(m) => m,
        None => default_m,
    }
}

/// A draw the distance policy can see. Indirect draws have no triangle count.
pub struct DrawInfo<'a> {
    pub vs: &'a str,
    pub ps: &'a str,
    pub triangles: u32,
    pub instanced: bool,
}

/// Why a placed draw is not issued, or `None` to draw it.
pub fn skip_reason(distance: f32, draw: &DrawInfo<'_>, dist: Distances) -> Option<&'static str> {
    let (vs, ps) = (effect(draw.vs), effect(draw.ps));
    if vegetation(vs, ps) {
        if grass(vs, ps) {
            return (distance > dist.grass).then_some("distant grass");
        }
        // Bushes and ground cover are small. A full tree stays, but its card
        // (an LOD shader, or a handful of triangles) does not.
        if undergrowth(vs, ps) {
            return (distance > dist.clutter).then_some("distant clutter");
        }
        let card = tree_lod(draw.vs, draw.ps) || (!draw.instanced && draw.triangles > 0 && draw.triangles <= 48);
        if card && distance > dist.tree_lod {
            return Some("distant tree");
        }
    } else if clutter(vs, ps) {
        if distance > dist.clutter {
            return Some("distant clutter");
        }
    } else if let Some(cut) = small_prop_cutoff(vs, ps, draw.triangles, draw.instanced, dist.clutter) {
        if distance > cut {
            return Some("distant detail");
        }
    }
    if distance > dist.view && !sky(vs) && !sky(ps) {
        return Some("beyond view");
    }
    None
}

/// Extra mips for this draw. Vegetation rises smoothly; everything else
/// keeps the stepped bands.
pub fn lod_add(vs: &str, ps: &str, distance_m: f32, bands: Bands, thermal: u8) -> f32 {
    let (vs_e, ps_e) = (effect(vs), effect(ps));
    if vegetation(vs_e, ps_e) {
        return vegetation_lod(distance_m, bands, thermal);
    }
    let base = mip_floor(distance_m, bands, thermal) as f32;
    if clutter(vs_e, ps_e) && distance_m >= bands.near * 0.5 { (base + 1.0).min(4.0) } else { base }
}

/// 0 at a quarter mip, 16 at four mips.
pub fn lod_quanta(lod: f32) -> u32 {
    if !lod.is_finite() || lod <= 0.0 { 0 } else { (lod.min(4.0) * 4.0).round() as u32 }
}

/// Trees and grass: full detail up close, then a smoothstep out to the far band.
pub fn vegetation_lod(distance_m: f32, bands: Bands, thermal: u8) -> f32 {
    let sharp = bands.near * 0.35;
    if distance_m <= sharp {
        return 0.0;
    }
    let span = (bands.far - sharp).max(1.0);
    let t = ((distance_m - sharp) / span).clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    let mut lod = s * 3.4;
    if thermal >= 1 && distance_m >= bands.near {
        lod += 0.35;
    }
    if thermal >= 2 {
        lod += 0.45;
    }
    lod.min(4.0)
}

fn effect(label: &str) -> &str {
    label.split_once("::").map(|(e, _)| e).unwrap_or(label)
}

fn program(label: &str) -> &str {
    label.split_once("::").map(|(_, p)| p).unwrap_or("")
}

fn contains(hay: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    hay.len() >= n.len()
        && hay.as_bytes().windows(n.len()).any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

fn any(effect: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| contains(effect, n))
}

const VEGETATION: &[&str] = &["tree", "grass", "foliage", "bush", "plant", "shrub", "palm", "leaf", "fern", "hedge", "ivy"];
const CLUTTER: &[&str] = &["decal", "debris", "litter", "rubbish", "garbage", "trash", "billboard", "ptfx", "particle", "cable"];
const SKY: &[&str] = &["sky", "cloud", "star", "sun", "moon", "atmos"];
/// Effects whose small draws are still a building, a person or a vehicle.
const STRUCTURAL: &[&str] = &[
    "ped", "veh", "water", "terrain", "weapon", "glass", "sky", "cloud", "radar", "hair", "mirror", "road", "light",
    "shadow", "defer", "post", "slod", "lod", "map", "city", "building", "im",
];

fn vegetation(vs: &str, ps: &str) -> bool {
    any(vs, VEGETATION) || any(ps, VEGETATION)
}

fn grass(vs: &str, ps: &str) -> bool {
    contains(vs, "grass") || contains(ps, "grass")
}

fn undergrowth(vs: &str, ps: &str) -> bool {
    const NAMES: &[&str] = &["bush", "shrub", "fern", "hedge", "ivy", "weed", "plant"];
    any(vs, NAMES) || any(ps, NAMES)
}

fn clutter(vs: &str, ps: &str) -> bool {
    any(vs, CLUTTER) || any(ps, CLUTTER)
}

fn sky(effect: &str) -> bool {
    any(effect, SKY)
}

fn tree_lod(vs: &str, ps: &str) -> bool {
    let tree = contains(effect(vs), "tree") || contains(effect(ps), "tree");
    tree && (contains(effect(vs), "lod") || contains(effect(ps), "lod") || contains(program(vs), "lod") || contains(program(ps), "lod"))
}

/// A handful of triangles of an ordinary material is litter or a small prop.
/// Building LODs and anything instanced (a grass batch, a row of lamps) are
/// not: the triangle count is one piece, not the object's size.
fn small_prop_cutoff(vs: &str, ps: &str, triangles: u32, instanced: bool, clutter_m: f32) -> Option<f32> {
    if instanced || triangles == 0 || triangles > 180 || !clutter_m.is_finite() {
        return None;
    }
    if is_hex(vs) || is_hex(ps) || any(vs, STRUCTURAL) || any(ps, STRUCTURAL) {
        return None;
    }
    Some(if triangles <= 48 { clutter_m } else { clutter_m * 2.0 })
}

fn is_hex(effect: &str) -> bool {
    !effect.is_empty() && effect.bytes().all(|b| b.is_ascii_hexdigit())
}

/// World translation of a placed object, or `None` when the matrix is not
/// one (the origin, a camera rig, a non-finite value).
pub fn placed_distance(translation: [f32; 3], camera: [f64; 3]) -> Option<f32> {
    if translation.iter().any(|c| !c.is_finite()) {
        return None;
    }
    if translation.iter().all(|c| c.abs() < 0.05) {
        return None;
    }
    let d = [
        f64::from(translation[0]) - camera[0],
        f64::from(translation[1]) - camera[1],
        f64::from(translation[2]) - camera[2],
    ];
    let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if !dist.is_finite() || dist < 2.0 {
        return None;
    }
    Some(dist as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_objects_and_the_camera_rig_stay_sharp() {
        let mobile = bands(true, false, 0);
        let desktop = bands(false, false, 0);
        assert_eq!(mip_floor(21.8, mobile, 0), 0, "a car-length away");
        assert_eq!(mip_floor(59.0, mobile, 0), 0);
        assert_eq!(mip_floor(99.0, desktop, 0), 0);
        assert_eq!(placed_distance([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]), None);
        assert_eq!(placed_distance([1.0, 0.0, 0.0], [0.0, 0.0, 0.0]), None);
        assert_eq!(placed_distance([f32::NAN, 0.0, 1.0], [0.0; 3]), None);
    }

    #[test]
    fn logged_sandbox_draw_is_inside_the_sharp_band() {
        // gWorld translation and gViewInverse translation from scene.rs's fixture.
        let dist = placed_distance([4.93608, -1453.50830, 29.49050], [-16.77208, -1453.15490, 31.17100]).unwrap();
        assert!((dist - 21.78).abs() < 0.05, "{dist}");
        assert_eq!(mip_floor(dist, bands(true, false, 0), 0), 0);
        assert_eq!(mip_floor(dist, bands(false, false, 0), 0), 0);
    }

    #[test]
    fn distant_buildings_drop_mips_and_hot_devices_drop_one_more() {
        let mobile = bands(true, false, 0);
        assert_eq!(mip_floor(80.0, mobile, 0), 1);
        assert_eq!(mip_floor(200.0, mobile, 0), 2);
        assert_eq!(mip_floor(400.0, mobile, 0), 3);
        assert_eq!(mip_floor(400.0, bands(false, false, 0), 0), 2);
        let hot = bands(true, false, 2);
        assert_eq!(mip_floor(30.0, hot, 2), 0, "a nearby facade stays sharp while hot");
        assert_eq!(mip_floor(80.0, hot, 2), 2);
        assert_eq!(mip_floor(400.0, hot, 3), 4);
    }

    #[test]
    fn aggressive_mode_and_shadow_cutoff_are_explicit() {
        let aggressive = bands(true, true, 0);
        assert!(aggressive.near < bands(true, false, 0).near);
        assert_eq!(mip_floor(50.0, aggressive, 0), 1);
        assert_eq!(shadow_cutoff(true, 0, None), 120.0);
        assert_eq!(shadow_cutoff(false, 0, None), 250.0);
        assert!((shadow_cutoff(true, 2, None) - 84.0).abs() < 0.01);
        assert!((shadow_cutoff(true, 3, None) - 66.0).abs() < 0.01);
        assert_eq!(shadow_cutoff(true, 3, Some(40.0)), 40.0);
        assert!(shadow_cutoff(true, 3, Some(0.0)).is_infinite());
    }

    #[test]
    fn policy_env_values() {
        let off = policy_from(Some("0"), None, None, None, None, None, None, true);
        assert!(!off.enabled);
        assert!(!off.fog);
        assert_eq!(off.render_boost, 1.0);
        let on = policy_from(None, None, None, None, None, None, None, true);
        assert!(on.enabled);
        assert!(on.fog);
        assert!((on.render_boost - 1.25).abs() < 1e-4);
        assert!((policy_from(None, None, None, None, None, None, None, false).render_boost - 1.15).abs() < 1e-4);
        let aggressive = policy_from(Some("2"), None, None, None, None, Some("0"), Some("1"), false);
        assert!(aggressive.aggressive);
        assert!(!aggressive.fog);
        assert_eq!(aggressive.render_boost, 1.0);
        assert_eq!(policy_from(None, Some("0"), None, None, None, None, None, true).shadow_override, Some(0.0));
        assert_eq!(policy_from(None, Some("nope"), None, None, None, None, None, true).shadow_override, None);
        assert_eq!(policy_from(None, None, Some("30"), None, None, None, Some("1.4"), true).detail_override, Some(30.0));
    }

    fn base() -> (Policy, Distances) {
        let policy = policy_from(None, None, None, None, None, None, None, true);
        (policy, distances(policy, 0))
    }

    #[test]
    fn clutter_grass_and_tree_cards_stop_where_they_are_configured() {
        let (policy, dist) = base();
        assert!((dist.clutter - 50.0).abs() < 1e-3);
        assert!((dist.grass - 40.0).abs() < 1e-3);
        assert!((dist.view - 260.0).abs() < 1e-3);
        let grass = DrawInfo { vs: "grass_batch::VS_Main", ps: "grass_batch::PS_Main", triangles: 2000, instanced: true };
        assert_eq!(skip_reason(39.0, &grass, dist), None);
        assert_eq!(skip_reason(41.0, &grass, dist), Some("distant grass"));
        let card = DrawInfo { vs: "trees_lod::VS_Main", ps: "trees_lod::PS_Main", triangles: 2, instanced: false };
        assert_eq!(skip_reason(120.0, &card, dist), None);
        assert_eq!(skip_reason(140.0, &card, dist), Some("distant tree"));
        let tree = DrawInfo { vs: "trees_normal::VS_Main", ps: "trees_normal::PS_Main", triangles: 800, instanced: false };
        assert_eq!(skip_reason(200.0, &tree, dist), None, "a full tree stays until the view distance");
        assert_eq!(skip_reason(270.0, &tree, dist), Some("beyond view"));
        let decal = DrawInfo { vs: "decal::VS_Main", ps: "decal::PS_Main", triangles: 2, instanced: false };
        assert_eq!(skip_reason(60.0, &decal, dist), Some("distant clutter"));
        let bush = DrawInfo { vs: "bush::VS_Main", ps: "bush::PS_Main", triangles: 80, instanced: false };
        assert_eq!(skip_reason(40.0, &bush, dist), None);
        assert_eq!(skip_reason(60.0, &bush, dist), Some("distant clutter"));
        let hot = distances(policy, 3);
        assert!(hot.view < dist.view);
        let custom = distances(policy_from(None, None, Some("0"), Some("90"), Some("0"), None, None, true), 3);
        assert!(custom.clutter.is_infinite());
        assert!((custom.tree_lod - 90.0).abs() < 1e-3, "an explicit distance is not scaled by heat");
        assert!(custom.view.is_infinite());
    }

    #[test]
    fn small_props_go_and_structural_draws_stay() {
        let (_, dist) = base();
        let litter = DrawInfo { vs: "normal_spec::VS_Main", ps: "normal_spec::PS_Main", triangles: 24, instanced: false };
        assert_eq!(skip_reason(30.0, &litter, dist), None);
        assert_eq!(skip_reason(60.0, &litter, dist), Some("distant detail"));
        let bench = DrawInfo { vs: "normal::VS_Main", ps: "normal::PS_Main", triangles: 120, instanced: false };
        assert_eq!(skip_reason(60.0, &bench, dist), None, "a medium prop lasts past the litter distance");
        assert_eq!(skip_reason(110.0, &bench, dist), Some("distant detail"));
        let wall = DrawInfo { vs: "normal_spec::VS_Main", ps: "normal_spec::PS_Main", triangles: 400, instanced: false };
        assert_eq!(skip_reason(100.0, &wall, dist), None);
        assert_eq!(skip_reason(300.0, &wall, dist), Some("beyond view"));
        let door = DrawInfo { vs: "building_lod::VS_Main", ps: "building_lod::PS_Main", triangles: 20, instanced: false };
        assert_eq!(skip_reason(80.0, &door, dist), None, "a building LOD is not litter");
        let ped = DrawInfo { vs: "ped::VS_Main", ps: "ped::PS_Main", triangles: 20, instanced: false };
        assert_eq!(skip_reason(80.0, &ped, dist), None);
        let car = DrawInfo { vs: "vehicle_mesh::VS_Main", ps: "vehicle_mesh::PS_Main", triangles: 12, instanced: false };
        assert_eq!(skip_reason(90.0, &car, dist), None);
        let lamps = DrawInfo { vs: "normal_spec::VS_Main", ps: "normal_spec::PS_Main", triangles: 12, instanced: true };
        assert_eq!(skip_reason(90.0, &lamps, dist), None, "an instanced batch is one draw for many objects");
        let sky = DrawInfo { vs: "sky::VS_Main", ps: "sky::PS_Main", triangles: 12, instanced: false };
        assert_eq!(skip_reason(10_000.0, &sky, dist), None);
    }

    #[test]
    fn vegetation_detail_rises_smoothly_and_stays_sharp_up_close() {
        let mobile = bands(true, false, 0);
        assert_eq!(vegetation_lod(15.0, mobile, 0), 0.0);
        let near = vegetation_lod(40.0, mobile, 0);
        let mid = vegetation_lod(160.0, mobile, 0);
        let far = vegetation_lod(350.0, mobile, 0);
        assert!(near < mid && mid < far && far <= 4.0, "{near} {mid} {far}");
        let mut prev = 0.0;
        for metres in (20..360).step_by(10) {
            let lod = vegetation_lod(metres as f32, mobile, 0);
            assert!(lod + 1e-4 >= prev, "not smooth at {metres}: {prev} -> {lod}");
            prev = lod;
        }
        let close = lod_add("trees::VS", "trees::PS", 18.0, mobile, 0);
        assert_eq!(lod_quanta(close), 0);
        let blurred = lod_add("trees::VS", "trees::PS", 250.0, mobile, 0);
        let building = lod_add("normal_spec::VS", "normal_spec::PS", 250.0, mobile, 0);
        assert!(blurred > building, "trees blur ahead of a building at the same distance");
        assert_eq!(lod_quanta(1.0), 4);
        assert_eq!(lod_quanta(0.24), 1);
        assert_eq!(lod_quanta(4.0), 16);
        assert!(vegetation_lod(200.0, mobile, 2) > vegetation_lod(200.0, mobile, 0));
    }

    #[test]
    fn resolution_gives_the_savings_back_until_the_device_is_hot() {
        assert!((resolution_factor(0, 1.25) - 1.25).abs() < 1e-4);
        assert_eq!(resolution_factor(1, 1.25), 1.0);
        assert!((resolution_factor(2, 1.25) - 0.75).abs() < 1e-4);
        assert!((resolution_factor(3, 1.25) - 0.6).abs() < 1e-4);
        assert_eq!(resolution_factor(0, 1.0), 1.0);
        assert_eq!(resolution_factor(0, 0.5), 1.0);
    }
}
