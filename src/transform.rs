//! A translation, rotation and scale, shared by scene instances, lights and attachment points.

use crate::{Invalid, Node};

/// How far a quaternion's length may be from 1 before [`Transform::validate`] and the animation
/// rules call it unnormalised.
pub const QUATERNION_TOLERANCE: f32 = 1e-3;

/// Why a quaternion is not a unit rotation, or `None` when it is fine.
pub(crate) fn quaternion_problem(q: &[f32; 4]) -> Option<String> {
    let len = q
        .iter()
        .map(|&c| f64::from(c) * f64::from(c))
        .sum::<f64>()
        .sqrt();
    if q.iter().any(|c| !c.is_finite()) {
        Some(format!("quaternion {q:?} is not finite"))
    } else if (len - 1.0).abs() > f64::from(QUATERNION_TOLERANCE) {
        Some(format!("quaternion {q:?} has length {len}, not 1"))
    } else {
        None
    }
}

/// A rigid-plus-scale transform with the same meaning as a glTF node's TRS: the matrix is
/// `T * R * S`, so a point is scaled first, then rotated, then translated.
///
/// Units and axes are glTF's: metres, +Y up, right-handed, quaternions as x, y, z, w.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    /// Metres.
    pub translation: [f32; 3],
    /// Unit quaternion as x, y, z, w.
    pub rotation: [f32; 4],
    /// Per-axis scale along the local axes, applied before the rotation. A negative component
    /// mirrors the object (and flips its triangle winding, as in glTF); zero is rejected by
    /// [`Transform::validate`].
    pub scale: [f32; 3],
}

impl Transform {
    /// No translation, no rotation, scale 1.
    pub const IDENTITY: Transform = Transform {
        translation: [0.0; 3],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
    };

    /// A pure translation in metres.
    pub const fn from_translation(translation: [f32; 3]) -> Self {
        Transform {
            translation,
            ..Transform::IDENTITY
        }
    }

    /// Why this transform is unusable, without saying whose it is.
    pub(crate) fn problem(&self) -> Option<String> {
        if self.translation.iter().any(|c| !c.is_finite()) {
            return Some(format!("translation {:?} is not finite", self.translation));
        }
        if let Some(p) = quaternion_problem(&self.rotation) {
            return Some(p);
        }
        if self.scale.iter().any(|c| !c.is_finite() || *c == 0.0) {
            return Some(format!("scale {:?} is zero or not finite", self.scale));
        }
        None
    }

    /// Checks that translation and scale are finite, the scale has no zero component and the
    /// rotation is a unit quaternion within [`QUATERNION_TOLERANCE`].
    pub fn validate(&self) -> Result<(), Invalid> {
        self.problem().map_or(Ok(()), |p| Err(Invalid(p)))
    }
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl From<&Node> for Transform {
    /// The node's local transform (relative to its parent).
    fn from(n: &Node) -> Self {
        Transform {
            translation: n.translation,
            rotation: n.rotation,
            scale: n.scale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_valid_and_default() {
        assert_eq!(Transform::default(), Transform::IDENTITY);
        assert_eq!(Transform::IDENTITY.validate(), Ok(()));
        assert_eq!(
            Transform::from_translation([1.0, 2.0, 3.0]).translation,
            [1.0, 2.0, 3.0]
        );
    }

    #[test]
    fn bad_transforms_are_reported() {
        let fails = |f: fn(&mut Transform)| {
            let mut t = Transform::IDENTITY;
            f(&mut t);
            t.validate().unwrap_err().0
        };
        assert!(fails(|t| t.translation[1] = f32::NAN).contains("translation"));
        assert!(fails(|t| t.rotation = [0.0, 0.0, 0.0, 2.0]).contains("length 2, not 1"));
        assert!(fails(|t| t.rotation[0] = f32::INFINITY).contains("not finite"));
        assert!(fails(|t| t.scale[2] = 0.0).contains("scale"));
        assert!(fails(|t| t.scale[0] = f32::INFINITY).contains("scale"));
    }

    #[test]
    fn mirroring_and_slightly_off_quaternions_are_fine() {
        let mut t = Transform::IDENTITY;
        t.scale = [-1.0, 1.0, 1.0];
        t.rotation = [0.0, 0.0, 0.0, 1.0 + QUATERNION_TOLERANCE / 2.0];
        assert_eq!(t.validate(), Ok(()));
    }

    #[test]
    fn nodes_convert() {
        let mut n = Node::new("n", None);
        n.translation = [1.0, 0.0, 0.0];
        assert_eq!(Transform::from(&n).translation, [1.0, 0.0, 0.0]);
    }
}
