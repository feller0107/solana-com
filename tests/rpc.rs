use bincode::serialize;
use log::*;
use reqwest;
use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};
use solana::rpc_request::get_rpc_request_str;
use solana::thin_client::new_fullnode;
use solana_sdk::hash::Hash;
use solana_sdk::signature::{Keypair, KeypairUtil};
use solana_sdk::system_transaction::SystemTransaction;
use std::fs::remove_dir_all;
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_rpc_send_tx() {
    solana_logger::setup();

    let (server, leader_data, alice, ledger_path) = new_fullnode();
    let server_exit = server.run(None);
    let bob_pubkey = Keypair::new().pubkey();

    let client = reqwest::Client::new();
    let request = json!({
       "jsonrpc": "2.0",
       "id": 1,
       "method": "getRecentBlockHash",
       "params": json!([])
    });
    let rpc_addr = leader_data.rpc;
    let rpc_string = get_rpc_request_str(rpc_addr, false);
    let mut response = client
        .post(&rpc_string)
        .header(CONTENT_TYPE, "application/json")
        .body(request.to_string())
        .send()
        .unwrap();
    let json: Value = serde_json::from_str(&response.text().unwrap()).unwrap();
    let block_hash_vec = bs58::decode(json["result"].as_str().unwrap())
        .into_vec()
        .unwrap();
    let block_hash = Hash::new(&block_hash_vec);

    info!("block_hash: {:?}", block_hash);
    let tx = SystemTransaction::new_move(&alice, bob_pubkey, 20, block_hash, 0);
    let serial_tx = serialize(&tx).unwrap();

    let client = reqwest::Client::new();
    let request = json!({
       "jsonrpc": "2.0",
       "id": 1,
       "method": "sendTransaction",
       "params": json!([serial_tx])
    });
    let rpc_addr = leader_data.rpc;
    let rpc_string = get_rpc_request_str(rpc_addr, false);
    let mut response = client
        .post(&rpc_string)
        .header(CONTENT_TYPE, "application/json")
        .body(request.to_string())
        .send()
        .unwrap();
    let json: Value = serde_json::from_str(&response.text().unwrap()).unwrap();
    let signature = &json["result"];

    let mut confirmed_tx = false;

    let client = reqwest::Client::new();
    let request = json!({
       "jsonrpc": "2.0",
       "id": 1,
       "method": "confirmTransaction",
       "params": [signature],
    });

    for _ in 0..solana_sdk::timing::DEFAULT_TICKS_PER_SLOT {
        let mut response = client
            .post(&rpc_string)
            .header(CONTENT_TYPE, "application/json")
            .body(request.to_string())
            .send()
            .unwrap();
        let response_json_text = response.text().unwrap();
        let json: Value = serde_json::from_str(&response_json_text).unwrap();

        if true == json["result"] {
            confirmed_tx = true;
            break;
        }

        sleep(Duration::from_millis(500));
    }

    assert_eq!(confirmed_tx, true);

    server_exit();
    remove_dir_all(ledger_path).unwrap();
}
