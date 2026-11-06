# ADR-0005 — Canonical project and manifest encoding

Status: **closed.** The manifest half — record framing, the manifest digest, and the
encoding of every field in a record — was accepted on 2026-11-06. The project-document half
was decided in WP03 on 2026-11-06 and is recorded in Decisions 9–12 below: field ordering
inside `project.anim.json`, where the document's schema version sits relative to
`AssetManifest.schema_version`, how a record collection is embedded rather than re-encoded,
and how canonical serialization is verified byte-for-byte against a saved file. Consequence 5
is the closure note; Consequence 7 is the one finding that changed a design decision.

Date: 2026-11-06 (manifest half), 2026-11-06 (document half, WP03)
Work package: WP02 (manifest) and WP03 (document) (spec register: "ADR-0005 · Canonical
project and manifest encoding · WP01–WP03")
Evidence: `crates/anim_assets/src/manifest.rs`, `crates/anim_assets/src/store.rs`,
`crates/anim_assets/src/hash.rs`; `crates/anim_assets/tests/store.rs`; unit tests named in
## Evidence. WP03: `crates/anim_project/src/{document,scene,property,schema,migrate,save,recovery}.rs`,
`crates/anim_project/tests/persistence.rs`, `crates/anim_commands/src/{command,envelope,registry}.rs`,
`schemas/project/v1.json`, `schemas/commands/v1.json`. Spec 11.6, 11.7, 11.8, 11.12, 12.7,
12.8, 12.12. Depends on ADR-0004 Decision 4; closed jointly with ADR-0006's recovery policy.

## Problem

Spec 11.7 states the manifest digest in one sentence: "Sort records by AssetId byte
order. Hash a domain separator, schema version, and each record's length-prefixed
canonical fields in order. Empty manifests are valid. Dependency IDs are sorted by ID;
duplicate dependencies are rejected." Every noun in it is already defined by ADR-0004
Decision 4 except three, and the three are the ones that persist in saved files forever:

1. **What a record's framing is.** "Length-prefixed canonical fields in order"
   describes fields, not records. Nothing says whether a record is a length-prefixed
   byte string, a count-delimited sequence of fields, or an unframed concatenation that
   only the schema's field list can parse.
2. **What a content digest looks like inside a canonical stream.** ADR-0004 gives two
   precedents that point opposite ways: `Rational` goes through its canonical *text*
   because the text is the published spelling of its normal form; `Uuid` goes through 16
   *raw bytes* because its width is fixed by the type. `ContentHash` is a fixed-width
   byte string whose published spelling is 64 lowercase hex.
3. **Which version number is being hashed.** `AssetManifest.schema_version` (spec 11.8)
   and the `anim_engine/<area>/v<n>` domain convention (ADR-0004 consequence 5) are two
   integers that look like duplicates, and spec 11.7 requires hashing the first while the
   encoder's own rule requires the second.

Getting (1) and (2) wrong is not cosmetic: a framing that a second reader can parse
differently means one manifest with two digests, which is the exact failure WP04's
snapshot identity and WP09's job hash cannot recover from.

## Alternatives considered

**Record framing.**

- **Unframed field concatenation, delimited only by the field list.** Rejected: it makes
  the byte stream parseable only by something that already knows the schema, so a
  migration cannot read an old record without compiling its field table first, which is
  the opposite of what `migrate.rs` needs.
- **A `u64` length prefix on each whole record.** Rejected. Every field inside a record
  is already self-delimiting — fixed widths for the ids and digest, `u64` prefixes on the
  two strings, `u64` counts for the dependency list — so a record length is a second copy
  of information already implied by its contents, and a disagreement between the two would
  have no defined winner.
- **A record count, then records back to back.** Chosen. `1` record and `11` records
  cannot be confused, no record can be read as a prefix of another, and the count is the
  one piece of information the field framing cannot supply.

**Content digest inside a canonical stream.**

- **`text(hash.to_hex())`** — consistent with `Rational`. Rejected: ADR-0004's reason for
  the rational rule is that the type's guarantee is a unique *normal form of a value*, and
  hashing the text avoids a second arithmetic layout. A digest has no normal form to
  preserve — it *is* bytes — and its width is fixed by the type, which is `Uuid`'s
  condition, not `Rational`'s. Prefixing 32 bytes with 8 more adds nothing that delimits.
- **Raw 32 bytes, via a new `CanonicalBytes::fixed_bytes`.** Chosen. It also keeps the rule
  checkable in one place: a width that is part of a type is never length-prefixed, and
  `fixed_width_values_are_written_raw_and_delimit_by_their_type` is the test that says so.

**Version numbers.**

- **Drop `schema_version` from the digest, since the domain string already says `v1`.**
  Rejected: spec 11.7 names it, and the two numbers mean different things — the domain
  suffix changes when the *byte layout* changes, the field changes when the *asset
  schema* changes. Folding them together would make a schema bump a hash-encoding event
  and would lose the ability to hash an old-version manifest under the current encoding.
- **Hash both, and state which is which.** Chosen.

**Kind tag.**

- **A `u8` discriminant.** Rejected: it is the array-position mistake ADR-0004 already
  rejected for rationals, and reordering six variants in `AssetKind` would redefine every
  manifest ever hashed without a single byte looking wrong in a diff.
- **The lowercase name that JSON already writes.** Chosen, so a record's persisted spelling
  and its hashed spelling are one string with no table between them.

**Insertion order.**

- **Sort at digest time.** Rejected: it leaves `BTreeMap`'s key order as an accident of
  insertion for every *other* consumer — dependency lists, export verification sets,
  preview diagnostics — and re-derives an order in one place that is unsorted elsewhere.

**Document half, decided in WP03.**

- **Flatten the manifest's fields into the document object**, so `project.anim.json` has one
  `schema_version` and one `records` member at the top level. Rejected: it would make
  `AssetManifest::canonical_bytes` a function of a *containing* document rather than of the
  record collection, so a manifest digest would stop being recomputable from the file the
  manifest is stored in, and spec 11.10's "export includes the ordered manifest digest"
  would have no fixed pre-image. It would also make one of spec 12.8's six fields vanish,
  which is not this document's to do.
- **One version number for the whole file.** Rejected: see the three-meanings argument in
  Decision 10.
- **Pretty-printed JSON, so `project.anim.json` is hand-editable.** Rejected twice over:
  `serde_json`'s pretty printer is an opt-in feature this build does not enable, so it would
  be a dependency change for a rendering; and an indent policy is a second thing to version.
  The file is a commit record whose only supported mutation path is an envelope.
- **A JSON Schema validator dependency to check documents on load.** Rejected: spec 5.1 names
  no validator, and every rule a validator would enforce here is enforced by a constructor
  that refuses the value — a non-canonical rational, an uppercase uuid, an unsorted
  dependency list, a duplicated key time. What a validator cannot do is catch the one defect
  that matters, which is two byte streams for one state; that needs the round trip of
  Decision 12.
  Keying the map by `AssetId`, whose `Ord` is the 16 bytes, makes the sort a property of
  the type once.

## Decision

1. **Manifest stream layout.** `domain tag` → `schema_version: u32` → `record count:
   u64` → each record, in `AssetId` byte order. Domain
   `anim_engine/manifest/v1`, exported as `MANIFEST_DOMAIN`.
2. **Record field order, fixed by spec 11.8's listing:** `asset_id` (16 raw bytes) →
   `content_hash` (32 raw bytes) → `byte_length: u64` → `kind` (length-prefixed lowercase
   text) → `importer_version: u32` → `dependency count: u64` → each dependency (16 raw
   bytes, sorted, deduplicated) → `display_name` (length-prefixed text).
3. **Fixed widths are never length-prefixed.** `anim_core::hash::CanonicalBytes` gains
   `fixed_bytes::<N>`; `uuid` now delegates to it. Variable-length items keep their `u64`
   prefixes. This is ADR-0004 Decision 4's rule, extended to the one type that did not
   exist when it was written.
4. **Both version numbers are hashed, and they are different things.** The domain suffix is
   the encoding version; `schema_version` is the asset-schema version, carried in the
   record collection and validated by WP03's migrations, not by this crate.
5. **`AssetKind` canonical spelling is its lowercase name**, identical to its JSON spelling.
   `as_str()` is the only source of it.
6. **Order comes from the map, not from a sort step.** `AssetManifest.records` is a
   `BTreeMap<AssetId, _>`; `AssetId::Ord` is the 16 bytes (`anim_core::id`, DECISIONS.md
   #22's rationale). Dependency lists are sorted and deduplicated at construction, which is
   why `AssetManifestEntry.dependencies` is private with a hand-written `Deserialize` —
   spec 10.8's "fields whose invariants require normalization are private" applied to a
   field spec 11.8 lists as `pub`.
7. **A manifest's JSON form is `{ "schema_version": <u32>, "records": { <id>: <record> } }`**,
   with record keys in the same `AssetId` byte order as the digest, and `ContentHash` and
   typed ids as lowercase strings. This is the *record collection's* encoding; the document
   that embeds it is WP03's, and any disagreement between the two is resolved in WP03, not
   by editing this list.
8. **A record's canonical bytes are available without its domain tag**
   (`AssetManifestEntry::canonical_bytes`), so a WP03 command payload that references a
   record hashes these exact bytes rather than inventing a second rendering.

The next four close the document half Consequence 5 left open.

9. **Document field ordering is spec 12.8's listing order, and it is the declaration
   order.** `ProjectDocument` is written with its six fields in 12.8's sequence and
   serialized by a derived `Serialize`, whose members are emitted in declaration order — so
   there is one list, not a serializer convention that has to be remembered separately. The
   persisted order is therefore `schema_version, project_id, revision, frame_rate, assets,
   scenes`. Nested objects follow the same rule: `AssetManifest` and `AssetManifestEntry`
   write 11.8's names in 11.8's order through their hand-written impls, and `SceneData`,
   `NodeData`, `TrackData`, `Marker`, `PropertyKey` and `PropertyCurve` do the same.
   `the_json_field_order_is_the_declaration_order_of_spec_12_8` asserts it on the bytes,
   against a `serde_json::Value`, which is the finding in Consequence 7.
10. **Three version numbers, three meanings, none of them the others'.**
    `ProjectDocument.schema_version` describes the document; it is the first member and is
    what `migrate.rs`'s chain advances. `AssetManifest.schema_version` describes the record
    collection and travels inside `assets` unchanged. `CommandEnvelope.schema_version`
    describes a transaction, and each command payload carries its own
    `payload_schema_version` beside it. `anim_project::migrate::check_document_schema` and
    `check_asset_schema` are the only places any of them is range-checked, which is what
    Consequence 4's "validated by WP03, not here" promised: `with_schema_version` stays
    permissive on both types so a migration can build the version it is migrating *to*.
    Folding any two of the three was rejected because a schema bump would then look like a
    byte-layout bump, and the migration chain has different remedies for the two.
11. **The record collection is embedded, not re-encoded.** `assets` is a nested
    `{ "schema_version": <u32>, "records": { <id>: <record> } }` object written by
    `AssetManifest`'s own `Serialize`, and its canonical byte stream — Decision 1's layout,
    `MANIFEST_DOMAIN` included — is unchanged by living inside a document. So
    `manifest_digest` is computable from the document file by anyone with `blake3` and this
    record, with no knowledge of the document's shape. `next_version_bump_of_the_document_does_
    not_move_the_manifest_digest` and the WP03 store-verification test both lean on that
    independence.
12. **Canonical serialization is `serde_json::to_vec` on the struct, and it is verified
    against the saved file, not against itself.** `anim_project::schema::serialize_canonical`
    validates first and writes second, in that order, because spec 12.7's "validate and
    serialize before touching disk" is the only safe ordering and an inferrable one is not
    the same as a mandatory one. `verify_canonical_bytes` then proves the stronger clause:
    read the file, load it, write it again, and require the two byte streams to be equal —
    the claim spec 12.12 names "canonical serialization", tested against a file rather than
    against a value. Two corollaries are recorded rather than left implicit. A file whose
    member order is not the serializer's is *valid and loadable* and still refused
    (`a_document_that_only_differs_in_field_order_is_refused`). And a migrated file, whose
    bytes necessarily came out of a `serde_json::Value`, is reported as complete but
    non-canonical by `recovery::CompleteCandidate::canonical` rather than silently rewritten
    or silently trusted.

## Evidence

- `digest_layout_is_what_adr_0005_says_it_is` — the byte stream rebuilt by hand from
  Decision 1 and 2, then compared to the encoder's output and to `blake3` over the
  hand-built bytes. The oracle is not the code under test.
- `fixed_width_values_are_written_raw_and_delimit_by_their_type` (`anim_core::hash`) —
  raw 32 bytes versus a length-prefixed 32 bytes produce different digests, and a 16-byte
  and a 32-byte field of the same repeating byte do not agree.
- `digest_is_independent_of_insertion_order` and
  `manifest_digests_agree_under_shuffled_insertion_after_real_imports` — the same twelve
  imported records, inserted forward, reversed and rotated, give one digest *and* one
  pre-image.
- `empty_manifest_is_valid_and_distinct_from_nothing` — Decision 1's count prefix is what
  makes zero records hash differently from no records at all.
- `a_record_survives_json_only_through_the_validating_constructor` — a duplicated
  dependency list smuggled in as JSON is refused, which is the rule Decision 6 exists to
  keep.
- `dependencies_are_sorted_and_deduplicated_at_construction`,
  `a_repeated_dependency_is_refused_not_collapsed`,
  `a_self_dependency_is_the_shortest_cycle_and_is_refused`.
- `a_record_and_a_manifest_survive_their_json_form_unchanged` — re-serializing a loaded
  manifest reproduces the original bytes, so a saved file hashes as itself.

Document half, WP03:

- `a_saved_file_is_the_documents_canonical_bytes_and_loads_back_identical`
  (`crates/anim_project/tests/persistence.rs`) — Decision 12's clause end to end: what
  reached the disk equals what the serializer produced, the file reloads equal, and
  re-serializing the loaded document reproduces the file byte for byte.
- `the_json_field_order_is_the_declaration_order_of_spec_12_8` — the six names located in
  the emitted byte stream in 12.8's order. Asserted on `to_vec` output rather than on a
  `Value`, for the reason in Consequence 7.
- `a_document_that_only_differs_in_field_order_is_refused` and
  `non_canonical_bytes_are_reported_with_the_offset_that_says_where` — a file with every
  value right and the wrong member order loads and is still refused, and the refusal names
  the offset where the two spellings part. The offset check exists because the first version
  of that test perturbed two trailing bytes, and the document ends in `}}`, so the "damage"
  was a no-op and the assertion passed on an untouched file.
- `bytes_do_not_depend_on_the_order_nodes_arrived_in`,
  `canonical_bytes_do_not_depend_on_insertion_order`,
  `a_loaded_scene_may_arrive_before_its_parents` — order independence proven for the
  document, for a scene's canonical stream, and for the one case where validating one node
  at a time would refuse a legal file.
- `the_committed_schema_matches_what_the_serializers_write` and
  `the_committed_command_schema_matches_the_encoder` — `schemas/project/v1.json` and
  `schemas/commands/v1.json` compared, field set by field set, against the bytes the Rust
  serializers actually emit. This is the check that replaces a schema validator: the risk is
  not a malformed document but an artifact and an encoder disagreeing, and it is the artifact
  a human reads.
- `an_invalid_document_produces_no_bytes`,
  `a_hand_edited_graph_that_parses_is_still_refused_on_load`,
  `a_manifest_version_the_document_cannot_migrate_is_refused_at_load` — Decisions 10 and 12's
  refusals, reached through the file path rather than through a constructor.

## Consequences

1. **A manifest digest is portable and auditable.** Because the pre-image is a documented
   byte layout published by `canonical_bytes()`, a recorded digest can be checked by anyone
   with `blake3` and no engine. Spec 11.10's "export includes the ordered manifest digest in
   its job hash" has something fixed to include.
2. **Adding a field to a record is a schema-version event, and reordering one is a
   corruption.** Nothing detects a reordered `AssetKind` variant any more than `Rational`'s
   two-integer object would have detected one; the count-and-order framing means a wrong
   field order produces a valid-looking parse of the wrong bytes. `MANIFEST_DOMAIN` changes
   only with the layout, never with a field addition.
3. **WP06/WP08's derived caches inherit the same rule.** Spec 11.6's cache key is
   `(content_hash, importer_version, decoder_configuration)`; Decision 3 says the digest
   goes in raw and Decision 5's spelling rule is why `DecoderConfiguration::as_str()` is a
   path segment rather than an integer directory. A cache written under a different spelling
   is invisible, not invalid, so the enum has no `String` escape hatch.
4. **`schema_version` is validated by WP03, not here.** `AssetManifest::with_schema_version`
   accepts any `u32` because a migration has to build a manifest at the version it is
   migrating to. The supported-range question belongs with the migration chain (spec 12.3's
   `migrate.rs`), and this crate refusing a version would make WP03 unable to write one.
5. **The document half was open, was named, and is now closed.** WP03 decided the four
   items this consequence listed — field ordering (Decision 9), where the document's schema
   version sits relative to the manifest's (Decision 10), how a record collection is
   embedded rather than re-encoded (Decision 11), and how canonical serialization is
   verified byte-for-byte against a saved file (Decision 12). Nothing in the manifest half
   moved to accommodate them: `manifest_digest`'s pre-image is byte-identical to what WP02
   certified, which is the test a decision to *embed* rather than *re-encode* has to pass.

6. **A `pub` field in a spec listing is not a promise.** DECISIONS.md #22 set the rule for
   `FramePos::subframe`; Decision 6 applies the same rule to `dependencies`. Spec 20's
   traceability matrix should record the field as private if the document is regenerated.

7. **`serde_json::Value` does not preserve field order, so it is not on the canonical
   path.** Found while writing WP03's test for Decision 9: the first version asserted the
   persisted order by round-tripping a document through `Value` and reading the keys back,
   and it failed — not because the serializer was wrong, but because `Value`'s map is a
   `BTreeMap<String, _>` and the keys came out alphabetical. Two consequences for anyone
   reading this record later. A canonical byte stream exists only on the direct
   `to_vec`/`to_writer` path, so going through `Value` produces a *different valid file* for
   the same state, which Decision 12 then refuses. And any migration step or report that
   emits a `Value` writes non-canonical bytes by construction, which is why
   `migrate::upgrade_with` returns the original bytes unchanged when no migration is needed,
   and why `recovery` reports `canonical: false` rather than quietly rewriting or quietly
   trusting. A future package that wanted an order-preserving `Value` would have to enable
   `serde_json`'s `preserve_order` feature, which changes the map type globally: a dependency
   decision, not a local one.

8. **A report can now recompute a manifest digest from a saved project with no engine.**
   Because Decision 11 keeps the record collection's own framing intact inside the document,
   the bytes that are hashed are present in the file: extract `assets`, apply Decision 1's
   layout, and `blake3` yields the value spec 11.10 wants in a job hash. WP09's export
   verification and WP11's fault-injection triage should read a manifest digest out of a file
   rather than trusting one recorded beside it.

## Schema/version impact

First persisted structure in the project: no migration exists to or from it, and none is
needed until a version 2 is written. `MANIFEST_SCHEMA_VERSION = 1`; a bump to 2 without a
migration in `anim_project::migrate` is a data-loss event, and the digest will show it as a
manifest that no longer matches its own file.

`PROJECT_SCHEMA_VERSION = 1` is the document's, and it is independent: a document that
embeds a version-1 manifest is at version 1 of the *document*, and the two move separately
(Decision 10). `anim_project::migrate::chain()` is empty today because version 1 is the
first version this project has written — spec 12.5's "fixtures for every schema version" has
exactly one fixture. The chain machinery is not therefore untested: a test-only `0 → 1` step
proves sequential application, the no-path refusal (`MigrationFailure`), the step-that-did-
not-advance guard, and that a current file passes through byte-for-byte rather than being
re-serialized into the `Value` spelling (Consequence 7). The first real migration is a step
added to that table, against code that has already been shown to run.
