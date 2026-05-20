# Redesign: interval book → single-price CLOB

## Why

An intent today carries a price *range* `[min, max]` and matching is interval
overlap. The economics make the range pointless and mildly harmful:

- Per contract, collateral, leverage, and notional are fixed by the instrument
  (`notional = collateral_bucket × leverage_bps`). Entry price never enters.
- `pnl_long = notional × (mark/entry − 1)`. A long therefore always prefers a
  lower entry; a short always prefers a higher one. Entry price is a pure
  zero-sum reference between the two parties.
- The "good side" bound (a long's `min`, a short's `max`) only ever excludes
  counterparties offering a strictly better price. A single limit price
  ("this price or better") matches a superset and is what every order-book
  perp does.

So: an intent becomes `side + one limit price + contract count`. The book
becomes a standard single-price CLOB. Solvency is untouched — it comes from
the `bounded_equity` clamp and the `2c` escrow per PMLC, neither of which
referenced the range.

## Design

- `IntentNode`: replace `min_price_fp` / `max_price_fp` / `subtree_max_fp`
  with a single `price_fp`. Node stays 96 bytes (pad the freed 16).
- Tree stays a red-black tree, keyed `(price, post_seq, id)`. **Key direction
  is side-aware**: longs sort price-descending, shorts price-ascending, so a
  plain in-order traversal of either tree yields best-first with FIFO
  tiebreak. Drop the interval augmentation (`subtree_max_fp`, `recompute_*`).
- Replace `for_each_containing` / `for_each_overlapping` / `first_active_containing`
  with `for_each_best_first` + `first_live` + `tree_minimum`/`tree_maximum`.
- Matching: a long crosses a short iff `long.price >= short.price`; entry is
  the midpoint. `scan_for_first_valid_pair` = best live long + best live
  short, cross check. `find_overlap_on_side` = best live crossing counterparty.
- PMLC reentry fields collapse from min/max pairs to single prices.

## Checklist

- [ ] 1. `math/fixed.rs`: rename `overlap_midpoint`→`price_midpoint`, drop
  `range_overlap`/`validate_range`, add `validate_price_ticked`. Fix tests.
- [ ] 2. `state/intent_book.rs`: `IntentNode` single `price_fp` + pad.
- [ ] 3. `state/pmlc.rs`: reentry min/max → single price, grow `_reserved`.
- [ ] 4. `state/intent_tree.rs`: side-aware key, drop augmentation, drop
  interval queries, add `for_each_best_first`/`first_live`/min-max. Rewrite tests.
- [ ] 5. `processor/match_ix.rs`: rewrite `find_overlap_on_side`,
  `scan_for_first_valid_pair`, cross logic in `match_pair_core`. Rewrite tests.
- [ ] 6. `processor/intent.rs`: `PostIntentArgs` single price, validation,
  node construction, inline-match call.
- [ ] 7. `processor/reentry.rs` + `processor/substitute.rs`: single price.
- [ ] 8. `instruction.rs`: `PostIntentArgs` single price.
- [ ] 9. Build `cargo build-sbf` clean; `cargo test` (program unit tests) pass.
- [ ] 10. Update `polyleverage-simulator` driver + scenario + tests + benchmarks.
- [ ] 11. Update `docs/architecture.md`.
