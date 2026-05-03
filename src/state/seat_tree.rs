//! Red-black tree of `SeatNode`s keyed by trader `Pubkey`.
//!
//! Non-augmented — we only need ordered lookup by trader. No rotations depend on
//! per-subtree state beyond colors.

use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use crate::error::PolyleverageError;

use super::intent_book::{
    BookMut, NODE_TAG_SEAT, NULL_IDX, RB_BLACK, RB_RED, SeatNode,
};

#[inline]
fn root(book: &BookMut) -> u32 {
    book.header.seat_tree_root
}

#[inline]
fn set_root(book: &mut BookMut, v: u32) {
    book.header.seat_tree_root = v;
}

#[inline]
fn get_left(book: &BookMut, idx: u32) -> Result<u32, ProgramError> {
    if idx == NULL_IDX {
        return Ok(NULL_IDX);
    }
    Ok(book.seat(idx)?.left)
}
#[inline]
fn get_right(book: &BookMut, idx: u32) -> Result<u32, ProgramError> {
    if idx == NULL_IDX {
        return Ok(NULL_IDX);
    }
    Ok(book.seat(idx)?.right)
}
#[inline]
fn get_parent(book: &BookMut, idx: u32) -> Result<u32, ProgramError> {
    if idx == NULL_IDX {
        return Ok(NULL_IDX);
    }
    Ok(book.seat(idx)?.parent)
}
#[inline]
fn get_color(book: &BookMut, idx: u32) -> Result<u8, ProgramError> {
    if idx == NULL_IDX {
        return Ok(RB_BLACK);
    }
    Ok(book.seat(idx)?.color)
}
#[inline]
fn set_left(book: &mut BookMut, idx: u32, v: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    book.seat_mut(idx)?.left = v;
    Ok(())
}
#[inline]
fn set_right(book: &mut BookMut, idx: u32, v: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    book.seat_mut(idx)?.right = v;
    Ok(())
}
#[inline]
fn set_parent(book: &mut BookMut, idx: u32, v: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    book.seat_mut(idx)?.parent = v;
    Ok(())
}
#[inline]
fn set_color(book: &mut BookMut, idx: u32, c: u8) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Ok(());
    }
    book.seat_mut(idx)?.color = c;
    Ok(())
}

fn rotate_left(book: &mut BookMut, x: u32) -> Result<(), ProgramError> {
    let y = get_right(book, x)?;
    let y_left = get_left(book, y)?;
    set_right(book, x, y_left)?;
    if y_left != NULL_IDX {
        set_parent(book, y_left, x)?;
    }
    let xp = get_parent(book, x)?;
    set_parent(book, y, xp)?;
    if xp == NULL_IDX {
        set_root(book, y);
    } else if get_left(book, xp)? == x {
        set_left(book, xp, y)?;
    } else {
        set_right(book, xp, y)?;
    }
    set_left(book, y, x)?;
    set_parent(book, x, y)?;
    Ok(())
}

fn rotate_right(book: &mut BookMut, x: u32) -> Result<(), ProgramError> {
    let y = get_left(book, x)?;
    let y_right = get_right(book, y)?;
    set_left(book, x, y_right)?;
    if y_right != NULL_IDX {
        set_parent(book, y_right, x)?;
    }
    let xp = get_parent(book, x)?;
    set_parent(book, y, xp)?;
    if xp == NULL_IDX {
        set_root(book, y);
    } else if get_right(book, xp)? == x {
        set_right(book, xp, y)?;
    } else {
        set_left(book, xp, y)?;
    }
    set_right(book, y, x)?;
    set_parent(book, x, y)?;
    Ok(())
}

/// Look up a seat by trader pubkey.
pub fn find(book: &BookMut, trader: &Pubkey) -> Result<u32, ProgramError> {
    let mut cur = root(book);
    while cur != NULL_IDX {
        let node = book.seat(cur)?;
        match trader.cmp(&node.trader) {
            core::cmp::Ordering::Less => cur = node.left,
            core::cmp::Ordering::Greater => cur = node.right,
            core::cmp::Ordering::Equal => return Ok(cur),
        }
    }
    Ok(NULL_IDX)
}

/// Insert a fresh seat node (already populated with trader field) into the tree.
pub fn insert(book: &mut BookMut, idx: u32) -> Result<(), ProgramError> {
    if idx == NULL_IDX {
        return Err(ProgramError::InvalidAccountData);
    }
    {
        let n = book.seat_mut(idx)?;
        n.left = NULL_IDX;
        n.right = NULL_IDX;
        n.parent = NULL_IDX;
        n.color = RB_RED;
    }

    let trader = book.seat(idx)?.trader;

    let mut r = root(book);
    if r == NULL_IDX {
        set_color(book, idx, RB_BLACK)?;
        set_root(book, idx);
        return Ok(());
    }
    let mut parent = NULL_IDX;
    let mut go_left = false;
    loop {
        parent = r;
        let existing = book.seat(r)?.trader;
        match trader.cmp(&existing) {
            core::cmp::Ordering::Less => {
                go_left = true;
                let l = get_left(book, r)?;
                if l == NULL_IDX {
                    break;
                }
                r = l;
            }
            core::cmp::Ordering::Greater => {
                go_left = false;
                let rg = get_right(book, r)?;
                if rg == NULL_IDX {
                    break;
                }
                r = rg;
            }
            core::cmp::Ordering::Equal => {
                return Err(PolyleverageError::AlreadyInitialized.into());
            }
        }
    }
    set_parent(book, idx, parent)?;
    if go_left {
        set_left(book, parent, idx)?;
    } else {
        set_right(book, parent, idx)?;
    }
    insert_fixup(book, idx)?;
    Ok(())
}

fn insert_fixup(book: &mut BookMut, mut z: u32) -> Result<(), ProgramError> {
    while get_parent(book, z)? != NULL_IDX && get_color(book, get_parent(book, z)?)? == RB_RED {
        let p = get_parent(book, z)?;
        let gp = get_parent(book, p)?;
        if p == get_left(book, gp)? {
            let y = get_right(book, gp)?;
            if y != NULL_IDX && get_color(book, y)? == RB_RED {
                set_color(book, p, RB_BLACK)?;
                set_color(book, y, RB_BLACK)?;
                set_color(book, gp, RB_RED)?;
                z = gp;
            } else {
                if z == get_right(book, p)? {
                    z = p;
                    rotate_left(book, z)?;
                }
                let p = get_parent(book, z)?;
                let gp = get_parent(book, p)?;
                set_color(book, p, RB_BLACK)?;
                set_color(book, gp, RB_RED)?;
                rotate_right(book, gp)?;
            }
        } else {
            let y = get_left(book, gp)?;
            if y != NULL_IDX && get_color(book, y)? == RB_RED {
                set_color(book, p, RB_BLACK)?;
                set_color(book, y, RB_BLACK)?;
                set_color(book, gp, RB_RED)?;
                z = gp;
            } else {
                if z == get_left(book, p)? {
                    z = p;
                    rotate_right(book, z)?;
                }
                let p = get_parent(book, z)?;
                let gp = get_parent(book, p)?;
                set_color(book, p, RB_BLACK)?;
                set_color(book, gp, RB_RED)?;
                rotate_left(book, gp)?;
            }
        }
    }
    let r = root(book);
    if r != NULL_IDX {
        set_color(book, r, RB_BLACK)?;
    }
    Ok(())
}

/// Find or create the seat for `trader`. Returns the existing index on a hit or
/// allocates + inserts a new one on a miss.
pub fn find_or_create(book: &mut BookMut, trader: Pubkey) -> Result<u32, ProgramError> {
    let existing = find(book, &trader)?;
    if existing != NULL_IDX {
        return Ok(existing);
    }
    let idx = book.alloc_node()?;
    book.write_seat(
        idx,
        SeatNode {
            tag: NODE_TAG_SEAT,
            color: RB_RED,
            _pad0: [0; 2],
            left: NULL_IDX,
            right: NULL_IDX,
            parent: NULL_IDX,
            _pad1: 0,
            trader,
            active_intent_count: 0,
            flags: 0,
            _reserved: [0; 36],
        },
    )?;
    insert(book, idx)?;
    Ok(idx)
}

// Seat-tree removal is rarely needed — seats persist even at zero active intents —
// so we defer implementation until Phase 3 adds explicit seat-closing.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{init_intent_book, intent_book_byte_size};
    use solana_program::pubkey::Pubkey;

    fn mk_book(capacity: u32) -> Vec<u8> {
        let mut buf = vec![0u8; intent_book_byte_size(capacity)];
        init_intent_book(&mut buf, Pubkey::new_unique(), capacity, 0, 0).unwrap();
        buf
    }

    #[test]
    fn test_find_or_create_is_idempotent() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        let trader = Pubkey::new_unique();

        let a = find_or_create(&mut book, trader).unwrap();
        let b = find_or_create(&mut book, trader).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn test_find_returns_null_for_unknown() {
        let mut buf = mk_book(64);
        let book = BookMut::load(&mut buf).unwrap();
        assert_eq!(find(&book, &Pubkey::new_unique()).unwrap(), NULL_IDX);
    }

    #[test]
    fn test_multiple_seats() {
        let mut buf = mk_book(64);
        let mut book = BookMut::load(&mut buf).unwrap();
        let traders: Vec<Pubkey> = (0..20).map(|_| Pubkey::new_unique()).collect();
        let mut indices = Vec::new();
        for t in &traders {
            indices.push(find_or_create(&mut book, *t).unwrap());
        }
        for (t, expected_idx) in traders.iter().zip(indices.iter()) {
            assert_eq!(find(&book, t).unwrap(), *expected_idx);
        }
    }
}
