# `philox4x32_10_test_vectors.json` — provenance and key-packing caveat

Fixture for spec 14.5 step 2 / 14.12 ("verify published Philox ... golden vectors").
Do not edit the numbers. Regenerate only by re-retrieving the sources below.

## Where the values came from

Retrieved over HTTPS on **2026-11-06** from the official Random123 repository of
D. E. Shaw Research (the library of Salmon, Moraes, Dror & Shaw, SC'11). Pinned to
`main` @ commit `9545ff6413f258be2f04c1d319d99aaef7521150`:

| set | file | sha256 of fetched bytes | rows used |
| --- | ---- | ----------------------- | --------- |
| `kat_vectors` | `tests/kat_vectors` | `aab5ebabf40003f63d6d87b24cbd2c8a02652e00cf8bad64226fd50586929183` | 3 |
| `old_kat_vectors` | `tests/old_kat_vectors` | `cb530c22ab16c7e9254a679ba23b50e82d710f705b6a51654a64e43157bc69d8` | 52 |

URL form: `https://raw.githubusercontent.com/DEShawResearch/random123/<commit>/tests/<file>`.
Only rows whose first field is `philox4x32` **and** whose round-count field is `10` were
taken; `philox4x32 7`, `philox2x32`, `philox2x64`, `philox4x64`, `threefry*`, `aesni*`
and `ars*` rows in the same files were excluded.

Cross-checks performed at retrieval time: `tests/kat_vectors` is byte-identical to the
independent copy at `github.com/lemire/random123` `examples/kat_vectors`; the same three
headline rows appear, attributed to Random123, in the third-party Rust port
`github.com/pawlenartowicz/rand_philox` (`src/philox.rs`). All 55 rows also reproduce
against a throwaway re-implementation written from the fetched `include/Random123/philox.h`
and `ROCm/rocRAND` headers. No value in the fixture was transcribed by hand or written
from memory.

## The caveat that matters: the published key is 64 bits, not 128

Spec 14.9 fixes `philox4x32_10(counter: [u32; 4], key: [u64; 2]) -> [u32; 4]`. Random123
publishes the Philox4x32 key as **two 32-bit words** (64 bits), per its own documentation
(`docs/cbrng.dox`: "Philox keys are half as wide as the counter") and code
(`philox4x32_key_t` = `r123array2x32`; `philox4x32keyinit()` is the identity). The same
holds for rocRAND's `philox4x32_10_engine` (`uint2 key`) and for the Rust port above.

Mapping used in this fixture: **positional zero-extension** — `key[0] = (u64) K0`,
`key[1] = (u64) K1`. The implementation must therefore consume only the low 32 bits of
each `u64` as round subkeys 0 and 1. The high halves are provably dead in Philox4x32: the
per-round bump is a 32-bit wrapping add, so bits 32..63 can never reach an output. The
alternative reading — `key = [K0 | (K1 << 32), 0]` — is wrong and fails 41 of the 55 rows.

Consequences to record rather than discover later:

- The fixture cannot certify behaviour that lives in the dead high bits, because every
  published key has zeros there.
- If the engine genuinely intends a 128-bit key, that is a different primitive with **no
  published vectors from the original authors** — raise it as a spec gap; do not invent
  golden values for it.
- A 64-bit seed `S` becomes `[(u64)(S & 0xFFFFFFFF), (u64)(S >> 32)]`, matching rocRAND's
  seed split. Relevant to the 14.12 stream-key vectors.

## Shape

`counter` = `[c0, c1, c2, c3]`, `key` = `[k0, k1]`, `expected` = `[r0, r1, r2, r3]`, all
unsigned decimal integers; source words are u32 (`0 .. 4294967295`). Columns are already in
array order — there is no endianness or word-order conversion (see `tests/README`: the KAT
vectors are byte-order independent). Each vector carries `set` (which source file) and
`source_line` (the verbatim hex line) for audit. Full details in the JSON `notes` field.
