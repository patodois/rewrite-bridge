//! `Model::validate` rules added after the code review (finding H2): each rule has a model that
//! used to pass and now fails, and a neighbouring model that still passes.

use rewrite_bridge::*;

fn triangle() -> Model {
    Model {
        name: "tri".into(),
        nodes: vec![Node::new("root", None)],
        meshes: vec![Mesh {
            positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            uvs: vec![[0.0; 2]; 3],
            indices: vec![0, 1, 2],
            ..Mesh::default()
        }],
        materials: vec![Material::default()],
        skins: vec![],
    }
}

fn skinned() -> Model {
    let mut m = triangle();
    m.nodes.push(Node::new("bone", Some(0)));
    m.skins.push(Skin {
        joints: vec![0, 1],
        inverse_bind: vec![[0.0; 16]; 2],
    });
    let mesh = &mut m.meshes[0];
    mesh.skin = Some(0);
    mesh.joints = vec![[0, 1, 0, 0]; 3];
    mesh.weights = vec![[0.25, 0.75, 0.0, 0.0]; 3];
    m
}

fn rejected(mut m: Model, f: impl FnOnce(&mut Model)) -> String {
    f(&mut m);
    m.validate().expect_err("the damaged model must fail").0
}

fn accepted(mut m: Model, f: impl FnOnce(&mut Model)) {
    f(&mut m);
    assert_eq!(m.validate(), Ok(()));
}

#[test]
fn every_weighted_slot_must_name_a_valid_joint() {
    // The skin has 2 joints. Slot 1 carries weight and names joint 9; slot 0 is fine, which is
    // all the old check looked at.
    let why = rejected(skinned(), |m| {
        m.meshes[0].joints = vec![[0, 9, 0, 0]; 3];
        m.meshes[0].weights = vec![[0.5, 0.5, 0.0, 0.0]; 3];
    });
    assert!(why.contains("joint 9"), "{why}");
    // A slot with no weight may hold anything.
    accepted(skinned(), |m| {
        m.meshes[0].joints = vec![[1, 0, 9, 9]; 3];
        m.meshes[0].weights = vec![[1.0, 0.0, 0.0, 0.0]; 3];
    });
}

#[test]
fn weights_must_be_finite() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let why = rejected(skinned(), |m| {
            m.meshes[0].weights = vec![[bad, 0.0, 0.0, 0.0]; 3];
        });
        assert!(why.contains("weight"), "{bad}: {why}");
    }
    // Also in a slot whose weight is otherwise zero-sum friendly.
    let why = rejected(skinned(), |m| {
        m.meshes[0].weights = vec![[1.0, f32::NAN, 0.0, 0.0]; 3];
    });
    assert!(why.contains("weight"), "{why}");
}

#[test]
fn weights_must_not_be_negative() {
    // Sums to 1, so only the sign can refuse it; the joint is valid too.
    let why = rejected(skinned(), |m| {
        m.meshes[0].weights = vec![[1.5, -0.5, 0.0, 0.0]; 3];
    });
    assert!(why.contains("negative") || why.contains("weight"), "{why}");
}

#[test]
fn weights_must_sum_to_one() {
    let why = rejected(skinned(), |m| {
        m.meshes[0].weights[1] = [0.5, 0.0, 0.0, 0.0];
    });
    assert!(why.contains("weights sum to 0.5"), "{why}");
    accepted(skinned(), |m| {
        m.meshes[0].weights = vec![[0.25, 0.7505, 0.0, 0.0]; 3];
    });
}

#[test]
fn node_transforms_must_be_finite() {
    for (what, f) in [
        (
            "translation",
            (|m: &mut Model| m.nodes[0].translation = [f32::NAN, 0.0, 0.0]) as fn(&mut Model),
        ),
        ("translation", |m| {
            m.nodes[0].translation = [0.0, f32::INFINITY, 0.0]
        }),
        ("rotation", |m| {
            m.nodes[0].rotation = [0.0, 0.0, f32::NAN, 1.0]
        }),
        ("scale", |m| m.nodes[0].scale = [1.0, f32::NAN, 1.0]),
        ("scale", |m| {
            m.nodes[0].scale = [1.0, 1.0, f32::NEG_INFINITY]
        }),
    ] {
        let why = rejected(triangle(), f);
        assert!(why.contains(what), "{what}: {why}");
        assert!(why.contains("node 0"), "{why}");
    }
}

#[test]
fn node_rotations_must_be_unit_quaternions() {
    let zero = rejected(triangle(), |m| m.nodes[0].rotation = [0.0; 4]);
    assert!(
        zero.contains("rotation") && zero.contains("node 0"),
        "{zero}"
    );
    let long = rejected(triangle(), |m| m.nodes[0].rotation = [0.0, 0.0, 0.0, 1.002]);
    assert!(long.contains("rotation"), "{long}");
    let big = rejected(triangle(), |m| m.nodes[0].rotation = [1.0, 1.0, 1.0, 1.0]);
    assert!(big.contains("rotation"), "{big}");
    // Within 1e-3 of length 1 is fine, and so is a real rotation.
    accepted(triangle(), |m| {
        m.nodes[0].rotation = [0.0, 0.0, 0.0, 1.0005];
    });
    accepted(triangle(), |m| {
        let h = std::f32::consts::FRAC_1_SQRT_2;
        m.nodes[0].rotation = [0.0, h, 0.0, h];
        m.nodes[0].translation = [1.0, 2.0, 3.0];
        m.nodes[0].scale = [2.0, 2.0, 2.0];
    });
}

#[test]
fn images_must_have_a_size() {
    for (w, h) in [(0, 4), (4, 0), (0, 0)] {
        let why = rejected(triangle(), |m| {
            m.materials[0].base_color = Some(Image {
                width: w,
                height: h,
                rgba: vec![],
            });
        });
        assert!(why.contains("image"), "{w}x{h}: {why}");
    }
    // Every slot of a material is checked.
    for slot in 0..4 {
        let why = rejected(triangle(), |m| {
            let empty = Some(Image {
                width: 0,
                height: 3,
                rgba: vec![],
            });
            let mat = &mut m.materials[0];
            match slot {
                0 => mat.base_color = empty,
                1 => mat.metallic_roughness = empty,
                2 => mat.normal = empty,
                _ => mat.emissive = empty,
            }
        });
        assert!(why.contains("image"), "slot {slot}: {why}");
    }
}

#[test]
fn images_must_hold_width_times_height_times_four_bytes() {
    let why = rejected(triangle(), |m| {
        m.materials[0].base_color = Some(Image {
            width: 2,
            height: 2,
            rgba: vec![0; 15],
        });
    });
    assert!(why.contains("holds 15 bytes"), "{why}");
    accepted(triangle(), |m| {
        m.materials[0].base_color = Some(Image {
            width: 2,
            height: 2,
            rgba: vec![0; 16],
        });
    });
}
