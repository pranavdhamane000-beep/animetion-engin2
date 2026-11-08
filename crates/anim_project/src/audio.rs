//! Authored audio clips (spec 17.2, 17.8).
//!
//! Spec 12.3 lists no file like this one, for the same reason [`crate::event`] and
//! [`crate::simulation`] exist: WP08's prerequisites (17.2) name authored clip semantics —
//! timeline range, source offset, loop, gain, fades, mute and channel — and **no section of
//! the spec defines a type that carries them**. `AudioCue` (`anim_scene::snapshot`) is a
//! point event `(occurrence_id, node, position, asset)`, `TrackData` is a property-keyframe
//! track, and even WP16's `ContactMarker.audio_cue` is an `EventDefinitionId`. So the
//! authored half lives here, in the crate that owns authored state, and `anim_scene`
//! exposes it through the snapshot because 17.4 lets `anim_audio` depend only on
//! `anim_core`, `anim_assets` and `anim_scene` — not on this crate. Ruled in DECISIONS.md
//! #46 ruling 1; recorded as a deviation row in `BUILD_STATE.md`.
//!
//! Every value here is exact. The timeline bounds are [`FramePos`] (frame plus a normalized
//! subframe), the source offset, gain and fades are [`Rational`], and nothing is an `f64`:
//! 17.6 forbids floating-point seconds from determining clip placement, and a gain or fade
//! authored as a float would be a second, platform-dependent spelling of one authored
//! value. The conversion to samples and to `f64` mix gains is WP08's arithmetic, done once
//! at the consumer against the project's sample rate, exactly as 13.10 does for cues.

use anim_core::hash::CanonicalBytes;
use anim_core::id::{AssetId, TrackId};
use anim_core::rational::Rational;
use anim_core::time::FramePos;
use serde::{Deserialize, Serialize};

use crate::{ProjectError, Result};

/// The channel layout of one clip's source (spec 17.7's channel conversion).
///
/// Closed, and deliberately only the two layouts 17.7 names a rule for — "Convert mono to
/// stereo using the declared gain; preserve stereo; reject unsupported channel layouts
/// until explicitly added." A third variant would be authoring a conversion rule WP08 has
/// not been given, so the enum stays at two and the refusal lives in the mixer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    /// One channel, up-mixed to stereo by the mixer's declared gain.
    Mono,
    /// Two channels, preserved.
    Stereo,
}

impl ChannelLayout {
    /// The canonical tag: the lowercase name JSON writes, with no table between the two
    /// (the rule `EventKind::as_str` and `Easing::as_str` follow).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Stereo => "stereo",
        }
    }

    /// The canonical byte tag: a `u8`, `0` for mono and `1` for stereo. A fixed-width
    /// discriminant rather than the text form, because the layout is closed and a length
    /// prefix would only let a malformed length disagree with a one-byte value.
    pub(crate) fn encode(self, cp: &mut CanonicalBytes) {
        cp.u8(match self {
            Self::Mono => 0,
            Self::Stereo => 1,
        });
    }
}

/// One authored audio clip: a source asset placed on the project timeline with the exact
/// semantics spec 17.2 lists and 17.8's `PlannedClip` consumes.
///
/// `clip_id` is a [`TrackId`] because 17.8 types `PlannedClip.clip_id` that way — reusing
/// the typed id rather than inventing a ninth one (spec 10.8 lists seven, and WP04 already
/// added the eighth for events). The collection is keyed by it, so storage order is
/// id-byte order and 17.5's plan order `(timeline_start, clip_id)` has a stable tie-break.
///
/// The fields are in declaration order, which is the order [`AudioClip::encode`] writes
/// and the order the JSON serializer emits — the same rule every persisted object in this
/// crate follows (ADR-0005 Decision 9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioClip {
    /// Identity, generated once and copied through save, load and undo (spec 10.8). The
    /// value 17.5's plan sorts by as its second term.
    pub clip_id: TrackId,
    /// The source asset whose PCM this clip plays. Existence is a document-level question
    /// (the manifest lives on the document, not the scene), so it is checked by WP08's
    /// `compile_plan` against the resolver, not here.
    pub asset_id: AssetId,
    /// Where the clip begins on the project timeline, exact (frame plus subframe).
    pub timeline_start: FramePos,
    /// Where the clip ends, exclusive. Must be strictly after `timeline_start`: an empty
    /// or reversed range is not a clip, and 17.7 rejects empty ranges at the export
    /// boundary for the same reason.
    pub timeline_end: FramePos,
    /// How far into the source asset playback begins, in exact seconds. Nonnegative;
    /// 17.7's source mapping adds the output-relative offset to this value.
    pub source_offset: Rational,
    /// Whether the source repeats for the whole timeline range (17.7's checked modulo by
    /// the canonical source length). A nonlooping source that runs out contributes silence.
    pub looped: bool,
    /// Exact linear gain applied to the source. A [`Rational`] is always finite, so 17.7's
    /// "finite gains" validation is satisfied by the type; the conversion to the mixer's
    /// `f64` gain is the consumer's single final numeric operation.
    pub gain: Rational,
    /// Fade-in duration in exact seconds, applied from the timeline start. Nonnegative.
    pub fade_in: Rational,
    /// Fade-out duration in exact seconds, applied up to the timeline end. Nonnegative.
    pub fade_out: Rational,
    /// Whether the clip is silenced. A muted clip is still authored and still planned;
    /// the mixer applies zero gain rather than the plan dropping it, so an unmute is a
    /// gain change and not a re-insertion.
    pub mute: bool,
    /// The source's channel layout, which decides the mixer's conversion rule.
    pub channels: ChannelLayout,
}

impl AudioClip {
    /// A new clip from its authored fields.
    #[must_use]
    pub fn new(
        clip_id: TrackId,
        asset_id: AssetId,
        timeline_start: FramePos,
        timeline_end: FramePos,
        source_offset: Rational,
        looped: bool,
        gain: Rational,
        fade_in: Rational,
        fade_out: Rational,
        mute: bool,
        channels: ChannelLayout,
    ) -> Self {
        Self {
            clip_id,
            asset_id,
            timeline_start,
            timeline_end,
            source_offset,
            looped,
            gain,
            fade_in,
            fade_out,
            mute,
            channels,
        }
    }

    /// Every rule this type exists to hold.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Validation`] naming this clip and the rule it broke: a timeline
    /// range that is empty or reversed (`start < end` is required, compared through
    /// [`FramePos`]'s `Ord`, which is time order because the subframe is normalized), or a
    /// negative source offset, fade-in or fade-out. Gain needs no check: a [`Rational`]
    /// cannot be nonfinite. Asset existence is deliberately not checked here — the
    /// manifest is document-level, and WP08's `compile_plan` is what resolves sources.
    pub fn validate(&self) -> Result<()> {
        if self.timeline_start >= self.timeline_end {
            return Err(ProjectError::Validation {
                location: format!("audio clip {}", self.clip_id),
                detail: format!(
                    "has timeline range [{}, {}), which is empty or reversed; a clip must \
                     span a strictly positive range",
                    self.timeline_start.frame, self.timeline_end.frame
                ),
            });
        }
        let zero = Rational::from_i128(0);
        for (field, value) in [
            ("source_offset", &self.source_offset),
            ("fade_in", &self.fade_in),
            ("fade_out", &self.fade_out),
        ] {
            if *value < zero {
                return Err(ProjectError::Validation {
                    location: format!("audio clip {}", self.clip_id),
                    detail: format!(
                        "has a negative {field} ({value}); durations and offsets are nonnegative"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Encode into a caller's builder, in declaration order.
    ///
    /// `pub` rather than `pub(crate)` because `anim_scene`'s snapshot calls it cross-crate:
    /// the evaluated clip *is* the authored clip (clip evaluation is immutable exposure,
    /// with no per-position computation), so the snapshot hashes these exact bytes rather
    /// than a second rendering of them. Infallible — there is no `f64` field to refuse.
    pub fn encode(&self, cp: &mut CanonicalBytes) {
        cp.fixed_bytes(self.clip_id.as_bytes());
        cp.fixed_bytes(self.asset_id.as_bytes());
        cp.i64(self.timeline_start.frame);
        cp.rational(&self.timeline_start.subframe());
        cp.i64(self.timeline_end.frame);
        cp.rational(&self.timeline_end.subframe());
        cp.rational(&self.source_offset);
        cp.bool(self.looped);
        cp.rational(&self.gain);
        cp.rational(&self.fade_in);
        cp.rational(&self.fade_out);
        cp.bool(self.mute);
        self.channels.encode(cp);
    }

    /// The clip's canonical field stream, with no domain tag, so a caller that hashes it
    /// adds the tag itself (ADR-0005 Decision 8's rule, as every `canonical_bytes` here
    /// follows).
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut cp = CanonicalBytes::new();
        self.encode(&mut cp);
        cp.into_vec()
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
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn track_id(label: &str) -> TrackId {
        TrackId::derive(&namespace(), label.as_bytes())
    }

    fn asset_id(label: &str) -> AssetId {
        AssetId::derive(&namespace(), label.as_bytes())
    }

    fn r(num: i128, den: i128) -> Rational {
        Rational::new(num, den).unwrap()
    }

    /// A clip with subframe timeline bounds and nonzero exact values, so the round trip
    /// and the byte oracle exercise every field rather than a row of zeros.
    fn clip() -> AudioClip {
        AudioClip::new(
            track_id("music"),
            asset_id("score"),
            FramePos::new(2, r(1, 4)).unwrap(),
            FramePos::new(10, r(1, 2)).unwrap(),
            r(3, 2),
            true,
            r(4, 5),
            r(1, 8),
            r(1, 4),
            false,
            ChannelLayout::Stereo,
        )
    }

    #[test]
    fn a_valid_clip_round_trips_through_json_and_reproduces_its_bytes() {
        let clip = clip();
        clip.validate().expect("the fixture clip is legal");
        let bytes = serde_json::to_vec(&clip).unwrap();
        let loaded: AudioClip = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded, clip);
        assert_eq!(serde_json::to_vec(&loaded).unwrap(), bytes);
        assert_eq!(loaded.canonical_bytes(), clip.canonical_bytes());
    }

    #[test]
    fn a_reversed_or_empty_timeline_is_refused() {
        let mut reversed = clip();
        reversed.timeline_start = FramePos::at_frame(12);
        reversed.timeline_end = FramePos::at_frame(4);
        assert!(matches!(
            reversed.validate(),
            Err(ProjectError::Validation { .. })
        ));

        let mut empty = clip();
        empty.timeline_start = FramePos::at_frame(4);
        empty.timeline_end = FramePos::at_frame(4);
        assert!(matches!(
            empty.validate(),
            Err(ProjectError::Validation { .. })
        ));

        // The same instant expressed two ways is still empty: `FramePos`'s normalized
        // subframe makes `Ord` time order, so a half-frame start and an equal half-frame
        // end compare equal and are refused.
        let mut subframe_empty = clip();
        subframe_empty.timeline_start = FramePos::new(4, r(1, 2)).unwrap();
        subframe_empty.timeline_end = FramePos::new(4, r(1, 2)).unwrap();
        assert!(subframe_empty.validate().is_err());

        // But a strictly positive subframe range is legal.
        let mut subframe_ok = clip();
        subframe_ok.timeline_start = FramePos::new(4, r(1, 4)).unwrap();
        subframe_ok.timeline_end = FramePos::new(4, r(3, 4)).unwrap();
        assert!(subframe_ok.validate().is_ok());
    }

    #[test]
    fn negative_source_offset_and_fades_are_refused() {
        for field in ["source_offset", "fade_in", "fade_out"] {
            let mut bad = clip();
            match field {
                "source_offset" => bad.source_offset = r(-1, 2),
                "fade_in" => bad.fade_in = r(-1, 2),
                _ => bad.fade_out = r(-1, 2),
            }
            let Err(ProjectError::Validation { location, detail }) = bad.validate() else {
                panic!("a negative {field} must be refused");
            };
            assert!(location.contains("audio clip"), "{location}");
            assert!(detail.contains(field), "{detail}");
        }
        // Zero is legal for all three: a clip may start at the source head with no fades.
        let mut zeroed = clip();
        zeroed.source_offset = Rational::from_i128(0);
        zeroed.fade_in = Rational::from_i128(0);
        zeroed.fade_out = Rational::from_i128(0);
        assert!(zeroed.validate().is_ok());
    }

    #[test]
    fn the_canonical_layout_is_the_documented_field_order_and_nothing_else() {
        // Rebuilt by hand from ADR-0004's layout rather than read off the encoder: an
        // oracle that calls the code under test proves only self-consistency (DECISIONS
        // #32's rule). Fixed-width ids raw, every variable-width field behind a
        // little-endian u64, rationals as their canonical text, the layout as a u8 tag.
        let clip = clip();
        fn prefixed(value: &str) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
            out
        }
        let mut expected = Vec::new();
        expected.extend_from_slice(clip.clip_id.as_bytes());
        expected.extend_from_slice(clip.asset_id.as_bytes());
        expected.extend_from_slice(&2i64.to_le_bytes());
        expected.extend_from_slice(&prefixed("1/4"));
        expected.extend_from_slice(&10i64.to_le_bytes());
        expected.extend_from_slice(&prefixed("1/2"));
        expected.extend_from_slice(&prefixed("3/2"));
        expected.push(1); // looped
        expected.extend_from_slice(&prefixed("4/5"));
        expected.extend_from_slice(&prefixed("1/8"));
        expected.extend_from_slice(&prefixed("1/4"));
        expected.push(0); // mute
        expected.push(1); // stereo
        assert_eq!(clip.canonical_bytes(), expected);

        // And the bytes depend on nothing incidental: the same values arriving through a
        // JSON object whose members are in a different order encode identically.
        let reordered = serde_json::json!({
            "channels": "stereo",
            "mute": false,
            "fade_out": "1/4",
            "fade_in": "1/8",
            "gain": "4/5",
            "looped": true,
            "source_offset": "3/2",
            "timeline_end": { "frame": 10, "subframe": "1/2" },
            "timeline_start": { "frame": 2, "subframe": "1/4" },
            "asset_id": clip.asset_id.to_canonical_string(),
            "clip_id": clip.clip_id.to_canonical_string(),
        });
        let from_reordered: AudioClip = serde_json::from_value(reordered).unwrap();
        assert_eq!(from_reordered.canonical_bytes(), clip.canonical_bytes());
    }

    #[test]
    fn channel_layout_spellings_agree_between_json_and_bytes() {
        for (layout, spelling, tag) in [
            (ChannelLayout::Mono, "mono", 0u8),
            (ChannelLayout::Stereo, "stereo", 1u8),
        ] {
            assert_eq!(layout.as_str(), spelling);
            assert_eq!(
                serde_json::to_string(&layout).unwrap(),
                format!("\"{spelling}\"")
            );
            let mut cp = CanonicalBytes::new();
            layout.encode(&mut cp);
            assert_eq!(cp.as_slice(), &[tag]);
        }
    }
}
