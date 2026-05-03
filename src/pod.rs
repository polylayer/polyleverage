//! Plain-old-data helpers for zero-copy structs used in the intent book node pool.
//!
//! We use `bytemuck::Pod + Zeroable` on `#[repr(C)]` structs so we can cast raw bytes
//! of the intent-book account directly into typed views without heap allocation. This
//! is the Manifest/Hypertree pattern — fixed-size, aligned, no hidden padding.

use bytemuck::Pod;
use solana_program::program_error::ProgramError;

use crate::error::PolyleverageError;

/// Treat a byte slice as a reference to a single Pod value. Length must match exactly.
pub fn try_cast_ref<T: Pod>(bytes: &[u8]) -> Result<&T, ProgramError> {
    bytemuck::try_from_bytes::<T>(bytes)
        .map_err(|_| PolyleverageError::AccountDataTooSmall.into())
}

/// Mutable variant of [`try_cast_ref`].
pub fn try_cast_mut<T: Pod>(bytes: &mut [u8]) -> Result<&mut T, ProgramError> {
    bytemuck::try_from_bytes_mut::<T>(bytes)
        .map_err(|_| PolyleverageError::AccountDataTooSmall.into())
}

/// Reinterpret a byte slice as a slice of T, length derived from byte length.
pub fn try_cast_slice<T: Pod>(bytes: &[u8]) -> Result<&[T], ProgramError> {
    bytemuck::try_cast_slice::<u8, T>(bytes)
        .map_err(|_| PolyleverageError::AccountDataTooSmall.into())
}

/// Mutable slice variant.
pub fn try_cast_slice_mut<T: Pod>(bytes: &mut [u8]) -> Result<&mut [T], ProgramError> {
    bytemuck::try_cast_slice_mut::<u8, T>(bytes)
        .map_err(|_| PolyleverageError::AccountDataTooSmall.into())
}

/// Compile-time assertion that a type's size matches an expected value.
#[macro_export]
macro_rules! const_assert_size {
    ($ty:ty, $expected:expr) => {
        const _: [(); $expected] = [(); ::core::mem::size_of::<$ty>()];
    };
}

/// Compile-time assertion that a type's alignment is at most `$max`.
#[macro_export]
macro_rules! const_assert_align {
    ($ty:ty, $max:expr) => {
        const _: () = assert!(::core::mem::align_of::<$ty>() <= $max);
    };
}

/// Zeroable/Pod-safe marker: both traits are unsafe impls on `#[repr(C)]` types.
/// Using this in `#[derive]` via bytemuck::Zeroable/Pod; this module only re-exports.
pub use bytemuck::{Pod as BytemuckPod, Zeroable as BytemuckZeroable};
