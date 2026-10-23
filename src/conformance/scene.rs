//! Scene checks.

use std::collections::HashSet;

use super::geometry::Aabb;
use super::report::{Location, Report, Severity};
use super::{Expect, INSTANCE_SCALE_EXTREME, NONUNIFORM_SCALE_TOLERANCE, SCENE_EXTENT_WARN_M};
use crate::{Scene, SceneModel};

/// Runs every scene check on a scene that already passed `validate`.
///
/// Instances are in world space, so the scene's extent is measured on their origins (and is
/// stored in `Measurements::bounds`); models that are only asset references have no size here.
/// Inline models get the model checks with a generic [`Expect`], and their findings keep their
/// codes and gain the model's index as location.
///
/// What matters to a consumer: a scene spread over more than 100 km loses float precision away
/// from the origin (jitter); an instance scaled by more than 100x or less than 0.01x is usually a
/// unit fix applied per instance instead of once in the model; a non-uniform scale skews normals
/// and a negative one flips the winding, which a consumer must handle; a model that no instance
/// uses, or that is listed twice and so loaded twice, is waste.
pub(crate) fn check(scene: &Scene, _expect: &Expect, rep: &mut Report) {
    rep.measured.instances = scene.instances.len();
    if scene.instances.is_empty() {
        rep.push(
            Severity::Warning,
            "scene.empty",
            Location::none(),
            "the scene has no instances".into(),
        );
    }

    let mut used = vec![false; scene.models.len()];
    for inst in &scene.instances {
        if let Some(u) = used.get_mut(inst.model) {
            *u = true;
        }
    }
    let mut seen_assets = HashSet::new();
    for (i, m) in scene.models.iter().enumerate() {
        if !used[i] {
            rep.push(
                Severity::Info,
                "scene.unused_model",
                Location::none().with_model(i),
                format!("no instance places model {i}"),
            );
        }
        match m {
            SceneModel::Inline(model) => {
                let mut sub = Report::new(model.name.clone());
                super::model::check(model, &[], &Expect::default(), &mut sub);
                for mut f in sub.findings {
                    f.location.model = Some(i);
                    rep.findings.push(f);
                }
            }
            SceneModel::Asset(a) if !seen_assets.insert((a.provider.clone(), a.asset.clone())) => {
                rep.push(
                    Severity::Info,
                    "scene.duplicate_asset",
                    Location::none().with_model(i),
                    format!(
                        "model {i} refers to asset {:?} again; one entry would be loaded once",
                        a.asset
                    ),
                );
            }
            SceneModel::Asset(_) => {}
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }

    let mut bounds = Aabb::new();
    let (mut nonuniform, mut negative, mut extreme) = (0usize, 0usize, 0usize);
    let mut first = [None; 3];
    for (i, inst) in scene.instances.iter().enumerate() {
        let t = &inst.transform;
        bounds.add(t.translation.map(f64::from));
        let a = t.scale.map(f32::abs);
        let (lo, hi) = (
            a.iter().copied().fold(f32::INFINITY, f32::min),
            a.iter().copied().fold(0.0, f32::max),
        );
        if hi - lo > NONUNIFORM_SCALE_TOLERANCE * hi {
            nonuniform += 1;
            first[0].get_or_insert(i);
        }
        if t.scale.iter().filter(|s| **s < 0.0).count() % 2 == 1 {
            negative += 1;
            first[1].get_or_insert(i);
        }
        if hi > INSTANCE_SCALE_EXTREME || lo < 1.0 / INSTANCE_SCALE_EXTREME {
            extreme += 1;
            first[2].get_or_insert(i);
        }
    }
    rep.measured.bounds = bounds.as_f32();
    if let Some((lo, hi)) = bounds.get() {
        let largest = (0..3).map(|k| hi[k] - lo[k]).fold(0.0, f64::max);
        if largest > f64::from(SCENE_EXTENT_WARN_M) {
            rep.push(
                Severity::Warning,
                "scene.too_large",
                Location::none(),
                format!(
                    "instances are spread over {largest:.0} m, more than {SCENE_EXTENT_WARN_M} m: \
                     float precision suffers far from the origin; wrong units?"
                ),
            );
        }
    }
    if extreme > 0 {
        rep.push(
            Severity::Warning,
            "scene.instance_scale_extreme",
            Location::instance(first[2].unwrap_or(0)),
            format!(
                "{extreme} instance(s) are scaled outside {}x to {INSTANCE_SCALE_EXTREME}x, the first is instance {}",
                1.0 / INSTANCE_SCALE_EXTREME,
                first[2].unwrap_or(0)
            ),
        );
    }
    if nonuniform > 0 {
        rep.push(
            Severity::Info,
            "scene.nonuniform_scale",
            Location::instance(first[0].unwrap_or(0)),
            format!(
                "{nonuniform} of {} instances have a non-uniform scale, the first is instance {}",
                scene.instances.len(),
                first[0].unwrap_or(0)
            ),
        );
    }
    if negative > 0 {
        rep.push(
            Severity::Info,
            "scene.negative_scale",
            Location::instance(first[1].unwrap_or(0)),
            format!(
                "{negative} of {} instances are mirrored (negative scale), the first is instance {}",
                scene.instances.len(),
                first[1].unwrap_or(0)
            ),
        );
    }
}

/// Whether every asset reference of the scene that belongs to `game` is in `known`.
pub(crate) fn check_refs(scene: &Scene, game: &str, known: &HashSet<String>, rep: &mut Report) {
    check_refs_with(scene, game, |asset| Ok(known.contains(asset)), rep);
}

/// Like [`check_refs`], asking `has` about each reference of the game: `Ok(false)` is a missing
/// asset, `Err` means the provider could not answer, which is a warning (the reference stays
/// unchecked) and never a silent pass.
pub(crate) fn check_refs_with(
    scene: &Scene,
    game: &str,
    mut has: impl FnMut(&str) -> Result<bool, String>,
    rep: &mut Report,
) {
    for (i, m) in scene.models.iter().enumerate() {
        if let SceneModel::Asset(a) = m {
            if !a.provider.as_deref().is_none_or(|p| p == game) {
                continue;
            }
            match has(&a.asset) {
                Ok(true) => {}
                Ok(false) => rep.push(
                    Severity::Error,
                    "scene.asset_missing",
                    Location::none().with_model(i),
                    format!(
                        "model {i} refers to asset {:?}, which {game} does not list",
                        a.asset
                    ),
                ),
                Err(e) => rep.push(
                    Severity::Warning,
                    "provider.list_failed",
                    Location::none().with_model(i),
                    format!(
                        "model {i} refers to asset {:?}, which could not be checked: \
                         Provider::list failed: {e}",
                        a.asset
                    ),
                ),
            }
        }
    }
}
