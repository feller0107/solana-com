//! The `rewards_transaction` module provides functionality for creating a global
//! rewards account and enabling stakers to redeem credits from their vote accounts.

use crate::id;
use crate::rewards_instruction::RewardsInstruction;
use crate::rewards_state::RewardsState;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, KeypairUtil};
use solana_sdk::system_transaction::SystemTransaction;
use solana_sdk::transaction::Transaction;
use solana_sdk::transaction_builder::TransactionBuilder;
use solana_sdk::vote_program::VoteInstruction;

pub struct RewardsTransaction {}

impl RewardsTransaction {
    pub fn new_account(
        from_keypair: &Keypair,
        rewards_id: Pubkey,
        blockhash: Hash,
        num_tokens: u64,
        fee: u64,
    ) -> Transaction {
        SystemTransaction::new_program_account(
            from_keypair,
            rewards_id,
            blockhash,
            num_tokens,
            RewardsState::max_size() as u64,
            id(),
            fee,
        )
    }

    pub fn new_redeem_credits(
        vote_keypair: &Keypair,
        rewards_id: Pubkey,
        blockhash: Hash,
        fee: u64,
    ) -> Transaction {
        let vote_id = vote_keypair.pubkey();
        TransactionBuilder::new(fee)
            .push(RewardsInstruction::new_redeem_vote_credits(
                vote_id, rewards_id,
            ))
            .push(VoteInstruction::new_clear_credits(vote_id))
            .sign(&[vote_keypair], blockhash)
    }
}
