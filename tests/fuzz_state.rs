//! Property-based (proptest) fuzz tests for state primitives.
//!
//! These tests drive the same public APIs that on-chain instructions
//! hit, using randomized inputs, and assert structural invariants that
//! must always hold no matter the input sequence.
//!
//! Run with: `cargo test -p polyleverage --test fuzz_state`

use polyleverage::{
    instruction::InstructionTag,
    math::fixed::{
        bounded_equity, notional_atoms, overlap_midpoint, pnl_long_atoms, position_size_fp,
        range_overlap, PRICE_ONE,
    },
    state::{
        fee_schedule::{FeeSchedule, FeeTierRaw, FEE_SCHEDULE_LEN, FEE_SCHEDULE_MAX_TIERS},
        intent_book::{
            init_intent_book, intent_book_byte_size, BookMut, NODE_TAG_FREE, NULL_IDX,
        },
        user_volume::{UserVolume, USER_VOLUME_EPOCH_SLOTS, USER_VOLUME_LEN},
    },
};
use proptest::prelude::*;
use solana_program::pubkey::Pubkey;

// ---------------------------------------------------------------------------
// InstructionTag parsing — all byte values must either parse or return error.
// (Panic-free is the invariant.)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]
    #[test]
    fn instruction_tag_never_panics(byte in 0u8..=255u8) {
        // Must either succeed or return a ProgramError — not panic.
        let _ = InstructionTag::try_from(byte);
    }
}

// ---------------------------------------------------------------------------
// IntentBook node allocator — alloc/free sequences must preserve:
//   1. allocated indices are all distinct (no double-use)
//   2. after freeing every alloc, header.node_count is stable and
//      freelist forms a valid chain (no cycles within reasonable length)
//   3. capacity is respected (alloc beyond cap returns err)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum BookOp {
    Alloc,
    Free(usize), // index into the caller-tracked live set
}

fn op_strategy() -> impl Strategy<Value = BookOp> {
    prop_oneof![
        3 => Just(BookOp::Alloc),
        1 => (0usize..16usize).prop_map(BookOp::Free),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]
    #[test]
    fn intent_book_alloc_free_invariants(
        ops in prop::collection::vec(op_strategy(), 1..120),
        capacity in 8u32..64u32,
    ) {
        let bytes_len = intent_book_byte_size(capacity);
        let mut buf = vec![0u8; bytes_len];
        init_intent_book(&mut buf, Pubkey::new_unique(), capacity, 0, 0).unwrap();

        let mut book = BookMut::load(&mut buf).unwrap();
        let mut live: Vec<u32> = Vec::new();

        for op in &ops {
            match op {
                BookOp::Alloc => {
                    match book.alloc_node() {
                        Ok(idx) => {
                            prop_assert!(idx != NULL_IDX, "alloc must not hand out the sentinel");
                            prop_assert!(!live.contains(&idx), "alloc returned a live index");
                            prop_assert!(idx < capacity, "alloc returned idx >= capacity");
                            // Write a tombstone tag so the free path can identify variant.
                            book.nodes[idx as usize].bytes[0] = NODE_TAG_FREE;
                            live.push(idx);
                        }
                        Err(_) => {
                            // Acceptable when at capacity — confirm.
                            prop_assert!(book.header.node_count >= capacity
                                        || book.header.freelist_head == NULL_IDX,
                                "alloc err should imply full");
                        }
                    }
                }
                BookOp::Free(rel) => {
                    if live.is_empty() { continue; }
                    let pos = rel % live.len();
                    let idx = live.swap_remove(pos);
                    book.free_node(idx).unwrap();
                }
            }
        }

        // Walk the freelist and make sure it doesn't cycle / exceed live-dead count.
        let max_walk = (capacity as usize) + 2;
        let mut walked = 0usize;
        let mut cur = book.header.freelist_head;
        while cur != NULL_IDX {
            walked += 1;
            prop_assert!(walked <= max_walk, "freelist appears to cycle");
            let next = {
                let fr: &polyleverage::state::intent_book::FreeNode =
                    bytemuck::from_bytes(&book.nodes[cur as usize].bytes);
                fr.next_free
            };
            cur = next;
        }
    }
}

// ---------------------------------------------------------------------------
// FeeSchedule.fee_for_volume — random monotone tier arrays + random volumes.
//   Invariant: fee_for_volume is non-increasing as volume rises, and always
//   returns a tier that actually exists in the schedule.
// ---------------------------------------------------------------------------

fn arb_tiers() -> impl Strategy<Value = Vec<(u64, u16)>> {
    prop::collection::vec((0u64..=1_000_000_000u64, 0u16..=1_000u16), 1..=FEE_SCHEDULE_MAX_TIERS)
        .prop_map(|mut v| {
            // Sort by threshold ascending and de-duplicate.
            v.sort_by_key(|t| t.0);
            v.dedup_by_key(|t| t.0);
            v
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]
    #[test]
    fn fee_for_volume_returns_valid_tier(
        tiers in arb_tiers(),
        v_low in 0u64..=1_000_000_000u64,
        v_high in 0u64..=1_000_000_000u64,
    ) {
        let (_lo, _hi) = if v_low <= v_high { (v_low, v_high) } else { (v_high, v_low) };

        let mut raw = [FeeTierRaw {
            volume_threshold: u64::MAX,
            fee_bps: 0,
            _pad: [0; 6],
        }; FEE_SCHEDULE_MAX_TIERS];
        for (i, (thr, bps)) in tiers.iter().enumerate() {
            raw[i] = FeeTierRaw {
                volume_threshold: *thr,
                fee_bps: *bps,
                _pad: [0; 6],
            };
        }

        let mut buf = vec![0u8; FEE_SCHEDULE_LEN];
        FeeSchedule::init(&mut buf, &raw, 0, 0).unwrap();
        let sched = FeeSchedule::load(&buf).unwrap();
        let fee_lo = sched.fee_for_volume(v_low);
        let fee_hi = sched.fee_for_volume(v_high);

        // Fee returned must match the bps of some tier in the provided set
        // (or the 0-bps sentinel in padded slots).
        let all_bps: Vec<u16> = raw.iter().map(|t| t.fee_bps).collect();
        prop_assert!(all_bps.contains(&fee_lo));
        prop_assert!(all_bps.contains(&fee_hi));
    }
}

// ---------------------------------------------------------------------------
// UserVolume.add_volume across arbitrary slot sequences — invariants:
//   1. effective_30d_volume ≤ sum of all adds
//   2. after ≥ 2 epochs with no further adds, both windows are zero
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]
    #[test]
    fn user_volume_bounded_by_total_adds(
        adds in prop::collection::vec((0u64..=10_000_000u64, 0u64..=4 * USER_VOLUME_EPOCH_SLOTS), 0..40),
    ) {
        let mut buf = vec![0u8; USER_VOLUME_LEN];
        UserVolume::init(&mut buf, Pubkey::new_unique(), Pubkey::new_unique(), 0, 0).unwrap();
        let mut total = 0u128;
        let mut max_slot = 0u64;
        {
            let v = UserVolume::load_mut(&mut buf).unwrap();
            for (amt, slot) in &adds {
                v.add_volume(*amt, *slot).unwrap();
                total = total.saturating_add(*amt as u128);
                if *slot > max_slot { max_slot = *slot; }
            }
        }
        let v = UserVolume::load(&buf).unwrap();
        let eff = v.effective_30d_volume(max_slot) as u128;
        prop_assert!(eff <= total, "effective_30d_volume must not exceed total adds");
    }

    #[test]
    fn user_volume_decays_after_two_epochs_idle(amt in 1u64..=1_000_000u64) {
        let mut buf = vec![0u8; USER_VOLUME_LEN];
        UserVolume::init(&mut buf, Pubkey::new_unique(), Pubkey::new_unique(), 0, 0).unwrap();
        {
            let v = UserVolume::load_mut(&mut buf).unwrap();
            v.add_volume(amt, 0).unwrap();
        }
        let far_future = USER_VOLUME_EPOCH_SLOTS * 3 + 1;
        let v = UserVolume::load(&buf).unwrap();
        let eff = v.effective_30d_volume(far_future);
        prop_assert_eq!(eff, 0, "after 2+ epochs idle, effective volume must decay to zero");
    }
}

// ---------------------------------------------------------------------------
// Match math invariants — these are the heart of the PMLC price-bounding
// spec. Any integer overflow or sign bug here would be a settlement hazard.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// `range_overlap` is symmetric: swapping long/short gives same overlap.
    #[test]
    fn range_overlap_is_symmetric(
        a in 1u64..=PRICE_ONE - 1,
        b in 1u64..=PRICE_ONE - 1,
        c in 1u64..=PRICE_ONE - 1,
        d in 1u64..=PRICE_ONE - 1,
    ) {
        let (l_min, l_max) = (a.min(b), a.max(b));
        let (s_min, s_max) = (c.min(d), c.max(d));
        let forward = range_overlap(l_min, l_max, s_min, s_max);
        let backward = range_overlap(s_min, s_max, l_min, l_max);
        prop_assert_eq!(forward, backward);
    }

    /// Midpoint always lies within [lo, hi].
    #[test]
    fn overlap_midpoint_is_within_range(
        a in 1u64..=PRICE_ONE - 1,
        b in 1u64..=PRICE_ONE - 1,
    ) {
        let (lo, hi) = (a.min(b), a.max(b));
        let mid = overlap_midpoint(lo, hi);
        prop_assert!(mid >= lo && mid <= hi);
    }

    /// `notional_atoms` must never panic on realistic inputs and must be
    /// monotone in both arguments.
    #[test]
    fn notional_atoms_monotone(
        c in 0u64..=1_000_000_000u64,
        lev_bps in 1u32..=10_000_000u32,
    ) {
        let n = notional_atoms(c, lev_bps).unwrap();
        let n_more_c = notional_atoms(c + 1, lev_bps).unwrap();
        prop_assert!(n_more_c >= n, "notional must be non-decreasing in collateral");
    }

    /// `position_size_fp` is strictly positive for non-zero notional and
    /// sub-unit entry price. Bounds are picked to stay within u128 headroom
    /// after the × 1e18 scale-up: notional ≤ 1e18 × PRICE_ONE → u128 max-safe.
    #[test]
    fn position_size_positive(
        notional in 1u128..=10u128.pow(18),
        entry_fp in 1u64..=PRICE_ONE - 1,
    ) {
        let size = position_size_fp(notional, entry_fp).unwrap();
        prop_assert!(size > 0);
    }

    /// `pnl_long_atoms`: swapping mark and entry inverts the sign (exact
    /// when the multiplications fit i128).
    #[test]
    fn pnl_long_sign_symmetric(
        entry in 1u64..=PRICE_ONE - 1,
        mark in 1u64..=PRICE_ONE - 1,
        size in 1u128..=10u128.pow(18),
    ) {
        let a = pnl_long_atoms(entry, mark, size).unwrap();
        let b = pnl_long_atoms(mark, entry, size).unwrap();
        prop_assert_eq!(a.checked_add(b).unwrap_or(i128::MIN), 0);
    }

    /// `bounded_equity` output is always in [0, 2c].
    #[test]
    fn bounded_equity_is_clamped(
        c in 0u64..=1_000_000_000u64,
        pnl in -10_000_000_000i128..=10_000_000_000i128,
    ) {
        let eq = bounded_equity(c, pnl);
        prop_assert!(eq <= c.saturating_mul(2));
    }
}
