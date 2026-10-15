//! Distance detail for placed world draws.
//!
//! The command stream has no mesh bounds and no impostor assets, so a far
//! building cannot be swapped for a billboard here. What the renderer can
//! see is the object's world position (`rage_matrices` slot 1, the same
//! constants the camera comes from). Past a near band that position selects
//! a coarser mip floor, which is the texture-side equivalent of a low-res
//! card. Depth-only shadow draws of those objects stop past a shorter
//! cutoff, so the city is not redrawn into every cascade.
//!
//! Anything near the camera, attached to it, or without a world position
//! (the HUD, full-screen passes, weapons) keeps the samplers and shadows
//! the engine asked for.

use std::sync::OnceLock;

/// Tunables read once. `GLUE5_DISTANT_DETAIL=0` disables both the mip floor
/// and the shadow cutoff. `2` starts them closer. `GLUE5_SHADOW_DISTANCE`
/// is metres (0 disables shadow culling only).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    pub enabled: bool,
    pub mobile: bool,
    pub aggressive: bool,
    /// `Some` when the environment set a shadow distance, including 0.
    pub shadow_override: Option<f32>,
}

pub fn policy() -> Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    *POLICY.get_or_init(|| {
        let detail = std::env::var("GLUE5_DISTANT_DETAIL").ok();
        let shadow = std::env::var("GLUE5_SHADOW_DISTANCE").ok();
        policy_from(detail.as_deref(), shadow.as_deref(), cfg!(any(target_os = "ios", target_os = "android")))
    })
}

pub fn policy_from(detail: Option<&str>, shadow: Option<&str>, mobile: bool) -> Policy {
    Policy {
        enabled: detail != Some("0"),
        mobile,
        aggressive: detail == Some("2"),
        shadow_override: shadow.and_then(|v| v.parse().ok()),
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
        assert!(!policy_from(Some("0"), None, true).enabled);
        assert!(policy_from(None, None, true).enabled);
        assert!(policy_from(Some("2"), None, false).aggressive);
        assert_eq!(policy_from(None, Some("0"), true).shadow_override, Some(0.0));
        assert_eq!(policy_from(None, Some("nope"), true).shadow_override, None);
    }
}
