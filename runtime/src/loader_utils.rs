use serde::Serialize;
use solana_sdk::client::Client;
use solana_sdk::instruction::AccountMeta;
use solana_sdk::loader_instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, KeypairUtil, Signature};
use solana_sdk::system_instruction;
use solana_sdk::transport::Result;

pub fn load_program<T: Client>(
    bank_client: &T,
    from_keypair: &Keypair,
    loader_pubkey: &Pubkey,
    program: Vec<u8>,
) -> Pubkey {
    let program_keypair = Keypair::new();
    let program_pubkey = program_keypair.pubkey();

    let instruction = system_instruction::create_account(
        &from_keypair.pubkey(),
        &program_pubkey,
        1,
        program.len() as u64,
        loader_pubkey,
    );
    bank_client
        .send_instruction(&from_keypair, instruction)
        .unwrap();

    let chunk_size = 256; // Size of the chunk needs to fit into the transaction
    let mut offset = 0;
    for chunk in program.chunks(chunk_size) {
        let instruction =
            loader_instruction::write(&program_pubkey, loader_pubkey, offset, chunk.to_vec());
        let message = Message::new_with_payer(vec![instruction], Some(&from_keypair.pubkey()));
        bank_client
            .send_message(&[from_keypair, &program_keypair], message)
            .unwrap();
        offset += chunk_size as u32;
    }

    let instruction = loader_instruction::finalize(&program_pubkey, loader_pubkey);
    let message = Message::new_with_payer(vec![instruction], Some(&from_keypair.pubkey()));
    bank_client
        .send_message(&[from_keypair, &program_keypair], message)
        .unwrap();

    program_pubkey
}

pub fn run_program<T: Client, D: Serialize>(
    bank_client: &T,
    from_keypair: &Keypair,
    loader_pubkey: &Pubkey,
    account_metas: Vec<AccountMeta>,
    data: &D,
) -> Result<Signature> {
    let instruction = loader_instruction::invoke_main(loader_pubkey, data, account_metas);
    bank_client.send_instruction(from_keypair, instruction)
}
