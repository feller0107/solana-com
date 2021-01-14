#![cfg(any(feature = "bpf_c", feature = "bpf_rust"))]

#[macro_use]
extern crate solana_bpf_loader_program;

use solana_bpf_loader_program::{
    create_vm,
    serialization::{deserialize_parameters, serialize_parameters},
    syscalls::register_syscalls,
    ThisInstructionMeter,
};
use solana_rbpf::vm::{Config, Executable, Tracer};
use solana_runtime::{
    bank::Bank,
    bank_client::BankClient,
    genesis_utils::{create_genesis_config, GenesisConfigInfo},
    loader_utils::{
        load_buffer_account, load_program, load_upgradeable_program, set_upgrade_authority,
        upgrade_program,
    },
};
use solana_sdk::{
    account::Account,
    bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable,
    client::SyncClient,
    clock::{DEFAULT_SLOTS_PER_EPOCH, MAX_PROCESSING_AGE},
    entrypoint::{MAX_PERMITTED_DATA_INCREASE, SUCCESS},
    instruction::{AccountMeta, CompiledInstruction, Instruction, InstructionError},
    keyed_account::KeyedAccount,
    message::Message,
    process_instruction::{InvokeContext, MockInvokeContext},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar::{clock, fees, rent, slot_hashes, stake_history},
    transaction::{Transaction, TransactionError},
};
use std::{cell::RefCell, env, fs::File, io::Read, path::PathBuf, sync::Arc};

/// BPF program file extension
const PLATFORM_FILE_EXTENSION_BPF: &str = "so";

/// Create a BPF program file name
fn create_bpf_path(name: &str) -> PathBuf {
    let mut pathbuf = {
        let current_exe = env::current_exe().unwrap();
        PathBuf::from(current_exe.parent().unwrap().parent().unwrap())
    };
    pathbuf.push("bpf/");
    pathbuf.push(name);
    pathbuf.set_extension(PLATFORM_FILE_EXTENSION_BPF);
    pathbuf
}

fn load_bpf_program(
    bank_client: &BankClient,
    loader_id: &Pubkey,
    payer_keypair: &Keypair,
    name: &str,
) -> Pubkey {
    let elf = read_bpf_program(name);
    load_program(bank_client, payer_keypair, loader_id, elf)
}

fn read_bpf_program(name: &str) -> Vec<u8> {
    let path = create_bpf_path(name);
    let mut file = File::open(&path).unwrap_or_else(|err| {
        panic!("Failed to open {}: {}", path.display(), err);
    });
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();

    elf
}

#[cfg(feature = "bpf_rust")]
fn write_bpf_program(
    bank_client: &BankClient,
    loader_id: &Pubkey,
    payer_keypair: &Keypair,
    program_keypair: &Keypair,
    elf: &[u8],
) {
    use solana_sdk::loader_instruction;

    let chunk_size = 256; // Size of chunk just needs to fit into tx
    let mut offset = 0;
    for chunk in elf.chunks(chunk_size) {
        let instruction =
            loader_instruction::write(&program_keypair.pubkey(), loader_id, offset, chunk.to_vec());
        let message = Message::new(&[instruction], Some(&payer_keypair.pubkey()));

        bank_client
            .send_and_confirm_message(&[payer_keypair, &program_keypair], message)
            .unwrap();

        offset += chunk_size as u32;
    }
}

fn load_upgradeable_bpf_program(
    bank_client: &BankClient,
    payer_keypair: &Keypair,
    name: &str,
) -> (Pubkey, Keypair) {
    let path = create_bpf_path(name);
    let mut file = File::open(&path).unwrap_or_else(|err| {
        panic!("Failed to open {}: {}", path.display(), err);
    });
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    load_upgradeable_program(bank_client, payer_keypair, elf)
}

fn upgrade_bpf_program(
    bank_client: &BankClient,
    payer_keypair: &Keypair,
    executable_pubkey: &Pubkey,
    authority_keypair: &Keypair,
    name: &str,
) {
    let path = create_bpf_path(name);
    let mut file = File::open(&path).unwrap_or_else(|err| {
        panic!("Failed to open {}: {}", path.display(), err);
    });
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let buffer_pubkey = load_buffer_account(bank_client, payer_keypair, &elf);
    upgrade_program(
        bank_client,
        payer_keypair,
        executable_pubkey,
        &buffer_pubkey,
        &authority_keypair,
        &payer_keypair.pubkey(),
    )
}

fn run_program(
    name: &str,
    program_id: &Pubkey,
    parameter_accounts: &[KeyedAccount],
    instruction_data: &[u8],
) -> Result<u64, InstructionError> {
    let path = create_bpf_path(name);
    let mut file = File::open(path).unwrap();

    let mut data = vec![];
    file.read_to_end(&mut data).unwrap();
    let loader_id = bpf_loader::id();
    let mut invoke_context = MockInvokeContext::default();
    let parameter_bytes = serialize_parameters(
        &bpf_loader::id(),
        program_id,
        parameter_accounts,
        &instruction_data,
    )
    .unwrap();
    let compute_meter = invoke_context.get_compute_meter();
    let mut instruction_meter = ThisInstructionMeter { compute_meter };

    let config = Config {
        max_call_depth: 20,
        stack_frame_size: 4096,
        enable_instruction_meter: true,
        enable_instruction_tracing: true,
    };
    let mut executable = Executable::from_elf(&data, None, config).unwrap();
    executable.set_syscall_registry(register_syscalls(&mut invoke_context).unwrap());
    executable.jit_compile().unwrap();

    let mut instruction_count = 0;
    let mut tracer = None;
    for i in 0..2 {
        let mut parameter_bytes = parameter_bytes.clone();
        let mut vm = create_vm(
            &loader_id,
            executable.as_ref(),
            &mut parameter_bytes,
            parameter_accounts,
            &mut invoke_context,
        )
        .unwrap();
        let result = if i == 0 {
            vm.execute_program_interpreted(&mut instruction_meter)
        } else {
            vm.execute_program_jit(&mut instruction_meter)
        };
        assert_eq!(SUCCESS, result.unwrap());
        deserialize_parameters(&bpf_loader::id(), parameter_accounts, &parameter_bytes).unwrap();
        if i == 1 {
            assert_eq!(instruction_count, vm.get_total_instruction_count());
        }
        instruction_count = vm.get_total_instruction_count();
        if config.enable_instruction_tracing {
            if i == 1 {
                if !Tracer::compare(tracer.as_ref().unwrap(), vm.get_tracer()) {
                    let mut tracer_display = String::new();
                    tracer
                        .as_ref()
                        .unwrap()
                        .write(&mut tracer_display, vm.get_program())
                        .unwrap();
                    println!("TRACE (interpreted): {}", tracer_display);
                    let mut tracer_display = String::new();
                    vm.get_tracer()
                        .write(&mut tracer_display, vm.get_program())
                        .unwrap();
                    println!("TRACE (jit): {}", tracer_display);
                    assert!(false);
                }
            }
            tracer = Some(vm.get_tracer().clone());
        }
    }

    Ok(instruction_count)
}

fn process_transaction_and_record_inner(
    bank: &Bank,
    tx: Transaction,
) -> (Result<(), TransactionError>, Vec<Vec<CompiledInstruction>>) {
    let signature = tx.signatures.get(0).unwrap().clone();
    let txs = vec![tx];
    let tx_batch = bank.prepare_batch(&txs, None);
    let (mut results, _, mut inner, _transaction_logs) = bank.load_execute_and_commit_transactions(
        &tx_batch,
        MAX_PROCESSING_AGE,
        false,
        true,
        false,
    );
    let inner_instructions = inner.swap_remove(0);
    let result = results
        .fee_collection_results
        .swap_remove(0)
        .and_then(|_| bank.get_signature_status(&signature).unwrap());
    (
        result,
        inner_instructions.expect("cpi recording should be enabled"),
    )
}

#[test]
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
fn test_program_bpf_sanity() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "bpf_c")]
    {
        programs.extend_from_slice(&[
            ("alloc", true),
            ("bpf_to_bpf", true),
            ("multiple_static", true),
            ("noop", true),
            ("noop++", true),
            ("panic", false),
            ("relative_call", true),
            ("sanity", true),
            ("sanity++", true),
            ("sha256", true),
            ("struct_pass", true),
            ("struct_ret", true),
        ]);
    }
    #[cfg(feature = "bpf_rust")]
    {
        programs.extend_from_slice(&[
            ("solana_bpf_rust_128bit", true),
            ("solana_bpf_rust_alloc", true),
            ("solana_bpf_rust_custom_heap", true),
            ("solana_bpf_rust_dep_crate", true),
            ("solana_bpf_rust_external_spend", false),
            ("solana_bpf_rust_iter", true),
            ("solana_bpf_rust_many_args", true),
            ("solana_bpf_rust_mem", true),
            ("solana_bpf_rust_noop", true),
            ("solana_bpf_rust_panic", false),
            ("solana_bpf_rust_param_passing", true),
            ("solana_bpf_rust_rand", true),
            ("solana_bpf_rust_ristretto", true),
            ("solana_bpf_rust_sanity", true),
            ("solana_bpf_rust_sha256", true),
            ("solana_bpf_rust_sysval", true),
        ]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program.0);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new(&genesis_config);
        let (name, id, entrypoint) = solana_bpf_loader_program!();
        bank.add_builtin(&name, id, entrypoint);
        let bank = Arc::new(bank);

        // Create bank with a specific slot, used by solana_bpf_rust_sysvar test
        let bank = Bank::new_from_parent(&bank, &Pubkey::default(), DEFAULT_SLOTS_PER_EPOCH + 1);
        let bank_client = BankClient::new(bank);

        // Call user program
        let program_id =
            load_bpf_program(&bank_client, &bpf_loader::id(), &mint_keypair, program.0);
        let account_metas = vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new(Keypair::new().pubkey(), false),
            AccountMeta::new(clock::id(), false),
            AccountMeta::new(fees::id(), false),
            AccountMeta::new(slot_hashes::id(), false),
            AccountMeta::new(stake_history::id(), false),
            AccountMeta::new(rent::id(), false),
        ];
        let instruction = Instruction::new(program_id, &1u8, account_metas);
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        if program.1 {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
        }
    }
}

#[test]
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
fn test_program_bpf_loader_deprecated() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "bpf_c")]
    {
        programs.extend_from_slice(&[("deprecated_loader")]);
    }
    #[cfg(feature = "bpf_rust")]
    {
        programs.extend_from_slice(&[("solana_bpf_rust_deprecated_loader")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new(&genesis_config);
        let (name, id, entrypoint) = solana_bpf_loader_deprecated_program!();
        bank.add_builtin(&name, id, entrypoint);
        let bank_client = BankClient::new(bank);

        let program_id = load_bpf_program(
            &bank_client,
            &bpf_loader_deprecated::id(),
            &mint_keypair,
            program,
        );
        let account_metas = vec![AccountMeta::new(mint_keypair.pubkey(), true)];
        let instruction = Instruction::new(program_id, &1u8, account_metas);
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert!(result.is_ok());
    }
}

#[test]
fn test_program_bpf_duplicate_accounts() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "bpf_c")]
    {
        programs.extend_from_slice(&[("dup_accounts")]);
    }
    #[cfg(feature = "bpf_rust")]
    {
        programs.extend_from_slice(&[("solana_bpf_rust_dup_accounts")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new(&genesis_config);
        let (name, id, entrypoint) = solana_bpf_loader_program!();
        bank.add_builtin(&name, id, entrypoint);
        let bank = Arc::new(bank);
        let bank_client = BankClient::new_shared(&bank);
        let program_id = load_bpf_program(&bank_client, &bpf_loader::id(), &mint_keypair, program);
        let payee_account = Account::new(10, 1, &program_id);
        let payee_pubkey = solana_sdk::pubkey::new_rand();
        bank.store_account(&payee_pubkey, &payee_account);

        let account = Account::new(10, 1, &program_id);
        let pubkey = solana_sdk::pubkey::new_rand();
        let account_metas = vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new(payee_pubkey, false),
            AccountMeta::new(pubkey, false),
            AccountMeta::new(pubkey, false),
        ];

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new(program_id, &1u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(data[0], 1);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new(program_id, &2u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(data[0], 2);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new(program_id, &3u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(data[0], 3);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new(program_id, &4u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let lamports = bank_client.get_balance(&pubkey).unwrap();
        assert!(result.is_ok());
        assert_eq!(lamports, 11);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new(program_id, &5u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let lamports = bank_client.get_balance(&pubkey).unwrap();
        assert!(result.is_ok());
        assert_eq!(lamports, 12);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new(program_id, &6u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let lamports = bank_client.get_balance(&pubkey).unwrap();
        assert!(result.is_ok());
        assert_eq!(lamports, 13);
    }
}

#[test]
fn test_program_bpf_error_handling() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "bpf_c")]
    {
        programs.extend_from_slice(&[("error_handling")]);
    }
    #[cfg(feature = "bpf_rust")]
    {
        programs.extend_from_slice(&[("solana_bpf_rust_error_handling")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new(&genesis_config);
        let (name, id, entrypoint) = solana_bpf_loader_program!();
        bank.add_builtin(&name, id, entrypoint);
        let bank_client = BankClient::new(bank);
        let program_id = load_bpf_program(&bank_client, &bpf_loader::id(), &mint_keypair, program);
        let account_metas = vec![AccountMeta::new(mint_keypair.pubkey(), true)];

        let instruction = Instruction::new(program_id, &1u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert!(result.is_ok());

        let instruction = Instruction::new(program_id, &2u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::InvalidAccountData)
        );

        let instruction = Instruction::new(program_id, &3u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::Custom(0))
        );

        let instruction = Instruction::new(program_id, &4u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::Custom(42))
        );

        let instruction = Instruction::new(program_id, &5u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let result = result.unwrap_err().unwrap();
        if TransactionError::InstructionError(0, InstructionError::InvalidInstructionData) != result
        {
            assert_eq!(
                result,
                TransactionError::InstructionError(0, InstructionError::InvalidError)
            );
        }

        let instruction = Instruction::new(program_id, &6u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let result = result.unwrap_err().unwrap();
        if TransactionError::InstructionError(0, InstructionError::InvalidInstructionData) != result
        {
            assert_eq!(
                result,
                TransactionError::InstructionError(0, InstructionError::InvalidError)
            );
        }

        let instruction = Instruction::new(program_id, &7u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let result = result.unwrap_err().unwrap();
        if TransactionError::InstructionError(0, InstructionError::InvalidInstructionData) != result
        {
            assert_eq!(
                result,
                TransactionError::InstructionError(0, InstructionError::AccountBorrowFailed)
            );
        }

        let instruction = Instruction::new(program_id, &8u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::InvalidInstructionData)
        );

        let instruction = Instruction::new(program_id, &9u8, account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::MaxSeedLengthExceeded)
        );
    }
}

#[test]
fn test_program_bpf_invoke() {
    solana_logger::setup();

    const TEST_SUCCESS: u8 = 1;
    const TEST_PRIVILEGE_ESCALATION_SIGNER: u8 = 2;
    const TEST_PRIVILEGE_ESCALATION_WRITABLE: u8 = 3;
    const TEST_PPROGRAM_NOT_EXECUTABLE: u8 = 4;
    const TEST_EMPTY_ACCOUNTS_SLICE: u8 = 5;
    const TEST_CAP_SEEDS: u8 = 6;
    const TEST_CAP_SIGNERS: u8 = 7;
    const TEST_ALLOC_ACCESS_VIOLATION: u8 = 8;
    const TEST_INSTRUCTION_DATA_TOO_LARGE: u8 = 9;
    const TEST_INSTRUCTION_META_TOO_LARGE: u8 = 10;
    const TEST_RETURN_ERROR: u8 = 11;

    #[allow(dead_code)]
    #[derive(Debug)]
    enum Languages {
        C,
        Rust,
    }
    let mut programs = Vec::new();
    #[cfg(feature = "bpf_c")]
    {
        programs.push((Languages::C, "invoke", "invoked", "noop"));
    }
    #[cfg(feature = "bpf_rust")]
    {
        programs.push((
            Languages::Rust,
            "solana_bpf_rust_invoke",
            "solana_bpf_rust_invoked",
            "solana_bpf_rust_noop",
        ));
    }
    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new(&genesis_config);
        let (name, id, entrypoint) = solana_bpf_loader_program!();
        bank.add_builtin(&name, id, entrypoint);
        let bank = Arc::new(bank);
        let bank_client = BankClient::new_shared(&bank);

        let invoke_program_id =
            load_bpf_program(&bank_client, &bpf_loader::id(), &mint_keypair, program.1);
        let invoked_program_id =
            load_bpf_program(&bank_client, &bpf_loader::id(), &mint_keypair, program.2);
        let noop_program_id =
            load_bpf_program(&bank_client, &bpf_loader::id(), &mint_keypair, program.3);

        let argument_keypair = Keypair::new();
        let account = Account::new(42, 100, &invoke_program_id);
        bank.store_account(&argument_keypair.pubkey(), &account);

        let invoked_argument_keypair = Keypair::new();
        let account = Account::new(10, 10, &invoked_program_id);
        bank.store_account(&invoked_argument_keypair.pubkey(), &account);

        let from_keypair = Keypair::new();
        let account = Account::new(84, 0, &solana_sdk::system_program::id());
        bank.store_account(&from_keypair.pubkey(), &account);

        let (derived_key1, bump_seed1) =
            Pubkey::find_program_address(&[b"You pass butter"], &invoke_program_id);
        let (derived_key2, bump_seed2) =
            Pubkey::find_program_address(&[b"Lil'", b"Bits"], &invoked_program_id);
        let (derived_key3, bump_seed3) =
            Pubkey::find_program_address(&[derived_key2.as_ref()], &invoked_program_id);

        let mint_pubkey = mint_keypair.pubkey();
        let account_metas = vec![
            AccountMeta::new(mint_pubkey, true),
            AccountMeta::new(argument_keypair.pubkey(), true),
            AccountMeta::new_readonly(invoked_program_id, false),
            AccountMeta::new(invoked_argument_keypair.pubkey(), true),
            AccountMeta::new_readonly(invoked_program_id, false),
            AccountMeta::new(argument_keypair.pubkey(), true),
            AccountMeta::new(derived_key1, false),
            AccountMeta::new(derived_key2, false),
            AccountMeta::new_readonly(derived_key3, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new(from_keypair.pubkey(), true),
        ];

        // success cases

        let instruction = Instruction::new(
            invoke_program_id,
            &[TEST_SUCCESS, bump_seed1, bump_seed2, bump_seed3],
            account_metas.clone(),
        );
        let noop_instruction = Instruction::new(noop_program_id, &(), vec![]);
        let message = Message::new(&[instruction, noop_instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        assert!(result.is_ok());
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();

        let expected_invoked_programs = match program.0 {
            Languages::C => vec![
                solana_sdk::system_program::id(),
                solana_sdk::system_program::id(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
            ],
            Languages::Rust => vec![
                solana_sdk::system_program::id(),
                solana_sdk::system_program::id(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
            ],
        };
        assert_eq!(invoked_programs.len(), expected_invoked_programs.len());
        assert_eq!(invoked_programs, expected_invoked_programs);

        let no_invoked_programs: Vec<Pubkey> = inner_instructions[1]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(no_invoked_programs.len(), 0);

        // failure cases

        let instruction = Instruction::new(
            invoke_program_id,
            &[
                TEST_PRIVILEGE_ESCALATION_SIGNER,
                bump_seed1,
                bump_seed2,
                bump_seed3,
            ],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );

        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![invoked_program_id.clone()]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation)
        );

        let instruction = Instruction::new(
            invoke_program_id,
            &[
                TEST_PRIVILEGE_ESCALATION_WRITABLE,
                bump_seed1,
                bump_seed2,
                bump_seed3,
            ],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![invoked_program_id.clone()]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation)
        );

        let instruction = Instruction::new(
            invoke_program_id,
            &[
                TEST_PPROGRAM_NOT_EXECUTABLE,
                bump_seed1,
                bump_seed2,
                bump_seed3,
            ],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::AccountNotExecutable)
        );

        let instruction = Instruction::new(
            invoke_program_id,
            &[
                TEST_EMPTY_ACCOUNTS_SLICE,
                bump_seed1,
                bump_seed2,
                bump_seed3,
            ],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::MissingAccount)
        );

        let instruction = Instruction::new(
            invoke_program_id,
            &[TEST_CAP_SEEDS, bump_seed1, bump_seed2, bump_seed3],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::MaxSeedLengthExceeded)
        );

        let instruction = Instruction::new(
            invoke_program_id,
            &[TEST_CAP_SIGNERS, bump_seed1, bump_seed2, bump_seed3],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
        );

        let instruction = Instruction::new(
            invoke_program_id,
            &[
                TEST_INSTRUCTION_DATA_TOO_LARGE,
                bump_seed1,
                bump_seed2,
                bump_seed3,
            ],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::ComputationalBudgetExceeded)
        );

        let instruction = Instruction::new(
            invoke_program_id,
            &[
                TEST_INSTRUCTION_META_TOO_LARGE,
                bump_seed1,
                bump_seed2,
                bump_seed3,
            ],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::ComputationalBudgetExceeded)
        );

        let instruction = Instruction::new(
            invoke_program_id,
            &[TEST_RETURN_ERROR, bump_seed1, bump_seed2, bump_seed3],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![invoked_program_id.clone()]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::Custom(42))
        );

        // Check final state

        assert_eq!(43, bank.get_balance(&derived_key1));
        let account = bank.get_account(&derived_key1).unwrap();
        assert_eq!(invoke_program_id, account.owner);
        assert_eq!(
            MAX_PERMITTED_DATA_INCREASE,
            bank.get_account(&derived_key1).unwrap().data.len()
        );
        for i in 0..20 {
            assert_eq!(i as u8, account.data[i]);
        }

        // Attempt to realloc into unauthorized address space
        let account = Account::new(84, 0, &solana_sdk::system_program::id());
        bank.store_account(&from_keypair.pubkey(), &account);
        bank.store_account(&derived_key1, &Account::default());
        let instruction = Instruction::new(
            invoke_program_id,
            &[
                TEST_ALLOC_ACCESS_VIOLATION,
                bump_seed1,
                bump_seed2,
                bump_seed3,
            ],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions) = process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| message.account_keys[ix.program_id_index as usize].clone())
            .collect();
        assert_eq!(invoked_programs, vec![solana_sdk::system_program::id()]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
        );
    }

    // Check for program id spoofing
    {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new(&genesis_config);
        let (name, id, entrypoint) = solana_bpf_loader_program!();
        bank.add_builtin(&name, id, entrypoint);
        let bank = Arc::new(bank);
        let bank_client = BankClient::new_shared(&bank);

        let malicious_swap_pubkey = load_bpf_program(
            &bank_client,
            &bpf_loader::id(),
            &mint_keypair,
            "solana_bpf_rust_spoof1",
        );
        let malicious_system_pubkey = load_bpf_program(
            &bank_client,
            &bpf_loader::id(),
            &mint_keypair,
            "solana_bpf_rust_spoof1_system",
        );

        let from_pubkey = Pubkey::new_unique();
        let account = Account::new(10, 0, &solana_sdk::system_program::id());
        bank.store_account(&from_pubkey, &account);

        let to_pubkey = Pubkey::new_unique();
        let account = Account::new(0, 0, &solana_sdk::system_program::id());
        bank.store_account(&to_pubkey, &account);

        let account_metas = vec![
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(malicious_system_pubkey, false),
            AccountMeta::new(from_pubkey, false),
            AccountMeta::new(to_pubkey, false),
        ];

        let instruction = Instruction::new(malicious_swap_pubkey, &(), account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature)
        );
        assert_eq!(10, bank.get_balance(&from_pubkey));
        assert_eq!(0, bank.get_balance(&to_pubkey));
    }

    // Check the caller has access to cpi program
    {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new(&genesis_config);
        let (name, id, entrypoint) = solana_bpf_loader_program!();
        bank.add_builtin(&name, id, entrypoint);
        let bank = Arc::new(bank);
        let bank_client = BankClient::new_shared(&bank);

        let caller_pubkey = load_bpf_program(
            &bank_client,
            &bpf_loader::id(),
            &mint_keypair,
            "solana_bpf_rust_caller_access",
        );
        let caller2_pubkey = load_bpf_program(
            &bank_client,
            &bpf_loader::id(),
            &mint_keypair,
            "solana_bpf_rust_caller_access",
        );
        let account_metas = vec![
            AccountMeta::new_readonly(caller_pubkey, false),
            AccountMeta::new_readonly(caller2_pubkey, false),
        ];
        let instruction = Instruction::new(caller_pubkey, &[1_u8], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::MissingAccount)
        );
    }
}

#[cfg(feature = "bpf_rust")]
#[test]
fn test_program_bpf_ro_modify() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let mut bank = Bank::new(&genesis_config);
    let (name, id, entrypoint) = solana_bpf_loader_program!();
    bank.add_builtin(&name, id, entrypoint);
    let bank = Arc::new(bank);
    let bank_client = BankClient::new_shared(&bank);

    let program_pubkey = load_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        "solana_bpf_rust_ro_modify",
    );

    let test_keypair = Keypair::new();
    let account = Account::new(10, 0, &solana_sdk::system_program::id());
    bank.store_account(&test_keypair.pubkey(), &account);

    let account_metas = vec![
        AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        AccountMeta::new(test_keypair.pubkey(), true),
    ];

    let instruction = Instruction::new(program_pubkey, &[1_u8], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair, &test_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
    );

    let instruction = Instruction::new(program_pubkey, &[3_u8], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair, &test_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
    );

    let instruction = Instruction::new(program_pubkey, &[4_u8], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair, &test_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
    );
}

#[cfg(feature = "bpf_rust")]
#[test]
fn test_program_bpf_call_depth() {
    use solana_sdk::process_instruction::BpfComputeBudget;

    solana_logger::setup();

    println!("Test program: solana_bpf_rust_call_depth");

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let mut bank = Bank::new(&genesis_config);
    let (name, id, entrypoint) = solana_bpf_loader_program!();
    bank.add_builtin(&name, id, entrypoint);
    let bank_client = BankClient::new(bank);
    let program_id = load_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        "solana_bpf_rust_call_depth",
    );

    let instruction = Instruction::new(
        program_id,
        &(BpfComputeBudget::default().max_call_depth - 1),
        vec![],
    );
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert!(result.is_ok());

    let instruction = Instruction::new(
        program_id,
        &BpfComputeBudget::default().max_call_depth,
        vec![],
    );
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert!(result.is_err());
}

#[test]
fn assert_instruction_count() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "bpf_c")]
    {
        programs.extend_from_slice(&[
            ("bpf_to_bpf", 13),
            ("multiple_static", 8),
            ("noop", 57),
            ("relative_call", 10),
            ("sanity", 176),
            ("sanity++", 176),
            ("struct_pass", 8),
            ("struct_ret", 22),
        ]);
    }
    #[cfg(feature = "bpf_rust")]
    {
        programs.extend_from_slice(&[
            ("solana_bpf_rust_128bit", 572),
            ("solana_bpf_rust_alloc", 12919),
            ("solana_bpf_rust_dep_crate", 2),
            ("solana_bpf_rust_external_spend", 514),
            ("solana_bpf_rust_iter", 724),
            ("solana_bpf_rust_many_args", 237),
            ("solana_bpf_rust_noop", 488),
            ("solana_bpf_rust_param_passing", 48),
            ("solana_bpf_rust_ristretto", 19399),
            ("solana_bpf_rust_sanity", 894),
        ]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program.0);
        let program_id = solana_sdk::pubkey::new_rand();
        let key = solana_sdk::pubkey::new_rand();
        let mut account = RefCell::new(Account::default());
        let parameter_accounts = vec![KeyedAccount::new(&key, false, &mut account)];
        let count = run_program(program.0, &program_id, &parameter_accounts[..], &[]).unwrap();
        println!("  {} : {:?} ({:?})", program.0, count, program.1,);
        assert!(count <= program.1);
    }
}

#[cfg(any(feature = "bpf_rust"))]
#[test]
fn test_program_bpf_instruction_introspection() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50_000);
    let mut bank = Bank::new(&genesis_config);

    let (name, id, entrypoint) = solana_bpf_loader_program!();
    bank.add_builtin(&name, id, entrypoint);
    let bank = Arc::new(bank);
    let bank_client = BankClient::new_shared(&bank);

    let program_id = load_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        "solana_bpf_rust_instruction_introspection",
    );

    // Passing transaction
    let account_metas = vec![AccountMeta::new_readonly(
        solana_sdk::sysvar::instructions::id(),
        false,
    )];
    let instruction0 = Instruction::new(program_id, &[0u8, 0u8], account_metas.clone());
    let instruction1 = Instruction::new(program_id, &[0u8, 1u8], account_metas.clone());
    let instruction2 = Instruction::new(program_id, &[0u8, 2u8], account_metas);
    let message = Message::new(
        &[instruction0, instruction1, instruction2],
        Some(&mint_keypair.pubkey()),
    );
    let result = bank_client.send_and_confirm_message(&[&mint_keypair], message);
    assert!(result.is_ok());

    // writable special instructions11111 key, should not be allowed
    let account_metas = vec![AccountMeta::new(
        solana_sdk::sysvar::instructions::id(),
        false,
    )];
    let instruction = Instruction::new(program_id, &0u8, account_metas);
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InvalidAccountIndex
    );

    // No accounts, should error
    let instruction = Instruction::new(program_id, &0u8, vec![]);
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(
            0,
            solana_sdk::instruction::InstructionError::NotEnoughAccountKeys
        )
    );
    assert!(bank
        .get_account(&solana_sdk::sysvar::instructions::id())
        .is_none());
}

#[cfg(feature = "bpf_rust")]
#[test]
fn test_program_bpf_test_use_latest_executor() {
    use solana_sdk::{loader_instruction, system_instruction};

    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let mut bank = Bank::new(&genesis_config);
    let (name, id, entrypoint) = solana_bpf_loader_program!();
    bank.add_builtin(&name, id, entrypoint);
    let bank_client = BankClient::new(bank);
    let panic_id = load_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        "solana_bpf_rust_panic",
    );

    let program_keypair = Keypair::new();

    // Write the panic program into the program account
    let elf = read_bpf_program("solana_bpf_rust_panic");
    let message = Message::new(
        &[system_instruction::create_account(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            1,
            elf.len() as u64 * 2,
            &bpf_loader::id(),
        )],
        Some(&mint_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &program_keypair], message)
        .is_ok());
    write_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        &program_keypair,
        &elf,
    );

    // Finalize the panic program, but fail the tx
    let message = Message::new(
        &[
            loader_instruction::finalize(&program_keypair.pubkey(), &bpf_loader::id()),
            Instruction::new(panic_id, &0u8, vec![]),
        ],
        Some(&mint_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &program_keypair], message)
        .is_err());

    // Write the noop program into the same program account
    let elf = read_bpf_program("solana_bpf_rust_noop");
    write_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        &program_keypair,
        &elf,
    );

    // Finalize the noop program
    let message = Message::new(
        &[loader_instruction::finalize(
            &program_keypair.pubkey(),
            &bpf_loader::id(),
        )],
        Some(&mint_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &program_keypair], message)
        .is_ok());

    // Call the noop program, should get noop not panic
    let message = Message::new(
        &[Instruction::new(program_keypair.pubkey(), &0u8, vec![])],
        Some(&mint_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair], message)
        .is_ok());
}

#[ignore] // Invoking BPF loaders from CPI not allowed
#[cfg(feature = "bpf_rust")]
#[test]
fn test_program_bpf_test_use_latest_executor2() {
    use solana_sdk::{loader_instruction, system_instruction};

    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let mut bank = Bank::new(&genesis_config);
    let (name, id, entrypoint) = solana_bpf_loader_program!();
    bank.add_builtin(&name, id, entrypoint);
    let bank_client = BankClient::new(bank);
    let invoke_and_error = load_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        "solana_bpf_rust_invoke_and_error",
    );
    let invoke_and_ok = load_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        "solana_bpf_rust_invoke_and_ok",
    );

    let program_keypair = Keypair::new();

    // Write the panic program into the program account
    let elf = read_bpf_program("solana_bpf_rust_panic");
    let message = Message::new(
        &[system_instruction::create_account(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            1,
            elf.len() as u64 * 2,
            &bpf_loader::id(),
        )],
        Some(&mint_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &program_keypair], message)
        .is_ok());
    write_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        &program_keypair,
        &elf,
    );

    // - invoke finalize and return error, swallow error
    let mut instruction =
        loader_instruction::finalize(&program_keypair.pubkey(), &bpf_loader::id());
    instruction.accounts.insert(
        0,
        AccountMeta {
            is_signer: false,
            is_writable: false,
            pubkey: instruction.program_id,
        },
    );
    instruction.program_id = invoke_and_ok;
    instruction.accounts.insert(
        0,
        AccountMeta {
            is_signer: false,
            is_writable: false,
            pubkey: invoke_and_error,
        },
    );
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &program_keypair], message)
        .is_ok());

    // invoke program, verify not found
    let message = Message::new(
        &[Instruction::new(program_keypair.pubkey(), &0u8, vec![])],
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        bank_client
            .send_and_confirm_message(&[&mint_keypair], message)
            .unwrap_err()
            .unwrap(),
        TransactionError::InvalidProgramForExecution
    );

    // Write the noop program into the same program account
    let elf = read_bpf_program("solana_bpf_rust_noop");
    write_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        &program_keypair,
        &elf,
    );

    // Finalize the noop program
    let message = Message::new(
        &[loader_instruction::finalize(
            &program_keypair.pubkey(),
            &bpf_loader::id(),
        )],
        Some(&mint_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &program_keypair], message)
        .is_ok());

    // Call the program, should get noop, not panic
    let message = Message::new(
        &[Instruction::new(program_keypair.pubkey(), &0u8, vec![])],
        Some(&mint_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair], message)
        .is_ok());
}

#[cfg(feature = "bpf_rust")]
#[test]
fn test_program_bpf_upgrade() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let mut bank = Bank::new(&genesis_config);
    let (name, id, entrypoint) = solana_bpf_loader_upgradeable_program!();
    bank.add_builtin(&name, id, entrypoint);
    let bank_client = BankClient::new(bank);

    // Deploy upgrade program
    let (program_id, authority_keypair) =
        load_upgradeable_bpf_program(&bank_client, &mint_keypair, "solana_bpf_rust_upgradeable");

    let mut instruction = Instruction::new(
        program_id,
        &[0],
        vec![
            AccountMeta::new(program_id.clone(), false),
            AccountMeta::new(clock::id(), false),
            AccountMeta::new(fees::id(), false),
        ],
    );

    // Call upgrade program
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(42))
    );

    // Upgrade program
    upgrade_bpf_program(
        &bank_client,
        &mint_keypair,
        &program_id,
        &authority_keypair,
        "solana_bpf_rust_upgraded",
    );

    // Call upgraded program
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(43))
    );

    // Set a new authority
    let new_authority_keypair = Keypair::new();
    set_upgrade_authority(
        &bank_client,
        &mint_keypair,
        &program_id,
        &authority_keypair,
        Some(&new_authority_keypair.pubkey()),
    );

    // Upgrade back to the original program
    upgrade_bpf_program(
        &bank_client,
        &mint_keypair,
        &program_id,
        &new_authority_keypair,
        "solana_bpf_rust_upgradeable",
    );

    // Call original program
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(42))
    );
}

#[cfg(feature = "bpf_rust")]
#[test]
fn test_program_bpf_invoke_upgradeable_via_cpi() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let mut bank = Bank::new(&genesis_config);
    let (name, id, entrypoint) = solana_bpf_loader_program!();
    bank.add_builtin(&name, id, entrypoint);
    let (name, id, entrypoint) = solana_bpf_loader_upgradeable_program!();
    bank.add_builtin(&name, id, entrypoint);
    let bank_client = BankClient::new(bank);
    let invoke_and_return = load_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        "solana_bpf_rust_invoke_and_return",
    );

    // Deploy upgradeable program
    let (program_id, authority_keypair) =
        load_upgradeable_bpf_program(&bank_client, &mint_keypair, "solana_bpf_rust_upgradeable");

    let mut instruction = Instruction::new(
        invoke_and_return,
        &[0],
        vec![
            AccountMeta::new(program_id, false),
            AccountMeta::new(program_id, false),
            AccountMeta::new(clock::id(), false),
            AccountMeta::new(fees::id(), false),
        ],
    );

    // Call invoker program to invoke the upgradeable program
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(42))
    );

    // Upgrade program
    upgrade_bpf_program(
        &bank_client,
        &mint_keypair,
        &program_id,
        &authority_keypair,
        "solana_bpf_rust_upgraded",
    );

    // Call the upgraded program
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(43))
    );

    // Set a new authority
    let new_authority_keypair = Keypair::new();
    set_upgrade_authority(
        &bank_client,
        &mint_keypair,
        &program_id,
        &authority_keypair,
        Some(&new_authority_keypair.pubkey()),
    );

    // Upgrade back to the original program
    upgrade_bpf_program(
        &bank_client,
        &mint_keypair,
        &program_id,
        &new_authority_keypair,
        "solana_bpf_rust_upgradeable",
    );

    // Call original program
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(42))
    );
}

#[test]
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
fn test_program_bpf_disguised_as_bpf_loader() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "bpf_c")]
    {
        programs.extend_from_slice(&[("noop")]);
    }
    #[cfg(feature = "bpf_rust")]
    {
        programs.extend_from_slice(&[("noop")]);
    }

    for program in programs.iter() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new(&genesis_config);
        let (name, id, entrypoint) = solana_bpf_loader_deprecated_program!();
        bank.add_builtin(&name, id, entrypoint);
        let bank_client = BankClient::new(bank);

        let program_id = load_bpf_program(
            &bank_client,
            &bpf_loader_deprecated::id(),
            &mint_keypair,
            program,
        );
        let account_metas = vec![AccountMeta::new_readonly(program_id, false)];
        let instruction = Instruction::new(bpf_loader_deprecated::id(), &1u8, account_metas);
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::IncorrectProgramId)
        );
    }
}

#[cfg(feature = "bpf_rust")]
#[test]
fn test_program_bpf_upgrade_via_cpi() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let mut bank = Bank::new(&genesis_config);
    let (name, id, entrypoint) = solana_bpf_loader_program!();
    bank.add_builtin(&name, id, entrypoint);
    let (name, id, entrypoint) = solana_bpf_loader_upgradeable_program!();
    bank.add_builtin(&name, id, entrypoint);
    let bank_client = BankClient::new(bank);
    let invoke_and_return = load_bpf_program(
        &bank_client,
        &bpf_loader::id(),
        &mint_keypair,
        "solana_bpf_rust_invoke_and_return",
    );

    // Deploy upgradeable program
    let (program_id, authority_keypair) =
        load_upgradeable_bpf_program(&bank_client, &mint_keypair, "solana_bpf_rust_upgradeable");

    let mut instruction = Instruction::new(
        invoke_and_return,
        &[0],
        vec![
            AccountMeta::new(program_id, false),
            AccountMeta::new(program_id, false),
            AccountMeta::new(clock::id(), false),
            AccountMeta::new(fees::id(), false),
        ],
    );

    // Call the upgraded program
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(42))
    );

    // Load the buffer account
    let path = create_bpf_path("solana_bpf_rust_upgraded");
    let mut file = File::open(&path).unwrap_or_else(|err| {
        panic!("Failed to open {}: {}", path.display(), err);
    });
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let buffer_pubkey = load_buffer_account(&bank_client, &mint_keypair, &elf);

    // Upgrade program via CPI
    let mut upgrade_instruction = bpf_loader_upgradeable::upgrade(
        &program_id,
        &buffer_pubkey,
        &authority_keypair.pubkey(),
        &mint_keypair.pubkey(),
    );
    upgrade_instruction.program_id = invoke_and_return;
    upgrade_instruction
        .accounts
        .insert(0, AccountMeta::new(bpf_loader_upgradeable::id(), false));
    let message = Message::new(&[upgrade_instruction], Some(&mint_keypair.pubkey()));
    bank_client
        .send_and_confirm_message(&[&mint_keypair, &authority_keypair], message)
        .unwrap();

    // Call the upgraded program
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(43))
    );
}
