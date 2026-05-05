//! Program-level errors. Translated to `ProgramError::Custom(code)` on return.

use num_enum::{IntoPrimitive, TryFromPrimitive};
use solana_program::{decode_error::DecodeError, program_error::ProgramError};
use thiserror::Error;

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u32)]
pub enum PolyleverageError {
    // ---- Account / validation ----
    #[error("invalid program config account")]
    InvalidProgramConfig = 0,
    #[error("invalid admin signer")]
    InvalidAdminSigner = 1,
    #[error("invalid PDA derivation")]
    InvalidPda = 2,
    #[error("account owner is not this program")]
    InvalidAccountOwner = 3,
    #[error("account already initialized")]
    AlreadyInitialized = 4,
    #[error("account not initialized")]
    NotInitialized = 5,
    #[error("missing required signer")]
    MissingSigner = 6,
    #[error("invalid mint")]
    InvalidMint = 7,
    #[error("invalid token vault")]
    InvalidVault = 8,
    #[error("invalid token account owner")]
    InvalidTokenAccountOwner = 9,
    #[error("account data too small")]
    AccountDataTooSmall = 10,

    // ---- State / logic ----
    #[error("insufficient free collateral")]
    InsufficientFreeCollateral = 100,
    #[error("insufficient reserved collateral")]
    InsufficientReservedCollateral = 101,
    #[error("insufficient locked collateral")]
    InsufficientLockedCollateral = 102,
    #[error("instrument paused")]
    InstrumentPaused = 103,
    #[error("global pause active")]
    GlobalPaused = 104,
    #[error("invalid leverage bucket")]
    InvalidLeverageBucket = 105,
    #[error("invalid collateral bucket")]
    InvalidCollateralBucket = 106,
    #[error("invalid price range")]
    InvalidPriceRange = 107,
    #[error("price not tick-aligned")]
    PriceNotTickAligned = 108,
    #[error("intent expired")]
    IntentExpired = 109,
    #[error("intent not found")]
    IntentNotFound = 110,
    #[error("intent book full")]
    IntentBookFull = 111,
    #[error("invalid contract count")]
    InvalidContractCount = 112,

    // ---- Matching ----
    #[error("intents not on opposite sides")]
    NotOppositeSides = 200,
    #[error("intents span different instruments")]
    InstrumentMismatch = 201,
    #[error("intent ranges do not overlap")]
    NoRangeOverlap = 202,
    #[error("arithmetic overflow")]
    ArithmeticOverflow = 203,
    #[error("arithmetic underflow")]
    ArithmeticUnderflow = 204,
    #[error("division by zero")]
    DivisionByZero = 205,

    // ---- PMLC / settlement ----
    #[error("PMLC not live")]
    PmlcNotLive = 300,
    #[error("not liquidatable at current mark")]
    NotLiquidatable = 301,
    #[error("invalid PMLC owner side")]
    InvalidPmlcSide = 302,

    // ---- Oracle attestation (vendor-neutral; current backend = EigenCloud) ----
    #[error("missing ed25519 verify instruction")]
    MissingEd25519Verify = 400,
    #[error("ed25519 verify pubkey mismatch")]
    Ed25519PubkeyMismatch = 401,
    #[error("ed25519 verify message mismatch")]
    Ed25519MessageMismatch = 402,
    #[error("attestation bad magic")]
    AttestationBadMagic = 403,
    #[error("attestation wrong type")]
    AttestationWrongType = 404,
    #[error("attestation wrong market")]
    AttestationWrongMarket = 405,
    #[error("attestation stale")]
    AttestationStale = 406,
    #[error("attestation nonce replayed")]
    AttestationNonceReplayed = 407,
    #[error("attestation does not bind to the targeted PMLC")]
    AttestationWrongPmlc = 408,

    // ---- Fees / treasury ----
    #[error("fee schedule not initialized")]
    FeeScheduleNotInitialized = 500,
    #[error("fee increase blocked; only decreases allowed without drain")]
    FeeIncreaseBlocked = 501,

    // ---- Misc ----
    #[error("timelock not yet executable")]
    TimelockNotExecutable = 600,
    #[error("timelock payload mismatch")]
    TimelockPayloadMismatch = 601,

    // ---- Admin multisig ----
    #[error("insufficient multisig approvals")]
    InsufficientMultisigApprovals = 700,
    #[error("multisig config invalid")]
    InvalidMultisigConfig = 701,

    // ---- Bucket / stablecoin registry ----
    #[error("leverage/bucket not in BucketRegistry")]
    BucketNotInRegistry = 710,
    #[error("stablecoin not accepted")]
    StablecoinNotAccepted = 720,
    #[error("stablecoin decimals mismatch")]
    StablecoinDecimalsMismatch = 721,
    #[error("insufficient stablecoin reserve for unwrap")]
    InsufficientStablecoinReserve = 722,

    // ---- Zero-click session (one-click trading) ----
    #[error("session not active (revoked / expired / cap reached)")]
    SessionNotActive = 800,
    #[error("session delegate signer mismatch")]
    SessionDelegateMismatch = 801,
    #[error("session owner mismatch with margin account")]
    SessionOwnerMismatch = 802,
    #[error("session not authorized for this instrument")]
    SessionInstrumentNotAllowed = 803,
    #[error("session per-intent collateral cap exceeded")]
    SessionPerIntentCapExceeded = 804,
    #[error("session cumulative collateral cap exceeded")]
    SessionCumulativeCapExceeded = 805,

    // ---- Catch-all ----
    #[error("unsupported instruction")]
    UnsupportedInstruction = 900,
    #[error("invalid instruction data")]
    InvalidInstructionData = 901,
}

impl From<PolyleverageError> for ProgramError {
    fn from(e: PolyleverageError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

impl<T> DecodeError<T> for PolyleverageError {
    fn type_of() -> &'static str {
        "PolyleverageError"
    }
}
