//! The zk-settlement verifier registry account layout (PDA `["registry", chain_id]` under
//! the settlement program). Each chain names its verifier(s) as `(curve, scheme, vkey_hash, layout_id)`
//! tuples instead of a hard-coded vkey, so switching provers is a program upgrade plus a registry
//! rotation, never a redesign.
//!
//! This account also carries the chain's `zk-inbox` program id (the root account has no spare bytes
//! left at `MIN_LEN` for it — every offset there is already accounted for — so it lives here instead;
//! `PostRoot`/`PostRootProved` read it to validate the inbox batch account's owner).
//!
//! ```text
//! magic 'ZKVR' u32 | chain_id u64 | inbox_program [32] | count u8
//! | entries: [ { curve u8, scheme u8, vkey_hash [32], layout_id u8 } ; MAX_ENTRIES ]
//! ```
//! All integers little-endian. `count` is the number of *populated* entries (`0..count`); the remaining
//! `MAX_ENTRIES - count` slots are zeroed and unused.
//!
//! **Entry rotation with an activation delay:** a per-entry `activation_slot: u64`
//! lives in a fixed tail appended after the entries, WITHOUT changing the shipped 35-byte entry
//! (`InitChainV2`'s body and every already-deployed v1 registry account are pinned): `OFF_ACTIVATION
//! = OFF_ENTRIES + MAX_ENTRIES * ENTRY_LEN`, `[u64; MAX_ENTRIES]` LE, `REGISTRY_LEN_V2 = OFF_ACTIVATION + 32`.
//! `read_header`/`entry_at` accept both lengths — a v1-length account (`REGISTRY_LEN`, exactly what
//! `InitChainV2` still writes) decodes with every `activation_slot` reading back `0` (active since
//! genesis); a v2-length account carries real per-entry activation slots. `find` takes an `at_slot: u64`
//! and skips any entry whose `activation_slot > at_slot` — an entry registered for the future is invisible
//! to `PostRootProved` until the chain's clock reaches it, the same refusal (`RegistryEntryNotFound`) a
//! never-registered vkey gets. The settlement program's `SetRegistryEntry` instruction is the only writer
//! of the v2 tail (realloc-in-place on first use); this crate only defines the bytes.
//!
//! **Rotation never touches another entry; retirement is explicit:** `SetRegistryEntry` keys on `(curve, scheme,
//! vkey_hash)` — the same key `find` uses, not `(curve, scheme, layout_id)`. A vkey already present has only its
//! `activation_slot` updated in place (`layout_id` must still match the stored entry, else `LayoutMismatch` — one vkey
//! is one ELF is one layout); an absent vkey is appended, never overwrites an unrelated entry that happens to share a
//! curve and scheme. **`RETIRED_SLOT` (`u64::MAX`) is the tombstone activation slot**: `SetRegistryEntry` with
//! `activation_slot = RETIRED_SLOT` on an existing vkey retires it immediately (`find`'s own `at_slot >
//! activation_slot` skip already excludes it for every real slot — `Clock::slot` never reaches `u64::MAX`), and a
//! retired slot is the first one `SetRegistryEntry` reuses once the registry is at `MAX_ENTRIES` and a new vkey needs a
//! slot.
//!
//! **Retirement is terminal; duplicate vkeys are unconstructable.** Once a vkey's stored `activation_slot ==
//! RETIRED_SLOT`, `SetRegistryEntry` refuses any OTHER `activation_slot` named against that same vkey (`EntryRetired`)
//! — a retired key cannot be re-activated while its entry stands, only a genuinely new vkey can take that slot;
//! retiring an already-retired vkey again is unaffected and stays a no-op success. Once the slot HAS been reused the
//! registry no longer remembers the retired vkey (four slots, not a memory) — a bound, not an absolute. This matters
//! only because two entries can never legitimately share a `(curve, scheme, vkey_hash)` in the first place:
//! `InitChainV2` refuses a genesis registry that does (even under two different `layout_id` values — one vkey is one
//! ELF is one layout), and `SetRegistryEntry` itself scans in the exact order `find` does (first populated match wins)
//! and refuses `InvalidAccountData` outright if a second match is ever found — defence in depth for a shape the writer
//! should never be able to see.
//!
//! `curve`: 0 = BN254, 1 = BLS12-381. `scheme`: 0 = Groth16, and every other value is the number of a ZisK
//! release: 1 = ZisK 1.2.0-alpha, 2 = ZisK 1.3.1-alpha (see [`ZISK_RELEASES`]). Every entry written before
//! releases were named carries `1`. A number is never reused and the next release takes the next unused one. `layout_id`: 1 = the full
//! v2, 208-byte public-values struct (`rome_zk_layouts::public_values`, accumulator- and
//! drift-bound; `PostRootProved` binds it, the guest commits it), 2 = the
//! header-only fallback (number/parent_hash/state_root from a block header, no inbox binding — today's
//! SP1 bincode-header path and the ZisK PLONK header-RLP path both use this).

pub const MAGIC: u32 = 0x5a4b_5652; // "ZKVR"

pub const CURVE_BN254: u8 = 0;
pub const CURVE_BLS12_381: u8 = 1;
pub const SCHEME_GROTH16: u8 = 0;
/// ZisK 1.2.0-alpha: the PLONK wrapper key every entry written before releases were named was registered
/// against.
pub const SCHEME_ZISK_1_2_0: u8 = 1;
/// ZisK 1.3.1-alpha.
pub const SCHEME_ZISK_1_3_1: u8 = 2;

/// One ZisK release as the registry names it: the `scheme` byte an entry carries and the release's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZiskRelease {
    pub scheme: u8,
    pub name: &'static str,
}

/// Every ZisK release, in scheme order. Numbers count up from 1; a row never changes and a number is
/// never reused. A new release is appended with the next unused number.
pub const ZISK_RELEASES: [ZiskRelease; 2] = [
    ZiskRelease {
        scheme: SCHEME_ZISK_1_2_0,
        name: "1.2.0-alpha",
    },
    ZiskRelease {
        scheme: SCHEME_ZISK_1_3_1,
        name: "1.3.1-alpha",
    },
];

/// The release a `scheme` byte names, or `None` for Groth16 and for numbers no release has taken yet.
pub fn zisk_release_name(scheme: u8) -> Option<&'static str> {
    ZISK_RELEASES
        .iter()
        .find(|r| r.scheme == scheme)
        .map(|r| r.name)
}

/// Whether `scheme` names a ZisK release.
pub fn is_zisk_scheme(scheme: u8) -> bool {
    zisk_release_name(scheme).is_some()
}
/// The full v2 public-values struct (accumulator- and drift-bound, 208 B — see
/// `crate::public_values`). `PostRootProved`'s layout-1 path binds a proof under this
/// layout id; the guest commits it.
pub const LAYOUT_ZISK_V1: u8 = 1;
/// Header-only continuity (number/parent_hash/state_root), no inbox binding. Today's only implemented
/// path, for both the SP1 Groth16 module and the ZisK PLONK module's header-RLP binding.
pub const LAYOUT_HEADER_FALLBACK: u8 = 2;

/// Fixed registry capacity for v1 (room for primary + fallback + headroom before rotation ships).
pub const MAX_ENTRIES: usize = 4;
/// The tombstone `activation_slot` a retired entry carries. `find`'s `activation_slot >
/// at_slot` skip already excludes it for every real slot on-chain (`Clock::slot` never reaches
/// `u64::MAX`); `SetRegistryEntry`'s own `ActivationInPast` check (`activation_slot < now`) never rejects
/// it either — `u64::MAX` is never in the past. A slot whose `activation_slot == RETIRED_SLOT` is what
/// `SetRegistryEntry` reuses first once the registry is at `MAX_ENTRIES`.
pub const RETIRED_SLOT: u64 = u64::MAX;
/// `curve u8 | scheme u8 | vkey_hash [32] | layout_id u8`.
pub const ENTRY_LEN: usize = 35;

pub const OFF_MAGIC: usize = 0;
pub const OFF_CHAIN_ID: usize = 4;
pub const OFF_INBOX_PROGRAM: usize = 12;
pub const OFF_COUNT: usize = 44;
pub const OFF_ENTRIES: usize = 45;
/// Full fixed-size v1 account length (still what `InitChainV2` writes — genesis entries are active from
/// slot 0, so no activation tail is needed until a rotation actually happens).
pub const REGISTRY_LEN: usize = OFF_ENTRIES + MAX_ENTRIES * ENTRY_LEN;

/// Start of the v2 activation-slot tail, right after the last fixed entry slot.
pub const OFF_ACTIVATION: usize = OFF_ENTRIES + MAX_ENTRIES * ENTRY_LEN;
/// One `u64` LE activation slot per entry index (`0..MAX_ENTRIES`), regardless of `count`.
pub const ACTIVATION_ENTRY_LEN: usize = 8;
/// Full v2 account length: the v1 bytes plus the fixed `[u64; MAX_ENTRIES]` activation tail.
pub const REGISTRY_LEN_V2: usize = OFF_ACTIVATION + MAX_ENTRIES * ACTIVATION_ENTRY_LEN;

/// `["registry", chain_id]`.
#[inline]
pub fn seeds(chain_id: u64) -> [Vec<u8>; 2] {
    [b"registry".to_vec(), chain_id.to_le_bytes().to_vec()]
}

/// Derives the registry PDA under `program_id` (the settlement program) — the one place this
/// derivation is computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    chain_id: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(chain_id);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1]], program_id)
}

const ENTRY_OFF_CURVE: usize = 0;
const ENTRY_OFF_SCHEME: usize = 1;
const ENTRY_OFF_VKEY_HASH: usize = 2;
const ENTRY_OFF_LAYOUT_ID: usize = 34;

/// One verifier registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryEntry {
    pub curve: u8,
    pub scheme: u8,
    pub vkey_hash: [u8; 32],
    pub layout_id: u8,
}

/// Field-for-field decode of the registry account's fixed header (`chain_id`, `inbox_program`, `count`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHeader {
    pub chain_id: u64,
    pub inbox_program: [u8; 32],
    pub count: u8,
}

/// Validates magic + minimum length and decodes the header (not the entries — see [`entry_at`]).
pub fn read_header(d: &[u8]) -> Result<RegistryHeader, crate::LayoutError> {
    if d.len() < REGISTRY_LEN {
        return Err(crate::LayoutError::TooShort {
            need: REGISTRY_LEN,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    Ok(RegistryHeader {
        chain_id: u64::from_le_bytes(d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].try_into().unwrap()),
        inbox_program: d[OFF_INBOX_PROGRAM..OFF_INBOX_PROGRAM + 32]
            .try_into()
            .unwrap(),
        count: d[OFF_COUNT],
    })
}

/// Decodes entry `i` (`0..MAX_ENTRIES`) regardless of `count` — callers that want only populated
/// entries should bound `i < header.count` themselves (e.g. via [`find`]) — plus its `activation_slot`:
/// `0` when `d` is v1-length (active since genesis), the real stored value when `d` is v2-length.
pub fn entry_at(d: &[u8], i: usize) -> Result<(RegistryEntry, u64), crate::LayoutError> {
    if d.len() < REGISTRY_LEN {
        return Err(crate::LayoutError::TooShort {
            need: REGISTRY_LEN,
            got: d.len(),
        });
    }
    if i >= MAX_ENTRIES {
        return Err(crate::LayoutError::BadMagic); // out-of-range index; reuse an existing variant, no new one needed for a crate this small
    }
    let base = OFF_ENTRIES + i * ENTRY_LEN;
    let entry = RegistryEntry {
        curve: d[base + ENTRY_OFF_CURVE],
        scheme: d[base + ENTRY_OFF_SCHEME],
        vkey_hash: d[base + ENTRY_OFF_VKEY_HASH..base + ENTRY_OFF_VKEY_HASH + 32]
            .try_into()
            .unwrap(),
        layout_id: d[base + ENTRY_OFF_LAYOUT_ID],
    };
    let activation_slot = if d.len() >= REGISTRY_LEN_V2 {
        let aoff = OFF_ACTIVATION + i * ACTIVATION_ENTRY_LEN;
        u64::from_le_bytes(d[aoff..aoff + 8].try_into().unwrap())
    } else {
        0
    };
    Ok((entry, activation_slot))
}

/// Writes entry `i` AND its `activation_slot` into a v2-length (or longer) buffer — the single writer of
/// both the fixed entry bytes and the v2 activation tail (the settlement program's
/// `SetRegistryEntry` is the only caller). Does not touch `count` — the caller decides append vs. replace
/// and updates `count` itself only on append.
pub fn write_entry(
    d: &mut [u8],
    i: usize,
    e: &RegistryEntry,
    activation_slot: u64,
) -> Result<(), crate::LayoutError> {
    if d.len() < REGISTRY_LEN_V2 {
        return Err(crate::LayoutError::TooShort {
            need: REGISTRY_LEN_V2,
            got: d.len(),
        });
    }
    if i >= MAX_ENTRIES {
        return Err(crate::LayoutError::BadMagic);
    }
    let base = OFF_ENTRIES + i * ENTRY_LEN;
    d[base + ENTRY_OFF_CURVE] = e.curve;
    d[base + ENTRY_OFF_SCHEME] = e.scheme;
    d[base + ENTRY_OFF_VKEY_HASH..base + ENTRY_OFF_VKEY_HASH + 32].copy_from_slice(&e.vkey_hash);
    d[base + ENTRY_OFF_LAYOUT_ID] = e.layout_id;
    let aoff = OFF_ACTIVATION + i * ACTIVATION_ENTRY_LEN;
    d[aoff..aoff + 8].copy_from_slice(&activation_slot.to_le_bytes());
    Ok(())
}

/// First populated entry (`index < header.count`) matching `(curve, scheme, vkey_hash)` whose
/// `activation_slot <= at_slot` (an entry registered for a future slot is invisible until then —
/// the same refusal a never-registered vkey gets), if any. Matching on
/// `(curve, scheme)` alone let a proof under a *registered fallback* vkey satisfy a check meant for the
/// *primary* entry (or vice versa) whenever two entries share a curve and scheme — the vkey is part of
/// the identity, not a value to double-check after the fact.
pub fn find(
    d: &[u8],
    curve: u8,
    scheme: u8,
    vkey_hash: &[u8; 32],
    at_slot: u64,
) -> Result<Option<(usize, RegistryEntry)>, crate::LayoutError> {
    let hdr = read_header(d)?;
    for i in 0..(hdr.count as usize).min(MAX_ENTRIES) {
        let (e, activation_slot) = entry_at(d, i)?;
        if activation_slot > at_slot {
            continue;
        }
        if e.curve == curve && e.scheme == scheme && &e.vkey_hash == vkey_hash {
            return Ok(Some((i, e)));
        }
    }
    Ok(None)
}

/// Why [`find_zisk`] could not answer.
#[derive(Debug, PartialEq, Eq)]
pub enum FindZiskError {
    /// The registry bytes did not decode.
    Layout(crate::LayoutError),
    /// Two active entries carry the same `vkey_hash` on BN254 under ZisK schemes. The program refuses to write
    /// that state, so reading it means the account is not one the program wrote.
    TwoActiveMatches,
}

impl From<crate::LayoutError> for FindZiskError {
    fn from(e: crate::LayoutError) -> Self {
        FindZiskError::Layout(e)
    }
}

/// The one active entry whose curve is BN254, whose scheme names a ZisK release and whose `vkey_hash` is
/// `vkey_hash`, whichever release that is. Same visibility rules as [`find`]: only populated entries, and
/// nothing whose `activation_slot > at_slot`, which also hides a retired entry. The proof carries no release;
/// the release is the matching entry's `scheme`. Two active matches are refused, so a vkey can never pick
/// between two releases' keys.
pub fn find_zisk(
    d: &[u8],
    vkey_hash: &[u8; 32],
    at_slot: u64,
) -> Result<Option<(usize, RegistryEntry)>, FindZiskError> {
    let hdr = read_header(d)?;
    let mut found = None;
    for i in 0..(hdr.count as usize).min(MAX_ENTRIES) {
        let (e, activation_slot) = entry_at(d, i)?;
        if activation_slot > at_slot {
            continue;
        }
        if e.curve == CURVE_BN254 && is_zisk_scheme(e.scheme) && &e.vkey_hash == vkey_hash {
            if found.is_some() {
                return Err(FindZiskError::TwoActiveMatches);
            }
            found = Some((i, e));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(chain_id: u64, inbox_program: [u8; 32], entries: &[RegistryEntry]) -> Vec<u8> {
        let mut d = vec![0u8; REGISTRY_LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
        d[OFF_INBOX_PROGRAM..OFF_INBOX_PROGRAM + 32].copy_from_slice(&inbox_program);
        d[OFF_COUNT] = entries.len() as u8;
        for (i, e) in entries.iter().enumerate() {
            let base = OFF_ENTRIES + i * ENTRY_LEN;
            d[base + ENTRY_OFF_CURVE] = e.curve;
            d[base + ENTRY_OFF_SCHEME] = e.scheme;
            d[base + ENTRY_OFF_VKEY_HASH..base + ENTRY_OFF_VKEY_HASH + 32]
                .copy_from_slice(&e.vkey_hash);
            d[base + ENTRY_OFF_LAYOUT_ID] = e.layout_id;
        }
        d
    }

    /// Extends a v1-built buffer to v2 length (zeroed activation tail) — mirrors what
    /// `SetRegistryEntry`'s one-time realloc does on chain.
    fn to_v2(d: &mut Vec<u8>) {
        d.resize(REGISTRY_LEN_V2, 0);
    }

    #[test]
    fn header_and_entries_round_trip() {
        let e0 = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_ZISK_1_2_0,
            vkey_hash: [7u8; 32],
            layout_id: LAYOUT_HEADER_FALLBACK,
        };
        let e1 = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_GROTH16,
            vkey_hash: [9u8; 32],
            layout_id: LAYOUT_HEADER_FALLBACK,
        };
        let d = build(11, [3u8; 32], &[e0, e1]);
        let hdr = read_header(&d).unwrap();
        assert_eq!(hdr.chain_id, 11);
        assert_eq!(hdr.inbox_program, [3u8; 32]);
        assert_eq!(hdr.count, 2);
        assert_eq!(entry_at(&d, 0).unwrap(), (e0, 0));
        assert_eq!(entry_at(&d, 1).unwrap(), (e1, 0));
        // an unpopulated slot decodes as zeroed, not an error
        let (e2, a2) = entry_at(&d, 2).unwrap();
        assert_eq!(e2.curve, 0);
        assert_eq!(e2.vkey_hash, [0u8; 32]);
        assert_eq!(a2, 0);
    }

    #[test]
    fn find_matches_curve_scheme_and_vkey_within_count_only() {
        let e0 = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_ZISK_1_2_0,
            vkey_hash: [7u8; 32],
            layout_id: LAYOUT_HEADER_FALLBACK,
        };
        let d = build(1, [0u8; 32], &[e0]);
        let (idx, got) = find(&d, CURVE_BN254, SCHEME_ZISK_1_2_0, &[7u8; 32], 0)
            .unwrap()
            .unwrap();
        assert_eq!(idx, 0);
        assert_eq!(got, e0);
        assert!(find(&d, CURVE_BN254, SCHEME_GROTH16, &[7u8; 32], 0)
            .unwrap()
            .is_none());
        assert!(find(&d, CURVE_BLS12_381, SCHEME_ZISK_1_2_0, &[7u8; 32], 0)
            .unwrap()
            .is_none());
    }

    /// Two entries share `(curve, scheme)` but carry different vkeys (a primary
    /// entry plus a registered fallback under the same curve/scheme) — `find` must select the one whose
    /// vkey the proof actually carries, never the first (curve, scheme) match.
    #[test]
    fn find_selects_by_vkey_when_curve_and_scheme_collide() {
        let primary = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_ZISK_1_2_0,
            vkey_hash: [1u8; 32],
            layout_id: LAYOUT_HEADER_FALLBACK,
        };
        let fallback = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_ZISK_1_2_0,
            vkey_hash: [2u8; 32],
            layout_id: LAYOUT_HEADER_FALLBACK,
        };
        let d = build(1, [0u8; 32], &[primary, fallback]);
        let (idx0, got0) = find(&d, CURVE_BN254, SCHEME_ZISK_1_2_0, &[1u8; 32], 0)
            .unwrap()
            .unwrap();
        assert_eq!(idx0, 0);
        assert_eq!(got0, primary);
        let (idx1, got1) = find(&d, CURVE_BN254, SCHEME_ZISK_1_2_0, &[2u8; 32], 0)
            .unwrap()
            .unwrap();
        assert_eq!(idx1, 1);
        assert_eq!(got1, fallback);
        // an unregistered vkey under the same (curve, scheme) matches neither entry
        assert!(find(&d, CURVE_BN254, SCHEME_ZISK_1_2_0, &[9u8; 32], 0)
            .unwrap()
            .is_none());
    }

    // --- v2 activation tail ---

    /// Pins the v2 tail's offsets against hand computed values, independently of the arithmetic the
    /// production code uses to derive them — a mutation that swaps the `OFF_ACTIVATION` arithmetic (e.g.
    /// off-by-one-entry) must turn this red.
    #[test]
    fn activation_offsets_and_v2_length_are_pinned() {
        assert_eq!(OFF_ENTRIES, 45);
        assert_eq!(ENTRY_LEN, 35);
        assert_eq!(MAX_ENTRIES, 4);
        assert_eq!(OFF_ACTIVATION, 45 + 4 * 35); // 185
        assert_eq!(REGISTRY_LEN_V2, 185 + 4 * 8); // 217
        assert_eq!(REGISTRY_LEN, 185);
    }

    #[test]
    fn read_header_accepts_both_v1_and_v2_lengths() {
        let d_v1 = build(7, [1u8; 32], &[]);
        assert_eq!(d_v1.len(), REGISTRY_LEN);
        assert_eq!(read_header(&d_v1).unwrap().chain_id, 7);

        let mut d_v2 = d_v1;
        to_v2(&mut d_v2);
        assert_eq!(d_v2.len(), REGISTRY_LEN_V2);
        assert_eq!(read_header(&d_v2).unwrap().chain_id, 7);
    }

    /// A v1-length account has no activation tail at all — every entry reads back `activation_slot == 0`
    /// (active since genesis), never an error.
    #[test]
    fn entry_at_activation_slot_is_zero_on_v1() {
        let e0 = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_ZISK_1_2_0,
            vkey_hash: [7u8; 32],
            layout_id: LAYOUT_ZISK_V1,
        };
        let d = build(1, [0u8; 32], &[e0]);
        assert_eq!(d.len(), REGISTRY_LEN);
        let (got, activation_slot) = entry_at(&d, 0).unwrap();
        assert_eq!(got, e0);
        assert_eq!(activation_slot, 0);
    }

    #[test]
    fn write_entry_and_entry_at_round_trip_activation_slot() {
        let mut d = build(1, [0u8; 32], &[]);
        to_v2(&mut d);
        let e = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_ZISK_1_2_0,
            vkey_hash: [0x44u8; 32],
            layout_id: LAYOUT_ZISK_V1,
        };
        write_entry(&mut d, 0, &e, 123_456).unwrap();
        let (got, activation_slot) = entry_at(&d, 0).unwrap();
        assert_eq!(got, e);
        assert_eq!(activation_slot, 123_456);
        // a different index's activation slot is untouched (still 0)
        let (_, other_activation) = entry_at(&d, 1).unwrap();
        assert_eq!(other_activation, 0);
    }

    /// The delay itself: an entry registered for a future slot must not be found before that slot, and
    /// must be found from it onward (activation is inclusive — "equal is allowed = immediate").
    #[test]
    fn find_skips_entries_whose_activation_slot_is_in_the_future() {
        let mut d = build(1, [0u8; 32], &[]);
        to_v2(&mut d);
        d[OFF_COUNT] = 1;
        let e = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_ZISK_1_2_0,
            vkey_hash: [0x44u8; 32],
            layout_id: LAYOUT_ZISK_V1,
        };
        write_entry(&mut d, 0, &e, 1_000).unwrap();

        assert!(find(&d, CURVE_BN254, SCHEME_ZISK_1_2_0, &e.vkey_hash, 999)
            .unwrap()
            .is_none());
        let (idx, got) = find(&d, CURVE_BN254, SCHEME_ZISK_1_2_0, &e.vkey_hash, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(idx, 0);
        assert_eq!(got, e);
        assert!(
            find(&d, CURVE_BN254, SCHEME_ZISK_1_2_0, &e.vkey_hash, 1_001)
                .unwrap()
                .is_some()
        );
    }

    /// `RETIRED_SLOT` is the tombstone value, and `find` already skips it for every real slot
    /// (`activation_slot > at_slot`) — pinned independently of `SetRegistryEntry`'s own use of the const.
    #[test]
    fn retired_slot_is_u64_max_and_find_skips_it_for_any_real_slot() {
        assert_eq!(RETIRED_SLOT, u64::MAX);

        let mut d = build(1, [0u8; 32], &[]);
        to_v2(&mut d);
        d[OFF_COUNT] = 1;
        let e = RegistryEntry {
            curve: CURVE_BN254,
            scheme: SCHEME_ZISK_1_2_0,
            vkey_hash: [0x55u8; 32],
            layout_id: LAYOUT_ZISK_V1,
        };
        write_entry(&mut d, 0, &e, RETIRED_SLOT).unwrap();

        // Every real slot a chain's Clock can ever report is well below u64::MAX.
        assert!(find(&d, CURVE_BN254, SCHEME_ZISK_1_2_0, &e.vkey_hash, 0)
            .unwrap()
            .is_none());
        assert!(find(
            &d,
            CURVE_BN254,
            SCHEME_ZISK_1_2_0,
            &e.vkey_hash,
            u64::MAX - 1
        )
        .unwrap()
        .is_none());
    }

    // --- ZisK releases: scheme numbers and `find_zisk` ---

    fn zisk_entry(scheme: u8, vk: u8) -> RegistryEntry {
        RegistryEntry {
            curve: CURVE_BN254,
            scheme,
            vkey_hash: [vk; 32],
            layout_id: LAYOUT_ZISK_V1,
        }
    }

    /// A v2-length registry holding `entries`, each with its own activation slot.
    fn v2_with(entries: &[(RegistryEntry, u64)]) -> Vec<u8> {
        let mut d = build(1, [0u8; 32], &[]);
        to_v2(&mut d);
        d[OFF_COUNT] = entries.len() as u8;
        for (i, (e, slot)) in entries.iter().enumerate() {
            write_entry(&mut d, i, e, *slot).unwrap();
        }
        d
    }

    #[test]
    fn find_zisk_returns_the_scheme_2_entry_for_its_programvk() {
        let other = zisk_entry(SCHEME_ZISK_1_2_0, 0x11);
        let wanted = zisk_entry(SCHEME_ZISK_1_3_1, 0x22);
        let d = v2_with(&[(other, 0), (wanted, 0)]);
        let (idx, got) = find_zisk(&d, &[0x22; 32], 0).unwrap().unwrap();
        assert_eq!(idx, 1);
        assert_eq!(got, wanted);
        assert_eq!(got.scheme, SCHEME_ZISK_1_3_1);
        // a vkey nobody registered finds nothing
        assert!(find_zisk(&d, &[0x33; 32], 0).unwrap().is_none());
    }

    #[test]
    fn find_zisk_finds_an_entry_written_before_releases_were_named() {
        // A v1-length account whose only entry carries scheme 1, the value every earlier entry has.
        let e = zisk_entry(1, 0x44);
        let d = build(1, [0u8; 32], &[e]);
        assert_eq!(d.len(), REGISTRY_LEN);
        let (idx, got) = find_zisk(&d, &[0x44; 32], 0).unwrap().unwrap();
        assert_eq!((idx, got), (0, e));
    }

    #[test]
    fn find_zisk_skips_retired_entries() {
        let d = v2_with(&[
            (zisk_entry(SCHEME_ZISK_1_3_1, 0x55), RETIRED_SLOT),
            (zisk_entry(SCHEME_ZISK_1_3_1, 0x66), 0),
        ]);
        assert!(find_zisk(&d, &[0x55; 32], 0).unwrap().is_none());
        assert!(find_zisk(&d, &[0x55; 32], u64::MAX - 1).unwrap().is_none());
        assert!(find_zisk(&d, &[0x66; 32], 0).unwrap().is_some());
    }

    #[test]
    fn find_zisk_skips_entries_that_start_in_the_future() {
        let e = zisk_entry(SCHEME_ZISK_1_3_1, 0x77);
        let d = v2_with(&[(e, 1_000)]);
        assert!(find_zisk(&d, &[0x77; 32], 999).unwrap().is_none());
        assert_eq!(find_zisk(&d, &[0x77; 32], 1_000).unwrap(), Some((0, e)));
        assert_eq!(find_zisk(&d, &[0x77; 32], 1_001).unwrap(), Some((0, e)));
    }

    #[test]
    fn find_zisk_refuses_two_active_matches_under_schemes_1_and_2() {
        let d = v2_with(&[
            (zisk_entry(SCHEME_ZISK_1_2_0, 0x88), 0),
            (zisk_entry(SCHEME_ZISK_1_3_1, 0x88), 0),
        ]);
        assert_eq!(
            find_zisk(&d, &[0x88; 32], 0).unwrap_err(),
            FindZiskError::TwoActiveMatches
        );
        // The same two entries are not ambiguous while one of them is not yet active, or once it is retired.
        let future = v2_with(&[
            (zisk_entry(SCHEME_ZISK_1_2_0, 0x88), 0),
            (zisk_entry(SCHEME_ZISK_1_3_1, 0x88), 500),
        ]);
        let (idx, got) = find_zisk(&future, &[0x88; 32], 499).unwrap().unwrap();
        assert_eq!((idx, got.scheme), (0, SCHEME_ZISK_1_2_0));
        assert_eq!(
            find_zisk(&future, &[0x88; 32], 500).unwrap_err(),
            FindZiskError::TwoActiveMatches
        );
        let retired = v2_with(&[
            (zisk_entry(SCHEME_ZISK_1_2_0, 0x88), RETIRED_SLOT),
            (zisk_entry(SCHEME_ZISK_1_3_1, 0x88), 0),
        ]);
        let (idx, got) = find_zisk(&retired, &[0x88; 32], 10).unwrap().unwrap();
        assert_eq!((idx, got.scheme), (1, SCHEME_ZISK_1_3_1));
    }

    #[test]
    fn find_zisk_refuses_the_same_vkey_twice_under_one_scheme() {
        let e = zisk_entry(SCHEME_ZISK_1_3_1, 0x99);
        let d = v2_with(&[(e, 0), (e, 0)]);
        assert_eq!(
            find_zisk(&d, &[0x99; 32], 0).unwrap_err(),
            FindZiskError::TwoActiveMatches
        );
    }

    #[test]
    fn find_zisk_ignores_other_curves_other_families_and_unassigned_numbers() {
        let groth = RegistryEntry {
            scheme: SCHEME_GROTH16,
            ..zisk_entry(SCHEME_ZISK_1_3_1, 0xaa)
        };
        let bls = RegistryEntry {
            curve: CURVE_BLS12_381,
            ..zisk_entry(SCHEME_ZISK_1_3_1, 0xaa)
        };
        let unassigned = zisk_entry(3, 0xaa);
        let top = zisk_entry(255, 0xaa);
        let d = v2_with(&[(groth, 0), (bls, 0), (unassigned, 0), (top, 0)]);
        assert!(find_zisk(&d, &[0xaa; 32], 0).unwrap().is_none());
    }

    #[test]
    fn find_zisk_reads_only_populated_entries() {
        let mut d = v2_with(&[(zisk_entry(SCHEME_ZISK_1_3_1, 0xbb), 0)]);
        d[OFF_COUNT] = 0;
        assert!(find_zisk(&d, &[0xbb; 32], 0).unwrap().is_none());
    }

    #[test]
    fn find_zisk_passes_layout_errors_through() {
        let d = vec![0u8; REGISTRY_LEN - 1];
        assert!(matches!(
            find_zisk(&d, &[0; 32], 0).unwrap_err(),
            FindZiskError::Layout(crate::LayoutError::TooShort { .. })
        ));
        let mut bad = build(1, [0u8; 32], &[]);
        bad[OFF_MAGIC] ^= 0xff;
        assert_eq!(
            find_zisk(&bad, &[0; 32], 0).unwrap_err(),
            FindZiskError::Layout(crate::LayoutError::BadMagic)
        );
    }

    /// The published release numbers. A row here never changes and a number is never reused: a new
    /// release is appended to `ZISK_RELEASES` with the next unused number and its row is appended here.
    const PUBLISHED_RELEASES: [(u8, &str); 2] = [(1, "1.2.0-alpha"), (2, "1.3.1-alpha")];

    #[test]
    fn zisk_release_numbers_are_never_reused() {
        assert!(ZISK_RELEASES.len() >= PUBLISHED_RELEASES.len());
        for (i, (scheme, name)) in PUBLISHED_RELEASES.iter().enumerate() {
            assert_eq!(
                ZISK_RELEASES[i].scheme, *scheme,
                "row {i} changed its number"
            );
            assert_eq!(ZISK_RELEASES[i].name, *name, "row {i} changed its name");
        }
        // Numbers count up from 1 with no gap, so a skipped or repeated number is a change to the list.
        for (i, r) in ZISK_RELEASES.iter().enumerate() {
            assert_eq!(
                r.scheme as usize,
                i + 1,
                "release numbers must be 1, 2, 3, ..."
            );
        }
        // Names are unique too, and none of them is the Groth16 value.
        for (i, a) in ZISK_RELEASES.iter().enumerate() {
            assert_ne!(a.scheme, SCHEME_GROTH16);
            for b in &ZISK_RELEASES[i + 1..] {
                assert_ne!(a.name, b.name);
            }
        }
    }

    #[test]
    fn release_constants_have_their_published_values() {
        assert_eq!(SCHEME_ZISK_1_2_0, 1);
        assert_eq!(SCHEME_ZISK_1_3_1, 2);
        assert_eq!(SCHEME_GROTH16, 0);
    }

    #[test]
    fn zisk_release_lookup_by_scheme() {
        assert_eq!(zisk_release_name(SCHEME_ZISK_1_2_0), Some("1.2.0-alpha"));
        assert_eq!(zisk_release_name(SCHEME_ZISK_1_3_1), Some("1.3.1-alpha"));
        assert_eq!(zisk_release_name(SCHEME_GROTH16), None);
        assert_eq!(zisk_release_name(3), None);
        assert_eq!(zisk_release_name(255), None);
        assert!(is_zisk_scheme(1) && is_zisk_scheme(2));
        assert!(!is_zisk_scheme(0) && !is_zisk_scheme(3) && !is_zisk_scheme(255));
    }

    #[test]
    fn read_header_rejects_bad_magic() {
        let mut d = build(1, [0u8; 32], &[]);
        d[OFF_MAGIC] ^= 0xff;
        assert_eq!(read_header(&d).unwrap_err(), crate::LayoutError::BadMagic);
    }

    #[test]
    fn read_header_rejects_too_short() {
        let d = vec![0u8; REGISTRY_LEN - 1];
        assert!(matches!(
            read_header(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    #[test]
    fn seeds_golden_bytes_for_a_fixed_chain_id() {
        let s = seeds(0x0102_0304_0506_0708);
        assert_eq!(s[0], b"registry".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_chain_id_and_program() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, 7);
        let (a2, _) = pda(&program, 7);
        assert_eq!(a1, a2);
        let (b, _) = pda(&program, 8);
        assert_ne!(a1, b);
    }
}
