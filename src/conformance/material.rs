//! Material and image checks.

use super::report::{Location, Report, Severity};
use super::{
    EMISSIVE_EXTREME, IMAGE_MAX_SIDE, NORMAL_MAP_MAX_XY_OFFSET, NORMAL_MAP_MIN_MEAN_BLUE,
    OPAQUE_TRANSPARENT_FRACTION,
};
use crate::{AlphaMode, Image, Model};

struct ImageStats {
    mean: [f64; 3],
    /// Every texel has alpha 255.
    all_opaque: bool,
    /// Fraction of texels with alpha below 128.
    transparent: f64,
}

fn scan(img: &Image) -> Option<ImageStats> {
    let n = img.rgba.len() / 4;
    if n == 0 {
        return None;
    }
    let mut sum = [0u64; 3];
    let mut transparent = 0usize;
    let mut all_opaque = true;
    for px in img.rgba.as_chunks::<4>().0 {
        sum[0] += u64::from(px[0]);
        sum[1] += u64::from(px[1]);
        sum[2] += u64::from(px[2]);
        if px[3] != 255 {
            all_opaque = false;
            if px[3] < 128 {
                transparent += 1;
            }
        }
    }
    let mean = sum.map(|s| s as f64 / (n as f64 * 255.0));
    Some(ImageStats {
        mean,
        all_opaque,
        transparent: transparent as f64 / n as f64,
    })
}

/// Materials and images. Factors are glTF's, so 0..=1 apart from emissive, which is relative
/// radiance with 1.0 as reference white. A renderer clamps or blows out whatever lies outside.
/// The alpha mode must agree with the base colour texture: blending a texture that has no
/// transparency costs a depth-sorted pass for nothing, and an opaque material ignores the
/// alpha of a texture that has some, so the cut-out parts (leaves, fences, glass) show up as
/// solid colour. A normal map that is not blue-dominant, or whose red and green are not centred,
/// is probably another kind of image or has its channels swapped, and lights wrongly.
pub(crate) fn check(model: &Model, material_used: &[bool], rep: &mut Report) {
    for (i, m) in model.materials.iter().enumerate() {
        let loc = Location::material(i);
        if !material_used.get(i).copied().unwrap_or(true) {
            rep.push(
                Severity::Info,
                "material.unused",
                loc.clone(),
                format!("no mesh uses material {i} ({})", m.name),
            );
        }
        let factors: [(&str, &[f32]); 4] = [
            ("base_color_factor", &m.base_color_factor),
            ("metallic", &[m.metallic]),
            ("roughness", &[m.roughness]),
            ("emissive_factor", &m.emissive_factor),
        ];
        for (what, vals) in factors {
            if vals.iter().any(|v| !v.is_finite()) {
                rep.push(
                    Severity::Error,
                    if what == "emissive_factor" {
                        "material.emissive_invalid"
                    } else {
                        "material.factor_invalid"
                    },
                    loc.clone(),
                    format!("{what} {vals:?} is not finite ({})", m.name),
                );
            }
        }
        if m.emissive_factor.iter().all(|v| v.is_finite()) {
            if m.emissive_factor.iter().any(|&v| v < 0.0) {
                rep.push(
                    Severity::Error,
                    "material.emissive_invalid",
                    loc.clone(),
                    format!(
                        "emissive_factor {:?} is negative ({})",
                        m.emissive_factor, m.name
                    ),
                );
            } else if m.emissive_factor.iter().any(|&v| v > EMISSIVE_EXTREME) {
                rep.push(
                    Severity::Warning,
                    "material.emissive_extreme",
                    loc.clone(),
                    format!(
                        "emissive_factor {:?} is over {EMISSIVE_EXTREME}, where 1.0 is reference white ({})",
                        m.emissive_factor, m.name
                    ),
                );
            }
        }
        if m.base_color_factor.iter().all(|v| v.is_finite())
            && m.base_color_factor.iter().any(|v| !(0.0..=1.0).contains(v))
        {
            rep.push(
                Severity::Warning,
                "material.base_color_factor_range",
                loc.clone(),
                format!(
                    "base_color_factor {:?} is outside 0..=1 ({})",
                    m.base_color_factor, m.name
                ),
            );
        }
        if [m.metallic, m.roughness]
            .iter()
            .any(|v| v.is_finite() && !(0.0..=1.0).contains(v))
        {
            rep.push(
                Severity::Warning,
                "material.metallic_roughness_range",
                loc.clone(),
                format!(
                    "metallic {} / roughness {} is outside 0..=1 ({})",
                    m.metallic, m.roughness, m.name
                ),
            );
        }
        if let AlphaMode::Mask(cutoff) = m.alpha {
            if !(0.0..=1.0).contains(&cutoff) {
                rep.push(
                    Severity::Warning,
                    "material.mask_cutoff_range",
                    loc.clone(),
                    format!("alpha cutoff {cutoff} is outside 0..=1 ({})", m.name),
                );
            }
        }

        let images: [(&str, &Option<Image>); 4] = [
            ("base_color", &m.base_color),
            ("metallic_roughness", &m.metallic_roughness),
            ("normal", &m.normal),
            ("emissive", &m.emissive),
        ];
        for (what, img) in images {
            let Some(img) = img else { continue };
            if img.width == 0 || img.height == 0 {
                rep.push(
                    Severity::Error,
                    "image.empty",
                    loc.clone(),
                    format!("{what} image is {}x{} ({})", img.width, img.height, m.name),
                );
            } else if img.width > IMAGE_MAX_SIDE || img.height > IMAGE_MAX_SIDE {
                rep.push(
                    Severity::Warning,
                    "image.too_large",
                    loc.clone(),
                    format!(
                        "{what} image is {}x{}, over {IMAGE_MAX_SIDE} on a side ({})",
                        img.width, img.height, m.name
                    ),
                );
            }
        }
        if let Some(img) = &m.normal {
            if let Some(s) = scan(img) {
                let xy = (s.mean[0] - 0.5).abs().max((s.mean[1] - 0.5).abs());
                if s.mean[2] < NORMAL_MAP_MIN_MEAN_BLUE || xy > NORMAL_MAP_MAX_XY_OFFSET {
                    rep.push(
                        Severity::Warning,
                        "image.normal_map_suspect",
                        loc.clone(),
                        format!(
                            "normal image has mean RGB ({:.2}, {:.2}, {:.2}); a tangent-space map is near (0.5, 0.5, 1) ({})",
                            s.mean[0], s.mean[1], s.mean[2], m.name
                        ),
                    );
                }
            }
        }
        if let Some(s) = m.base_color.as_ref().and_then(scan) {
            match m.alpha {
                AlphaMode::Blend if s.all_opaque && m.base_color_factor[3] >= 0.999 => {
                    rep.push(
                        Severity::Warning,
                        "material.blend_opaque_texture",
                        loc.clone(),
                        format!(
                            "alpha mode is Blend but the base colour texture is fully opaque ({})",
                            m.name
                        ),
                    );
                }
                AlphaMode::Opaque if s.transparent > OPAQUE_TRANSPARENT_FRACTION => {
                    rep.push(
                        Severity::Warning,
                        "material.opaque_transparent_texture",
                        loc.clone(),
                        format!(
                            "alpha mode is Opaque but {:.0}% of the base colour texels are transparent ({})",
                            s.transparent * 100.0,
                            m.name
                        ),
                    );
                }
                _ => {}
            }
        }
    }
}
