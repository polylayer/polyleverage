//! Oracle attestation parsing + verification.
//!
//! The on-chain program is deliberately vendor-neutral: it consumes
//! Ed25519-signed attestations carrying a fixed binary layout (magic +
//! type + market_id + timestamp + nonce + payload). The signing key is
//! whatever pubkey is registered in `ProgramConfig.attestation_signer`
//! and rotated via the 24h `ProposeSetAttestationSigner` /
//! `ExecuteSetAttestationSigner` timelock pair.
//!
//! Verification model: clients submit a tx that includes an
//! `Ed25519SigVerify` precompile instruction immediately followed by
//! the program's settlement instruction. This module introspects the
//! preceding precompile via the instructions sysvar and confirms the
//! signature verified matches the expected attestation_signer pubkey
//! and the expected attestation bytes.
//!
//! Spec §5.

use arrayref::array_ref;
use solana_program::{
    program_error::ProgramError, pubkey::Pubkey, sysvar::instructions as sysvar_instructions,
};

use crate::error::PolyleverageError;

pub const ATTESTATION_MAGIC: [u8; 4] = *b"ATT1";
pub const ATTESTATION_LEN: usize = 4 + 1 + 3 + 32 + 8 + 8 + 48; // = 104
pub const ATTESTATION_PAYLOAD_OFFSET: usize = 4 + 1 + 3 + 32 + 8 + 8;

// Attestation types.
pub const ATT_TYPE_PRICE_TWAP: u8 = 1;
pub const ATT_TYPE_RESOLUTION: u8 = 2;
/// Per-PMLC liquidation attestation. Payload binds the attestation to a
/// specific PMLC pubkey and carries the historical TWAP price + unix
/// timestamp at which the position first became liquidatable. The
/// attestor (TEE) computes this offline against Polymarket's price
/// history; on-chain we settle at the historical mark, regardless of
/// when the `Liquidate` tx is actually submitted. This eliminates the
/// path-dependency where a slow attestor / submitter would let a
/// position un-liquidate by recovering before settlement.
pub const ATT_TYPE_HISTORICAL_LIQUIDATION: u8 = 3;

/// Ed25519SigVerify program ID (native precompile).
pub const ED25519_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("Ed25519SigVerify111111111111111111111111111");

/// Parsed attestation; type-specific payload accessors are below.
#[derive(Debug, Clone, Copy)]
pub struct Attestation<'a> {
    pub bytes: &'a [u8],
}

impl<'a> Attestation<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ProgramError> {
        if bytes.len() != ATTESTATION_LEN {
            return Err(PolyleverageError::InvalidInstructionData.into());
        }
        let magic = array_ref![bytes, 0, 4];
        if *magic != ATTESTATION_MAGIC {
            return Err(PolyleverageError::AttestationBadMagic.into());
        }
        Ok(Self { bytes })
    }

    pub fn attestation_type(&self) -> u8 {
        self.bytes[4]
    }

    pub fn market_id(&self) -> [u8; 32] {
        let s = array_ref![self.bytes, 8, 32];
        *s
    }

    pub fn timestamp(&self) -> u64 {
        let s = array_ref![self.bytes, 40, 8];
        u64::from_le_bytes(*s)
    }

    pub fn nonce(&self) -> u64 {
        let s = array_ref![self.bytes, 48, 8];
        u64::from_le_bytes(*s)
    }

    // PRICE_TWAP payload at offset ATTESTATION_PAYLOAD_OFFSET (56):
    //   [0..8]   price_fp (u64 LE)
    //   [8..16]  twap_window_slots (u64 LE)
    //   [16..24] observation_start_ts (u64 LE)
    //   [24..32] observation_end_ts (u64 LE)
    pub fn price_fp(&self) -> Result<u64, ProgramError> {
        self.require_type(ATT_TYPE_PRICE_TWAP)?;
        let s = array_ref![self.bytes, ATTESTATION_PAYLOAD_OFFSET, 8];
        Ok(u64::from_le_bytes(*s))
    }

    pub fn twap_window_slots(&self) -> Result<u64, ProgramError> {
        self.require_type(ATT_TYPE_PRICE_TWAP)?;
        let s = array_ref![self.bytes, ATTESTATION_PAYLOAD_OFFSET + 8, 8];
        Ok(u64::from_le_bytes(*s))
    }

    // RESOLUTION payload at offset ATTESTATION_PAYLOAD_OFFSET:
    //   [0..2]  final_outcome_bps (u16 LE) — 0, 5000, or 10000
    //   [2..10] resolved_at_ts (u64 LE)
    pub fn final_outcome_bps(&self) -> Result<u16, ProgramError> {
        self.require_type(ATT_TYPE_RESOLUTION)?;
        let s = array_ref![self.bytes, ATTESTATION_PAYLOAD_OFFSET, 2];
        Ok(u16::from_le_bytes(*s))
    }

    // HISTORICAL_LIQUIDATION payload at offset ATTESTATION_PAYLOAD_OFFSET (56):
    //   [0..32]  pmlc_pubkey
    //   [32..40] breach_mark_fp (u64 LE)  — TWAP at moment of first breach
    //   [40..48] breach_unix_ts (i64 LE)  — unix ts of breach (≥0, ≤ now)
    pub fn breach_pmlc_pubkey(&self) -> Result<[u8; 32], ProgramError> {
        self.require_type(ATT_TYPE_HISTORICAL_LIQUIDATION)?;
        Ok(*array_ref![self.bytes, ATTESTATION_PAYLOAD_OFFSET, 32])
    }

    pub fn breach_mark_fp(&self) -> Result<u64, ProgramError> {
        self.require_type(ATT_TYPE_HISTORICAL_LIQUIDATION)?;
        let s = array_ref![self.bytes, ATTESTATION_PAYLOAD_OFFSET + 32, 8];
        Ok(u64::from_le_bytes(*s))
    }

    pub fn breach_unix_ts(&self) -> Result<i64, ProgramError> {
        self.require_type(ATT_TYPE_HISTORICAL_LIQUIDATION)?;
        let s = array_ref![self.bytes, ATTESTATION_PAYLOAD_OFFSET + 40, 8];
        Ok(i64::from_le_bytes(*s))
    }

    fn require_type(&self, t: u8) -> Result<(), ProgramError> {
        if self.attestation_type() != t {
            return Err(PolyleverageError::AttestationWrongType.into());
        }
        Ok(())
    }
}

/// Verify that the Ed25519 precompile instruction at `ix_index` of the
/// current transaction validated a signature from `expected_pubkey`
/// over the returned bytes. Returns the attestation message on success.
pub fn verify_via_ed25519_sysvar<'info>(
    sysvar_instructions_ai: &solana_program::account_info::AccountInfo<'info>,
    expected_pubkey: &Pubkey,
    ix_index: usize,
) -> Result<[u8; ATTESTATION_LEN], ProgramError> {
    let ix = sysvar_instructions::load_instruction_at_checked(ix_index, sysvar_instructions_ai)
        .map_err(|_| PolyleverageError::MissingEd25519Verify)?;

    if ix.program_id != ED25519_PROGRAM_ID {
        return Err(PolyleverageError::MissingEd25519Verify.into());
    }

    // Layout per solana-sdk/src/ed25519_instruction.rs:
    //   [0]   u8  num_signatures
    //   [1]   u8  padding
    // Per-signature offset block (repeats num_signatures times):
    //   [2..4]  u16 signature_offset
    //   [4..6]  u16 signature_instruction_index
    //   [6..8]  u16 public_key_offset
    //   [8..10] u16 public_key_instruction_index
    //   [10..12] u16 message_data_offset
    //   [12..14] u16 message_data_size
    //   [14..16] u16 message_instruction_index
    let data = &ix.data;
    if data.len() < 16 {
        return Err(PolyleverageError::MissingEd25519Verify.into());
    }
    let num_signatures = data[0];
    if num_signatures != 1 {
        return Err(PolyleverageError::MissingEd25519Verify.into());
    }
    let signature_offset = u16::from_le_bytes([data[2], data[3]]) as usize;
    let public_key_offset = u16::from_le_bytes([data[6], data[7]]) as usize;
    let message_offset = u16::from_le_bytes([data[10], data[11]]) as usize;
    let message_size = u16::from_le_bytes([data[12], data[13]]) as usize;

    let pk_bytes = data
        .get(public_key_offset..public_key_offset + 32)
        .ok_or(PolyleverageError::MissingEd25519Verify)?;
    let _sig_bytes = data
        .get(signature_offset..signature_offset + 64)
        .ok_or(PolyleverageError::MissingEd25519Verify)?;
    let msg_bytes = data
        .get(message_offset..message_offset + message_size)
        .ok_or(PolyleverageError::MissingEd25519Verify)?;

    if pk_bytes != expected_pubkey.as_ref() {
        return Err(PolyleverageError::Ed25519PubkeyMismatch.into());
    }
    if msg_bytes.len() != ATTESTATION_LEN {
        return Err(PolyleverageError::Ed25519MessageMismatch.into());
    }

    let mut out = [0u8; ATTESTATION_LEN];
    out.copy_from_slice(msg_bytes);
    Ok(out)
}

/// Apply freshness + nonce + market-id checks to a parsed attestation
/// against cached state.
pub fn validate_freshness_and_nonce(
    att: &Attestation,
    expected_market_id: &[u8; 32],
    last_accepted_nonce: u64,
    now_unix_secs: u64,
    max_staleness_secs: u64,
) -> Result<(), ProgramError> {
    if att.market_id() != *expected_market_id {
        return Err(PolyleverageError::AttestationWrongMarket.into());
    }
    let ts = att.timestamp();
    if ts > now_unix_secs {
        return Err(PolyleverageError::AttestationStale.into());
    }
    if now_unix_secs - ts > max_staleness_secs {
        return Err(PolyleverageError::AttestationStale.into());
    }
    if att.nonce() <= last_accepted_nonce {
        return Err(PolyleverageError::AttestationNonceReplayed.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_attestation(
        att_type: u8,
        market_id: [u8; 32],
        ts: u64,
        nonce: u64,
        payload: &[u8],
    ) -> [u8; ATTESTATION_LEN] {
        let mut bytes = [0u8; ATTESTATION_LEN];
        bytes[0..4].copy_from_slice(&ATTESTATION_MAGIC);
        bytes[4] = att_type;
        bytes[8..40].copy_from_slice(&market_id);
        bytes[40..48].copy_from_slice(&ts.to_le_bytes());
        bytes[48..56].copy_from_slice(&nonce.to_le_bytes());
        let plen = payload.len().min(48);
        bytes[ATTESTATION_PAYLOAD_OFFSET..ATTESTATION_PAYLOAD_OFFSET + plen]
            .copy_from_slice(&payload[..plen]);
        bytes
    }

    #[test]
    fn parse_price_twap() {
        let mut payload = [0u8; 48];
        payload[0..8].copy_from_slice(&500_000_000_000_000_000_u64.to_le_bytes());
        payload[8..16].copy_from_slice(&750_u64.to_le_bytes());
        let market_id = [7u8; 32];
        let bytes = build_attestation(ATT_TYPE_PRICE_TWAP, market_id, 1_000_000, 42, &payload);
        let att = Attestation::parse(&bytes).unwrap();
        assert_eq!(att.attestation_type(), ATT_TYPE_PRICE_TWAP);
        assert_eq!(att.market_id(), market_id);
        assert_eq!(att.timestamp(), 1_000_000);
        assert_eq!(att.nonce(), 42);
        assert_eq!(att.price_fp().unwrap(), 500_000_000_000_000_000);
        assert_eq!(att.twap_window_slots().unwrap(), 750);
    }

    #[test]
    fn parse_resolution() {
        let mut payload = [0u8; 48];
        payload[0..2].copy_from_slice(&10_000u16.to_le_bytes());
        let market_id = [3u8; 32];
        let bytes = build_attestation(ATT_TYPE_RESOLUTION, market_id, 1_000_000, 7, &payload);
        let att = Attestation::parse(&bytes).unwrap();
        assert_eq!(att.final_outcome_bps().unwrap(), 10_000);
    }

    #[test]
    fn parse_historical_liquidation() {
        let mut payload = [0u8; 48];
        let pmlc_pubkey = [0xABu8; 32];
        let breach_mark_fp: u64 = 123_456_789;
        let breach_ts: i64 = 1_700_000_000;
        payload[0..32].copy_from_slice(&pmlc_pubkey);
        payload[32..40].copy_from_slice(&breach_mark_fp.to_le_bytes());
        payload[40..48].copy_from_slice(&breach_ts.to_le_bytes());
        let market_id = [9u8; 32];
        let bytes = build_attestation(
            ATT_TYPE_HISTORICAL_LIQUIDATION,
            market_id,
            1_700_000_005,
            42,
            &payload,
        );
        let att = Attestation::parse(&bytes).unwrap();
        assert_eq!(att.attestation_type(), ATT_TYPE_HISTORICAL_LIQUIDATION);
        assert_eq!(att.breach_pmlc_pubkey().unwrap(), pmlc_pubkey);
        assert_eq!(att.breach_mark_fp().unwrap(), breach_mark_fp);
        assert_eq!(att.breach_unix_ts().unwrap(), breach_ts);
    }

    #[test]
    fn historical_accessors_reject_wrong_type() {
        // PRICE_TWAP attestation should not yield historical-liquidation fields.
        let bytes = build_attestation(ATT_TYPE_PRICE_TWAP, [0u8; 32], 1000, 1, &[0; 48]);
        let att = Attestation::parse(&bytes).unwrap();
        assert!(att.breach_pmlc_pubkey().is_err());
        assert!(att.breach_mark_fp().is_err());
        assert!(att.breach_unix_ts().is_err());
    }

    #[test]
    fn freshness_passes_within_bound() {
        let market_id = [9u8; 32];
        let bytes = build_attestation(ATT_TYPE_PRICE_TWAP, market_id, 1000, 1, &[0; 48]);
        let att = Attestation::parse(&bytes).unwrap();
        validate_freshness_and_nonce(&att, &market_id, 0, 1000, 60).unwrap();
    }

    #[test]
    fn freshness_fails_when_stale() {
        let market_id = [9u8; 32];
        let bytes = build_attestation(ATT_TYPE_PRICE_TWAP, market_id, 1000, 1, &[0; 48]);
        let att = Attestation::parse(&bytes).unwrap();
        let err = validate_freshness_and_nonce(&att, &market_id, 0, 2000, 60).unwrap_err();
        match err {
            ProgramError::Custom(code) => {
                assert_eq!(code, PolyleverageError::AttestationStale as u32);
            }
            _ => panic!("expected custom error"),
        }
    }

    #[test]
    fn nonce_replay_rejected() {
        let market_id = [9u8; 32];
        let bytes = build_attestation(ATT_TYPE_PRICE_TWAP, market_id, 1000, 5, &[0; 48]);
        let att = Attestation::parse(&bytes).unwrap();
        let err = validate_freshness_and_nonce(&att, &market_id, 5, 1000, 60).unwrap_err();
        match err {
            ProgramError::Custom(code) => {
                assert_eq!(code, PolyleverageError::AttestationNonceReplayed as u32);
            }
            _ => panic!("expected custom error"),
        }
    }

    #[test]
    fn wrong_market_rejected() {
        let market_id = [9u8; 32];
        let bytes = build_attestation(ATT_TYPE_PRICE_TWAP, market_id, 1000, 1, &[0; 48]);
        let att = Attestation::parse(&bytes).unwrap();
        let err = validate_freshness_and_nonce(&att, &[1u8; 32], 0, 1000, 60).unwrap_err();
        match err {
            ProgramError::Custom(code) => {
                assert_eq!(code, PolyleverageError::AttestationWrongMarket as u32);
            }
            _ => panic!("expected custom error"),
        }
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = [0u8; ATTESTATION_LEN];
        bytes[0] = b'X';
        let err = Attestation::parse(&bytes).unwrap_err();
        match err {
            ProgramError::Custom(code) => {
                assert_eq!(code, PolyleverageError::AttestationBadMagic as u32);
            }
            _ => panic!("expected custom error"),
        }
    }
}
