//! Authored simulation settings (spec 14.2, 14.9).
//!
//! Spec 14.2 lists `ProjectDocument.simulation.ticks_per_frame` as a WP05 prerequisite and
//! 14.8's `SimulationClock` makes the tick rate part of the simulation state, but spec
//! 12.8 declares six `ProjectDocument` fields and none of them is a simulation section. So
//! the settings WP05 was told to require did not exist, and the question the package had to
//! answer first was where they come from.
//!
//! **They are authored project data.** The alternative — a rate the caller supplies at
//! process start — was rejected on 14.12's own criteria rather than on taste. Two of those
//! criteria are "changing checkpoint cadence or warm-up chunk size cannot change the
//! exported state" and "compare sequential execution with seeks to every tick"; both are
//! statements about one saved revision replaying to one state. A per-session rate makes
//! that false by construction: the same `Revision` stepped at 4 ticks per frame and at 8
//! produces different worlds, and nothing in the file says which was meant. `scheduled_tick
//! = ceil(t * simulation_rate)` (14.7) is derived from the same number, so an event's tick
//! placement — and therefore every subframe spawn age — moves with it too.
//!
//! The consequence is that this is a data-format change, so it arrived as
//! `PROJECT_SCHEMA_VERSION` 2 → 3 with one step in [`crate::migrate::chain`] and a
//! committed version 2 fixture, exactly as WP04's event section did.
//!
//! The seed lives here as well, for the same reason 10.8's no-entropy rule forces it:
//! 14.7 derives every `StreamKey` from "project seed", so a seed generated at open would
//! make a saved project's random streams unreproducible from its own bytes. It is authored,
//! persisted, and never substituted for — 14.11 names silent seed substitution as an error
//! this package must not commit.
//!
//! What is deliberately **not** here is the frame rate. It is
//! [`crate::document::ProjectDocument::frame_rate`], and copying it into this section would
//! give one timeline two rate bindings that can disagree, which is the failure
//! [`crate::event`] refuses for an audio cue's asset for exactly that reason. The
//! simulation clock is built from this section *plus* the document's rate by the consumer
//! that holds both.

use anim_core::hash::CanonicalBytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ProjectError, Result};

/// The tick rate a document this build creates starts with.
///
/// One tick per frame, which is the conservative choice: it is the only nonzero value that
/// cannot make a scene replay slower than authored, and an operator raising it is an
/// explicit, persisted edit rather than something a new project outgrows silently.
pub const DEFAULT_TICKS_PER_FRAME: u32 = 1;

/// The width of [`SimulationSettings::project_seed`], spelled once.
///
/// Spec 14.8's `StreamKey.project_seed` is `[u8; 32]`, and 14.7 hashes the seed into that
/// key, so the width is the BLAKE3 digest width this engine already uses everywhere —
/// not a size chosen here.
pub const PROJECT_SEED_BYTES: usize = 32;

/// The seed a brand-new document carries: all zero, a value and not an absence.
///
/// This is not 14.11's forbidden "silently substitute a seed". Substitution would be
/// picking something else when the data named a seed; a project that has never been saved
/// names nothing, and zero is the stable, reportable answer that makes two fresh documents
/// with one id and one rate identical in every stream. It is visible in the file, an
/// operator changes it by authoring, and no code path replaces it behind their back.
pub const INITIAL_PROJECT_SEED: [u8; PROJECT_SEED_BYTES] = [0u8; PROJECT_SEED_BYTES];

/// The persisted simulation configuration: what a saved project says about how it steps.
///
/// Two fields, both load-bearing for identity. `#[derive(PartialEq, Eq)]` is honest here
/// because a `[u8; 32]` and a nonzero-checked `u32` have no alternate spellings of one
/// value, which is the test DECISIONS.md #22 applies when it decides whether a field has to
/// be private; the validation that `ticks_per_frame` is nonzero runs in
/// [`SimulationSettings::validate`] and again in the hand-written `Deserialize` below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimulationSettings {
    /// Ticks simulated per timeline frame. Nonzero — 14.2's requirement, and the reason
    /// `rate` in the clock is `frame_rate * ticks_per_frame` rather than a constant.
    pub ticks_per_frame: u32,
    /// The project's random-stream seed (spec 14.7, 14.8).
    ///
    /// Persisted as exactly 64 lowercase hex characters, the same single-text-spelling rule
    /// [`anim_assets::ContentHash`] follows. A tolerant parser would accept a spelling this
    /// build would not write, which is two byte streams for one revision.
    pub project_seed: [u8; PROJECT_SEED_BYTES],
}

impl SimulationSettings {
    /// The settings for a document this build creates.
    #[must_use]
    pub const fn for_new_document() -> Self {
        Self {
            ticks_per_frame: DEFAULT_TICKS_PER_FRAME,
            project_seed: INITIAL_PROJECT_SEED,
        }
    }

    /// Author explicit settings, refusing an illegal rate rather than storing it.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] if `ticks_per_frame` is zero.
    pub fn new(ticks_per_frame: u32, project_seed: [u8; PROJECT_SEED_BYTES]) -> Result<Self> {
        let settings = Self {
            ticks_per_frame,
            project_seed,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// The rule 14.2 states: `ticks_per_frame` is a nonzero `u32`.
    ///
    /// Checked on construction, on every load, and by `ProjectDocument::validate`, because
    /// the field is public and a caller that assigns to it directly must not be able to put
    /// a document into a state whose simulation has no step duration.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] naming the field.
    pub fn validate(&self) -> Result<()> {
        if self.ticks_per_frame == 0 {
            return Err(ProjectError::Validation {
                location: "simulation.ticks_per_frame".to_string(),
                detail: "is zero; the simulation rate is frame_rate * ticks_per_frame, so a \
                        zero rate has no step duration and no tick an event could schedule \
                        to (spec 14.2)"
                    .to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) {
        cp.u32(self.ticks_per_frame);
        cp.fixed_bytes(&self.project_seed);
    }

    /// The canonical field stream of this section, with no domain tag.
    ///
    /// ADR-0005 Decision 8's rule as every project object follows it: a command payload
    /// that carries these settings hashes this stream rather than a second rendering.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        cp.into_vec()
    }

    /// The seed as the one text form the file uses.
    #[must_use]
    pub fn seed_hex(&self) -> String {
        encode_seed(&self.project_seed)
    }
}

impl Default for SimulationSettings {
    fn default() -> Self {
        Self::for_new_document()
    }
}

/// The JSON shape: an object with the two members, the seed as a hex string.
///
/// Written by hand rather than derived so the member order is this declaration order and
/// the seed's spelling has exactly one producer. ADR-0005 Decision 9 makes a struct's
/// declaration order the persisted order, and `SimulationSettings`' declaration order is
/// `ticks_per_frame` then `project_seed` because 14.2 names the rate and 14.7 only then
/// reaches for a seed.
impl Serialize for SimulationSettings {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut state = serializer.serialize_struct("SimulationSettings", 2)?;
        state.serialize_field("ticks_per_frame", &self.ticks_per_frame)?;
        state.serialize_field("project_seed", &self.seed_hex())?;
        state.end()
    }
}

/// Reads the same two members and refuses what the constructors refuse.
///
/// Hand-written on #22's reasoning: a derived impl would fill the public fields straight
/// from disk, so a `project.anim.json` could carry `ticks_per_frame: 0` or an uppercase or
/// hyphenated seed and be read as though it had been authored by this build.
impl<'de> Deserialize<'de> for SimulationSettings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            ticks_per_frame: u32,
            #[serde(deserialize_with = "deserialize_seed_hex")]
            project_seed: [u8; PROJECT_SEED_BYTES],
        }
        let raw = Raw::deserialize(deserializer)?;
        let settings = Self {
            ticks_per_frame: raw.ticks_per_frame,
            project_seed: raw.project_seed,
        };
        settings.validate().map_err(serde::de::Error::custom)?;
        Ok(settings)
    }
}

fn encode_seed(seed: &[u8; PROJECT_SEED_BYTES]) -> String {
    let mut out = String::with_capacity(PROJECT_SEED_BYTES * 2);
    for byte in seed {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

const fn hex_digit(nibble: u8) -> char {
    // Written as a table rather than `char::from_digit` because the latter is not `const`,
    // and this runs in `seed_hex` on a path that must not allocate a formatter.
    if nibble < 10 {
        (b'0' + nibble) as char
    } else {
        (b'a' + nibble - 10) as char
    }
}

fn deserialize_seed_hex<'de, D>(
    deserializer: D,
) -> std::result::Result<[u8; PROJECT_SEED_BYTES], D::Error>
where
    D: Deserializer<'de>,
{
    let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
    decode_seed(&raw).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "project seed must be exactly {} lowercase hex characters, got {raw:?}",
            PROJECT_SEED_BYTES * 2
        ))
    })
}

/// The strict decode: exact length, all lowercase hex, nothing else.
///
/// Uppercase is refused rather than folded, for the reason `ContentHash::from_hex` gives:
/// a parser that accepts a spelling the writer would not produce lets one state have two
/// files, and one revision then has two digests.
fn decode_seed(raw: &str) -> Option<[u8; PROJECT_SEED_BYTES]> {
    if raw.len() != PROJECT_SEED_BYTES * 2 {
        return None;
    }
    let mut seed = [0u8; PROJECT_SEED_BYTES];
    let bytes = raw.as_bytes();
    for (index, pair) in bytes[..].chunks(2).enumerate() {
        let high = nibble(pair[0])?;
        let low = nibble(pair[1])?;
        seed[index] = (high << 4) | low;
    }
    Some(seed)
}

const fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
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

    fn seed(value: u8) -> [u8; PROJECT_SEED_BYTES] {
        [value; PROJECT_SEED_BYTES]
    }

    #[test]
    fn a_zero_tick_rate_is_refused_by_every_route_in() {
        // 14.2's "nonzero u32" is worth nothing if one of the three ways a value arrives
        // skips it: the constructor, the assignment path that goes through validate, and
        // the load path that goes through the hand-written Deserialize.
        assert!(SimulationSettings::new(0, seed(0)).is_err());
        let mut settings = SimulationSettings::for_new_document();
        settings.ticks_per_frame = 0;
        assert!(settings.validate().is_err());
        let json = br#"{"ticks_per_frame":0,"project_seed":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
        assert!(serde_json::from_slice::<SimulationSettings>(json).is_err());
    }

    #[test]
    fn the_seed_has_exactly_one_text_spelling_in_each_direction() {
        let settings = SimulationSettings::new(4, seed(0xab)).unwrap();
        assert_eq!(
            settings.seed_hex(),
            "ab".repeat(PROJECT_SEED_BYTES),
            "every byte 0xab is the two characters `ab`, lower case, no separators"
        );
        let text = String::from_utf8(serde_json::to_vec(&settings).unwrap()).unwrap();
        assert!(text.contains(&format!("\"project_seed\":\"{}\"", settings.seed_hex())));

        let lowercase = "ab".repeat(PROJECT_SEED_BYTES);
        for wrong in [
            "AB".repeat(PROJECT_SEED_BYTES),
            lowercase[..lowercase.len() - 2].to_string(),
            "g".repeat(PROJECT_SEED_BYTES * 2),
            lowercase[..lowercase.len() - 1].to_string(),
        ] {
            let json = format!("{{\"ticks_per_frame\":4,\"project_seed\":\"{wrong}\"}}");
            assert!(
                serde_json::from_str::<SimulationSettings>(&json).is_err(),
                "a seed this build would not write is refused, not normalized: {wrong}"
            );
        }
    }

    #[test]
    fn a_seed_round_trips_byte_for_byte_through_the_documents_form() {
        let settings = SimulationSettings::new(8, seed(0x5c)).unwrap();
        let bytes = serde_json::to_vec(&settings).unwrap();
        let loaded: SimulationSettings = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded, settings);
        assert_eq!(serde_json::to_vec(&loaded).unwrap(), bytes);
    }

    #[test]
    fn the_json_member_order_is_the_declaration_order() {
        let text =
            String::from_utf8(serde_json::to_vec(&SimulationSettings::for_new_document()).unwrap())
                .unwrap();
        let rate = text.find("ticks_per_frame").unwrap();
        let seed_at = text.find("project_seed").unwrap();
        assert!(
            rate < seed_at,
            "14.2 names the rate first and 14.7 reaches for a seed only after it, so the \
             persisted order follows that: {text}"
        );
    }

    #[test]
    fn the_canonical_stream_is_rebuilt_by_hand_from_adr_0004_s_layout() {
        // The oracle is written from the layout rule, not read off `encode`, because an
        // expectation produced by the code under test proves only self-consistency — the
        // defect class DECISIONS.md #32 lists twice now.
        let settings = SimulationSettings::new(3, seed(0x7f)).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&3u32.to_le_bytes());
        expected.extend_from_slice(&[0x7fu8; PROJECT_SEED_BYTES]);
        assert_eq!(settings.canonical_bytes(), expected);
    }

    #[test]
    fn a_new_documents_seed_is_a_value_rather_than_an_absence() {
        let settings = SimulationSettings::for_new_document();
        assert_eq!(settings.project_seed, INITIAL_PROJECT_SEED);
        assert_eq!(settings.ticks_per_frame, DEFAULT_TICKS_PER_FRAME);
        settings.validate().expect("the defaults are valid");
        assert_eq!(
            SimulationSettings::new(DEFAULT_TICKS_PER_FRAME, INITIAL_PROJECT_SEED).unwrap(),
            settings,
            "the default is an authored value two fresh documents agree on, which is what \
             keeps it from being 14.11's substituted seed"
        );
    }
}
