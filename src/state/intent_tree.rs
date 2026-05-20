//! Price-sorted red-black tree over `IntentNode`s in the book node pool —
//! one tree per side, forming a standard single-price limit order book.
//!
//! Ordering key: `(price, post_seq ASC, id ASC)`. The price direction is
//! **side-aware**: the long (bid) tree sorts price-descending and the short
//! (ask) tree sorts price-ascending, so a plain in-order traversal of either
//! tree yields the most aggressive order first, with FIFO tiebreak. The
//! matcher relies on this via `for_each_best_first` / `first_live`.
//!
//! All operations take a `&mut BookMut<'_>` and a `Side` tag selecting which
//! root to use. Node index `0` is the null sentinel.

use solana_program::program_error::ProgramError;

use crate::error::PolyleverageError;

use super::intent_book::{BookMut, IntentNode, NULL_IDX, RB_BLACK, RB_RED, SIDE_LONG, SIDE_SHORT};

/// Order key: `(price, post_seq, id)`. Longs sort price-descending so the
/// highest bid is leftmost; shorts sort price-ascending so the lowest ask is
/// leftmost. In-order traversal of either tree is therefore best-first.
#[inline]
fn cmp_key(side: u8, a: &IntentNode, b: &IntentNode) -> core::cmp::Ordering {
    use core::cmp::Ordering::*;
    let price = if side == SIDE_LONG {
        b.price_fp.cmp(&a.price_fp)
    } else {
        a.price_fp.cmp(&b.price_fp)
    };
    match price {
        Equal => match a.post_seq.cmp(&b.post_seq) {
            Equal => a.id.cmp(&b.id),
            other => other,
        },
        other => other,
    }
}

// --- Root access ------------------------------------------------------------

#[inline]
fn tree_root(book: &BookMut, side: u8) -> Result<u32, ProgramError> {
    match side {
        SIDE_LONG => Ok(book.header.long_tree_root),
        SIDE_SHORT => Ok(book.header.short_tree_root),
        _ => Err(PolyleverageError::InvalidInstructionData.into()),
    }
}

#[inline]
fn set_tree_root(book: &mut BookMut, side: u8, root: u32) -> Result<(), ProgramError> {
    match side {
        SIDE_LONG => book.header.long_tree_root = root,
        SIDE_SHORT => book.header.short_tree_root = root,
        _ => return Err(PolyleverageError::InvalidInstructionData.into()),
    }
    Ok(())
}

// --- Field accessors --------------------------------------------------------
// Separated so we can use multiple disjoint borrows when needed.

#[inline]
fn get_left(book: &BookMut, idx: u32) -> Result<u32, ProgramError> {
    if idx == NULL_IDX {
        return Ok(NULL_IDX);
    }
    Ok(book.intent(idx)?.left)
}

#[inline]
fn get_right(book: &BookMut, idx: u32) -> Result<u32, ProgramError> {
    if idx == NULL_IDX {
        return Ok(NULL_IDX);
    }
    Ok(book.intent(idx)?.right)
}

#[inline]
fn get_parent(book: &BookMut, idx: u32) -> Result<u32, ProgramError> {
    if idx == NULL_IDX {
        return Ok(NULL_IDX);
    }
    Ok(book.intent(idx)?.parent)
}

#[inline]
fn get_color(book: &BookMut, idx: u32) -> Result<u8, ProgramError> {
    // Null nodes are treated as BLACK per standard RB convention.
    if idx == NULL_IDX {
        return Ok(RB_BLACK);
    }
    Ok(book.intent(idx)?.color)
}

#[inline]
fn set_left(book: &mut BookMut, idx: u32, v: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    book.intent_mut(idx)?.left = v;
    Ok(())
}

#[inline]
fn set_right(book: &mut BookMut, idx: u32, v: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    book.intent_mut(idx)?.right = v;
    Ok(())
}

#[inline]
fn set_parent(book: &mut BookMut, idx: u32, v: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    book.intent_mut(idx)?.parent = v;
    Ok(())
}

#[inline]
fn set_color(book: &mut BookMut, idx: u32, c: u8) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    book.intent_mut(idx)?.color = c;
    Ok(())
}

// --- Rotations --------------------------------------------------------------
// Standard RB rotations.

fn rotate_left(book: &mut BookMut, side: u8, x: u32) -> Result<(), ProgramError> {
    let y = get_right(book, x)?;
    if y == NULL_IDX {
        return Err(ProgramError::InvalidAccountData); // invariant
    }
    let y_left = get_left(book, y)?;
    set_right(book, x, y_left)?;
    if y_left != NULL_IDX {
        set_parent(book, y_left, x)?;
    }
    let x_parent = get_parent(book, x)?;
    set_parent(book, y, x_parent)?;
    if x_parent == NULL_IDX {
        set_tree_root(book, side, y)?;
    } else {
        if get_left(book, x_parent)? == x {
            set_left(book, x_parent, y)?;
        } else {
            set_right(book, x_parent, y)?;
        }
    }
    set_left(book, y, x)?;
    set_parent(book, x, y)?;
    Ok(())
}

fn rotate_right(book: &mut BookMut, side: u8, x: u32) -> Result<(), ProgramError> {
    let y = get_left(book, x)?;
    if y == NULL_IDX {
        return Err(ProgramError::InvalidAccountData);
    }
    let y_right = get_right(book, y)?;
    set_left(book, x, y_right)?;
    if y_right != NULL_IDX {
        set_parent(book, y_right, x)?;
    }
    let x_parent = get_parent(book, x)?;
    set_parent(book, y, x_parent)?;
    if x_parent == NULL_IDX {
        set_tree_root(book, side, y)?;
    } else {
        if get_right(book, x_parent)? == x {
            set_right(book, x_parent, y)?;
        } else {
            set_left(book, x_parent, y)?;
        }
    }
    set_right(book, y, x)?;
    set_parent(book, x, y)?;
    Ok(())
}

// --- Insert ----------------------------------------------------------------

/// Insert node at `idx` into the `side` tree. The node must already be
/// populated (price/seq/id/etc) and have `left = right = parent = 0`,
/// `color = RED`.
pub fn insert(book: &mut BookMut, side: u8, idx: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Err(ProgramError::InvalidAccountData);
    }

    // Ensure starting state.
    {
        let n = book.intent_mut(idx)?;
        n.left = NULL_IDX;
        n.right = NULL_IDX;
        n.parent = NULL_IDX;
        n.color = RB_RED;
    }

    let mut root = tree_root(book, side)?;
    if root == NULL_IDX {
        // Empty tree: new node is root, colored BLACK.
        set_color(book, idx, RB_BLACK)?;
        set_tree_root(book, side, idx)?;
        return Ok(());
    }

    // BST insert.
    let mut parent = NULL_IDX;
    let mut go_left = false;
    loop {
        parent = root;
        // Compare new node with root.
        let ord = {
            let a = book.intent(idx)?;
            let b = book.intent(root)?;
            cmp_key(side, a, b)
        };
        match ord {
            core::cmp::Ordering::Less => {
                go_left = true;
                let l = get_left(book, root)?;
                if l == NULL_IDX {
                    break;
                }
                root = l;
            }
            core::cmp::Ordering::Greater | core::cmp::Ordering::Equal => {
                go_left = false;
                let r = get_right(book, root)?;
                if r == NULL_IDX {
                    break;
                }
                root = r;
            }
        }
    }
    set_parent(book, idx, parent)?;
    if go_left {
        set_left(book, parent, idx)?;
    } else {
        set_right(book, parent, idx)?;
    }

    // RB insert-fixup.
    insert_fixup(book, side, idx)?;
    Ok(())
}

fn insert_fixup(book: &mut BookMut, side: u8, mut z: u32) -> Result<(), ProgramError> {
    while get_parent(book, z)? != NULL_IDX && get_color(book, get_parent(book, z)?)? == RB_RED {
        let p = get_parent(book, z)?;
        let gp = get_parent(book, p)?;
        if p == get_left(book, gp)? {
            let y = get_right(book, gp)?; // uncle
            if y != NULL_IDX && get_color(book, y)? == RB_RED {
                // Case 1: uncle red
                set_color(book, p, RB_BLACK)?;
                set_color(book, y, RB_BLACK)?;
                set_color(book, gp, RB_RED)?;
                z = gp;
            } else {
                if z == get_right(book, p)? {
                    // Case 2: z is right child → rotate left around p
                    z = p;
                    rotate_left(book, side, z)?;
                }
                // Case 3: z is left child
                let p = get_parent(book, z)?;
                let gp = get_parent(book, p)?;
                set_color(book, p, RB_BLACK)?;
                set_color(book, gp, RB_RED)?;
                rotate_right(book, side, gp)?;
            }
        } else {
            // Mirror of the above (parent is right child)
            let y = get_left(book, gp)?;
            if y != NULL_IDX && get_color(book, y)? == RB_RED {
                set_color(book, p, RB_BLACK)?;
                set_color(book, y, RB_BLACK)?;
                set_color(book, gp, RB_RED)?;
                z = gp;
            } else {
                if z == get_left(book, p)? {
                    z = p;
                    rotate_right(book, side, z)?;
                }
                let p = get_parent(book, z)?;
                let gp = get_parent(book, p)?;
                set_color(book, p, RB_BLACK)?;
                set_color(book, gp, RB_RED)?;
                rotate_left(book, side, gp)?;
            }
        }
    }
    let root = tree_root(book, side)?;
    if root != NULL_IDX {
        set_color(book, root, RB_BLACK)?;
    }
    Ok(())
}

// --- Remove ----------------------------------------------------------------

/// Minimum (leftmost) descendant of `idx`. Returns `idx` itself if it has no left child.
fn tree_minimum(book: &BookMut, mut idx: u32) -> Result<u32, ProgramError> {
    while idx != NULL_IDX {
        let l = get_left(book, idx)?;
        if l == NULL_IDX {
            return Ok(idx);
        }
        idx = l;
    }
    Ok(NULL_IDX)
}

/// Replace subtree at `u` with subtree at `v`. Also updates `v`'s parent pointer.
fn transplant(book: &mut BookMut, side: u8, u: u32, v: u32) -> Result<(), ProgramError> {
    let up = get_parent(book, u)?;
    if up == NULL_IDX {
        set_tree_root(book, side, v)?;
    } else if u == get_left(book, up)? {
        set_left(book, up, v)?;
    } else {
        set_right(book, up, v)?;
    }
    // Always set v's parent, even if v == NULL_IDX (we'd no-op for null).
    if v != NULL_IDX {
        set_parent(book, v, up)?;
    }
    Ok(())
}

/// Remove node `z` from the `side` tree. Node fields are NOT zeroed (caller can
/// repurpose or return slot to freelist).
pub fn remove(book: &mut BookMut, side: u8, z: u32) -> Result<(), ProgramError> {
    if z == NULL_IDX {
        return Err(PolyleverageError::IntentNotFound.into());
    }
    let mut y = z;
    let mut y_original_color = get_color(book, y)?;
    let x;
    let x_parent;

    let z_left = get_left(book, z)?;
    let z_right = get_right(book, z)?;

    if z_left == NULL_IDX {
        x = z_right;
        let zp = get_parent(book, z)?;
        transplant(book, side, z, z_right)?;
        x_parent = if x == NULL_IDX {
            zp
        } else {
            get_parent(book, x)?
        };
    } else if z_right == NULL_IDX {
        x = z_left;
        let zp = get_parent(book, z)?;
        transplant(book, side, z, z_left)?;
        x_parent = if x == NULL_IDX {
            zp
        } else {
            get_parent(book, x)?
        };
    } else {
        y = tree_minimum(book, z_right)?;
        y_original_color = get_color(book, y)?;
        x = get_right(book, y)?;
        if get_parent(book, y)? == z {
            x_parent = y;
            if x != NULL_IDX {
                set_parent(book, x, y)?;
            }
        } else {
            let yp = get_parent(book, y)?;
            transplant(book, side, y, get_right(book, y)?)?;
            set_right(book, y, z_right)?;
            set_parent(book, z_right, y)?;
            x_parent = yp;
        }
        transplant(book, side, z, y)?;
        set_left(book, y, z_left)?;
        set_parent(book, z_left, y)?;
        let z_color = get_color(book, z)?;
        set_color(book, y, z_color)?;
    }

    if y_original_color == RB_BLACK {
        remove_fixup(book, side, x, x_parent)?;
    }
    Ok(())
}

fn remove_fixup(
    book: &mut BookMut,
    side: u8,
    mut x: u32,
    mut x_parent: u32,
) -> Result<(), ProgramError> {
    while x != tree_root(book, side)? && get_color(book, x)? == RB_BLACK {
        if x == get_left(book, x_parent)? {
            let mut w = get_right(book, x_parent)?;
            if get_color(book, w)? == RB_RED {
                set_color(book, w, RB_BLACK)?;
                set_color(book, x_parent, RB_RED)?;
                rotate_left(book, side, x_parent)?;
                w = get_right(book, x_parent)?;
            }
            if get_color(book, get_left(book, w)?)? == RB_BLACK
                && get_color(book, get_right(book, w)?)? == RB_BLACK
            {
                set_color(book, w, RB_RED)?;
                x = x_parent;
                x_parent = get_parent(book, x)?;
            } else {
                if get_color(book, get_right(book, w)?)? == RB_BLACK {
                    let wl = get_left(book, w)?;
                    set_color(book, wl, RB_BLACK)?;
                    set_color(book, w, RB_RED)?;
                    rotate_right(book, side, w)?;
                    w = get_right(book, x_parent)?;
                }
                let xp_color = get_color(book, x_parent)?;
                set_color(book, w, xp_color)?;
                set_color(book, x_parent, RB_BLACK)?;
                let wr = get_right(book, w)?;
                set_color(book, wr, RB_BLACK)?;
                rotate_left(book, side, x_parent)?;
                x = tree_root(book, side)?;
                break;
            }
        } else {
            let mut w = get_left(book, x_parent)?;
            if get_color(book, w)? == RB_RED {
                set_color(book, w, RB_BLACK)?;
                set_color(book, x_parent, RB_RED)?;
                rotate_right(book, side, x_parent)?;
                w = get_left(book, x_parent)?;
            }
            if get_color(book, get_right(book, w)?)? == RB_BLACK
                && get_color(book, get_left(book, w)?)? == RB_BLACK
            {
                set_color(book, w, RB_RED)?;
                x = x_parent;
                x_parent = get_parent(book, x)?;
            } else {
                if get_color(book, get_left(book, w)?)? == RB_BLACK {
                    let wr = get_right(book, w)?;
                    set_color(book, wr, RB_BLACK)?;
                    set_color(book, w, RB_RED)?;
                    rotate_left(book, side, w)?;
                    w = get_left(book, x_parent)?;
                }
                let xp_color = get_color(book, x_parent)?;
                set_color(book, w, xp_color)?;
                set_color(book, x_parent, RB_BLACK)?;
                let wl = get_left(book, w)?;
                set_color(book, wl, RB_BLACK)?;
                rotate_right(book, side, x_parent)?;
                x = tree_root(book, side)?;
                break;
            }
        }
    }
    if x != NULL_IDX {
        set_color(book, x, RB_BLACK)?;
    }
    Ok(())
}

// --- Best-first traversal --------------------------------------------------

/// Visit every intent on `side` in best-first order — most aggressive price
/// first, FIFO within a price level. This is a plain in-order traversal; the
/// side-aware key (see `cmp_key`) makes in-order equal best-first.
///
/// `visitor` returns `true` to continue or `false` to stop early. `O(log n)`
/// to reach the best order, `O(k)` to visit `k` of them.
pub fn for_each_best_first<F>(
    book: &BookMut,
    side: u8,
    mut visitor: F,
) -> Result<(), ProgramError>
where
    F: FnMut(u32, &IntentNode) -> bool,
{
    let root = tree_root(book, side)?;
    if root == NULL_IDX {
        return Ok(());
    }
    // Iterative stack-based in-order walk. Fixed-size stack avoids heap
    // allocation; RB-tree depth is at most 2·log2(n) — 64 is comfortable.
    const STACK_CAP: usize = 64;
    let mut stack: [u32; STACK_CAP] = [NULL_IDX; STACK_CAP];
    let mut sp: usize = 0;
    let mut cur = root;

    loop {
        while cur != NULL_IDX {
            if sp >= STACK_CAP {
                return Err(PolyleverageError::ArithmeticOverflow.into());
            }
            stack[sp] = cur;
            sp += 1;
            cur = get_left(book, cur)?;
        }
        if sp == 0 {
            break;
        }
        sp -= 1;
        let top = stack[sp];
        let node = book.intent(top)?;
        if !visitor(top, node) {
            return Ok(());
        }
        cur = node.right;
    }
    Ok(())
}

/// Return the best-priced live intent on `side` — positive
/// `contracts_remaining` and not expired. `(idx, node_copy)` or `None`.
pub fn first_live(
    book: &BookMut,
    side: u8,
    now_slot: u64,
) -> Result<Option<(u32, IntentNode)>, ProgramError> {
    let mut found: Option<(u32, IntentNode)> = None;
    for_each_best_first(book, side, |idx, node| {
        if node.contracts_remaining > 0 && node.expiration_slot > now_slot {
            found = Some((idx, *node));
            false // stop — first one reached is the best price
        } else {
            true
        }
    })?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        init_intent_book, intent_book_byte_size, BookMut, IntentNode, NODE_TAG_INTENT,
    };
    use bytemuck::Zeroable;
    use solana_program::pubkey::Pubkey;

    fn mk_book(capacity: u32) -> Vec<u8> {
        let mut buf = vec![0u8; intent_book_byte_size(capacity)];
        init_intent_book(&mut buf, Pubkey::new_unique(), capacity, 0, 0).unwrap();
        buf
    }

    fn mk_intent(side: u8, price: u64, seq: u64, id: u64) -> IntentNode {
        IntentNode {
            tag: NODE_TAG_INTENT,
            side,
            color: RB_RED,
            flags: 0,
            left: NULL_IDX,
            right: NULL_IDX,
            parent: NULL_IDX,
            _pad0: [0; 2],
            price_fp: price,
            _pad1: [0; 2],
            id,
            owner_seat: 1,
            contracts_total: 1,
            contracts_remaining: 1,
            expiration_slot: u64::MAX,
            post_seq: seq,
            reserved_collateral: 0,
            fee_buffer: 0,
        }
    }

    /// Insert an intent and return its node index.
    fn put(book: &mut BookMut, side: u8, price: u64, seq: u64, id: u64) -> u32 {
        let idx = book.alloc_node().unwrap();
        book.write_intent(idx, mk_intent(side, price, seq, id)).unwrap();
        insert(book, side, idx).unwrap();
        idx
    }

    /// Collect `(idx, price, post_seq)` in best-first order.
    fn best_first(book: &BookMut, side: u8) -> Vec<(u32, u64, u64)> {
        let mut out = Vec::new();
        for_each_best_first(book, side, |idx, n| {
            out.push((idx, n.price_fp, n.post_seq));
            true
        })
        .unwrap();
        out
    }

    #[test]
    fn test_single_insert() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        let idx = put(&mut book, SIDE_LONG, 500, 1, 1);
        assert_eq!(book.header.long_tree_root, idx);
        assert_eq!(get_color(&book, idx).unwrap(), RB_BLACK);
        assert_eq!(best_first(&book, SIDE_LONG), vec![(idx, 500, 1)]);
    }

    #[test]
    fn test_best_first_long_descending() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        // Insert in shuffled price order; best-first must yield highest first.
        for (seq, price) in [(1, 300u64), (2, 100), (3, 500), (4, 200), (5, 400)] {
            put(&mut book, SIDE_LONG, price, seq, seq);
        }
        let prices: Vec<u64> = best_first(&book, SIDE_LONG).iter().map(|t| t.1).collect();
        assert_eq!(prices, vec![500, 400, 300, 200, 100]);
    }

    #[test]
    fn test_best_first_short_ascending() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        for (seq, price) in [(1, 300u64), (2, 100), (3, 500), (4, 200), (5, 400)] {
            put(&mut book, SIDE_SHORT, price, seq, seq);
        }
        let prices: Vec<u64> = best_first(&book, SIDE_SHORT).iter().map(|t| t.1).collect();
        assert_eq!(prices, vec![100, 200, 300, 400, 500]);
    }

    #[test]
    fn test_fifo_within_price_level() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        // Three longs at the same price, posted seq 7, 3, 9.
        put(&mut book, SIDE_LONG, 500, 7, 1);
        put(&mut book, SIDE_LONG, 500, 3, 2);
        put(&mut book, SIDE_LONG, 500, 9, 3);
        let seqs: Vec<u64> = best_first(&book, SIDE_LONG).iter().map(|t| t.2).collect();
        // Same price → FIFO by post_seq.
        assert_eq!(seqs, vec![3, 7, 9]);
    }

    #[test]
    fn test_remove_preserves_order() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        let a = put(&mut book, SIDE_SHORT, 100, 1, 1);
        let b = put(&mut book, SIDE_SHORT, 300, 2, 2);
        let c = put(&mut book, SIDE_SHORT, 500, 3, 3);
        remove(&mut book, SIDE_SHORT, b).unwrap();
        let idxs: Vec<u32> = best_first(&book, SIDE_SHORT).iter().map(|t| t.0).collect();
        assert_eq!(idxs, vec![a, c]);
    }

    #[test]
    fn test_first_live_skips_dead() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        // Best price (lowest ask) is exhausted; next is expired; third is live.
        let a = book.alloc_node().unwrap();
        let mut na = mk_intent(SIDE_SHORT, 100, 1, 1);
        na.contracts_remaining = 0;
        book.write_intent(a, na).unwrap();
        insert(&mut book, SIDE_SHORT, a).unwrap();

        let b = book.alloc_node().unwrap();
        let mut nb = mk_intent(SIDE_SHORT, 200, 2, 2);
        nb.expiration_slot = 50;
        book.write_intent(b, nb).unwrap();
        insert(&mut book, SIDE_SHORT, b).unwrap();

        let c = put(&mut book, SIDE_SHORT, 300, 3, 3);

        let found = first_live(&book, SIDE_SHORT, 100).unwrap();
        assert_eq!(found.map(|(i, _)| i), Some(c));
    }

    #[test]
    fn test_rb_invariants_after_many_inserts() {
        let mut buf = mk_book(512);
        let mut book = BookMut::load(&mut buf).unwrap();
        for &side in &[SIDE_LONG, SIDE_SHORT] {
            let mut seq = 1u64;
            for price in (10..400).step_by(7) {
                put(&mut book, side, price, seq, seq);
                seq += 1;
            }
        }
        for &side in &[SIDE_LONG, SIDE_SHORT] {
            verify_rb(&book, side);
            // Best-first prices must be monotone in the side's direction.
            let prices: Vec<u64> =
                best_first(&book, side).iter().map(|t| t.1).collect();
            let mut sorted = prices.clone();
            if side == SIDE_LONG {
                sorted.sort_by(|a, b| b.cmp(a));
            } else {
                sorted.sort();
            }
            assert_eq!(prices, sorted, "best-first not monotone for side {}", side);
        }
    }

    fn verify_rb(book: &BookMut, side: u8) {
        let root = tree_root(book, side).unwrap();
        assert_eq!(get_color(book, root).unwrap(), RB_BLACK, "root must be black");
        black_height(book, root);
    }

    /// Recurse: assert no red node has a red child, and that black-height is
    /// equal across both children. Returns the subtree's black-height.
    fn black_height(book: &BookMut, idx: u32) -> u32 {
        if idx == NULL_IDX {
            return 1;
        }
        let left = get_left(book, idx).unwrap();
        let right = get_right(book, idx).unwrap();
        if get_color(book, idx).unwrap() == RB_RED {
            assert_eq!(get_color(book, left).unwrap(), RB_BLACK, "red-red at {}", idx);
            assert_eq!(get_color(book, right).unwrap(), RB_BLACK, "red-red at {}", idx);
        }
        let lh = black_height(book, left);
        let rh = black_height(book, right);
        assert_eq!(lh, rh, "black-height mismatch at idx {}", idx);
        lh + if get_color(book, idx).unwrap() == RB_BLACK { 1 } else { 0 }
    }
}
