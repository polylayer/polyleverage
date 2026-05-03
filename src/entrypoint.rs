//! Program entrypoint.

use crate::processor::process;
use solana_program::{
    account_info::AccountInfo, entrypoint, entrypoint::ProgramResult, pubkey::Pubkey,
};

entrypoint!(program_entry);

pub fn program_entry(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    process(program_id, accounts, instruction_data)
}
