"""One-shot generator for schemas/project/v3.json, derived from v2.json.

Not part of the build, and deliberately versioned this time. Spec 12.5 step 9 makes the
schema files inputs, and `schemas/project/v2.json` was produced by a script kept only in
the git-ignored `.tools/`, so the only reproducible statement about how it was derived was
its commit message. A generator that survives its own use is what lets the next bump be
checked against this one rather than retyped from memory, which is how a JSON pattern that
disagrees with `simulation.rs`'s decoder would enter the repository.

Member order is emitted, not sorted: `SimulationSettings`' declaration order is
`ticks_per_frame` then `project_seed`, and the document's is 12.8's six followed by the
section WP05 appended, so the schema's `required` and `properties` lists say the same thing
the `Serialize` impls do.

Run from the repository root:  python tools/make_v3_schema.py
"""

import collections
import io
import json

Ordered = collections.OrderedDict

with io.open("schemas/project/v2.json", encoding="utf-8") as handle:
    doc = json.load(handle, object_pairs_hook=Ordered)

doc["$id"] = "https://anim-engine.local/schemas/project/v3.json"
doc["title"] = "anim_engine project document, schema version 3"

sv = doc["properties"]["schema_version"]
sv["const"] = 3
sv["description"] = (
    "The document version. Independent of assets.schema_version and of the command envelope "
    "version (ADR-0005 Decision 4, Consequence 5). Version 2 is WP04's scene-level events "
    "section; version 3 is WP05's document-level simulation section, because spec 14.2 makes "
    "ProjectDocument.simulation.ticks_per_frame a prerequisite and 12.8 lists no such field. "
    "The encoding domain anim_engine/document/v1 did NOT move: appending a section to an "
    "unchanged framing is a field addition, not a byte-layout change, which is the split "
    "lib.rs states and that version 2 already established."
)

defs = doc["$defs"]

defs["project_seed"] = Ordered(
    [
        ("type", "string"),
        ("pattern", "^[0-9a-f]{64}$"),
        (
            "description",
            "The random-stream seed as exactly 64 lowercase hex characters, which is "
            "anim_project::simulation's one accepted spelling and the shape spec 14.8's "
            "StreamKey.project_seed [u8; 32] is persisted in. Uppercase, hyphenated and "
            "short forms are refused by the decoder rather than normalized, for the reason "
            "ContentHash gives: a parser tolerant of a spelling the writer would not produce "
            "leaves one revision with two byte streams and so two digests. An all-zero seed "
            "is a legal authored value, not an absent one.",
        ),
    ]
)

defs["simulation_settings"] = Ordered(
    [
        ("type", "object"),
        ("required", Ordered([("0", "ticks_per_frame"), ("1", "project_seed")])),
        ("additionalProperties", False),
        (
            "properties",
            Ordered(
                [
                    (
                        "ticks_per_frame",
                        Ordered(
                            [
                                ("type", "integer"),
                                ("minimum", 1),
                                ("maximum", 4294967295),
                                (
                                    "description",
                                    "Spec 14.2's nonzero u32. The simulation rate is "
                                    "frame_rate * ticks_per_frame, so this is part of what a "
                                    "saved project means rather than a per-session choice: "
                                    "whole frame f begins at tick f * N, an event at time t "
                                    "schedules to ceil(t * rate), and a rate supplied at "
                                    "runtime would let one revision replay to two different "
                                    "states, which is what 14.12's cadence and seek criteria "
                                    "exist to catch. Minimum is 1, not 0.",
                                ),
                            ]
                        ),
                    ),
                    ("project_seed", Ordered([("$ref", "#/$defs/project_seed")])),
                ]
            ),
        ),
        (
            "description",
            "How this project steps and what its random streams are keyed by (spec 14.2, "
            "14.7, 14.9). One section rather than two loose document fields because 14.2's "
            "path is ProjectDocument.simulation and the rate and the seed are one decision: "
            "changing either changes what a saved revision replays to. The frame rate is "
            "deliberately not here -- it is the document's frame_rate, and copying it would "
            "give one timeline two rate bindings that can disagree.",
        ),
    ]
)
# `required` is a JSON array; Ordered above is only to keep the key insertion order.
defs["simulation_settings"]["required"] = ["ticks_per_frame", "project_seed"]

props = doc["properties"]
props["simulation"] = Ordered(
    [
        ("$ref", "#/$defs/simulation_settings"),
        (
            "description",
            "Appended after 12.8's six fields so that the byte prefix every existing file "
            "has is unchanged by the section that came after it.",
        ),
    ]
)
doc["required"] = doc["required"] + ["simulation"]

with io.open("schemas/project/v3.json", "w", encoding="utf-8", newline="\n") as handle:
    json.dump(doc, handle, indent=2)
    handle.write("\n")

print("wrote schemas/project/v3.json")
