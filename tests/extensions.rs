//! Animation, scenes and sockets as a downstream crate sees them, and a guard that the types that
//! existed before them still build the way downstream crates build them.

use rewrite_bridge::{
    AlphaMode, AnimatedModel, Animation, AssetInfo, AssetKind, AssetRef, BoxError, Channel,
    Environment, Image, Instance, Interpolation, Light, LightKind, Material, Mesh, Model, Node,
    NotSupported, Provider, Scene, SceneModel, Skin, Socket, Target, Transform,
};

/// Written the way a provider written before the extensions is: only the three original methods,
/// and every field of the original types spelled out in struct literals (no `..Default`), so
/// adding a field to any of them would fail to compile here.
struct OldGame;

impl Provider for OldGame {
    fn game(&self) -> &str {
        "Old Game"
    }

    fn list(&self, filter: &str) -> Result<Vec<AssetInfo>, BoxError> {
        let all = vec![AssetInfo {
            id: "crate".into(),
            name: "Crate".into(),
            kind: AssetKind::Prop,
        }];
        Ok(all.into_iter().filter(|a| a.id.contains(filter)).collect())
    }

    fn load(&self, _id: &str) -> Result<Model, BoxError> {
        Ok(Model {
            name: "crate".into(),
            nodes: vec![Node {
                name: "root".into(),
                parent: None,
                translation: [0.0; 3],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
            }],
            meshes: vec![Mesh {
                name: "box".into(),
                node: 0,
                material: 0,
                positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![],
                tangents: vec![],
                uvs: vec![],
                joints: vec![],
                weights: vec![],
                skin: None,
                indices: vec![0, 1, 2],
            }],
            materials: vec![Material {
                name: "wood".into(),
                base_color: Some(Image {
                    width: 1,
                    height: 1,
                    rgba: vec![255; 4],
                }),
                base_color_factor: [1.0; 4],
                metallic_roughness: None,
                metallic: 0.0,
                roughness: 0.8,
                normal: None,
                emissive: None,
                emissive_factor: [0.0; 3],
                alpha: AlphaMode::Opaque,
                double_sided: false,
            }],
            skins: vec![Skin {
                joints: vec![],
                inverse_bind: vec![],
            }],
        })
    }
}

#[test]
fn old_providers_get_working_defaults() {
    let game = OldGame;
    let animated = game.load_animated("crate").unwrap();
    assert_eq!(animated.model, game.load("crate").unwrap());
    assert!(animated.animations.is_empty());
    assert!(animated.sockets.is_empty());
    assert!(animated.emissive_strength.is_empty());
    assert_eq!(animated.validate(), Ok(()));

    let err = game.load_scene("level1").unwrap_err();
    assert_eq!(err.to_string(), "Old Game: scenes not supported");
    assert_eq!(
        err.downcast_ref::<NotSupported>(),
        Some(&NotSupported {
            game: "Old Game".into(),
            what: "scenes".into()
        })
    );
}

/// A provider that offers everything, used through a trait object.
struct NewGame {
    robot: AnimatedModel,
    yard: Scene,
}

impl NewGame {
    fn new() -> Self {
        let mut robot = AnimatedModel::from(Model {
            name: "robot".into(),
            nodes: vec![Node::new("root", None), Node::new("hand", Some(0))],
            materials: vec![Material {
                emissive_factor: [0.0, 0.5, 1.0],
                ..Material::default()
            }],
            ..Model::default()
        });
        robot.animations = vec![Animation {
            name: "reach".into(),
            duration_s: 1.0,
            channels: vec![
                Channel::new(
                    1,
                    Target::Translation,
                    Interpolation::Linear,
                    vec![0.0, 1.0],
                    vec![0.0, 0.0, 0.0, 0.0, 0.5, 0.0],
                ),
                Channel::new(
                    1,
                    Target::Rotation,
                    Interpolation::Step,
                    vec![0.0],
                    vec![0.0, 0.0, 0.0, 1.0],
                ),
            ],
        }];
        robot.sockets = vec![Socket::new(
            "grip",
            1,
            Transform::from_translation([0.0, 0.05, 0.0]),
        )];
        robot.emissive_strength = vec![3.0];

        let yard = Scene {
            name: "yard".into(),
            models: vec![
                SceneModel::Inline(robot.model.clone()),
                SceneModel::Asset(AssetRef::local("robot")),
                SceneModel::Asset(AssetRef::of("Old Game", "crate")),
            ],
            instances: (0..1000)
                .map(|i| Instance::new(i % 3, Transform::from_translation([i as f32, 0.0, 0.0])))
                .collect(),
            lights: vec![Light {
                name: "sun".into(),
                kind: LightKind::Directional,
                color: [1.0; 3],
                intensity: 90_000.0,
                transform: Transform::IDENTITY,
            }],
            environment: Some(Environment::Color([0.2, 0.4, 0.8])),
        };
        Self { robot, yard }
    }
}

impl Provider for NewGame {
    fn game(&self) -> &str {
        "New Game"
    }
    fn list(&self, _: &str) -> Result<Vec<AssetInfo>, BoxError> {
        Ok(vec![])
    }
    fn load(&self, id: &str) -> Result<Model, BoxError> {
        Ok(self.load_animated(id)?.model)
    }
    fn load_animated(&self, _: &str) -> Result<AnimatedModel, BoxError> {
        Ok(self.robot.clone())
    }
    fn load_scene(&self, _: &str) -> Result<Scene, BoxError> {
        Ok(self.yard.clone())
    }
}

#[test]
fn everything_round_trips_through_a_provider_object() {
    let game: Box<dyn Provider> = Box::new(NewGame::new());

    let robot = game.load_animated("robot").unwrap();
    assert_eq!(robot.validate(), Ok(()));
    assert_eq!(robot.animation("reach").unwrap().channels.len(), 2);
    assert_eq!(robot.socket("grip").unwrap().node, 1);
    assert_eq!(robot.emissive_factor(0), Some([0.0, 1.5, 3.0]));
    assert_eq!(robot, robot.clone());
    // The plain model is the same asset.
    assert_eq!(game.load("robot").unwrap(), robot.model);

    let yard = game.load_scene("yard").unwrap();
    assert_eq!(yard.validate(), Ok(()));
    assert_eq!(yard.instances.len(), 1000);
    assert_eq!(yard.models.len(), 3);
    assert_eq!(yard, yard.clone());
    match &yard.models[1] {
        SceneModel::Asset(a) => {
            // A consumer resolves the reference through a provider, once per model.
            assert_eq!(game.load(&a.asset).unwrap().name, "robot");
        }
        _ => panic!("expected an asset reference"),
    }
}
