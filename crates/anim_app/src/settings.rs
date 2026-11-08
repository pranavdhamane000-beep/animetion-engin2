//! Validated memory-policy configuration (spec 16.3).
//!
//! The app's composition layer (WP10) owns where configuration comes from; this module
//! owns the gate it passes before reaching the renderer: a [`MemorySettings`] cannot
//! exist with an invalid policy, because the constructor validates and the fields are
//! private (the DECISIONS.md #22 pattern — no second door into a half-checked value).
//!
//! No persistence lives here. Spec 16 names no settings schema, and inventing one would
//! be a data format WP10 has not asked for; the values travel as a validated struct.

use anim_render::{MemoryError, MemoryMode, MemoryPolicy};

/// A validated memory policy paired with the degradation regime it runs under.
#[derive(Clone, Debug)]
pub struct MemorySettings {
    policy: MemoryPolicy,
    mode: MemoryMode,
}

impl MemorySettings {
    /// Validate and keep a policy/mode pair.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidPolicy`] naming the field that broke — a zero sample count
    /// or a fraction outside `(0, 1]` is refused here rather than at first pressure.
    pub fn new(policy: MemoryPolicy, mode: MemoryMode) -> Result<Self, MemoryError> {
        policy.validate()?;
        Ok(Self { policy, mode })
    }

    /// The spec 16.6 defaults in the fixed-quality regime.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidPolicy`] — unreachable for the defaults, carried because
    /// the constructor is the only door.
    pub fn export_defaults() -> Result<Self, MemoryError> {
        Self::new(MemoryPolicy::default(), MemoryMode::Export)
    }

    /// The spec 16.6 defaults in the degrading preview regime.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidPolicy`] — unreachable for the defaults.
    pub fn preview_defaults() -> Result<Self, MemoryError> {
        Self::new(MemoryPolicy::default(), MemoryMode::Preview)
    }

    /// The validated policy.
    #[must_use]
    pub const fn policy(&self) -> &MemoryPolicy {
        &self.policy
    }

    /// The degradation regime.
    #[must_use]
    pub const fn mode(&self) -> MemoryMode {
        self.mode
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

    #[test]
    fn the_defaults_validate_in_both_regimes() {
        let export = MemorySettings::export_defaults().unwrap();
        let preview = MemorySettings::preview_defaults().unwrap();
        assert_eq!(export.mode(), MemoryMode::Export);
        assert_eq!(preview.mode(), MemoryMode::Preview);
        assert_eq!(export.policy().target_fraction, 0.80);
    }

    #[test]
    fn an_invalid_policy_is_refused_at_the_door() {
        let policy = MemoryPolicy {
            target_fraction: 1.5,
            ..MemoryPolicy::default()
        };
        assert!(MemorySettings::new(policy, MemoryMode::Export).is_err());
        let policy = MemoryPolicy {
            over_budget_samples: 0,
            ..MemoryPolicy::default()
        };
        assert!(MemorySettings::new(policy, MemoryMode::Preview).is_err());
    }

    #[test]
    fn a_validated_pair_keeps_what_it_was_given() {
        let policy = MemoryPolicy {
            safety_reserve_bytes: 512,
            ..MemoryPolicy::default()
        };
        let settings = MemorySettings::new(policy, MemoryMode::Preview).unwrap();
        assert_eq!(settings.policy().safety_reserve_bytes, 512);
        assert_eq!(settings.mode(), MemoryMode::Preview);
    }
}
