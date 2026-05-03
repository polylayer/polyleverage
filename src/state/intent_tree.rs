//! Interval-augmented red-black tree over `IntentNode`s in the book node pool.
//!
//! Ordering key: `(min_price_fp ASC, post_seq ASC, id ASC)` — deterministic, FIFO tiebreak.
//! Augmentation: `subtree_max_fp = max(self.max_price_fp, left.subtree_max, right.subtree_max)`.
//!
//! All operations take a `&mut BookMut<'_>` and a `Side` tag selecting which root to use.
//! Node index `0` is the null sentinel.

use solana_program::program_error::ProgramError;

use crate::error::PolyleverageError;

use super::intent_book::{
    BookMut, IntentNode, NULL_IDX, RB_BLACK, RB_RED, SIDE_LONG, SIDE_SHORT,
};

/// Order key: (min_price_fp, post_seq, id). Returns `Ordering` as i8 (-1, 0, 1).
#[inline]
fn cmp_key(a: &IntentNode, b: &IntentNode) -> core::cmp::Ordering {
    use core::cmp::Ordering::*;
    match a.min_price_fp.cmp(&b.min_price_fp) {
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

// --- Augmentation maintenance ----------------------------------------------

/// Recompute `subtree_max_fp` for node `idx` from its children's cached values.
#[inline]
fn recompute_augment(book: &mut BookMut, idx: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    let left = get_left(book, idx)?;
    let right = get_right(book, idx)?;
    let self_max = book.intent(idx)?.max_price_fp;
    let left_max = if left == NULL_IDX {
        0
    } else {
        book.intent(left)?.subtree_max_fp
    };
    let right_max = if right == NULL_IDX {
        0
    } else {
        book.intent(right)?.subtree_max_fp
    };
    let new_max = self_max.max(left_max).max(right_max);
    book.intent_mut(idx)?.subtree_max_fp = new_max;
    Ok(())
}

/// Walk from `idx` up to the root, recomputing `subtree_max_fp` at each ancestor.
#[inline]
fn fix_augment_up(book: &mut BookMut, mut idx: u32) -> Result<(), ProgramError> {
    while idx != NULL_IDX {
        recompute_augment(book, idx)?;
        idx = get_parent(book, idx)?;
    }
    Ok(())
}

// --- Rotations --------------------------------------------------------------
// Standard RB rotations. Update augmentation at rotated nodes.

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
    // Augmentation: x is now below y; recompute x then y.
    recompute_augment(book, x)?;
    recompute_augment(book, y)?;
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
    recompute_augment(book, x)?;
    recompute_augment(book, y)?;
    Ok(())
}

// --- Insert ----------------------------------------------------------------

/// Insert node at `idx` into the `side` tree. The node must already be populated
/// (min/max/seq/id/etc) and have `left = right = parent = 0`, `color = RED`.
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
        n.subtree_max_fp = n.max_price_fp;
    }

    let mut root = tree_root(book, side)?;
    if root == NULL_IDX {
        // Empty tree: new node is root, colored BLACK.
        set_color(book, idx, RB_BLACK)?;
        recompute_augment(book, idx)?;
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
            cmp_key(a, b)
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

    // Fix augmentation along the path from idx upward.
    fix_augment_up(book, idx)?;

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
        x_parent = if x == NULL_IDX { zp } else { get_parent(book, x)? };
    } else if z_right == NULL_IDX {
        x = z_left;
        let zp = get_parent(book, z)?;
        transplant(book, side, z, z_left)?;
        x_parent = if x == NULL_IDX { zp } else { get_parent(book, x)? };
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

    // Fix augmentation upward starting from x's parent.
    fix_augment_up(book, x_parent)?;

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

// --- Range-point query -----------------------------------------------------

/// Visit every intent whose interval `[min, max]` contains `point_fp`.
/// Traversal is in-order (sorted by key) subject to augmentation pruning.
///
/// `visitor` returns `true` to continue or `false` to stop early.
pub fn for_each_containing<F>(
    book: &BookMut,
    side: u8,
    point_fp: u64,
    mut visitor: F,
) -> Result<(), ProgramError>
where
    F: FnMut(u32, &IntentNode) -> bool,
{
    let root = tree_root(book, side)?;
    if root == NULL_IDX {
        return Ok(());
    }
    // Iterative stack-based walk. Using fixed-size stack avoids heap allocation.
    // RB-tree depth is at most 2·log2(n); for 1M nodes that's 40. 64 is comfortable.
    const STACK_CAP: usize = 64;
    let mut stack: [u32; STACK_CAP] = [NULL_IDX; STACK_CAP];
    let mut sp: usize = 0;
    let mut cur = root;

    loop {
        // Traverse left as far as possible.
        while cur != NULL_IDX {
            // Prune: if point_fp > subtree_max, nothing in this subtree intersects.
            let sm = book.intent(cur)?.subtree_max_fp;
            if point_fp > sm {
                break;
            }
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
        if node.min_price_fp <= point_fp && point_fp <= node.max_price_fp {
            let cont = visitor(top, node);
            if !cont {
                return Ok(());
            }
        }
        // Only descend right if right subtree could contain the point.
        if point_fp >= node.min_price_fp {
            cur = node.right;
        } else {
            cur = NULL_IDX;
        }
    }
    Ok(())
}

/// Shortcut: return the first (leftmost in-order) intent containing `point_fp` with
/// positive `contracts_remaining`. Returns `(idx, node_copy)` or `None`.
pub fn first_active_containing(
    book: &BookMut,
    side: u8,
    point_fp: u64,
) -> Result<Option<(u32, IntentNode)>, ProgramError> {
    let mut found: Option<(u32, IntentNode)> = None;
    for_each_containing(book, side, point_fp, |idx, node| {
        if node.contracts_remaining > 0 {
            found = Some((idx, *node));
            false // stop iterating
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

    fn mk_intent(side: u8, min: u64, max: u64, seq: u64, id: u64) -> IntentNode {
        IntentNode {
            tag: NODE_TAG_INTENT,
            side,
            color: RB_RED,
            flags: 0,
            left: NULL_IDX,
            right: NULL_IDX,
            parent: NULL_IDX,
            _pad0: [0; 2],
            min_price_fp: min,
            max_price_fp: max,
            subtree_max_fp: max,
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

    #[test]
    fn test_single_insert_and_query() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();

        let idx = book.alloc_node().unwrap();
        let node = mk_intent(SIDE_LONG, 500, 600, 1, 1);
        book.write_intent(idx, node).unwrap();

        insert(&mut book, SIDE_LONG, idx).unwrap();
        assert_eq!(book.header.long_tree_root, idx);

        // Root must be black post-insert.
        assert_eq!(get_color(&book, idx).unwrap(), RB_BLACK);

        // Point in range.
        let mut hits = Vec::new();
        for_each_containing(&book, SIDE_LONG, 550, |i, _| {
            hits.push(i);
            true
        })
        .unwrap();
        assert_eq!(hits, vec![idx]);

        // Point out of range.
        let mut hits = Vec::new();
        for_each_containing(&book, SIDE_LONG, 400, |i, _| {
            hits.push(i);
            true
        })
        .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_many_inserts_and_in_order() {
        let mut buf = mk_book(256);
        let mut book = BookMut::load(&mut buf).unwrap();

        // Insert intents with various min_price_fp in shuffled order.
        let seqs: [u64; 10] = [50, 10, 30, 70, 20, 40, 60, 80, 90, 100];
        let mut indices = Vec::new();
        for (i, &seq) in seqs.iter().enumerate() {
            let idx = book.alloc_node().unwrap();
            let node = mk_intent(SIDE_LONG, seq * 10, seq * 10 + 5, seq, (i + 1) as u64);
            book.write_intent(idx, node).unwrap();
            insert(&mut book, SIDE_LONG, idx).unwrap();
            indices.push((seq, idx));
        }
        indices.sort_by_key(|(s, _)| *s);

        // Stab each and we should find exactly one.
        for &(seq, idx) in &indices {
            let mut found = None;
            for_each_containing(&book, SIDE_LONG, seq * 10 + 2, |i, _| {
                found = Some(i);
                false
            })
            .unwrap();
            assert_eq!(found, Some(idx));
        }
    }

    #[test]
    fn test_remove_preserves_others() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();

        let a = book.alloc_node().unwrap();
        let b = book.alloc_node().unwrap();
        let c = book.alloc_node().unwrap();
        book.write_intent(a, mk_intent(SIDE_LONG, 100, 200, 1, 1)).unwrap();
        book.write_intent(b, mk_intent(SIDE_LONG, 300, 400, 2, 2)).unwrap();
        book.write_intent(c, mk_intent(SIDE_LONG, 500, 600, 3, 3)).unwrap();

        insert(&mut book, SIDE_LONG, a).unwrap();
        insert(&mut book, SIDE_LONG, b).unwrap();
        insert(&mut book, SIDE_LONG, c).unwrap();

        // Remove middle.
        remove(&mut book, SIDE_LONG, b).unwrap();

        // Search for point in removed interval: no hits.
        let mut hits = Vec::new();
        for_each_containing(&book, SIDE_LONG, 350, |i, _| {
            hits.push(i);
            true
        })
        .unwrap();
        assert!(hits.is_empty());

        // Remaining intervals still findable.
        let mut hits = Vec::new();
        for_each_containing(&book, SIDE_LONG, 150, |i, _| {
            hits.push(i);
            true
        })
        .unwrap();
        assert_eq!(hits, vec![a]);
        let mut hits = Vec::new();
        for_each_containing(&book, SIDE_LONG, 550, |i, _| {
            hits.push(i);
            true
        })
        .unwrap();
        assert_eq!(hits, vec![c]);
    }

    #[test]
    fn test_overlapping_intervals_multiple_hits() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();

        // Three intervals all containing point 500.
        let a = book.alloc_node().unwrap();
        let b = book.alloc_node().unwrap();
        let c = book.alloc_node().unwrap();
        book.write_intent(a, mk_intent(SIDE_LONG, 100, 600, 1, 1)).unwrap();
        book.write_intent(b, mk_intent(SIDE_LONG, 300, 700, 2, 2)).unwrap();
        book.write_intent(c, mk_intent(SIDE_LONG, 450, 550, 3, 3)).unwrap();

        insert(&mut book, SIDE_LONG, a).unwrap();
        insert(&mut book, SIDE_LONG, b).unwrap();
        insert(&mut book, SIDE_LONG, c).unwrap();

        let mut hits = Vec::new();
        for_each_containing(&book, SIDE_LONG, 500, |i, _| {
            hits.push(i);
            true
        })
        .unwrap();
        // Should contain all three in sorted order by min_price_fp.
        assert_eq!(hits, vec![a, b, c]);
    }

    #[test]
    fn test_first_active_skips_exhausted() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();

        let a = book.alloc_node().unwrap();
        let b = book.alloc_node().unwrap();
        let mut na = mk_intent(SIDE_LONG, 100, 600, 1, 1);
        na.contracts_remaining = 0; // exhausted
        let nb = mk_intent(SIDE_LONG, 200, 700, 2, 2);
        book.write_intent(a, na).unwrap();
        book.write_intent(b, nb).unwrap();
        insert(&mut book, SIDE_LONG, a).unwrap();
        insert(&mut book, SIDE_LONG, b).unwrap();

        let found = first_active_containing(&book, SIDE_LONG, 500).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().0, b);
    }

    #[test]
    fn test_subtree_max_invariant_after_rotations() {
        // Stress: insert many intervals and verify subtree_max is correct everywhere.
        let mut buf = mk_book(512);
        let mut book = BookMut::load(&mut buf).unwrap();

        let sides = [SIDE_LONG, SIDE_SHORT];
        for (tree_side, _) in sides.iter().enumerate() {
            let side = sides[tree_side];
            let mut seq = 1u64;
            for min in (10..200).step_by(7) {
                let max = min + 50;
                let idx = book.alloc_node().unwrap();
                book.write_intent(idx, mk_intent(side, min, max, seq, seq)).unwrap();
                insert(&mut book, side, idx).unwrap();
                seq += 1;
            }
        }

        // Verify invariant on each side's tree.
        for &side in &sides {
            verify_augment_invariant(&book, side);
        }
    }

    fn verify_augment_invariant(book: &BookMut, side: u8) {
        let root = tree_root(book, side).unwrap();
        verify_subtree(book, root);
    }

    fn verify_subtree(book: &BookMut, idx: u32) -> u64 {
        if idx == NULL_IDX {
            return 0;
        }
        let left = get_left(book, idx).unwrap();
        let right = get_right(book, idx).unwrap();
        let lmax = verify_subtree(book, left);
        let rmax = verify_subtree(book, right);
        let self_max = book.intent(idx).unwrap().max_price_fp;
        let expected = self_max.max(lmax).max(rmax);
        let cached = book.intent(idx).unwrap().subtree_max_fp;
        assert_eq!(cached, expected, "subtree_max mismatch at idx {}", idx);
        expected
    }
}
