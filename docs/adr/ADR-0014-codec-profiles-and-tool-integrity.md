# ADR-0014 — Codec profiles, colour metadata and alpha matrix

Status: **partially closed.** WP00's probe-facing half is measured; delivery profiles and the alpha matrix close in WP09.
Date: 2026-11-05
Work package: WP00 / WP09 (spec register: "ADR-0014 · Codec profiles, colour metadata and alpha matrix · WP00/WP09")
Evidence: `docs/adr/evidence/wp00/wp00_media.json`, `docs/adr/evidence/wp00/wp00_export.json`, `tools/ffmpeg/ffmpeg-manifest.json`

## Problem

WP09 has to pick codecs, prove counts, and make the result verifiable. Two things had
to be settled before then, because they are platform facts rather than taste:
which encoder can carry a provable frame count through a round trip, and whether
counts can be read back at all from the containers in question.

## Decision — probe encoder

`ffv1` for the raw-video round trip, in a Matroska container.

Lossless, intra-frame, present in every FFmpeg build, and it accepts RGB directly.
`libx264` was measured as an alternative and is available here, but it is a
delivery codec: its reference frames and B-frame decisions make "did my pipeline
produce N frames" a question with an indirect answer. WP09 will still need libx264
for delivery profiles, which is a different decision from this one.

Verified present in the pinned build by reading `ffmpeg -hide_banner -encoders` from
the vendored executable and asserting the name appears — not by trusting the
distribution's name.

## Measured facts WP09 must design around

These were each observed, not assumed. Several contradict the obvious approach.

1. **`-count_frames` is mandatory for counting.** Matroska stores no `nb_frames`;
   asking `ffprobe` for `nb_frames` on a Matroska file returns an **empty object
   with exit code 0**. Absent is not zero, and a verifier that treats a missing
   field as falsy reports success on a file it never measured.
2. **FFmpeg exits 0 on truncated and on empty raw-video input.** A 6-frame feed under
   a `64x64` header produced a valid container with **6** counted frames and a clean
   exit. A 0-byte feed produced a 15 439-byte container whose video frame count is
   unreadable, also with a clean exit and no stderr. **Exit status carries no
   information about frame counts.** The export verifier must count the output.
3. **Audio frame counts cannot be read from the container.** A 48 000-frame FLAC
   track inside Matroska reports `nb_read_frames = 12` and **no duration** — that is
   12 packets, not 12 frames. The probe therefore demuxes to PCM and counts there,
   which returned exactly 48 000. WP09's audio verification takes this second path.
   This is recorded in `wp00_export.json` as `export.audio_not_container_countable`
   with severity `warning`, deliberately not as a pass.
4. **Stream indices are stable at 0 = video, 1 = audio** for the verified output, so
   a verifier may address streams by index rather than scanning.
5. **`-fflags +bitexact` strips the `encoder: Lavf…` tag and `-map_metadata -1`
   removes tags**, which is what makes two runs of the same input comparable.
   Full byte-determinism of the container is a WP09 question; the probe only needs
   its own evidence not to drift.
6. **Decoder delay is declared and exact for Vorbis.** gapless-minus-untrimmed equals
   the track's advertised `delay 128 + padding 508`. WAV and FLAC declare no delay.
   A sample-count assertion therefore has a stated tolerance rather than an
   assumption.
7. **Resampling 44 100 → 48 000 over 44 100 frames yields 48 001**, one above the
   exact ratio. The band the probe asserts is `floor..=ceil+1`; WP08 pins it against
   PCM goldens under ADR-0013.

## Toolchain integrity decision

FFmpeg 9.0.2 gyan-essentials is vendored into `tools/ffmpeg/` and pinned by SHA-256
in `tools/ffmpeg/ffmpeg-manifest.json`. Every probe hashes the file at the configured
path and compares it to the manifest **before** spawning, and the media probe
demonstrates the rejection: presenting `ffprobe.exe` under the `ffmpeg` role fails as
`tool.digest_mismatch`. The copy on `PATH` is never consulted (spec 9.11).

SHA-256 rather than the workspace's blake3, because spec 5.1 names SHA-256 for
executable verification and an operator must be able to reproduce the digest with a
standard tool. That made `sha2 = 0.11.1` the third dependency exception recorded in
ADR-0001, alongside `ash` and `pollster`.

## Not decided here

Delivery codec profiles, bitrates and GOP structure; the colour metadata matrix
(colour primaries, transfer characteristics, matrix coefficients) and how they are
written per profile; the alpha transport matrix (which codec carries alpha how).
Those are WP09's subject and need its export settings schema, which does not exist
yet.

## Consequences

1. **The export verifier counts output; it does not trust exit codes.** Fact 2 makes
   this mandatory rather than stylistic.
2. **Audio is verified in PCM.** Fact 3 makes container counting actively wrong, not
   merely less precise.
3. **`ffv1` is a probe codec, not a delivery codec.** WP09 chooses delivery
   separately and must not read this ADR as having chosen for it.

## Schema/version impact

`tools/ffmpeg/ffmpeg-manifest.json` carries `schema_version: 1`. Its shape — an
`executables` array plus a `capabilities` block — is now load-bearing for probe and
export code, so changing it is a schema change.
