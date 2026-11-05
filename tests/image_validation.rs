use rewrite_bridge::{Environment, Image, Material, Model, Scene};

fn model(width: u32, height: u32, rgba: Vec<u8>) -> Model {
    Model {
        materials: vec![Material {
            base_color: Some(Image {
                width,
                height,
                rgba,
            }),
            ..Material::default()
        }],
        ..Model::default()
    }
}
fn scene(width: u32, height: u32, rgba: Vec<u8>) -> Scene {
    Scene {
        environment: Some(Environment::Equirectangular {
            image: Image {
                width,
                height,
                rgba,
            },
            intensity: 1.0,
        }),
        ..Scene::default()
    }
}
#[test]
fn model_rejects_image_whose_byte_count_overflows() {
    for (w, h) in [(1 << 31, 1 << 31), (u32::MAX, u32::MAX)] {
        assert!(model(w, h, vec![]).validate().is_err());
    }
}
#[test]
fn scene_rejects_image_whose_byte_count_overflows() {
    for (w, h) in [(1 << 31, 1 << 31), (u32::MAX, u32::MAX)] {
        assert!(scene(w, h, vec![]).validate().is_err());
    }
}
#[test]
fn small_images_still_require_exact_nonzero_dimensions() {
    for (w, h, bytes, valid) in [
        (1, 1, 4, true),
        (3, 2, 24, true),
        (0, 1, 0, false),
        (1, 0, 0, false),
        (3, 2, 23, false),
        (3, 2, 25, false),
    ] {
        assert_eq!(model(w, h, vec![0; bytes]).validate().is_ok(), valid);
        assert_eq!(scene(w, h, vec![0; bytes]).validate().is_ok(), valid);
    }
}
