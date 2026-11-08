"""One-shot generator for schemas/project/v5.json, derived from v4.json.

Not part of the build, and versioned for the reason `make_v3_schema.py` and
`make_v4_schema.py` state: spec 12.5 step 9 makes the schema files inputs, and "generated
from the previous version, not retyped" has to be repeatable to be a claim. It is also the
check on this bump: the `audio_clip` field set here is derived from what `anim_project`'s
`AudioClip` serializer actually writes, so a schema that disagrees with `audio.rs` is a bug
in one of them and this script makes which-one answerable rather than leaving two
hand-written descriptions of one format.

What version 5 adds is one section per scene — spec 17.2's prerequisite that authored audio
clips carry timeline range, source offset, loop, gain, fades, mute and channel metadata,
ruled in DECISIONS.md #46 ruling 1 to live in `anim_project` and reach `anim_audio` through
the snapshot (17.4 forbids `anim_audio` depending on `anim_project`). The section is keyed
by the clip's `TrackId` (spec 17.8's `PlannedClip.clip_id`), so it joins the id-keyed maps
and its storage order is id-byte order.

Member order is emitted, not sorted: `audio_clips` is last in `SceneData` and in its
`Serialize` impl, so the canonical byte stream appends it and every existing section keeps
its position. `required` lists it because the Rust `Deserialize` has no `serde(default)` for
it — a version 4 scene that omits the key is not a version 5 scene, which is the same
strictness `events` and `simulation` were given, and the reason `V4_TO_V5` writes the key
explicitly instead of relying on a default.

One accuracy correction travels with this bump: v4.json's embedded `manifest.schema_version`
`const` was left at 1 by `make_v4_schema.py` even though a version 4 document embeds a
version 2 manifest (the `image` claims moved `MANIFEST_SCHEMA_VERSION` 1 -> 2). The drift
test checks required-lists and not nested consts, so it never caught the stale number. This
generator sets it to 2 so v5.json describes what the serializer writes; v4.json is kept
unchanged as the frozen description of the files at that version.

Run from the repository root:  python tools/make_v5_schema.py
"""

import collections
import io
import json

Ordered = collections.OrderedDict

with io.open("schemas/project/v4.json", encoding="utf-8") as handle:
    doc = json.load(handle, object_pairs_hook=Ordered)

doc["$id"] = "https://anim-engine.local/schemas/project/v5.json"
doc["title"] = "anim_engine project document, schema version 5"
doc["description"] = (
    "The persisted shape of crates/anim_project's ProjectDocument (spec 12.8) and the types "
    "it embeds, at schema version 5. Hand-authored, and checked against the bytes the Rust "
    "serializers actually write by `the_committed_schema_matches_what_the_serializers_write` "
    "in crates/anim_project/tests/persistence.rs -- that test is the reason this file can be "
    "read without running anything and still be trusted. Version 5 adds the scene-level "
    "`audio_clips` section and the `audio_clip` and `channel_layout` definitions, and "
    "`schemas/project/v4.json` stays as the description of the files that still exist on "
    "disk at that version: `anim_project::migrate::V4_TO_V5` is what carries one into the "
    "other. Acceptance of a real file happens at the typed boundary in anim_project::migrate "
    "and anim_project::schema, not by a validator: spec 5.1 names no JSON Schema dependency, "
    "and a non-canonical rational or a malformed uuid is already refused by its own "
    "constructor. A member's ORDER inside one object is not expressible in JSON Schema and "
    "is not asserted here; it belongs to canonical serialization (ADR-0005, closed by WP03) "
    "and is proven byte-for-byte by anim_project::schema::verify_canonical_bytes."
)

sv = doc["properties"]["schema_version"]
sv["const"] = 5
sv["description"] = (
    "The document version. Independent of assets.schema_version and of the command envelope "
    "version (ADR-0005 Decision 4, Consequence 5). Version 2 is WP04's scene-level events "
    "section; version 3 is WP05's document-level simulation section; version 4 is WP06's "
    "per-asset image color-space and alpha claims; version 5 is WP08's scene-level "
    "audio_clips section, because spec 17.2 makes authored clip semantics a prerequisite of "
    "the offline mixer and no section of the spec defined a type to carry them. The encoding "
    "domain anim_engine/document/v1 did NOT move: appending one section to each scene is a "
    "field addition on an unchanged framing, the split lib.rs states and that versions 2, 3 "
    "and 4 already established. assets.schema_version did not move either (it is 2 from "
    "version 4 on): the clip section lives in the scene, not in the embedded record "
    "collection, so no manifest_digest changes."
)

defs = doc["$defs"]

defs["channel_layout"] = Ordered(
    [
        ("type", "string"),
        ("enum", ["mono", "stereo"]),
        (
            "description",
            "The channel layout of one clip's source, exactly as anim_project::ChannelLayout "
            "spells it: the lowercase string is the persisted form, and the canonical byte "
            "stream writes a u8 tag (0 mono, 1 stereo) rather than the text. Closed at the "
            "two layouts spec 17.7 names a conversion rule for -- mono is up-mixed to stereo "
            "by the declared gain, stereo is preserved -- and a third layout is refused by "
            "the mixer until explicitly added, not authored here.",
        ),
    ]
)

defs["audio_clip"] = Ordered(
    [
        ("type", "object"),
        (
            "required",
            [
                "clip_id",
                "asset_id",
                "timeline_start",
                "timeline_end",
                "source_offset",
                "looped",
                "gain",
                "fade_in",
                "fade_out",
                "mute",
                "channels",
            ],
        ),
        ("additionalProperties", False),
        (
            "properties",
            Ordered(
                [
                    (
                        "clip_id",
                        Ordered(
                            [
                                ("$ref", "#/$defs/uuid"),
                                (
                                    "description",
                                    "The TrackId. Spec 17.8 types PlannedClip.clip_id this way, "
                                    "so the clip reuses the typed id rather than inventing a "
                                    "ninth one, and 17.5's plan order (timeline_start, clip_id) "
                                    "has a stable tie-break.",
                                ),
                            ]
                        ),
                    ),
                    (
                        "asset_id",
                        Ordered(
                            [
                                ("$ref", "#/$defs/uuid"),
                                (
                                    "description",
                                    "The source asset whose PCM the clip plays. Existence is a "
                                    "document-level question (the manifest lives on the "
                                    "document), checked by WP08's compile_plan against the "
                                    "resolver, not by the scene.",
                                ),
                            ]
                        ),
                    ),
                    (
                        "timeline_start",
                        Ordered(
                            [
                                ("$ref", "#/$defs/frame_pos"),
                                ("description", "Where the clip begins, exact (frame plus subframe)."),
                            ]
                        ),
                    ),
                    (
                        "timeline_end",
                        Ordered(
                            [
                                ("$ref", "#/$defs/frame_pos"),
                                (
                                    "description",
                                    "Where the clip ends, exclusive. Must be strictly after "
                                    "timeline_start; AudioClip::validate refuses an empty or "
                                    "reversed range, compared through FramePos's time order.",
                                ),
                            ]
                        ),
                    ),
                    (
                        "source_offset",
                        Ordered(
                            [
                                ("$ref", "#/$defs/rational"),
                                (
                                    "description",
                                    "How far into the source playback begins, in exact seconds. "
                                    "Nonnegative; the nonnegativity is enforced by "
                                    "AudioClip::validate, not by the rational pattern, which "
                                    "spells any canonical rational including negative ones.",
                                ),
                            ]
                        ),
                    ),
                    (
                        "looped",
                        Ordered(
                            [
                                ("type", "boolean"),
                                (
                                    "description",
                                    "Whether the source repeats for the whole timeline range "
                                    "(17.7's checked modulo by the canonical source length). A "
                                    "nonlooping source that runs out contributes silence.",
                                ),
                            ]
                        ),
                    ),
                    (
                        "gain",
                        Ordered(
                            [
                                ("$ref", "#/$defs/rational"),
                                (
                                    "description",
                                    "Exact linear gain. A Rational is always finite, so 17.7's "
                                    "'finite gains' rule is satisfied by the type; the conversion "
                                    "to the mixer's f64 gain is the consumer's single final "
                                    "numeric operation.",
                                ),
                            ]
                        ),
                    ),
                    (
                        "fade_in",
                        Ordered(
                            [
                                ("$ref", "#/$defs/rational"),
                                ("description", "Fade-in duration in exact seconds, nonnegative."),
                            ]
                        ),
                    ),
                    (
                        "fade_out",
                        Ordered(
                            [
                                ("$ref", "#/$defs/rational"),
                                ("description", "Fade-out duration in exact seconds, nonnegative."),
                            ]
                        ),
                    ),
                    (
                        "mute",
                        Ordered(
                            [
                                ("type", "boolean"),
                                (
                                    "description",
                                    "Whether the clip is silenced. A muted clip is still authored "
                                    "and planned; the mixer applies zero gain rather than the plan "
                                    "dropping it, so an unmute is a gain change, not a re-insertion.",
                                ),
                            ]
                        ),
                    ),
                    ("channels", Ordered([("$ref", "#/$defs/channel_layout")])),
                ]
            ),
        ),
        (
            "description",
            "One authored audio clip (anim_project::AudioClip, fields in declaration order). "
            "Every value is exact: the timeline bounds are FramePos and the offset, gain and "
            "fades are Rational, with no f64 anywhere -- spec 17.6 forbids floating-point "
            "seconds from determining clip placement. The conversion to samples and to f64 "
            "mix gains is WP08's arithmetic, done once at the consumer against the project's "
            "sample rate.",
        ),
    ]
)

scene = defs["scene"]
scene["properties"]["audio_clips"] = Ordered(
    [
        ("type", "object"),
        ("propertyNames", Ordered([("$ref", "#/$defs/uuid")])),
        ("additionalProperties", Ordered([("$ref", "#/$defs/audio_clip")])),
        (
            "description",
            "Keyed by the clip's TrackId (clip_id), so storage order is id-byte order and the "
            "canonical stream needs no sort step. This is the version 5 addition: a version 4 "
            "file has no such member and is read by anim_project::migrate::V4_TO_V5, which "
            "inserts an empty object per scene. There is no default for it in the Rust "
            "Deserialize, precisely so that an unmigrated version 4 file is refused instead "
            "of quietly loading as a version 5 document with no clips in it.",
        ),
    ]
)
scene["required"] = list(scene["required"]) + ["audio_clips"]

# Accuracy correction (see the module docstring): a version 4/5 document embeds a version 2
# manifest, so the nested const must say 2. v4.json left it at 1; v4.json itself is frozen.
defs["manifest"]["properties"]["schema_version"]["const"] = 2

with io.open("schemas/project/v5.json", "w", encoding="utf-8", newline="\n") as handle:
    json.dump(doc, handle, indent=2)
    handle.write("\n")

print("wrote schemas/project/v5.json")