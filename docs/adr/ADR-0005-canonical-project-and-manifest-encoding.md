# ADR-0005 — Canonical project and manifest encoding

Status: **partially closed.** The manifest half — record framing, the manifest digest,
and the encoding of every field in a record — is accepted here. The project-document
half is not decidable until the document exists: field ordering inside
`project.anim.json`, where the document's schema version lives, and how a record
collection is embedded in it close in WP03. This record stays open until then, which is
what ADR-0004's "opened but not closed" note asked for.

Date: 2026-11-06 (manifest half)
Work package: WP02 (spec register: "ADR-0005 · Canonical project and manifest encoding · WP01–WP03")
Evidence: `crates/anim_assets/src/manifest.rs`, `crates/anim_assets/src/store.rs`,
`crates/anim_assets/src/hash.rs`; `crates/anim_assets/tests/store.rs`; unit tests named in
## Evidence. Spec 11.6, 11.7, 11.8, 11.12. Depends on ADR-0004 Decision 4.

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
5. **The document half is still open, and it is named.** WP03 closes this ADR by deciding:
   field ordering for the document object; where the document's own schema version sits
   relative to the manifest's; how a record collection is embedded rather than re-encoded;
   and how canonical serialization of the whole project is verified byte-for-byte against a
   saved file (spec 12.12's "canonical serialization" clause of G2).
6. **A `pub` field in a spec listing is not a promise.** DECISIONS.md #22 set the rule for
   `FramePos::subframe`; Decision 6 applies the same rule to `dependencies`. Spec 20's
   traceability matrix should record the field as private if the document is regenerated.

## Schema/version impact

First persisted structure in the project: no migration exists to or from it, and none is
needed until WP03 changes it. `MANIFEST_SCHEMA_VERSION = 1`; a bump to 2 without a
migration in `anim_project` is a data-loss event, and the digest will show it as a
manifest that no longer matches its own file.
