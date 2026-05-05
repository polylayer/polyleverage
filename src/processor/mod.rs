//! Instruction dispatcher.

use borsh::BorshDeserialize;
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult,
    pubkey::Pubkey,
};

use crate::{
    error::PolyleverageError,
    instruction::{InstructionTag, *},
};

pub mod admin;
pub mod close_pmlc;
pub mod close_prune;
pub mod fees;
pub mod instrument;
pub mod intent;
pub mod margin;
pub mod match_ix;
pub mod multisig;
pub mod novate;
pub mod reentry;
pub mod registries;
pub mod session;
pub mod settlement;
pub mod substitute;
pub mod timelock;
pub mod wusd;

pub fn process(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (tag_byte, payload) = data
        .split_first()
        .ok_or(PolyleverageError::InvalidInstructionData)?;
    let tag = InstructionTag::try_from(*tag_byte)?;

    match tag {
        InstructionTag::InitProgramConfig => {
            let args = InitProgramConfigArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            admin::process_init_program_config(program_id, accounts, args)
        }
        InstructionTag::SetGlobalPause => {
            if payload.len() != 1 {
                return Err(PolyleverageError::InvalidInstructionData.into());
            }
            admin::process_set_global_pause(program_id, accounts, payload[0] != 0)
        }

        InstructionTag::CreateInstrument => {
            let args = CreateInstrumentArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            instrument::process_create_instrument(program_id, accounts, args)
        }
        InstructionTag::PauseInstrument => {
            instrument::process_set_instrument_status(program_id, accounts, true)
        }
        InstructionTag::ResumeInstrument => {
            instrument::process_set_instrument_status(program_id, accounts, false)
        }
        InstructionTag::ExpandIntentBook => {
            let args = ExpandIntentBookArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            instrument::process_expand_intent_book(program_id, accounts, args)
        }

        InstructionTag::CreateMarginAccount => {
            margin::process_create_margin_account(program_id, accounts)
        }
        InstructionTag::Deposit => {
            let args = DepositArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            margin::process_deposit(program_id, accounts, args)
        }
        InstructionTag::Withdraw => {
            let args = WithdrawArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            margin::process_withdraw(program_id, accounts, args)
        }

        InstructionTag::PostIntent => {
            let args = PostIntentArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            intent::process_post_intent(program_id, accounts, args)
        }
        InstructionTag::CancelIntent => {
            let args = CancelIntentArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            intent::process_cancel_intent(program_id, accounts, args)
        }
        InstructionTag::MatchPair => {
            let args = MatchPairArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            match_ix::process_match_pair(program_id, accounts, args)
        }
        InstructionTag::MatchBestAvailable => {
            let args = MatchBestArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            match_ix::process_match_best_available(program_id, accounts, args)
        }

        InstructionTag::Liquidate => {
            let args = LiquidateArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            settlement::process_liquidate(program_id, accounts, args)
        }
        InstructionTag::Resolve => {
            let args = ResolveArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            settlement::process_resolve(program_id, accounts, args)
        }
        InstructionTag::ClaimReentry => {
            let args = ClaimReentryArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            reentry::process_claim_reentry(program_id, accounts, args)
        }
        InstructionTag::CloseMutual => {
            let args = CloseMutualArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            close_prune::process_close_mutual(program_id, accounts, args)
        }
        InstructionTag::PruneExpired => {
            close_prune::process_prune_expired(program_id, accounts)
        }
        InstructionTag::CreateSession => {
            let args = CreateSessionArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            session::process_create_session(program_id, accounts, args)
        }
        InstructionTag::RevokeSession => {
            session::process_revoke_session(program_id, accounts)
        }
        InstructionTag::PostIntentDelegated => {
            let args = PostIntentArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            intent::process_post_intent_delegated(program_id, accounts, args)
        }
        InstructionTag::CancelIntentDelegated => {
            let args = CancelIntentArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            intent::process_cancel_intent_delegated(program_id, accounts, args)
        }
        InstructionTag::Novate => {
            let args = NovateArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            novate::process_novate(program_id, accounts, args)
        }
        InstructionTag::MatchSubstitute => {
            let args = MatchPairArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            substitute::process_match_substitute(program_id, accounts, args)
        }
        InstructionTag::MatchSubstituteWithSettle => {
            let args = MatchPairArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            substitute::process_match_substitute_with_settle(program_id, accounts, args)
        }
        InstructionTag::ClosePmlc => close_pmlc::process_close_pmlc(program_id, accounts),

        InstructionTag::InitFeeSchedule => {
            let args = InitFeeScheduleArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            fees::process_init_fee_schedule(program_id, accounts, args)
        }
        InstructionTag::SetFeeSchedule => {
            let args = SetFeeScheduleArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            fees::process_set_fee_schedule(program_id, accounts, args)
        }
        InstructionTag::SweepFees => {
            let args = SweepFeesArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            fees::process_sweep_fees(program_id, accounts, args)
        }

        InstructionTag::ProposeSetAttestationSigner => {
            let args = ProposeSetAttestationSignerArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            timelock::process_propose_set_attestation_signer(program_id, accounts, args)
        }
        InstructionTag::ExecuteSetAttestationSigner => {
            let args = ExecuteTimelockArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            timelock::process_execute_set_attestation_signer(program_id, accounts, args)
        }
        InstructionTag::CancelTimelockProposal => {
            let args = CancelTimelockArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            timelock::process_cancel_timelock(program_id, accounts, args)
        }

        InstructionTag::InitAdminMultisig => {
            let args = InitAdminMultisigArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            multisig::process_init_admin_multisig(program_id, accounts, args)
        }
        InstructionTag::RotateAdminMultisig => {
            let args = RotateAdminMultisigArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            multisig::process_rotate_admin_multisig(program_id, accounts, args)
        }

        InstructionTag::InitBucketRegistry => {
            let args = InitBucketRegistryArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            registries::process_init_bucket_registry(program_id, accounts, args)
        }
        InstructionTag::SetBucketRegistry => {
            let args = SetBucketRegistryArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            registries::process_set_bucket_registry(program_id, accounts, args)
        }

        InstructionTag::InitStablecoinRegistry => {
            let args = InitStablecoinRegistryArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            registries::process_init_stablecoin_registry(program_id, accounts, args)
        }
        InstructionTag::AddStablecoinMint => {
            registries::process_add_stablecoin_mint(program_id, accounts)
        }
        InstructionTag::RemoveStablecoinMint => {
            registries::process_remove_stablecoin_mint(program_id, accounts)
        }
        InstructionTag::WrapStablecoin => {
            let args = WrapStablecoinArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            wusd::process_wrap_stablecoin(program_id, accounts, args)
        }
        InstructionTag::UnwrapStablecoin => {
            let args = UnwrapStablecoinArgs::try_from_slice(payload)
                .map_err(|_| PolyleverageError::InvalidInstructionData)?;
            wusd::process_unwrap_stablecoin(program_id, accounts, args)
        }
    }
}
