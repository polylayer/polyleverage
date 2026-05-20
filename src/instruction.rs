//! Instruction enum + Borsh codec. Clients build these bytes; `processor` dispatches.
//!
//! First byte of instruction_data is a one-byte tag (u8). The remaining bytes are
//! Borsh-encoded per-variant payloads. Admin / timelocked variants carry explicit
//! proposal IDs; settlement variants carry the index of the preceding
//! Ed25519SigVerify instruction (for CRE attestation introspection).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::program_error::ProgramError;

use crate::error::PolyleverageError;

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct InitProgramConfigArgs {
    pub attestation_signer: [u8; 32],
    pub default_max_staleness_secs: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct CreateInstrumentArgs {
    /// Market-data source. See `state::instrument_config::source`.
    /// `0 = POLYMARKET`. Other values reserved.
    pub source: u8,
    /// Source-specific market identifier. For `POLYMARKET`: the
    /// bytes32 condition_id verbatim. The on-chain program treats
    /// these bytes as opaque.
    pub market_id: [u8; 32],
    /// First outcome token id (Polymarket: YES token uint256 → 32 B LE).
    pub source_token_id_a: [u8; 32],
    /// Second outcome token id (Polymarket: NO).
    pub source_token_id_b: [u8; 32],
    pub leverage_bps: u32,
    pub collateral_bucket: u64,
    pub twap_window_slots: u64,
    pub tick_fp: u64,
    pub liquidation_bps: u32,
    pub liquidation_bounty_bps: u16,
    pub max_staleness_secs: u64,
    pub initial_book_capacity: u32,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct DepositArgs {
    pub amount: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct WithdrawArgs {
    pub amount: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct PostIntentArgs {
    pub side: u8, // SIDE_LONG | SIDE_SHORT
    /// Limit price (18-decimal fixed point), tick-aligned. A long fills at
    /// this price or lower; a short at this price or higher.
    pub price_fp: u64,
    pub contracts: u16,
    pub expiration_slot: u64,
    pub reentry_enabled: u8,
    pub try_match: u8,
    pub max_pairs: u8,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct CancelIntentArgs {
    pub intent_id: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct MatchPairArgs {
    pub long_intent_id: u64,
    pub short_intent_id: u64,
    pub max_contracts: u16,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct MatchBestArgs {
    pub max_pairs: u8,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct PruneArgs {
    pub max_n: u8,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct ExpandBookArgs {
    pub additional_nodes: u32,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct LiquidateArgs {
    pub attestation_ix_offset: u8,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct ResolveArgs {
    pub attestation_ix_offset: u8,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct ClaimReentryArgs {
    /// Which side of the closed PMLC to re-enter (SIDE_LONG or SIDE_SHORT).
    pub side: u8,
    /// Intent expiry for the re-posted intent.
    pub expiration_slot: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct CloseMutualArgs {
    pub attestation_ix_offset: u8,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct MatchBestArgsV2 {
    pub max_pairs: u8,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct NovateArgs {
    /// Which side of the PMLC is being transferred (SIDE_LONG or SIDE_SHORT).
    pub side: u8,
}

// --- v2 polish: fees + volume tiers + timelock ------------------------

/// Fee schedule tier table. Up to 8 volume buckets.
#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct FeeTier {
    /// Rolling 30d volume (quote atoms) at which this tier applies.
    pub volume_threshold: u64,
    /// Fee in bps (1 bps = 0.01%).
    pub fee_bps: u16,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct InitFeeScheduleArgs {
    pub tiers: [FeeTier; 8],
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct SetFeeScheduleArgs {
    pub tiers: [FeeTier; 8],
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct SweepFeesArgs {
    pub amount: u64,
}

/// Expand the intent book by an additional number of node slots. The caller pays
/// the incremental rent.
#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct ExpandIntentBookArgs {
    pub additional_nodes: u32,
}

/// Propose a new attestation Ed25519 signer. 24h timelock. Kind-agnostic
/// across providers (Chainlink CRE, EigenCloud TEE, KMS-backed key, …).
#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct ProposeSetAttestationSignerArgs {
    pub proposal_id: u64,
    pub new_signer: [u8; 32],
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct ExecuteTimelockArgs {
    pub proposal_id: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct CancelTimelockArgs {
    pub proposal_id: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct InitAdminMultisigArgs {
    /// `n_signers` (≤ 16) followed by that many 32-byte pubkeys.
    pub threshold: u8,
    pub signers: Vec<[u8; 32]>,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct RotateAdminMultisigArgs {
    pub threshold: u8,
    pub signers: Vec<[u8; 32]>,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct InitBucketRegistryArgs {
    /// Leverage values in bps. Empty → defaults `[2×, 5×, 10×, 20×, 40×]`.
    pub leverage_bps: Vec<u32>,
    /// Collateral buckets in atoms. Empty → defaults `[$100, $1 000]`
    /// assuming a 6-decimal quote.
    pub collateral_buckets: Vec<u64>,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct SetBucketRegistryArgs {
    pub leverage_bps: Vec<u32>,
    pub collateral_buckets: Vec<u64>,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct InitStablecoinRegistryArgs {
    pub wusd_decimals: u8,
    /// Initial accepted stablecoin mints (e.g. USDC, USDT). May be empty —
    /// admins can `AddStablecoinMint` later.
    pub initial_mints: Vec<[u8; 32]>,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct WrapStablecoinArgs {
    pub amount: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct UnwrapStablecoinArgs {
    pub amount: u64,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub struct CreateSessionArgs {
    /// TEE-derived per-user delegate Ed25519 pubkey.
    pub delegate: [u8; 32],
    /// Solana slot at which the session auto-revokes. Must be > current slot.
    pub expires_at_slot: u64,
    /// Per-PostIntent collateral cap (atoms = contracts × bucket × leverage,
    /// pre-fee). Bounds the blast radius of a single delegate-signed action.
    pub per_intent_max_collateral_atoms: u64,
    /// Cumulative cap over the session's lifetime (sum of per-intent
    /// collateral). Bounds the blast radius of a TEE compromise.
    pub cumulative_collateral_cap: u64,
    /// Allowlist of instrument PDAs the session can trade. Empty = any
    /// instrument (NOT recommended; UIs should always populate).
    /// Capped at MAX_SESSION_INSTRUMENTS = 8.
    pub allowed_instruments: Vec<[u8; 32]>,
}

/// Instruction tag. Stable wire-format byte — do not reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InstructionTag {
    // Admin
    InitProgramConfig = 0,
    SetGlobalPause = 1,

    // Instrument admin
    CreateInstrument = 10,
    PauseInstrument = 11,
    ResumeInstrument = 12,
    ExpandIntentBook = 13,

    // User account
    CreateMarginAccount = 20,
    Deposit = 21,
    Withdraw = 22,

    // Trading
    PostIntent = 30,
    CancelIntent = 31,
    MatchPair = 32,
    MatchBestAvailable = 33,
    PruneExpired = 34,
    // One-click trading sessions (delegate-signed PostIntent / CancelIntent)
    CreateSession = 35,
    RevokeSession = 36,
    PostIntentDelegated = 37,
    CancelIntentDelegated = 38,

    // Position lifecycle
    Liquidate = 40,
    Resolve = 41,
    CloseMutual = 42,
    ClaimReentry = 43,
    Novate = 44,
    MatchSubstitute = 45,
    MatchSubstituteWithSettle = 46,
    ClosePmlc = 47,

    // Fees
    InitFeeSchedule = 50,
    SetFeeSchedule = 51,
    SweepFees = 52,

    // Attestation signer rotation (24h timelock)
    ProposeSetAttestationSigner = 60,
    ExecuteSetAttestationSigner = 61,
    CancelTimelockProposal = 62,

    // Admin multisig
    InitAdminMultisig = 70,
    RotateAdminMultisig = 71,

    // Bucket registry (leverages + collateral buckets allowlist)
    InitBucketRegistry = 80,
    SetBucketRegistry = 81,

    // Stablecoin registry + wUSD wrapper
    InitStablecoinRegistry = 90,
    AddStablecoinMint = 91,
    RemoveStablecoinMint = 92,
    WrapStablecoin = 93,
    UnwrapStablecoin = 94,
}

impl TryFrom<u8> for InstructionTag {
    type Error = ProgramError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        use InstructionTag::*;
        Ok(match v {
            0 => InitProgramConfig,
            1 => SetGlobalPause,
            10 => CreateInstrument,
            11 => PauseInstrument,
            12 => ResumeInstrument,
            13 => ExpandIntentBook,
            20 => CreateMarginAccount,
            21 => Deposit,
            22 => Withdraw,
            30 => PostIntent,
            31 => CancelIntent,
            32 => MatchPair,
            33 => MatchBestAvailable,
            34 => PruneExpired,
            35 => CreateSession,
            36 => RevokeSession,
            37 => PostIntentDelegated,
            38 => CancelIntentDelegated,
            40 => Liquidate,
            41 => Resolve,
            42 => CloseMutual,
            43 => ClaimReentry,
            44 => Novate,
            45 => MatchSubstitute,
            46 => MatchSubstituteWithSettle,
            47 => ClosePmlc,
            50 => InitFeeSchedule,
            51 => SetFeeSchedule,
            52 => SweepFees,
            60 => ProposeSetAttestationSigner,
            61 => ExecuteSetAttestationSigner,
            62 => CancelTimelockProposal,
            70 => InitAdminMultisig,
            71 => RotateAdminMultisig,
            80 => InitBucketRegistry,
            81 => SetBucketRegistry,
            90 => InitStablecoinRegistry,
            91 => AddStablecoinMint,
            92 => RemoveStablecoinMint,
            93 => WrapStablecoin,
            94 => UnwrapStablecoin,
            _ => return Err(PolyleverageError::UnsupportedInstruction.into()),
        })
    }
}
