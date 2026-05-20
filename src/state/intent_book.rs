//! `IntentBook` — single expandable account per instrument holding:
//!
//!   - header (counters, tree roots, freelist head)
//!   - dynamic node pool of fixed-size 96-byte nodes (`IntentNode` | `SeatNode` | `FreeNode`)
//!
//! Spec §1.5 and §2.
//!
//! Node index `0` is reserved as a null sentinel; valid nodes live at indices `1..=node_count`.

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_BOOK};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use super::DISC_INTENT_BOOK;

pub const NODE_SIZE: usize = 96;

// Node tag discriminants.
pub const NODE_TAG_FREE: u8 = 0;
pub const NODE_TAG_INTENT: u8 = 1;
pub const NODE_TAG_SEAT: u8 = 2;

// RB colors.
pub const RB_RED: u8 = 0;
pub const RB_BLACK: u8 = 1;

// Intent flags (bitfield).
pub const INTENT_FLAG_REENTRY: u8 = 0b0000_0001;

// Side.
pub const SIDE_LONG: u8 = 0;
pub const SIDE_SHORT: u8 = 1;

/// Null node index sentinel.
pub const NULL_IDX: u32 = 0;

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct IntentBookHeader {
    pub discriminator: u8,
    pub bump: u8,
    pub _pad0: [u8; 6],

    pub instrument: Pubkey,

    /// RB-tree roots (node index into the pool). `0` when empty.
    pub long_tree_root: u32,
    pub short_tree_root: u32,
    pub seat_tree_root: u32,
    /// Singly-linked freelist head. `0` when empty.
    pub freelist_head: u32,

    /// Number of live (non-null) nodes allocated, including sentinels.
    pub node_count: u32,
    /// Total addressable nodes (counting the sentinel at index 0).
    pub capacity: u32,

    pub next_intent_id: u64,
    pub next_seq: u64,

    /// Cached max tier fee at time of latest PostIntent. When new intents are posted,
    /// they reserve fees based on this value. If the admin lowers `max_tier_fee_bps`
    /// globally (see spec §8), cached values on existing intents remain valid.
    pub cached_max_fee_bps: u16,
    pub _pad1: [u8; 6],

    pub _reserved: [u8; 32],
}

pub const INTENT_BOOK_HEADER_LEN: usize =
    1 + 1 + 6 + 32 + 4 + 4 + 4 + 4 + 4 + 4 + 8 + 8 + 2 + 6 + 32;
const_assert_size!(IntentBookHeader, INTENT_BOOK_HEADER_LEN);

// ---------------------------------------------------------------------------
// Node variants (all occupy `NODE_SIZE` bytes)
// ---------------------------------------------------------------------------

/// Intent / order node — a single-price limit order in the RB tree.
///
/// Layout is 96 bytes with natural 8-byte alignment (no injected padding).
/// Offsets:
/// ```text
///   0:  tag, side, color, flags (4 × u8)
///   4:  left, right, parent (3 × u32)
///  16:  _pad0 [u32; 2]   -- 8 bytes so next u64 is 8-aligned
///  24:  price_fp (u64), _pad1 [u64; 2]
///  48:  id (u64)
///  56:  owner_seat (u32), contracts_total, contracts_remaining (2 × u16)
///  64:  expiration_slot, post_seq (2 × u64)
///  80:  reserved_collateral, fee_buffer (2 × u64)
///  96:  -- end --
/// ```
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct IntentNode {
    pub tag: u8,
    pub side: u8,
    pub color: u8,
    pub flags: u8,
    pub left: u32,
    pub right: u32,
    pub parent: u32,
    pub _pad0: [u32; 2],

    /// Limit price (18-decimal fixed point). A long crosses a short iff
    /// `long.price_fp >= short.price_fp`.
    pub price_fp: u64,
    pub _pad1: [u64; 2],

    pub id: u64,
    pub owner_seat: u32,
    pub contracts_total: u16,
    pub contracts_remaining: u16,

    pub expiration_slot: u64,
    pub post_seq: u64,

    pub reserved_collateral: u64,
    pub fee_buffer: u64,
}
const_assert_size!(IntentNode, NODE_SIZE);

/// Seat node — owner registry indexed by trader pubkey (RB tree).
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct SeatNode {
    pub tag: u8,
    pub color: u8,
    pub _pad0: [u8; 2],
    pub left: u32,
    pub right: u32,
    pub parent: u32,
    pub _pad1: u32,

    pub trader: Pubkey,

    pub active_intent_count: u32,
    pub flags: u32,

    pub _reserved: [u8; 36],
}
const_assert_size!(SeatNode, NODE_SIZE);

/// Freelist entry — tag is `NODE_TAG_FREE` and `next_free` points to next free slot (or `0`).
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct FreeNode {
    pub tag: u8,
    pub _pad0: [u8; 3],
    pub next_free: u32,
    pub _pad1: [u8; 88],
}
const_assert_size!(FreeNode, NODE_SIZE);

/// Type-erased view of a single node slot.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct RawNode {
    pub bytes: [u8; NODE_SIZE],
}
const_assert_size!(RawNode, NODE_SIZE);

impl RawNode {
    pub fn tag(&self) -> u8 {
        self.bytes[0]
    }

    pub fn as_intent(&self) -> Option<&IntentNode> {
        if self.tag() == NODE_TAG_INTENT {
            Some(bytemuck::from_bytes(&self.bytes))
        } else {
            None
        }
    }

    pub fn as_intent_mut(&mut self) -> Option<&mut IntentNode> {
        if self.tag() == NODE_TAG_INTENT {
            Some(bytemuck::from_bytes_mut(&mut self.bytes))
        } else {
            None
        }
    }

    pub fn as_seat(&self) -> Option<&SeatNode> {
        if self.tag() == NODE_TAG_SEAT {
            Some(bytemuck::from_bytes(&self.bytes))
        } else {
            None
        }
    }

    pub fn as_seat_mut(&mut self) -> Option<&mut SeatNode> {
        if self.tag() == NODE_TAG_SEAT {
            Some(bytemuck::from_bytes_mut(&mut self.bytes))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Book view — splits account bytes into header + node pool
// ---------------------------------------------------------------------------

/// Immutable view of an intent-book account.
pub struct BookRef<'a> {
    pub header: &'a IntentBookHeader,
    pub nodes: &'a [RawNode],
}

/// Mutable view of an intent-book account.
pub struct BookMut<'a> {
    pub header: &'a mut IntentBookHeader,
    pub nodes: &'a mut [RawNode],
}

impl<'a> BookRef<'a> {
    pub fn load(bytes: &'a [u8]) -> Result<Self, ProgramError> {
        if bytes.len() < INTENT_BOOK_HEADER_LEN {
            return Err(PolyleverageError::AccountDataTooSmall.into());
        }
        let (header_bytes, pool_bytes) = bytes.split_at(INTENT_BOOK_HEADER_LEN);
        let header: &IntentBookHeader = pod::try_cast_ref(header_bytes)?;
        if header.discriminator != DISC_INTENT_BOOK {
            return Err(PolyleverageError::NotInitialized.into());
        }
        let nodes: &[RawNode] = pod::try_cast_slice(pool_bytes)?;
        Ok(Self { header, nodes })
    }
}

impl<'a> BookMut<'a> {
    pub fn load(bytes: &'a mut [u8]) -> Result<Self, ProgramError> {
        if bytes.len() < INTENT_BOOK_HEADER_LEN {
            return Err(PolyleverageError::AccountDataTooSmall.into());
        }
        let (header_bytes, pool_bytes) = bytes.split_at_mut(INTENT_BOOK_HEADER_LEN);
        let header: &mut IntentBookHeader = pod::try_cast_mut(header_bytes)?;
        if header.discriminator != DISC_INTENT_BOOK {
            return Err(PolyleverageError::NotInitialized.into());
        }
        let nodes: &mut [RawNode] = pod::try_cast_slice_mut(pool_bytes)?;
        Ok(Self { header, nodes })
    }

    // ---- Node allocator --------------------------------------------------

    /// Allocate a fresh node slot. Prefers freelist; falls back to capacity growth.
    /// Returns index into the node pool (always `>= 1` because 0 is sentinel).
    pub fn alloc_node(&mut self) -> Result<u32, ProgramError> {
        if self.header.freelist_head != NULL_IDX {
            let idx = self.header.freelist_head;
            let free_node: &FreeNode = bytemuck::from_bytes(&self.nodes[idx as usize].bytes);
            self.header.freelist_head = free_node.next_free;
            // Caller must overwrite bytes with their node variant and set tag appropriately.
            return Ok(idx);
        }
        // Otherwise, grow into fresh capacity.
        if self.header.node_count >= self.header.capacity {
            return Err(PolyleverageError::IntentBookFull.into());
        }
        let idx = self.header.node_count;
        self.header.node_count = self
            .header
            .node_count
            .checked_add(1)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(idx)
    }

    /// Return a node slot to the freelist. Writes a `FreeNode` into the slot.
    pub fn free_node(&mut self, idx: u32) -> Result<(), ProgramError> {
        if idx == NULL_IDX || (idx as usize) >= self.nodes.len() {
            return Err(ProgramError::InvalidAccountData);
        }
        let next = self.header.freelist_head;
        let slot = &mut self.nodes[idx as usize];
        slot.bytes = [0u8; NODE_SIZE];
        let free: &mut FreeNode = bytemuck::from_bytes_mut(&mut slot.bytes);
        free.tag = NODE_TAG_FREE;
        free.next_free = next;
        self.header.freelist_head = idx;
        Ok(())
    }

    // ---- Typed node access -----------------------------------------------

    pub fn intent(&self, idx: u32) -> Result<&IntentNode, ProgramError> {
        let slot = self
            .nodes
            .get(idx as usize)
            .ok_or(ProgramError::InvalidAccountData)?;
        slot.as_intent()
            .ok_or_else(|| PolyleverageError::IntentNotFound.into())
    }

    pub fn intent_mut(&mut self, idx: u32) -> Result<&mut IntentNode, ProgramError> {
        let slot = self
            .nodes
            .get_mut(idx as usize)
            .ok_or(ProgramError::InvalidAccountData)?;
        slot.as_intent_mut()
            .ok_or_else(|| PolyleverageError::IntentNotFound.into())
    }

    pub fn seat(&self, idx: u32) -> Result<&SeatNode, ProgramError> {
        let slot = self
            .nodes
            .get(idx as usize)
            .ok_or(ProgramError::InvalidAccountData)?;
        slot.as_seat()
            .ok_or_else(|| PolyleverageError::IntentNotFound.into())
    }

    pub fn seat_mut(&mut self, idx: u32) -> Result<&mut SeatNode, ProgramError> {
        let slot = self
            .nodes
            .get_mut(idx as usize)
            .ok_or(ProgramError::InvalidAccountData)?;
        slot.as_seat_mut()
            .ok_or_else(|| PolyleverageError::IntentNotFound.into())
    }

    /// Write a fully-populated `IntentNode` into slot `idx`.
    pub fn write_intent(&mut self, idx: u32, node: IntentNode) -> Result<(), ProgramError> {
        let slot = self
            .nodes
            .get_mut(idx as usize)
            .ok_or(ProgramError::InvalidAccountData)?;
        // IntentNode and slot.bytes are both NODE_SIZE (compile-time asserted).
        slot.bytes.copy_from_slice(bytemuck::bytes_of(&node));
        debug_assert_eq!(slot.tag(), NODE_TAG_INTENT);
        Ok(())
    }

    /// Write a fully-populated `SeatNode` into slot `idx`.
    pub fn write_seat(&mut self, idx: u32, node: SeatNode) -> Result<(), ProgramError> {
        let slot = self
            .nodes
            .get_mut(idx as usize)
            .ok_or(ProgramError::InvalidAccountData)?;
        slot.bytes.copy_from_slice(bytemuck::bytes_of(&node));
        debug_assert_eq!(slot.tag(), NODE_TAG_SEAT);
        Ok(())
    }

    /// Next post_seq, incrementing the counter.
    pub fn next_seq(&mut self) -> Result<u64, ProgramError> {
        let s = self.header.next_seq;
        self.header.next_seq = s
            .checked_add(1)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(s)
    }

    /// Next intent_id, incrementing the counter.
    pub fn next_intent_id(&mut self) -> Result<u64, ProgramError> {
        let id = self.header.next_intent_id;
        self.header.next_intent_id = id
            .checked_add(1)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Compute total byte size needed for a book with `capacity` node slots (including sentinel).
pub const fn intent_book_byte_size(capacity: u32) -> usize {
    INTENT_BOOK_HEADER_LEN + (capacity as usize) * NODE_SIZE
}

/// Initialize a fresh intent book account:
///   - zero out bytes
///   - write header discriminator + instrument
///   - reserve node 0 as sentinel (leave zeroed; tag == NODE_TAG_FREE but never allocated)
///   - leave freelist empty and node_count = 1 (sentinel reserved)
pub fn init_intent_book(
    bytes: &mut [u8],
    instrument: Pubkey,
    capacity: u32,
    bump: u8,
    cached_max_fee_bps: u16,
) -> Result<(), ProgramError> {
    if bytes.len() != intent_book_byte_size(capacity) {
        return Err(PolyleverageError::AccountDataTooSmall.into());
    }
    // Ensure fully zeroed prior to any typed write (new accounts are already zero on Solana,
    // but we defensively memset here to be safe on realloc).
    for b in bytes.iter_mut() {
        *b = 0;
    }
    let (header_bytes, _pool_bytes) = bytes.split_at_mut(INTENT_BOOK_HEADER_LEN);
    let header: &mut IntentBookHeader = pod::try_cast_mut(header_bytes)?;
    *header = IntentBookHeader {
        discriminator: DISC_INTENT_BOOK,
        bump,
        _pad0: [0; 6],
        instrument,
        long_tree_root: NULL_IDX,
        short_tree_root: NULL_IDX,
        seat_tree_root: NULL_IDX,
        freelist_head: NULL_IDX,
        node_count: 1, // reserve sentinel at index 0
        capacity,
        next_intent_id: 1,
        next_seq: 1,
        cached_max_fee_bps,
        _pad1: [0; 6],
        _reserved: [0; 32],
    };
    Ok(())
}

pub fn find_book_pda(program_id: &Pubkey, instrument: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[SEED_BOOK, instrument.as_ref()], program_id)
}
