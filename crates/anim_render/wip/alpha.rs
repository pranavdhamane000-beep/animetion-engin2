//! Alpha modes, conversion and blend descriptors (spec 15.3).
//!
//! Spec 15.7's "Alpha conversion" paragraph is five sentences and this file is those five
//! sentences as data, so that a blend state is never typed in beside a pipeline and left to
//! drift. Two of them are load-bearing in a way the prose does not make obvious:
//!
//!  * **"Assets already marked premultiplied are not multiplied again."** A second multiply
//!    is not a wrong-looking image — it is a *darker* image, and a darker image is what an
//!    author with a low-opacity layer expects to see anyway. That is why
//!    [`premultiply_in_shader`] is a total function over [`AlphaMode`] and why the golden
//!    compares a `Straight` and a `Premultiplied` asset holding the *same* pixels.
//!  * **"Zero-alpha standard-blend output contributes no color."** With `One`/`OneMinusSrcAlpha`
//!    that is arithmetic, not policy: `rgb * 0 = 0`. It stops being arithmetic at the
//!    straight-alpha boundary, where a zero-alpha pixel has no defined RGB at all, so the
//!    value in [`ZERO_ALPHA_RGB`] is a decision and ADR-0010 is where it is recorded.
//!
//! The straight-to-premultiplied multiply itself happens in linear light, after the decode
//! — 15.7 says "decoded to linear and premultiplied as `rgb_premul = rgb_linear * alpha`",
//! and the order is not stylistic: multiplying gamma-encoded values and then decoding
//! attenuates the wrong quantity, which darkens a 50 % layer to something like 21 %.

use anim_core::id::NodeId;

use crate::RenderError;

/// Spec 15.8's `AlphaMode`, verbatim.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlphaMode {
    Opaque,
    AlphaTest { cutoff: f32 },
    Straight,
    Premultiplied,
    AdditivePremultiplied,
}

impl AlphaMode {
    /// Whether this mode's color must be scaled by its alpha in the shader.
    ///
    /// Total, with no `_` arm: `Premultiplied` and `AdditivePremultiplied` are *already*
    /// scaled and multiplying them again is 15.7's named prohibition, and a `None`-style
    /// fallback would let a new variant take the wrong branch silently.
    #[must_use]
    pub const fn premultiply_in_shader(self) -> bool {
        matches!(self, Self::Straight)
    }

    /// Whether the pass discards below a cutoff before writing any attachment — 15.7's
    /// "alpha-test discards below cutoff before attachment writes", with "before attachment
    /// writes" doing the work: a discarded fragment writes no depth either, so an
    /// alpha-tested sprite cannot leave a depth hole shaped like its bounding quad.
    #[must_use]
    pub const fn alpha_test_cutoff(self) -> Option<f32> {
        match self {
            Self::AlphaTest { cutoff } => Some(cutoff),
            _ => None,
        }
    }

    /// Whether this mode needs a blend state at all, or an overwrite.
    #[must_use]
    pub const fn blends(self) -> bool {
        !matches!(self, Self::Opaque | Self::AlphaTest { .. })
    }

    /// The lowercase spelling recorded in the pass log and in ADR-0010's convention table.
    ///
    /// A stable name rather than `{:?}` because the pass log is a gate artifact (15.12) and
    /// a debug format is not a contract.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opaque => "opaque",
            Self::AlphaTest { .. } => "alpha-test",
            Self::Straight => "straight",
            Self::Premultiplied => "premultiplied",
            Self::AdditivePremultiplied => "additive-premultiplied",
        }
    }

    /// Validate an alpha-test cutoff.
    ///
    /// The predicate 15.11 leaves unstated: finite and in `[0, 1]`. Outside that range the
    /// comparison still runs and still produces pixels, which is the reason it has to be
    /// refused rather than clamped — a clamped `cutoff = 7.0` draws as `1.0` and looks like
    /// an invisible layer the author can find no explanation for.
    pub fn validate(self, entity: NodeId) -> Result<Self, RenderError> {
        if let Self::AlphaTest { cutoff } = self {
            if !cutoff.is_finite() || !(0.0..=1.0).contains(&cutoff) {
                return Err(RenderError::InvalidAlphaCutoff {
                    entity,
                    cutoff: if cutoff.is_finite() {
                        cutoff
                    } else {
                        // NaN in the message would print as "NaN" and hide which of the two
                        // refusals fired, so the non-finite case is reported as itself.
                        f32::NAN
                    },
                });
            }
        }
        Ok(self)
    }
}

/// The RGB a fully transparent pixel carries when the output is converted to straight
/// alpha, where its color is otherwise undefined.
///
/// 15.7 requires "zero-alpha RGB explicitly defined" and does not define it. Zero is the
/// value that makes the conversion its own inverse: `premul = straight * alpha` gives zero
/// at `alpha = 0` for any straight color, so dividing back out is what needs a rule, and a
/// rule of zero keeps `premul -> straight -> premul` a fixed point. Any non-zero choice
/// would mean a round trip through a straight-alpha codec changes the image, which is a
/// defect that would surface as a colour fringing report two packages later.
pub const ZERO_ALPHA_RGB: [f32; 3] = [0.0, 0.0, 0.0];

/// Which auxiliary attachments a pass may write. Spec 15.6's second paragraph, as one type.
///
/// This is the rule that keeps the forward graph honest about what the post chain can
/// assume: 15.6 says transparent objects "do not write depth, normals or velocity, and post
/// effects document the resulting limitation", so the limitation has to be *attached to the
/// pass* rather than remembered by whoever writes the next shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuxWrites {
    /// HDR color, depth, normals and velocity. The opaque and alpha-test passes.
    All,
    /// HDR color only; depth is read, never written. The ordered 2D and transparent passes.
    ColorOnly,
}

/// The blend state 15.7 names for each mode, or `None` when the pass writes through.
#[must_use]
pub fn blend_descriptor(mode: AlphaMode) -> Option<wgpu::BlendState> {
    match mode {
        AlphaMode::Opaque | AlphaMode::AlphaTest { .. } => None,
        // "Standard premultiplied-over uses source One and destination OneMinusSrcAlpha for
        // color and alpha." Both channels `One`/`OneMinusSrcAlpha`, so an accumulated alpha
        // behaves the way the color does instead of reporting a coverage the blend ignored.
        AlphaMode::Straight | AlphaMode::Premultiplied => Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        }),
        // "Additive premultiplied uses source One, destination One; shader RGB already
        // includes coverage and intensity." Alpha also adds, so an additive stack reports
        // growing coverage rather than saturating at the first layer's.
        AlphaMode::AdditivePremultiplied => Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        }),
    }
}

/// Which of 15.6's three content classes one blended item belongs to.
///
/// The spec distinguishes them by what they *are* — "ordered unlit 2D content" versus
/// "transparent items" that "depth-test" — and nothing in the authored data says which is
/// which, because there is no material type yet (WP12's components and WP14's chain
/// parameters are where one starts to exist). So the routing is derived from the one depth
/// signal the snapshot does carry: **an item with no authored depth is flat 2D content and
/// is composited in layer order; an item with a non-zero authored `z` has depth and is
/// therefore depth-tested and depth-sorted.** `z` is exact `f64` from 13.8, so "zero" is one
/// comparison and not a tolerance.
///
/// The consequence for the gate is that all three passes have real inputs today, and the
/// consequence for WP12/WP14 is that they replace this function, not the passes it feeds.
#[must_use]
pub fn classify(alpha: AlphaMode, authored_z: f64) -> crate::graph::PassKind {
    use crate::graph::PassKind;
    match alpha {
        AlphaMode::Opaque | AlphaMode::AlphaTest { .. } => PassKind::Opaque,
        AlphaMode::Straight | AlphaMode::Premultiplied | AlphaMode::AdditivePremultiplied => {
            if authored_z == 0.0 {
                PassKind::Ordered2d
            } else {
                PassKind::Transparent
            }
        }
    }
}

/// What each pass may write, as one lookup rather than five shader comments.
#[must_use]
pub const fn aux_writes(pass: crate::graph::PassKind) -> AuxWrites {
    match pass {
        crate::graph::PassKind::Opaque => AuxWrites::All,
        crate::graph::PassKind::Ordered2d
        | crate::graph::PassKind::Transparent
        | crate::graph::PassKind::Post { .. }
        | crate::graph::PassKind::ToneMap
        | crate::graph::PassKind::Grade
        | crate::graph::PassKind::Output => AuxWrites::ColorOnly,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    /// The RFC 4122 DNS namespace every fixture in this workspace derives from.
    fn namespace() -> uuid::Uuid {
        uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn test_node(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    #[test]
    fn only_straight_is_multiplied_in_the_shader() {
        // 15.7's prohibition, tested as a partition: exactly one mode multiplies, and the
        // two premultiplied modes are on the other side of it.
        assert!(AlphaMode::Straight.premultiply_in_shader());
        for mode in [
            AlphaMode::Opaque,
            AlphaMode::AlphaTest { cutoff: 0.5 },
            AlphaMode::Premultiplied,
            AlphaMode::AdditivePremultiplied,
        ] {
            assert!(
                !mode.premultiply_in_shader(),
                "{mode:?} must not be multiplied again"
            );
        }
    }

    #[test]
    fn the_two_standard_modes_share_one_descriptor_and_differ_only_in_the_multiply() {
        let straight = blend_descriptor(AlphaMode::Straight).unwrap_or_default();
        let premul = blend_descriptor(AlphaMode::Premultiplied).unwrap_or_default();
        assert_eq!(straight, premul, "15.7 gives both One/OneMinusSrcAlpha");
        for component in [straight.color, straight.alpha] {
            assert_eq!(component.src_factor, wgpu::BlendFactor::One);
            assert_eq!(component.dst_factor, wgpu::BlendFactor::OneMinusSrcAlpha);
        }
    }

    #[test]
    fn additive_uses_one_and_one_for_color_and_alpha() {
        let additive = blend_descriptor(AlphaMode::AdditivePremultiplied).unwrap_or_default();
        assert_eq!(additive.color.src_factor, wgpu::BlendFactor::One);
        assert_eq!(additive.color.dst_factor, wgpu::BlendFactor::One);
        assert_eq!(additive.alpha.dst_factor, wgpu::BlendFactor::One);
    }

    #[test]
    fn opaque_and_alpha_test_write_through_rather_than_blend() {
        assert!(blend_descriptor(AlphaMode::Opaque).is_none());
        assert!(blend_descriptor(AlphaMode::AlphaTest { cutoff: 0.5 }).is_none());
        // ...and the cutoff is available to the shader, which is the only place a discard
        // can happen before attachment writes.
        assert_eq!(
            AlphaMode::AlphaTest { cutoff: 0.375 }.alpha_test_cutoff(),
            Some(0.375)
        );
        assert_eq!(AlphaMode::Straight.alpha_test_cutoff(), None);
    }

    #[test]
    fn a_cutoff_outside_the_unit_interval_is_refused_not_clamped() {
        let entity = anim_core::id::NodeId::derive(
            &anim_core::id::DERIVED_NAMESPACE,
            b"anim_render/test/alpha-cutoff",
        );
        for cutoff in [-0.5f32, 1.5, 2.0, f32::NAN, f32::INFINITY] {
            let mode = AlphaMode::AlphaTest { cutoff };
            let err = mode.validate(entity).err();
            assert!(err.is_some(), "{cutoff} must be refused, not clamped");
            match err {
                Some(RenderError::InvalidAlphaCutoff { cutoff: reported, .. }) => {
                    if cutoff.is_finite() {
                        assert_eq!(reported, cutoff, "the refusal names the value");
                    }
                }
                Some(other) => panic!("expected a cutoff refusal, got {other}"),
                None => unreachable!("asserted above"),
            }
        }
        for cutoff in [0.0f32, 0.5, 1.0] {
            assert!(
                AlphaMode::AlphaTest { cutoff }.validate(entity).is_ok(),
                "{cutoff} is inside the range the comparison means"
            );
        }
    }

    /// The flat-depth rule that gives all three of 15.6's content classes a real input,
    /// including the boundary: `z = 0` is 2D, `z = 0.0001` is depth-sorted.
    #[test]
    fn a_blended_item_at_zero_depth_is_ordered_2d_and_any_other_depth_is_transparent() {
        use crate::graph::PassKind;
        assert_eq!(
            classify(AlphaMode::Straight, 0.0),
            PassKind::Ordered2d,
            "15.6: ordered unlit 2D content follows project layer order"
        );
        assert_eq!(classify(AlphaMode::Straight, 1.0), PassKind::Transparent);
        assert_eq!(classify(AlphaMode::Straight, -1.0), PassKind::Transparent);
        assert_eq!(
            classify(AlphaMode::Straight, f64::MIN),
            PassKind::Transparent,
            "an extreme depth is still a depth"
        );
        for alpha in [
            AlphaMode::Opaque,
            AlphaMode::AlphaTest { cutoff: 0.5 },
        ] {
            assert_eq!(classify(alpha, 0.0), PassKind::Opaque, "{alpha:?} is forward");
            assert_eq!(classify(alpha, 400.0), PassKind::Opaque);
        }
    }

    #[test]
    fn only_the_forward_opaque_pass_writes_auxiliaries() {
        use crate::graph::PassKind;
        assert_eq!(aux_writes(PassKind::Opaque), AuxWrites::All);
        for pass in [PassKind::Ordered2d, PassKind::Transparent, PassKind::Output] {
            assert_eq!(
                aux_writes(pass),
                AuxWrites::ColorOnly,
                "{pass:?} must not write normals or velocity: 15.6 says so, and the post \
                 chain's limitation statement depends on it"
            );
        }
    }

    #[test]
    fn the_zero_alpha_rule_is_a_fixed_point_of_the_conversion_it_defines() {
        // premul = straight * alpha at alpha 0 is zero for every straight color, so the
        // only choice that survives a round trip through a straight-alpha codec is zero.
        for straight in [[1.0, 0.0, 0.0], [0.2, 0.7, 0.9], [0.0, 0.0, 0.0]] {
            let premul = [
                straight[0] * 0.0,
                straight[1] * 0.0,
                straight[2] * 0.0,
            ];
            assert_eq!(premul, ZERO_ALPHA_RGB);
        }
    }
}
