//! `Session` — one-click trading delegate authorization for polyleverage.
//!
//! Mirrors the eigen-tee zero-click pattern used for Polymarket V2
//! orders, but ENFORCED ON-CHAIN. The user signs `CreateSession` once
//! with their Phantom wallet, registering a TEE-derived delegate
//! pubkey and per-session bounds. The polyleverage program then
//! accepts that delegate's signature on `PostIntentDelegated` /
//! `CancelIntentDelegated` ixs in lieu of the owner's signature, while
//! enforcing the bounds atomically.
//!
//! Defense-in-depth design:
//!   1. TEE checks bounds before signing (avoids wasted gas on rejects)
//!   2. Program re-checks the same bounds in the handler (so a TEE
//!      compromise can't drain users beyond their session caps)
//!   3. Owner can revoke any time with a single Phantom signature
//!
//! ─── Layout ────────────────────────────────────────────────────────
//!
//!   [0]    discriminator (= DISC_SESSION = 17)
//!   [1]    bump
//!   [2]    revoked (0/1 — sticky once flipped)
//!   [3]    n_allowed_instruments
//!   [4..8] _pad0
//!   [8..40]   owner (Pubkey)
//!   [40..72]  delegate (Pubkey)
//!   [72..80]  expires_at_slot (u64)
//!   [80..88]  per_intent_max_collateral_atoms (u64)
//!   [88..96]  cumulative_collateral_used (u64)
//!   [96..104] cumulative_collateral_cap (u64)
//!   [104..112] created_at_slot (u64)
//!   [112..120] version (u64) — bumps on every state-changing op
//!   [120..120+32*MAX_SESSION_INSTRUMENTS] allowed_instruments [Pubkey; 8]
//!   [..end] _reserved [u8; 64]
//!
//! Seeds: `[SEED_SESSION, owner]`. One active session per user; new
//! `CreateSession` after the previous one expired or was revoked
//! reuses the same PDA (the program checks for stale state and zeroes
//! before re-init).

use crate::{const_assert_size, error::PolyleverageError, pod, seeds::SEED_SESSION};
use bytemuck::{Pod, Zeroable};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

pub const DISC_SESSION: u8 = 17;

/// Maximum instruments a single session can be scoped to. Keeping this
/// small bounds the on-chain account size and the per-PostIntent
/// allowlist scan cost.
pub const MAX_SESSION_INSTRUMENTS: usize = 8;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct Session {
    pub discriminator: u8,
    pub bump: u8,
    /// Sticky flag: once 1, the session is dead until re-created.
    pub revoked: u8,
    /// Active count in `allowed_instruments[..n_allowed_instruments]`.
    /// 0 means "no allowlist — session can trade ANY instrument" (used
    /// rarely; UIs default to a populated list).
    pub n_allowed_instruments: u8,
    pub _pad0: [u8; 4],

    pub owner: Pubkey,
    pub delegate: Pubkey,

    /// Solana slot at which this session auto-revokes. 0 means
    /// "no time limit" (rejected at `CreateSession` to avoid mistakes).
    pub expires_at_slot: u64,

    /// Hard cap on `contracts × instrument.collateral_bucket` per
    /// single PostIntentDelegated. Combined with the cumulative cap
    /// below, scopes the blast radius of a TEE compromise.
    pub per_intent_max_collateral_atoms: u64,
    /// Running total of collateral committed via this session.
    /// Incremented at PostIntentDelegated; never decremented.
    pub cumulative_collateral_used: u64,
    /// Hard ceiling for `cumulative_collateral_used`.
    pub cumulative_collateral_cap: u64,

    pub created_at_slot: u64,
    /// Bumps on every state-changing op (post / cancel / revoke).
    pub version: u64,

    pub allowed_instruments: [Pubkey; MAX_SESSION_INSTRUMENTS],

    pub _reserved: [u8; 64],
}

pub const SESSION_LEN: usize =
    1 + 1 + 1 + 1 + 4
    + 32 + 32
    + 8 + 8 + 8 + 8 + 8 + 8
    + 32 * MAX_SESSION_INSTRUMENTS
    + 64;

const_assert_size!(Session, SESSION_LEN);

impl Session {
    pub fn load<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let s: &Self = pod::try_cast_ref(bytes)?;
        if s.discriminator != DISC_SESSION {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(s)
    }

    pub fn load_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        let s: &mut Self = pod::try_cast_mut(bytes)?;
        if s.discriminator != DISC_SESSION {
            return Err(PolyleverageError::NotInitialized.into());
        }
        Ok(s)
    }

    /// Initialize a fresh session (or overwrite an expired/revoked one).
    /// Caller MUST have allocated + assigned the account already.
    pub fn init(
        bytes: &mut [u8],
        owner: Pubkey,
        delegate: Pubkey,
        bump: u8,
        expires_at_slot: u64,
        per_intent_max_collateral_atoms: u64,
        cumulative_collateral_cap: u64,
        allowed_instruments: &[Pubkey],
        created_at_slot: u64,
    ) -> Result<(), ProgramError> {
        if expires_at_slot <= created_at_slot {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        if per_intent_max_collateral_atoms == 0 || cumulative_collateral_cap == 0 {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        if per_intent_max_collateral_atoms > cumulative_collateral_cap {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        if allowed_instruments.len() > MAX_SESSION_INSTRUMENTS {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }

        let s: &mut Self = pod::try_cast_mut(bytes)?;
        // Zero everything first so a re-init can't leak prior state.
        *s = Session::zeroed();
        s.discriminator = DISC_SESSION;
        s.bump = bump;
        s.revoked = 0;
        s.n_allowed_instruments = allowed_instruments.len() as u8;
        s.owner = owner;
        s.delegate = delegate;
        s.expires_at_slot = expires_at_slot;
        s.per_intent_max_collateral_atoms = per_intent_max_collateral_atoms;
        s.cumulative_collateral_used = 0;
        s.cumulative_collateral_cap = cumulative_collateral_cap;
        s.created_at_slot = created_at_slot;
        s.version = 1;

        for (i, p) in allowed_instruments.iter().enumerate() {
            s.allowed_instruments[i] = *p;
        }
        Ok(())
    }

    /// Returns the (PDA, bump) for `owner`'s session.
    pub fn find_pda(program_id: &Pubkey, owner: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SEED_SESSION, owner.as_ref()], program_id)
    }

    /// True iff the session is live: not revoked, not expired, and the
    /// cumulative cap hasn't been hit.
    pub fn is_active(&self, current_slot: u64) -> bool {
        self.revoked == 0
            && self.expires_at_slot > current_slot
            && self.cumulative_collateral_used < self.cumulative_collateral_cap
    }

    /// Returns true if the supplied instrument is allowed under this
    /// session (or if no allowlist is set).
    pub fn allows_instrument(&self, instrument: &Pubkey) -> bool {
        if self.n_allowed_instruments == 0 {
            return true;
        }
        let n = (self.n_allowed_instruments as usize).min(MAX_SESSION_INSTRUMENTS);
        for slot in &self.allowed_instruments[..n] {
            if slot == instrument {
                return true;
            }
        }
        false
    }

    /// Charge `collateral_atoms` against the session's caps. Bumps the
    /// version. Returns the post-charge cumulative_collateral_used.
    /// Caller must have already verified `is_active` + `allows_instrument`.
    pub fn record_intent(
        &mut self,
        collateral_atoms: u64,
    ) -> Result<u64, ProgramError> {
        if collateral_atoms == 0 {
            // Posting a zero-collateral intent isn't possible upstream;
            // surface the bug instead of silently passing.
            return Err(PolyleverageError::InvalidContractCount.into());
        }
        if collateral_atoms > self.per_intent_max_collateral_atoms {
            return Err(PolyleverageError::SessionPerIntentCapExceeded.into());
        }
        let new_used = self
            .cumulative_collateral_used
            .checked_add(collateral_atoms)
            .ok_or(PolyleverageError::ArithmeticOverflow)?;
        if new_used > self.cumulative_collateral_cap {
            return Err(PolyleverageError::SessionCumulativeCapExceeded.into());
        }
        self.cumulative_collateral_used = new_used;
        self.version = self.version.saturating_add(1);
        Ok(new_used)
    }

    /// Mark the session revoked. Idempotent (calling twice is fine).
    pub fn revoke(&mut self) {
        self.revoked = 1;
        self.version = self.version.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fresh_buf() -> Vec<u8> {
        vec![0u8; SESSION_LEN]
    }

    fn keys(n: usize) -> Vec<Pubkey> {
        (0..n).map(|_| Pubkey::new_unique()).collect()
    }

    #[test]
    fn init_round_trip_with_allowlist() {
        let mut buf = fresh_buf();
        // Pre-poke the discriminator so init doesn't reject pre-zeroed.
        // Pre-zeroed via vec![0u8; ...] — Session::init handles re-zeroing.

        let owner = Pubkey::new_unique();
        let delegate = Pubkey::new_unique();
        let allowed = keys(3);

        Session::init(
            &mut buf,
            owner,
            delegate,
            42,
            1000,
            1_000_000,
            10_000_000,
            &allowed,
            500,
        )
        .unwrap();
        let s = Session::load(&buf).unwrap();
        assert_eq!(s.owner, owner);
        assert_eq!(s.delegate, delegate);
        assert_eq!(s.bump, 42);
        assert_eq!(s.revoked, 0);
        assert_eq!(s.n_allowed_instruments, 3);
        assert_eq!(s.expires_at_slot, 1000);
        assert_eq!(s.cumulative_collateral_cap, 10_000_000);
        assert_eq!(s.version, 1);
        for (i, k) in allowed.iter().enumerate() {
            assert_eq!(s.allowed_instruments[i], *k);
        }
        assert!(s.is_active(500));
        assert!(s.allows_instrument(&allowed[0]));
        assert!(!s.allows_instrument(&Pubkey::new_unique()));
    }

    #[test]
    fn rejects_zero_caps_and_inverted_window() {
        let mut buf = fresh_buf();
        // Pre-zeroed via vec![0u8; ...] — Session::init handles re-zeroing.
        let owner = Pubkey::new_unique();
        let delegate = Pubkey::new_unique();

        // expires_at_slot <= created_at_slot
        assert!(Session::init(&mut buf, owner, delegate, 1, 100, 1, 10, &[], 100).is_err());
        // per_intent cap = 0
        assert!(Session::init(&mut buf, owner, delegate, 1, 200, 0, 10, &[], 100).is_err());
        // cumulative cap = 0
        assert!(Session::init(&mut buf, owner, delegate, 1, 200, 1, 0, &[], 100).is_err());
        // per_intent > cumulative
        assert!(Session::init(&mut buf, owner, delegate, 1, 200, 100, 10, &[], 100).is_err());
        // too many instruments
        let too_many = keys(MAX_SESSION_INSTRUMENTS + 1);
        assert!(
            Session::init(&mut buf, owner, delegate, 1, 200, 1, 10, &too_many, 100).is_err()
        );
    }

    #[test]
    fn record_intent_enforces_caps() {
        let mut buf = fresh_buf();
        // Pre-zeroed via vec![0u8; ...] — Session::init handles re-zeroing.
        Session::init(
            &mut buf,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1,
            1000,
            500,
            2000,
            &[],
            0,
        )
        .unwrap();
        let s = Session::load_mut(&mut buf).unwrap();

        // Within per-intent cap, cumulative grows.
        assert_eq!(s.record_intent(500).unwrap(), 500);
        assert_eq!(s.record_intent(500).unwrap(), 1000);
        // Exactly at cumulative cap is allowed (cap is inclusive).
        assert_eq!(s.record_intent(500).unwrap(), 1500);
        // Per-intent cap exceeded (501 > 500).
        assert!(s.record_intent(501).is_err());
        // Cumulative cap exceeded.
        assert!(s.record_intent(501).is_err()); // first fails per-intent
        // bring cumulative just under the cap and try to bump it past
        let _ = s.record_intent(500); // now at 2000
        assert!(s.record_intent(1).is_err()); // would be 2001 > cap
    }

    #[test]
    fn revoke_is_sticky() {
        let mut buf = fresh_buf();
        // Pre-zeroed via vec![0u8; ...] — Session::init handles re-zeroing.
        Session::init(
            &mut buf,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1,
            1000,
            100,
            1000,
            &[],
            0,
        )
        .unwrap();
        let s = Session::load_mut(&mut buf).unwrap();
        assert!(s.is_active(500));
        s.revoke();
        assert!(!s.is_active(500));
        s.revoke(); // idempotent
        assert!(!s.is_active(500));
    }

    #[test]
    fn allows_instrument_with_no_allowlist_means_any() {
        let mut buf = fresh_buf();
        // Pre-zeroed via vec![0u8; ...] — Session::init handles re-zeroing.
        Session::init(
            &mut buf,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1,
            1000,
            100,
            1000,
            &[], // empty allowlist
            0,
        )
        .unwrap();
        let s = Session::load(&buf).unwrap();
        assert!(s.allows_instrument(&Pubkey::new_unique()));
    }

    #[test]
    fn expired_session_is_inactive() {
        let mut buf = fresh_buf();
        // Pre-zeroed via vec![0u8; ...] — Session::init handles re-zeroing.
        Session::init(
            &mut buf,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1,
            1000,
            100,
            1000,
            &[],
            0,
        )
        .unwrap();
        let s = Session::load(&buf).unwrap();
        assert!(s.is_active(999));
        assert!(!s.is_active(1000));
        assert!(!s.is_active(1001));
    }
}
