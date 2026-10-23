use super::*;
use crate::{
    AlphaMode, AnimatedModel, Animation, AssetInfo, AssetKind, AssetRef, BoxError, Channel, Image,
    Instance, Interpolation, Material, Mesh, Model, Node, NotSupported, Provider, Scene,
    SceneModel, Skin, Socket, Target, Transform,
};

/// A box with half extents `h`, standing on y = 0, with outward normals and counter-clockwise
/// faces: 24 vertices, 12 triangles.
fn box_mesh(h: [f32; 3]) -> Mesh {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    let mut m = Mesh {
        name: "box".into(),
        ..Mesh::default()
    };
    for (n, u, v) in faces {
        let base = m.positions.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [0, 1, 2].map(|k| h[k] * (n[k] + su * u[k] + sv * v[k]));
            m.positions.push([p[0], p[1] + h[1], p[2]]);
            m.normals.push(n);
            m.uvs.push([0.5 + 0.5 * su, 0.5 + 0.5 * sv]);
        }
        m.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    m
}

fn box_model(h: [f32; 3]) -> Model {
    Model {
        name: "box".into(),
        nodes: vec![Node::new("root", None)],
        meshes: vec![box_mesh(h)],
        materials: vec![Material::default()],
        skins: vec![],
    }
}

fn cube() -> Model {
    box_model([0.5; 3])
}

fn codes(r: &Report) -> Vec<&'static str> {
    r.findings.iter().map(|f| f.code).collect()
}

#[test]
fn a_correct_cube_passes_clean() {
    let r = check_model(&cube(), &Expect::default());
    assert!(r.findings.is_empty(), "{r}");
    assert!(r.passed() && r.is_clean());
    assert_eq!(r.measured.size(), Some([1.0, 1.0, 1.0]));
    assert_eq!(r.measured.triangles, 12);
    assert_eq!(r.measured.facing, Facing::Unknown);
}

#[test]
fn structural_errors_stop_the_other_checks() {
    let mut m = cube();
    m.meshes[0].indices[0] = 99;
    let r = check_model(&m, &Expect::default());
    assert_eq!(codes(&r), ["structure.invalid"]);
    assert!(!r.passed());
    assert!(r.findings[0].message.contains("index 99"));
}

#[test]
fn scale_is_checked_in_the_world_and_against_the_caller() {
    // A 30 m cube scaled x100 by its node is 3 km.
    let mut big = box_model([15.0; 3]);
    big.nodes[0].scale = [100.0; 3];
    assert!(check_model(&big, &Expect::default()).has("scale.too_large"));
    // The same cube is fine when it is not scaled.
    assert!(check_model(&box_model([15.0; 3]), &Expect::default()).is_clean());

    let mut tiny = cube();
    tiny.nodes[0].scale = [0.01; 3];
    assert!(check_model(&tiny, &Expect::default()).has("scale.too_small"));

    // A person 1.8 m tall is fine; the same person x100 is 180 m, which only the caller's range
    // can call wrong.
    let person = box_model([0.3, 0.9, 0.2]);
    assert!(check_model(&person, &Expect::human()).is_clean());
    let mut huge = person.clone();
    huge.nodes[0].scale = [100.0; 3];
    let r = check_model(&huge, &Expect::human());
    assert!(r.has("scale.height_out_of_range"), "{r}");
    assert!(!r.has("scale.too_large"));

    // Tank: 2 to 4 m tall, 4 to 10 m long.
    let tank = box_model([1.5, 1.0, 3.5]);
    assert!(check_model(&tank, &Expect::tank()).is_clean());
    let short = box_model([1.5, 1.0, 1.0]);
    assert!(check_model(&short, &Expect::tank()).has("scale.length_out_of_range"));
}

#[test]
fn skinned_vertices_are_measured_where_the_skin_puts_them() {
    let mut m = skinned_box();
    // Moving the joint moves the box in the node pose, and the bounds follow.
    m.nodes[1].translation = [0.0, 1.0, 0.0];
    m.skins[0].inverse_bind[1] = translation_matrix([0.0, -1.0, 0.0]);
    let r = check_model(&m, &Expect::default());
    assert_eq!(r.measured.bounds.map(|b| b.0[1]), Some(0.0), "{r}");
    // Without moving the inverse bind, the box rides up by one metre.
    m.skins[0].inverse_bind[1] = translation_matrix([0.0; 3]);
    let r = check_model(&m, &Expect::default());
    assert_eq!(r.measured.bounds.map(|b| b.0[1]), Some(1.0));
}

fn translation_matrix(t: [f32; 3]) -> [f32; 16] {
    let mut m = [0.0; 16];
    m[0] = 1.0;
    m[5] = 1.0;
    m[10] = 1.0;
    m[15] = 1.0;
    m[12..15].copy_from_slice(&t);
    m
}

/// A box bound entirely to one joint (node 1) of a two-node skin.
fn skinned_box() -> Model {
    let mut m = cube();
    m.nodes.push(Node::new("bone", Some(0)));
    m.skins.push(Skin {
        joints: vec![0, 1],
        inverse_bind: vec![translation_matrix([0.0; 3]); 2],
    });
    let n = m.meshes[0].positions.len();
    m.meshes[0].skin = Some(0);
    m.meshes[0].joints = vec![[1, 0, 0, 0]; n];
    m.meshes[0].weights = vec![[1.0, 0.0, 0.0, 0.0]; n];
    m
}

#[test]
fn characters_stand_upright_on_the_ground() {
    let person = box_model([0.3, 0.9, 0.2]);
    let r = check_model(&person, &Expect::human());
    assert!(r.findings.is_empty(), "{r}");

    // Lying on its back: Z-up data read as Y-up.
    let lying = box_model([0.3, 0.12, 0.9]);
    let r = check_model(&lying, &Expect::default().with_character(true));
    assert!(r.has("orientation.not_upright"), "{r}");

    // Wider than tall but not lying: only info.
    let t_pose = box_model([0.9, 0.7, 0.2]);
    // A long low creature is wider than tall, which is only info.
    let rat = box_model([0.3, 0.4, 1.1]);
    let r = check_model(&rat, &Expect::default().with_character(true));
    assert!(
        r.has("orientation.wide") && !r.has("orientation.not_upright"),
        "{r}"
    );
    let r = check_model(&t_pose, &Expect::default().with_character(true));
    assert!(
        r.has("orientation.wide") && !r.has("orientation.not_upright"),
        "{r}"
    );

    // Feet in the air.
    let mut floating = person.clone();
    floating.nodes[0].translation = [0.0, 1.0, 0.0];
    assert!(check_model(&floating, &Expect::human()).has("orientation.feet_off_ground"));
    // Sunk into the ground.
    floating.nodes[0].translation = [0.0, -0.5, 0.0];
    assert!(check_model(&floating, &Expect::human()).has("orientation.feet_off_ground"));
    // Within 0.3 m is fine.
    floating.nodes[0].translation = [0.0, -0.25, 0.0];
    assert!(!check_model(&floating, &Expect::human()).has("orientation.feet_off_ground"));

    // Off centre is info, and a prop is not held to any of this.
    let mut aside = person.clone();
    aside.nodes[0].translation = [2.0, 0.0, 0.0];
    let r = check_model(&aside, &Expect::human());
    assert_eq!(codes(&r), ["orientation.off_centre"]);
    assert_eq!(r.worst(), Some(Severity::Info));
    assert!(check_model(&lying, &Expect::default()).is_clean());
    // A skin makes a model a character unless the caller says otherwise.
    let mut skinned = skinned_box();
    skinned.nodes[0].translation = [0.0, 2.0, 0.0];
    assert!(check_model(&skinned, &Expect::default()).has("orientation.feet_off_ground"));
    assert!(
        !check_model(&skinned, &Expect::default().with_character(false))
            .has("orientation.feet_off_ground")
    );
}

#[test]
fn the_front_is_only_read_from_names() {
    let mut m = box_model([1.0, 1.0, 3.0]);
    // Nothing named: unknown, and no finding.
    let r = check_model(&m, &Expect::default());
    assert_eq!(r.measured.facing, Facing::Unknown);
    assert!(r.is_clean());

    // A muzzle at +Z: the convention.
    m.nodes.push(Node::new("gun_muzzle", Some(0)));
    m.nodes[1].translation = [0.0, 1.0, 3.0];
    let r = check_model(&m, &Expect::default());
    assert_eq!(r.measured.facing, Facing::PlusZ);
    assert!(r.is_clean());

    // At -Z the model faces backwards.
    m.nodes[1].translation = [0.0, 1.0, -3.0];
    let r = check_model(&m, &Expect::default());
    assert_eq!(r.measured.facing, Facing::MinusZ);
    assert!(r.has("front.not_plus_z"));

    // Along X.
    m.nodes[1].translation = [3.0, 1.0, 0.0];
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::PlusX
    );

    // A tail light at -Z says the front is +Z.
    m.nodes[1].name = "Tail_light_01".into();
    m.nodes[1].translation = [0.0, 1.0, -3.0];
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::PlusZ
    );

    // An exhaust is at the rear of a tank and the front of a radial-engined aircraft: no vote.
    m.nodes[1].name = "emtr_exhaust_01".into();
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::Unknown
    );
    // A plain "front" counts; a part's own front ("superstructure_front") does not.
    m.nodes[1].name = "Front_L".into();
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::MinusZ
    );
    m.nodes[1].name = "superstructure_front".into();
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::Unknown
    );
    // A root named after the asset ("...FrontB") says nothing.
    m.nodes[1].name = "front".into();
    m.nodes[1].parent = None;
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::Unknown
    );
    m.nodes[1].parent = Some(0);

    // Words must match whole: "eyebrow" is not "eye", "backpack" is not a rear.
    m.nodes[1].name = "eyebrow".into();
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::Unknown
    );
    // camelCase is split: "leftEye" has "eye".
    m.nodes[1].name = "leftEye".into();
    m.nodes[1].translation = [0.0, 1.0, -3.0];
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::MinusZ
    );
    // "Eye-Back" is the back eye, not a front point.
    m.nodes[1].name = "Eye-Back".into();
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::Unknown
    );
    m.nodes[1].name = "leftEye".into();
    // A point on the middle says nothing, and neither does a diagonal.
    m.nodes[1].translation = [0.0, 1.0, 0.0];
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::Unknown
    );
    m.nodes[1].translation = [2.0, 1.0, 2.5];
    assert_eq!(
        check_model(&m, &Expect::default()).measured.facing,
        Facing::Unknown
    );

    // Sockets count too.
    let mut a = AnimatedModel::new(box_model([1.0, 1.0, 3.0]));
    a.sockets.push(Socket::new(
        "muzzle",
        0,
        Transform::from_translation([0.0, 1.0, -3.0]),
    ));
    let r = check_animated(&a, &Expect::default());
    assert_eq!(r.measured.facing, Facing::MinusZ);
    assert!(r.has("front.not_plus_z"));
}

#[test]
fn flipped_winding_is_an_error() {
    let mut m = cube();
    for t in m.meshes[0].indices.as_chunks_mut::<3>().0 {
        t.swap(1, 2);
    }
    let r = check_model(&m, &Expect::default());
    assert!(r.has("geometry.winding_flipped"), "{r}");
    assert!(!r.passed());
    assert_eq!(r.findings[0].location.mesh, Some(0));
}

#[test]
fn partly_flipped_winding_is_only_mixed() {
    let mut m = cube();
    // Flip 4 of 12 triangles.
    for t in m.meshes[0]
        .indices
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .take(4)
    {
        t.swap(1, 2);
    }
    let r = check_model(&m, &Expect::default());
    assert!(
        r.has("geometry.winding_mixed") && !r.has("geometry.winding_flipped"),
        "{r}"
    );
    assert!(r.passed());
}

#[test]
fn winding_is_judged_in_mesh_space_so_mirrors_do_not_hide_it() {
    // A negative scale flips the world winding, but the mesh itself is still right.
    let mut m = cube();
    m.nodes[0].scale = [-1.0, 1.0, 1.0];
    assert!(check_model(&m, &Expect::default()).is_clean());
}

#[test]
fn non_finite_numbers_are_errors_and_do_not_panic() {
    for (what, f) in [
        (
            "positions",
            (|m: &mut Mesh| m.positions[3][1] = f32::NAN) as fn(&mut Mesh),
        ),
        ("normals", |m| m.normals[0][0] = f32::INFINITY),
        ("uvs", |m| m.uvs[5][1] = f32::NAN),
    ] {
        let mut m = cube();
        f(&mut m.meshes[0]);
        let r = check_model(&m, &Expect::default());
        assert!(r.has("geometry.non_finite"), "{what}: {r}");
        assert!(!r.passed());
    }
    // Bounds ignore the NaN vertex.
    let mut m = cube();
    m.meshes[0].positions[3] = [f32::NAN; 3];
    let r = check_model(&m, &Expect::default());
    assert!(r.measured.bounds.is_some());
}

#[test]
fn normals_must_be_unit_length() {
    let mut m = cube();
    m.meshes[0].normals[2] = [0.0, 0.0, 2.0];
    let r = check_model(&m, &Expect::default());
    assert!(r.has("geometry.normals_not_unit"), "{r}");
    let mut m = cube();
    m.meshes[0].normals[2] = [0.0, 0.0, 1.0005];
    assert!(check_model(&m, &Expect::default()).is_clean());
}

#[test]
fn degenerate_triangles_unused_vertices_and_missing_normals_are_counted() {
    let mut m = cube();
    // One collapsed triangle among thirteen: under the warning fraction? 1/13 is over 1%.
    m.meshes[0].indices.extend([0, 0, 1]);
    let r = check_model(&m, &Expect::default());
    let f = r.with_code("geometry.degenerate_triangles").next().unwrap();
    assert_eq!(f.severity, Severity::Warning);
    // Many triangles dilute it to info.
    let mut big = cube();
    for _ in 0..40 {
        let more = big.meshes[0].indices.clone();
        big.meshes[0].indices.extend(more);
        if big.meshes[0].indices.len() > 30_000 {
            break;
        }
    }
    big.meshes[0].indices.extend([0, 0, 1]);
    let r = check_model(&big, &Expect::default());
    let f = r.with_code("geometry.degenerate_triangles").next().unwrap();
    assert_eq!(f.severity, Severity::Info);

    let mut m = cube();
    m.meshes[0].positions.push([0.0; 3]);
    m.meshes[0].normals.push([0.0, 1.0, 0.0]);
    m.meshes[0].uvs.push([0.0; 2]);
    let r = check_model(&m, &Expect::default());
    assert_eq!(codes(&r), ["geometry.unused_vertices"]);

    let mut m = cube();
    m.meshes[0].normals.clear();
    let r = check_model(&m, &Expect::default());
    assert_eq!(codes(&r), ["geometry.no_normals"]);

    let mut m = cube();
    m.meshes[0].indices.clear();
    let r = check_model(&m, &Expect::default());
    assert!(
        r.has("geometry.empty_mesh") && r.has("geometry.no_vertices"),
        "{r}"
    );
}

#[test]
fn skin_weights_are_checked_beyond_the_sum() {
    let ok = skinned_box();
    let r = check_model(&ok, &Expect::default());
    assert!(r.findings.is_empty(), "{r}");

    // Since contract revision 4 `validate` itself rejects negative or non-finite weights and any
    // weighted slot past the skin, so conformance reports them as `structure.invalid`.
    let invalid = |m: &Model| {
        let r = check_model(m, &Expect::default());
        assert_eq!(codes(&r), ["structure.invalid"], "{r}");
        assert!(!r.passed());
        r.findings[0].message.clone()
    };

    // Negative weights that still sum to 1.
    let mut m = ok.clone();
    m.meshes[0].weights[4] = [1.5, -0.5, 0.0, 0.0];
    assert!(invalid(&m).contains("negative weight"));

    let mut m = ok.clone();
    m.meshes[0].weights[4] = [f32::NAN, 0.0, 0.0, 0.0];
    assert!(invalid(&m).contains("non-finite weight"));

    // A second weighted slot past the skin.
    let mut m = ok.clone();
    m.meshes[0].joints[4] = [1, 7, 0, 0];
    m.meshes[0].weights[4] = [0.5, 0.5, 0.0, 0.0];
    assert!(invalid(&m).contains("uses joint 7"));

    // Bad sums are validate's.
    let mut m = ok;
    m.meshes[0].weights[4] = [0.5, 0.0, 0.0, 0.0];
    assert_eq!(
        codes(&check_model(&m, &Expect::default())),
        ["structure.invalid"]
    );
}

#[test]
fn inverse_bind_matrices_must_be_usable() {
    let mut m = skinned_box();
    m.skins[0].inverse_bind[1] = [0.0; 16];
    let r = check_model(&m, &Expect::default());
    assert!(r.has("skin.inverse_bind_singular"), "{r}");
    assert!(!r.passed());

    let mut m = skinned_box();
    m.skins[0].inverse_bind[0][3] = f32::NAN;
    assert!(check_model(&m, &Expect::default()).has("skin.inverse_bind_non_finite"));

    // A tiny uniform scale is still invertible.
    let mut m = skinned_box();
    let mut s = translation_matrix([0.0; 3]);
    s[0] = 1e-4;
    s[5] = 1e-4;
    s[10] = 1e-4;
    m.skins[0].inverse_bind[0] = s;
    assert!(!check_model(&m, &Expect::default()).has("skin.inverse_bind_singular"));

    let mut m = skinned_box();
    m.skins[0].inverse_bind[1][15] = 2.0;
    assert!(check_model(&m, &Expect::default()).has("skin.inverse_bind_not_affine"));
}

#[test]
fn a_node_pose_that_is_not_the_bind_pose_is_info() {
    let mut m = skinned_box();
    m.nodes[1].translation = [0.0, 0.5, 0.0];
    let r = check_model(&m, &Expect::default());
    let f = r
        .with_code("skin.bind_pose_mismatch")
        .next()
        .expect("reported");
    assert_eq!(f.severity, Severity::Info);
    assert!(r.passed());
    // Moving the inverse bind to match makes it a bind pose.
    m.skins[0].inverse_bind[1] = translation_matrix([0.0, -0.5, 0.0]);
    assert!(!check_model(&m, &Expect::default()).has("skin.bind_pose_mismatch"));
}

fn textured(alpha: AlphaMode, a: u8) -> Model {
    let mut m = cube();
    m.materials[0] = Material {
        name: "mat".into(),
        base_color: Some(Image {
            width: 2,
            height: 2,
            rgba: [200, 100, 50, a].repeat(4),
        }),
        alpha,
        ..Material::default()
    };
    m
}

#[test]
fn materials_are_checked() {
    let fine = textured(AlphaMode::Opaque, 255);
    assert!(check_model(&fine, &Expect::default()).is_clean());

    let mut m = cube();
    m.materials[0].base_color_factor = [2.0, 1.0, 1.0, 1.0];
    m.materials[0].roughness = -1.0;
    let r = check_model(&m, &Expect::default());
    assert!(
        r.has("material.base_color_factor_range") && r.has("material.metallic_roughness_range")
    );

    let mut m = cube();
    m.materials[0].metallic = f32::NAN;
    assert!(check_model(&m, &Expect::default()).has("material.factor_invalid"));

    let mut m = cube();
    m.materials[0].emissive_factor = [-1.0, 0.0, 0.0];
    assert!(check_model(&m, &Expect::default()).has("material.emissive_invalid"));
    m.materials[0].emissive_factor = [f32::INFINITY, 0.0, 0.0];
    assert!(check_model(&m, &Expect::default()).has("material.emissive_invalid"));
    m.materials[0].emissive_factor = [5000.0, 0.0, 0.0];
    assert!(check_model(&m, &Expect::default()).has("material.emissive_extreme"));
    m.materials[0].emissive_factor = [4.0, 4.0, 4.0];
    assert!(check_model(&m, &Expect::default()).is_clean());

    let mut m = cube();
    m.materials[0].alpha = AlphaMode::Mask(1.5);
    assert!(check_model(&m, &Expect::default()).has("material.mask_cutoff_range"));

    let mut m = cube();
    m.materials.push(Material::default());
    assert_eq!(
        codes(&check_model(&m, &Expect::default())),
        ["material.unused"]
    );
}

#[test]
fn alpha_mode_must_agree_with_the_texture() {
    let r = check_model(&textured(AlphaMode::Blend, 255), &Expect::default());
    assert!(r.has("material.blend_opaque_texture"), "{r}");
    // A translucent factor is a reason to blend.
    let mut m = textured(AlphaMode::Blend, 255);
    m.materials[0].base_color_factor[3] = 0.5;
    assert!(check_model(&m, &Expect::default()).is_clean());
    // Blend with real transparency is fine.
    assert!(check_model(&textured(AlphaMode::Blend, 10), &Expect::default()).is_clean());

    let r = check_model(&textured(AlphaMode::Opaque, 10), &Expect::default());
    assert!(r.has("material.opaque_transparent_texture"), "{r}");
    // Mask is how cut-outs are done.
    assert!(check_model(&textured(AlphaMode::Mask(0.5), 10), &Expect::default()).is_clean());
}

#[test]
fn images_are_checked() {
    let mut m = cube();
    m.materials[0].base_color = Some(Image {
        width: 0,
        height: 0,
        rgba: vec![],
    });
    // `validate` refuses a zero-sized image, so this is `structure.invalid` now.
    let r = check_model(&m, &Expect::default());
    assert_eq!(codes(&r), ["structure.invalid"], "{r}");
    assert!(r.findings[0].message.contains("0x0"), "{r}");

    // Mean (0.5, 0.5, 1.0) is a flat tangent-space normal map; red-dominant is not.
    let mut m = cube();
    let normal = |px: [u8; 4]| Image {
        width: 2,
        height: 2,
        rgba: px.repeat(4),
    };
    m.materials[0].normal = Some(normal([128, 128, 255, 255]));
    assert!(check_model(&m, &Expect::default()).is_clean());
    m.materials[0].normal = Some(normal([255, 40, 40, 255]));
    assert!(check_model(&m, &Expect::default()).has("image.normal_map_suspect"));
    m.materials[0].normal = Some(normal([128, 128, 100, 255]));
    assert!(check_model(&m, &Expect::default()).has("image.normal_map_suspect"));
}

// ---- animation ----

fn rig() -> AnimatedModel {
    let mut m = Model {
        name: "rig".into(),
        nodes: vec![
            Node::new("armature", None),
            Node::new("hips", Some(0)),
            Node::new("head", Some(1)),
        ],
        ..Model::default()
    };
    m.nodes[1].translation = [0.0, 1.0, 0.0];
    m.nodes[2].translation = [0.0, 0.8, 0.0];
    let ibm = |y: f32| translation_matrix([0.0, -y, 0.0]);
    m.skins.push(Skin {
        joints: vec![0, 1, 2],
        inverse_bind: vec![ibm(0.0), ibm(1.0), ibm(1.8)],
    });
    AnimatedModel::new(m)
}

fn walk() -> Animation {
    Animation {
        name: "walk".into(),
        duration_s: 1.0,
        channels: vec![
            // Hips travel 1.5 m in a second.
            Channel::new(
                1,
                Target::Translation,
                Interpolation::Linear,
                vec![0.0, 1.0],
                vec![0.0, 1.0, 0.0, 0.0, 1.0, 1.5],
            ),
            Channel::new(
                2,
                Target::Rotation,
                Interpolation::Linear,
                vec![0.0, 0.5, 1.0],
                [
                    [0.0, 0.0, 0.0, 1.0],
                    [0.0, 0.1, 0.0, 0.995],
                    [0.0, 0.0, 0.0, 1.0],
                ]
                .concat()
                .to_vec(),
            ),
        ],
    }
}

fn with_clip(a: Animation) -> AnimatedModel {
    let mut r = rig();
    r.animations.push(a);
    r
}

#[test]
fn a_plausible_walk_passes_and_reports_its_speed() {
    let r = check_animated(&with_clip(walk()), &Expect::default());
    assert!(r.passed(), "{r}");
    assert_eq!(codes(&r), ["animation.root_motion"], "{r}");
    assert!(r.findings[0].message.contains("1.50 m/s"), "{r}");
    assert_eq!(r.measured.clips, 1);
    // Many clips are summarised in one finding.
    let mut m = with_clip(walk());
    let mut run = walk();
    run.name = "run".into();
    run.channels[0].values = vec![0.0, 1.0, 0.0, 0.0, 1.0, 6.0];
    m.animations.push(run);
    let r = check_animated(&m, &Expect::default());
    assert_eq!(codes(&r), ["animation.root_motion"], "{r}");
    let msg = &r.findings[0].message;
    assert!(
        msg.starts_with("2 clips")
            && msg.contains("slowest 1.50")
            && msg.contains("fastest 6.00 m/s (\"run\")"),
        "{msg}"
    );
}

#[test]
fn a_sign_flip_between_keys_is_a_warning() {
    let mut a = walk();
    // The middle key negated: the same rotation, in the other hemisphere.
    a.channels[1].values[4..8].copy_from_slice(&[0.0, -0.1, 0.0, -0.995]);
    let r = check_animated(&with_clip(a.clone()), &Expect::default());
    let f = r
        .with_code("animation.quaternion_sign_flip")
        .next()
        .expect("flagged");
    assert_eq!(f.severity, Severity::Warning);
    assert_eq!(f.location.clip.as_deref(), Some("walk"));
    assert_eq!(f.location.channel, Some(1));
    assert_eq!(f.location.node, Some(2));
    assert!(f.message.contains("2 rotation key"), "{}", f.message);
    // A step channel is not interpolated, so it cannot spin.
    a.channels[1].interpolation = Interpolation::Step;
    assert!(
        !check_animated(&with_clip(a), &Expect::default()).has("animation.quaternion_sign_flip")
    );
}

#[test]
fn cubic_spline_keys_are_read_at_their_value_slot() {
    // in-tangent, value, out-tangent: the tangents are not rotations and must not trigger.
    let mut a = walk();
    let key = |q: [f32; 4]| [[9.0; 4], q, [-9.0; 4]].concat();
    a.channels[1] = Channel::new(
        2,
        Target::Rotation,
        Interpolation::CubicSpline,
        vec![0.0, 1.0],
        [key([0.0, 0.0, 0.0, 1.0]), key([0.0, 0.0, 0.0, 1.0])].concat(),
    );
    let r = check_animated(&with_clip(a), &Expect::default());
    assert!(!r.has("animation.quaternion_sign_flip"), "{r}");
}

#[test]
fn zero_scale_keys_are_a_warning() {
    let mut a = walk();
    a.channels.push(Channel::new(
        2,
        Target::Scale,
        Interpolation::Linear,
        vec![0.0, 1.0],
        vec![1.0, 1.0, 1.0, 1.0, 0.0, 1.0],
    ));
    let r = check_animated(&with_clip(a), &Expect::default());
    assert!(r.has("animation.zero_scale"), "{r}");
}

#[test]
fn durations_names_and_empty_clips_are_checked() {
    let mut a = walk();
    a.duration_s = 0.0;
    a.channels.iter_mut().for_each(|c| {
        c.times = vec![0.0];
        c.values.truncate(c.target.components());
    });
    assert!(check_animated(&with_clip(a), &Expect::default()).has("animation.zero_duration"));

    let mut a = walk();
    a.duration_s = 5000.0;
    assert!(check_animated(&with_clip(a), &Expect::default()).has("animation.duration_suspect"));

    let mut m = with_clip(walk());
    m.animations.push(walk());
    m.animations.push(Animation::default());
    m.animations[2].name.clear();
    let r = check_animated(&m, &Expect::default());
    assert!(
        r.has("animation.duplicate_name") && r.has("animation.unnamed") && r.has("animation.empty")
    );
}

#[test]
fn root_motion_in_the_wrong_unit_is_extreme() {
    let mut a = walk();
    // Centimetres in the keys: 150 m in a second.
    a.channels[0].values = vec![0.0, 100.0, 0.0, 0.0, 100.0, 150.0];
    let r = check_animated(&with_clip(a), &Expect::default());
    assert!(r.has("animation.root_motion_extreme"), "{r}");
    assert!(!r.has("animation.root_motion"));
}

#[test]
fn a_clip_in_other_units_than_the_model_stretches_the_skeleton() {
    let mut a = walk();
    // The head's local height in centimetres against a model in metres.
    a.channels.push(Channel::new(
        2,
        Target::Translation,
        Interpolation::Linear,
        vec![0.0, 1.0],
        vec![0.0, 80.0, 0.0, 0.0, 80.0, 0.0],
    ));
    let r = check_animated(&with_clip(a), &Expect::default());
    let f = r
        .with_code("animation.pose_extent_mismatch")
        .next()
        .expect("flagged");
    assert_eq!(f.severity, Severity::Warning);
    // A clip that moves the skeleton about does not.
    assert!(!check_animated(&with_clip(walk()), &Expect::default())
        .has("animation.pose_extent_mismatch"));
}

#[test]
fn animation_structure_errors_are_validates() {
    let mut a = walk();
    a.channels[1].times = vec![0.0, 0.0, 1.0];
    let r = check_animated(&with_clip(a), &Expect::default());
    assert_eq!(codes(&r), ["structure.invalid"]);
    let mut a = walk();
    a.channels[1].values[0..4].copy_from_slice(&[0.0, 0.0, 0.0, 2.0]);
    assert_eq!(
        codes(&check_animated(&with_clip(a), &Expect::default())),
        ["structure.invalid"]
    );
}

// ---- scenes ----

fn scene_with(instances: Vec<Instance>) -> Scene {
    Scene {
        name: "yard".into(),
        models: vec![
            SceneModel::Inline(cube()),
            SceneModel::Asset(AssetRef::local("tree")),
        ],
        instances,
        ..Scene::default()
    }
}

#[test]
fn scenes_are_checked() {
    let at = |x: f32| Instance::new(0, Transform::from_translation([x, 0.0, 0.0]));
    let tree = |x: f32| Instance::new(1, Transform::from_translation([x, 0.0, 0.0]));
    let ok = scene_with(vec![at(0.0), tree(10.0)]);
    let r = check_scene(&ok, &Expect::default());
    assert!(r.findings.is_empty(), "{r}");
    assert_eq!(r.measured.instances, 2);
    assert_eq!(r.measured.bounds, Some(([0.0; 3], [10.0, 0.0, 0.0])));

    let r = check_scene(&scene_with(vec![]), &Expect::default());
    assert!(r.has("scene.empty") && r.has("scene.unused_model"));

    let r = check_scene(&scene_with(vec![at(0.0)]), &Expect::default());
    assert_eq!(codes(&r), ["scene.unused_model"]);

    let r = check_scene(
        &scene_with(vec![at(0.0), tree(250_000.0)]),
        &Expect::default(),
    );
    assert!(r.has("scene.too_large"));

    let mut skew = at(0.0);
    skew.transform.scale = [1.0, 2.0, 1.0];
    let mut mirror = tree(1.0);
    mirror.transform.scale = [-1.0, 1.0, 1.0];
    let mut wild = at(2.0);
    wild.transform.scale = [500.0; 3];
    let r = check_scene(
        &scene_with(vec![skew, mirror, wild, tree(0.0)]),
        &Expect::default(),
    );
    let n = |code| r.with_code(code).next().unwrap();
    assert!(n("scene.nonuniform_scale").message.starts_with("1 of 4"));
    assert!(n("scene.negative_scale").message.starts_with("1 of 4"));
    assert_eq!(n("scene.instance_scale_extreme").location.instance, Some(2));
    assert!(r.passed());

    // A mirrored instance with a mirrored axis pair is not mirrored.
    let mut twice = at(0.0);
    twice.transform.scale = [-1.0, -1.0, 1.0];
    let r = check_scene(&scene_with(vec![twice, tree(0.0)]), &Expect::default());
    assert!(!r.has("scene.negative_scale"));
}

#[test]
fn inline_models_are_checked_and_located() {
    let mut s = scene_with(vec![
        Instance::new(0, Transform::IDENTITY),
        Instance::new(1, Transform::IDENTITY),
    ]);
    if let SceneModel::Inline(m) = &mut s.models[0] {
        for t in m.meshes[0].indices.as_chunks_mut::<3>().0 {
            t.swap(1, 2);
        }
    }
    let r = check_scene(&s, &Expect::default());
    let f = r
        .with_code("geometry.winding_flipped")
        .next()
        .expect("flagged");
    assert_eq!(f.location.model, Some(0));
    assert_eq!(f.location.mesh, Some(0));
    s.models.push(SceneModel::Asset(AssetRef::local("tree")));
    assert!(check_scene(&s, &Expect::default()).has("scene.duplicate_asset"));
    s.instances[0].transform.scale = [0.0; 3];
    assert_eq!(
        codes(&check_scene(&s, &Expect::default())),
        ["structure.invalid"]
    );
}

// ---- report ----

#[test]
fn reports_print_sort_and_allow() {
    let mut m = cube();
    m.meshes[0].normals.clear();
    m.nodes[0].scale = [3000.0; 3];
    m.meshes[0].positions[0][0] = f32::NAN;
    let r = check_model(&m, &Expect::default());
    let text = r.to_string();
    assert!(
        text.starts_with("box: 1 error(s), 1 warning(s), 1 info"),
        "{text}"
    );
    let first = text.lines().nth(1).unwrap();
    assert!(
        first.trim_start().starts_with("error") && first.contains("[mesh 0]"),
        "{text}"
    );
    assert!(r.render(Severity::Warning).lines().count() == 3);
    assert_eq!(r.worst(), Some(Severity::Error));

    let allowed = check_model(
        &m,
        &Expect::default()
            .allowing("geometry.non_finite")
            .allowing("scale.too_large"),
    );
    assert_eq!(codes(&allowed), ["geometry.no_normals"]);
    assert!(Severity::Error > Severity::Warning && Severity::Warning > Severity::Info);
}

#[test]
fn tally_groups_by_code() {
    let mut a = Report::new("a");
    a.push(Severity::Warning, "x.y", Location::none(), "1".into());
    a.push(Severity::Warning, "x.y", Location::none(), "2".into());
    let mut b = Report::new("b");
    b.push(Severity::Warning, "x.y", Location::none(), "3".into());
    b.push(Severity::Error, "z", Location::none(), "4".into());
    let t = tally([&a, &b]);
    assert_eq!(t[0].code, "z");
    assert_eq!((t[1].code, t[1].count, t[1].assets), ("x.y", 3, 2));
    assert_eq!(t[1].examples, ["a", "b"]);
}

// ---- providers ----

enum Entry {
    Model(Box<dyn Fn() -> Model>),
    Scene(Scene),
    Fail,
    Panic,
}

struct Fake {
    entries: Vec<(AssetInfo, Entry)>,
    /// `load` returns a different model than `load_animated`.
    skew_load: bool,
    scenes: bool,
    /// `list` fails for these filters (the empty string is the full listing).
    list_fails_for: Vec<&'static str>,
}

fn info(id: &str, kind: AssetKind) -> AssetInfo {
    AssetInfo {
        id: id.into(),
        name: id.into(),
        kind,
    }
}

impl Provider for Fake {
    fn game(&self) -> &str {
        "Fake"
    }
    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError> {
        if self.list_fails_for.contains(&filter) {
            return Err(format!("listing {filter:?} is down").into());
        }
        Ok(self
            .entries
            .iter()
            .map(|(i, _)| i.clone())
            .filter(|i| i.id.contains(filter))
            .collect())
    }
    fn load(&self, id: &str) -> Result<Model, BoxError> {
        let mut m = self.load_animated(id)?.model;
        if self.skew_load {
            m.name.push('!');
        }
        Ok(m)
    }
    fn load_animated(&self, id: &str) -> Result<AnimatedModel, BoxError> {
        match self.entries.iter().find(|(i, _)| i.id == id) {
            Some((_, Entry::Model(f))) => Ok(AnimatedModel::new(f())),
            Some((_, Entry::Panic)) => panic!("boom"),
            _ => Err(format!("no model {id}").into()),
        }
    }
    fn load_scene(&self, id: &str) -> Result<Scene, BoxError> {
        if !self.scenes {
            return Err(Box::new(NotSupported {
                game: "Fake".into(),
                what: "scenes".into(),
            }));
        }
        match self.entries.iter().find(|(i, _)| i.id == id) {
            Some((_, Entry::Scene(s))) => Ok(s.clone()),
            _ => Err("no scene".into()),
        }
    }
}

fn fake() -> Fake {
    let scene = Scene {
        name: "yard".into(),
        models: vec![
            SceneModel::Asset(AssetRef::local("cube")),
            SceneModel::Asset(AssetRef::local("ghost")),
            SceneModel::Asset(AssetRef::of("Other Game", "elsewhere")),
        ],
        instances: (0..3)
            .map(|m| Instance::new(m, Transform::IDENTITY))
            .collect(),
        ..Scene::default()
    };
    Fake {
        entries: vec![
            (info("cube", AssetKind::Prop), Entry::Model(Box::new(cube))),
            (
                info("giant", AssetKind::Character),
                Entry::Model(Box::new(|| box_model([30.0, 90.0, 20.0]))),
            ),
            (info("missing", AssetKind::Prop), Entry::Fail),
            (info("crash", AssetKind::Prop), Entry::Panic),
            (info("yard", AssetKind::Level), Entry::Scene(scene)),
        ],
        skew_load: false,
        scenes: true,
        list_fails_for: vec![],
    }
}

fn by_id<'a>(rs: &'a [(String, Report)], id: &str) -> &'a Report {
    &rs.iter().find(|(i, _)| i == id).unwrap().1
}

#[test]
fn a_provider_is_checked_asset_by_asset() {
    let rs = check_provider(&fake(), &[] as &[&str], &[]);
    assert_eq!(rs.len(), 5);
    assert!(by_id(&rs, "cube").findings.is_empty());
    // Character kind brings its own expectation: 0.2 to 6 m.
    assert!(by_id(&rs, "giant").has("scale.height_out_of_range"));
    assert!(by_id(&rs, "missing").has("provider.load_failed"));
    assert!(by_id(&rs, "crash").has("provider.panicked"));
    let yard = by_id(&rs, "yard");
    let missing: Vec<_> = yard.with_code("scene.asset_missing").collect();
    // "ghost" is missing; "elsewhere" belongs to another game and is not ours to resolve.
    assert_eq!(missing.len(), 1, "{yard}");
    assert!(missing[0].message.contains("ghost"));
    assert_eq!(by_id(&rs, "cube").subject, "cube");
}

/// Review finding L40: a failed full listing used to switch the reference check off silently.
#[test]
fn asset_references_are_checked_when_the_full_listing_fails() {
    let mut f = fake();
    f.list_fails_for = vec![""];
    let rs = check_provider(&f, &["yard"], &[]);
    let yard = by_id(&rs, "yard");
    let missing: Vec<_> = yard.with_code("scene.asset_missing").collect();
    assert_eq!(missing.len(), 1, "{yard}");
    assert!(missing[0].message.contains("ghost"));
    // The same through the command line.
    let mut out = Vec::new();
    cli::run_to(&f, ["--id", "yard"].map(String::from), &mut out);
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("scene.asset_missing"), "{out}");
}

/// When even the single-asset lookups fail, the references cannot be judged: no false
/// `scene.asset_missing`, but a finding that says the check could not run.
#[test]
fn unverifiable_references_are_reported_not_skipped() {
    let mut f = fake();
    f.list_fails_for = vec!["", "ghost"];
    let rs = check_provider(&f, &["yard"], &[]);
    let yard = by_id(&rs, "yard");
    assert!(!yard.has("scene.asset_missing"), "{yard}");
    let failed: Vec<_> = yard.with_code("provider.list_failed").collect();
    assert_eq!(failed.len(), 1, "{yard}");
    assert!(failed[0].message.contains("ghost"), "{yard}");
    assert!(yard.passed(), "a warning, not an error");
}

#[test]
fn ids_and_kinds_select() {
    let f = fake();
    let rs = check_provider(&f, &["cube", "nope"], &[]);
    assert_eq!(rs.len(), 2);
    assert!(by_id(&rs, "nope").has("provider.unlisted_id"));
    assert!(by_id(&rs, "nope").has("provider.load_failed"));
    let rs = check_provider(&f, &[] as &[&str], &[AssetKind::Prop]);
    assert_eq!(rs.len(), 3);
    let rs = check_provider(
        &f,
        &[] as &[&str],
        &[AssetKind::Level, AssetKind::Character],
    );
    assert_eq!(rs.len(), 2);
}

#[test]
fn providers_without_scenes_are_told_so_gently() {
    let mut f = fake();
    f.scenes = false;
    let rs = check_provider(&f, &["yard"], &[]);
    let r = by_id(&rs, "yard");
    assert_eq!(codes(r), ["provider.scene_unsupported"]);
    assert!(r.passed());
}

#[test]
fn load_and_load_animated_are_compared_on_request() {
    let mut f = fake();
    f.skew_load = true;
    let opts = ProviderOptions {
        compare_load: true,
        ..ProviderOptions::default()
    };
    let mut got = Vec::new();
    check_provider_with(&f, &["cube"], &[], &opts, |_, r| got.push(r));
    assert!(got[0].has("provider.load_mismatch"));
    // Off by default.
    assert!(check_provider(&f, &["cube"], &[])[0].1.findings.is_empty());
    // And allowing a code drops it.
    let opts = ProviderOptions {
        compare_load: true,
        allow: vec!["provider.load_mismatch".into()],
        ..ProviderOptions::default()
    };
    let mut got = Vec::new();
    check_provider_with(&f, &["cube"], &[], &opts, |_, r| got.push(r));
    assert!(got[0].findings.is_empty());
}

#[test]
fn the_command_line_runs_and_sets_the_exit_status() {
    let run = |args: &[&str]| {
        let mut out = Vec::new();
        let code = cli::run_to(&fake(), args.iter().map(|s| s.to_string()), &mut out);
        (code, String::from_utf8(out).unwrap())
    };
    let (code, out) = run(&[]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("[FAIL] missing"), "{out}");
    assert!(out.contains("[ok  ] cube"), "{out}");
    assert!(out.contains("5 asset(s) checked"), "{out}");
    assert!(out.contains("provider.load_failed"), "{out}");

    let (code, out) = run(&["--id", "cube"]);
    assert_eq!(code, 0, "{out}");
    let (code, _) = run(&["--id", "giant"]);
    assert_eq!(code, 0);
    let (code, _) = run(&["--id", "giant", "--strict"]);
    assert_eq!(code, 1);
    let (code, _) = run(&[
        "--id",
        "giant",
        "--strict",
        "--allow",
        "scale.height_out_of_range",
    ]);
    assert_eq!(code, 0);

    let (code, out) = run(&["--kind", "prop", "--limit", "1"]);
    assert_eq!(
        (code, out.contains("1 asset(s) checked")),
        (0, true),
        "{out}"
    );
    let (_, out) = run(&["--filter", "gi"]);
    assert!(out.contains("giant") && !out.contains("cube"), "{out}");

    let (code, out) = run(&["--kind", "wizard"]);
    assert_eq!(code, 2);
    assert!(out.contains("unknown kind"));
    assert_eq!(run(&["--bogus"]).0, 2);
    assert_eq!(run(&["--limit"]).0, 2);
    // Review finding L40: `--limit 0` used to check every asset.
    let (code, out) = run(&["--limit", "0"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("--limit") && !out.contains("asset(s) checked"),
        "{out}"
    );
    let (code, out) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("--compare-load"));
}

#[test]
fn verbose_shows_info() {
    let mut f = fake();
    f.entries[0].1 = Entry::Model(Box::new(|| {
        let mut m = cube();
        m.meshes[0].normals.clear();
        m
    }));
    let run = |args: &[&str]| {
        let mut out = Vec::new();
        cli::run_to(&f, args.iter().map(|s| s.to_string()), &mut out);
        String::from_utf8(out).unwrap()
    };
    assert!(!run(&["--id", "cube"]).contains("geometry.no_normals"));
    assert!(run(&["--id", "cube", "-v"]).contains("geometry.no_normals"));
}

#[test]
fn every_documented_code_exists_in_the_docs_table() {
    // The module docs are the list of stable codes: every code a test above provokes is in them.
    let docs = include_str!("mod.rs");
    for code in [
        "structure.invalid",
        "scale.too_small",
        "scale.too_large",
        "orientation.not_upright",
        "front.not_plus_z",
        "geometry.winding_flipped",
        "skin.negative_weights",
        "animation.quaternion_sign_flip",
        "scene.nonuniform_scale",
        "provider.load_failed",
    ] {
        assert!(docs.contains(&format!("`{code}`")), "{code}");
    }
}
