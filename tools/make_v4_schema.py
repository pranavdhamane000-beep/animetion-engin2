"""One-shot generator for schemas/project/v4.json, derived from v3.json.

Not part of the build, and versioned for the reason `make_v3_schema.py` states: spec 12.5
step 9 makes the schema files inputs, and "generated from the previous version, not retyped"
has to be repeatable to be a claim. It is also the check on this bump: the `image` field's
spelling here is derived from what `anim_assets`'s serializer actually writes, so a schema
that disagrees with `manifest.rs`'s `WireEntry` is a bug in one of them and this script makes
which-one answerable rather than leaving two hand-written descriptions of one format.

What version 4 adds is one field per asset record — spec 15.2's prerequisite that image
assets *carry* color-space and alpha metadata, ruled in DECISIONS.md #40 ruling 2 to arrive
as manifest data rather than as a second header read at render time. Because
`ProjectDocument.assets` embeds the record collection rather than re-encoding a digest of it
(ADR-0005 Decision 11, deviation row 11), a record-format change is simultaneously a
document-format change, which is why `MANIFEST_SCHEMA_VERSION` 1->2 and this constant 3->4
move in one commit.

Member order is emitted, not sorted: `image` is last in `WireEntry` and in
`AssetManifestEntry`, so the canonical byte stream appends it and every existing field keeps
its position. `required` lists it because `Option` is serialized as `null` when absent rather
than skipped — a record that omits the key entirely is not a version 4 record, which is the
same strictness `events` and `simulation` were given, and the reason `V3_TO_V4` writes the
key explicitly instead of relying on a default.

Run from the repository root:  python tools/make_v4_schema.py
"""

import collections
import io
import json

Ordered = collections.OrderedDict

with io.open("schemas/project/v3.json", encoding="utf-8") as handle:
    doc = json.load(handle, object_pairs_hook=Ordered)

doc["$id"] = "https://anim-engine.local/schemas/project/v4.json"
doc["title"] = "anim_engine project document, schema version 4"

sv = doc["properties"]["schema_version"]
sv["const"] = 4
sv["description"] = (
    "The document version. Independent of assets.schema_version and of the command envelope "
    "version (ADR-0005 Decision 4, Consequence 5). Version 2 is WP04's scene-level events "
    "section; version 3 is WP05's document-level simulation section; version 4 is WP06's "
    "per-asset image color-space and alpha claims, because spec 15.2 makes them a "
    "prerequisite of the render graph and 11.8 gave the record no field for them. The "
    "encoding domain anim_engine/document/v1 did NOT move: appending one field to each "
    "record of an embedded collection is a field addition on an unchanged framing, the split "
    "lib.rs states and that versions 2 and 3 already established. assets.schema_version did "
    "move, 1 -> 2, because the record's canonical byte stream grew a term, and that changes "
    "every manifest_digest by design -- see the digest test in anim_assets/src/manifest.rs."
)

defs = doc["$defs"]

defs["image_color_space"] = Ordered(
    [
        ("type", "string"),
        ("enum", ["srgb", "linear", "unclassified"]),
        (
            "description",
            "The transfer function and primaries the stored samples are in, exactly as "
            "anim_assets::ImageColorSpace spells them: the lowercase string is both the "
            "persisted form and the canonical-encoding term, so a saved manifest and a hashed "
            "record state the same value with no integer table between them. 'unclassified' "
            "has two producers and no other meaning -- an embedded ICC profile the engine does "
            "not model, reported rather than approximated, and the V3_TO_V4 migration of a "
            "record that stored nothing. A renderer reading it refuses rather than assumes, "
            "because spec 15.11 forbids a silent transfer-function fallback.",
        ),
    ]
)

defs["image_alpha"] = Ordered(
    [
        ("type", "string"),
        ("enum", ["opaque", "straight", "unclassified"]),
        (
            "description",
            "How the file's own alpha channel relates to its colour. PNG states this directly: "
            "its alpha channel is independent of the colour channels, so an RGBA cel is "
            "'straight' by the definition of the format and not by inference from its pixels, "
            "and a JPEG has no alpha to be anything but 'opaque'. Premultiplication is NOT a "
            "metadata claim -- it is a decode choice the renderer makes against a "
            "DecoderConfiguration, which is why it appears in the derived-cache key and not "
            "here. 'unclassified' is written only by V3_TO_V4.",
        ),
    ]
)

defs["image_asset_claims"] = Ordered(
    [
        ("type", "object"),
        ("required", ["color_space", "alpha"]),
        ("additionalProperties", False),
        (
            "properties",
            Ordered(
                [
                    ("color_space", Ordered([("$ref", "#/$defs/image_color_space")])),
                    ("alpha", Ordered([("$ref", "#/$defs/image_alpha")])),
                ]
            ),
        ),
        (
            "description",
            "What one stored image asserts about its own samples (anim_assets::ImageAssetClaims, "
            "fields in declaration order). Both terms are required together: a half-classified "
            "asset is the state a hand-edited file reaches and the state a renderer cannot draw "
            "from, so the record either says both or is not an image record.",
        ),
    ]
)

record = defs["manifest_record"]
record["properties"]["image"] = Ordered(
    [
        ("type", ["object", "null"]),
        ("anyOf", [Ordered([("$ref", "#/$defs/image_asset_claims")]), Ordered([("type", "null")])]),
        (
            "description",
            "The claims for an AssetKind::Image record, and null for every other kind. The "
            "null is present, not absent: AssetManifestEntry::new refuses an Image with no "
            "claims (MissingImageClaims) and refuses claims on a non-image (UnexpectedImageClaims), "
            "so the key's existence is not what carries the kind agreement -- its value is. "
            "Appended last so every earlier field keeps its byte position.",
        ),
    ]
)
record["required"] = list(record["required"]) + ["image"]

with io.open("schemas/project/v4.json", "w", encoding="utf-8", newline="\n") as handle:
    json.dump(doc, handle, indent=2)
    handle.write("\n")

print("wrote schemas/project/v4.json")
