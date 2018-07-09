#[macro_use]
extern crate log;
extern crate bincode;
extern crate serde_json;
extern crate solana;

use solana::crdt::TestNode;
use solana::crdt::{Crdt, ReplicatedData};
use solana::entry_writer::EntryWriter;
use solana::fullnode::{FullNode, InFile, OutFile};
use solana::logger;
use solana::mint::Mint;
use solana::ncp::Ncp;
use solana::signature::{KeyPair, KeyPairUtil, PublicKey};
use solana::streamer::default_window;
use solana::thin_client::ThinClient;
use std::fs::File;
use std::mem;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::thread::sleep;
use std::time::Duration;

fn converge(leader: &ReplicatedData, num_nodes: usize) -> Vec<ReplicatedData> {
    //lets spy on the network
    let exit = Arc::new(AtomicBool::new(false));
    let mut spy = TestNode::new();
    let daddr = "0.0.0.0:0".parse().unwrap();
    let me = spy.data.id.clone();
    spy.data.contact_info.tvu = daddr;
    spy.data.contact_info.rpu = daddr;
    let mut spy_crdt = Crdt::new(spy.data);
    spy_crdt.insert(&leader);
    spy_crdt.set_leader(leader.id);
    let spy_ref = Arc::new(RwLock::new(spy_crdt));
    let spy_window = default_window();
    let ncp = Ncp::new(
        spy_ref.clone(),
        spy_window,
        spy.sockets.gossip,
        spy.sockets.gossip_send,
        exit.clone(),
    ).unwrap();
    //wait for the network to converge
    let mut converged = false;
    let mut rv = vec![];
    for _ in 0..30 {
        let num = spy_ref.read().unwrap().convergence();
        let mut v: Vec<ReplicatedData> = spy_ref
            .read()
            .unwrap()
            .table
            .values()
            .into_iter()
            .filter(|x| x.id != me)
            .filter(|x| x.contact_info.rpu != daddr)
            .cloned()
            .collect();
        if num >= num_nodes as u64 && v.len() >= num_nodes {
            rv.append(&mut v);
            converged = true;
            break;
        }
        sleep(Duration::new(1, 0));
    }
    assert!(converged);
    ncp.close().unwrap();
    rv
}

fn genesis(num: i64) -> (Mint, String) {
    let mint = Mint::new(num);
    let id = {
        let ids: Vec<_> = mint.pubkey().iter().map(|id| format!("{}", id)).collect();
        ids.join("")
    };
    let path = format!("target/test_multi_node_dynamic_network-{}.log", id);
    let mut writer = File::create(path.clone()).unwrap();

    EntryWriter::write_entries(&mut writer, mint.create_entries()).unwrap();
    (mint, path.to_string())
}

#[test]
fn test_multi_node_validator_catchup_from_zero() {
    logger::setup();
    const N: usize = 5;
    trace!("test_multi_node_validator_catchup_from_zero");
    let leader = TestNode::new();
    let leader_data = leader.data.clone();
    let bob_pubkey = KeyPair::new().pubkey();

    let (alice, ledger_path) = genesis(10_000);
    let server = FullNode::new(leader, true, InFile::Path(ledger_path.clone()), None, None);
    let mut nodes = vec![server];
    for _ in 0..N {
        let validator = TestNode::new();
        let mut val = FullNode::new(
            validator,
            false,
            InFile::Path(ledger_path.clone()),
            Some(leader_data.contact_info.ncp),
            None,
        );
        nodes.push(val);
    }
    let servers = converge(&leader_data, N + 1);
    //contains the leader addr as well
    assert_eq!(servers.len(), N + 1);
    //verify leader can do transfer
    let leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, None).unwrap();
    assert_eq!(leader_balance, 500);
    //verify validator has the same balance
    let mut success = 0usize;
    for server in servers.iter() {
        info!("0server: {:x}", server.debug_id());
        let mut client = mk_client(server);
        if let Ok(bal) = client.poll_get_balance(&bob_pubkey) {
            info!("validator balance {}", bal);
            if bal == leader_balance {
                success += 1;
            }
        }
    }
    assert_eq!(success, servers.len());

    success = 0;
    // start up another validator, converge and then check everyone's balances
    let val = FullNode::new(
        TestNode::new(),
        false,
        InFile::Path(ledger_path.clone()),
        Some(leader_data.contact_info.ncp),
        None,
    );
    nodes.push(val);
    //contains the leader and new node
    let servers = converge(&leader_data, N + 2);

    let mut leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, None).unwrap();
    info!("leader balance {}", leader_balance);
    loop {
        let mut client = mk_client(&leader_data);
        leader_balance = client.poll_get_balance(&bob_pubkey).unwrap();
        if leader_balance == 1000 {
            break;
        }
        sleep(Duration::from_millis(300));
    }
    assert_eq!(leader_balance, 1000);

    for server in servers.iter() {
        let mut client = mk_client(server);
        info!("1server: {:x}", server.debug_id());
        for _ in 0..10 {
            if let Ok(bal) = client.poll_get_balance(&bob_pubkey) {
                info!("validator balance {}", bal);
                if bal == leader_balance {
                    success += 1;
                    break;
                }
            }
            sleep(Duration::from_millis(500));
        }
    }
    assert_eq!(success, servers.len());

    for node in nodes {
        node.close().unwrap();
    }
}

#[test]
fn test_multi_node_basic() {
    logger::setup();
    const N: usize = 5;
    trace!("test_multi_node_basic");
    let leader = TestNode::new();
    let leader_data = leader.data.clone();
    let bob_pubkey = KeyPair::new().pubkey();
    let (alice, ledger_path) = genesis(10_000);
    let server = FullNode::new(leader, true, InFile::Path(ledger_path.clone()), None, None);
    let mut nodes = vec![server];
    for _ in 0..N {
        let validator = TestNode::new();
        let val = FullNode::new(
            validator,
            false,
            InFile::Path(ledger_path.clone()),
            Some(leader_data.contact_info.ncp),
            None,
        );
        nodes.push(val);
    }
    let servers = converge(&leader_data, N + 1);
    //contains the leader addr as well
    assert_eq!(servers.len(), N + 1);
    //verify leader can do transfer
    let leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, None).unwrap();
    assert_eq!(leader_balance, 500);
    //verify validator has the same balance
    let mut success = 0usize;
    for server in servers.iter() {
        let mut client = mk_client(server);
        if let Ok(bal) = client.poll_get_balance(&bob_pubkey) {
            trace!("validator balance {}", bal);
            if bal == leader_balance {
                success += 1;
            }
        }
    }
    assert_eq!(success, servers.len());

    for node in nodes {
        node.close().unwrap();
    }
    std::fs::remove_file(ledger_path).unwrap();
}

#[test]
fn test_boot_validator_from_file() {
    logger::setup();
    let leader = TestNode::new();
    let bob_pubkey = KeyPair::new().pubkey();
    let (alice, ledger_path) = genesis(100_000);
    let leader_data = leader.data.clone();
    let leader_fullnode = FullNode::new(
        leader,
        true,
        InFile::Path(ledger_path.clone()),
        None,
        Some(OutFile::Path(ledger_path.clone())),
    );
    let leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, Some(500)).unwrap();
    assert_eq!(leader_balance, 500);
    let leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, Some(1000)).unwrap();
    assert_eq!(leader_balance, 1000);

    let validator = TestNode::new();
    let validator_data = validator.data.clone();
    let val_fullnode = FullNode::new(
        validator,
        false,
        InFile::Path(ledger_path.clone()),
        Some(leader_data.contact_info.ncp),
        None,
    );

    let mut client = mk_client(&validator_data);
    let getbal = retry_get_balance(&mut client, &bob_pubkey, Some(leader_balance));
    assert!(getbal == Some(leader_balance));

    leader_fullnode.close().unwrap();
    val_fullnode.close().unwrap();
    std::fs::remove_file(ledger_path).unwrap();
}

fn restart_leader(
    leader_fullnode: Option<FullNode>,
    ledger_path: String,
) -> (ReplicatedData, FullNode) {
    if let Some(leader_fullnode) = leader_fullnode {
        // stop the leader
        leader_fullnode.close().unwrap();
    }

    let leader = TestNode::new();
    let leader_data = leader.data.clone();
    let leader_fullnode = FullNode::new(
        leader,
        true,
        InFile::Path(ledger_path.clone()),
        None,
        Some(OutFile::Path(ledger_path.clone())),
    );
    (leader_data, leader_fullnode)
}

#[test]
fn test_leader_restart_validator_start_from_old_ledger() {
    // this test verifies that a freshly started leader makes his ledger available
    //    in the repair window to validators that are started with an older
    //    ledger (currently up to WINDOW_SIZE entries)
    logger::setup();

    let (alice, ledger_path) = genesis(100_000);
    let bob_pubkey = KeyPair::new().pubkey();

    let (leader_data, leader_fullnode) = restart_leader(None, ledger_path.clone());

    // lengthen the ledger
    let leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, Some(500)).unwrap();
    assert_eq!(leader_balance, 500);

    // create a "stale" ledger by copying current ledger
    let mut stale_ledger_path = ledger_path.clone();
    stale_ledger_path.insert_str(ledger_path.rfind("/").unwrap() + 1, "stale_");

    std::fs::copy(&ledger_path, &stale_ledger_path)
        .expect(format!("copy {} to {}", &ledger_path, &stale_ledger_path,).as_str());

    // restart the leader
    let (leader_data, leader_fullnode) = restart_leader(Some(leader_fullnode), ledger_path.clone());

    // lengthen the ledger
    let leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, Some(1000)).unwrap();
    assert_eq!(leader_balance, 1000);

    // restart the leader
    let (leader_data, leader_fullnode) = restart_leader(Some(leader_fullnode), ledger_path.clone());

    // start validator from old ledger
    let validator = TestNode::new();
    let validator_data = validator.data.clone();
    let val_fullnode = FullNode::new(
        validator,
        false,
        InFile::Path(stale_ledger_path.clone()),
        Some(leader_data.contact_info.ncp),
        None,
    );

    // trigger broadcast, validator should catch up from leader, whose window contains
    //   the entries missing from the stale ledger
    //   send requests so the validator eventually sees a gap and requests a repair
    let mut expected = 1500;
    let mut client = mk_client(&validator_data);
    for _ in 0..10 {
        let leader_balance =
            send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, Some(expected))
                .unwrap();
        assert_eq!(leader_balance, expected);
        let getbal = retry_get_balance(&mut client, &bob_pubkey, Some(leader_balance));
        if getbal == Some(leader_balance) {
            break;
        }
        expected += 500;
    }
    let getbal = retry_get_balance(&mut client, &bob_pubkey, Some(expected));
    assert_eq!(getbal, Some(expected));

    leader_fullnode.close().unwrap();
    val_fullnode.close().unwrap();
    std::fs::remove_file(ledger_path).unwrap();
    std::fs::remove_file(stale_ledger_path).unwrap();
}

//TODO: this test will run a long time so it's disabled for CI
#[test]
#[ignore]
fn test_multi_node_dynamic_network() {
    logger::setup();
    const N: usize = 25;
    let leader = TestNode::new();
    let bob_pubkey = KeyPair::new().pubkey();
    let (alice, ledger_path) = genesis(100_000);
    let leader_data = leader.data.clone();
    let server = FullNode::new(
        leader,
        true,
        InFile::Path(ledger_path.clone()),
        None,
        Some(OutFile::Path(ledger_path.clone())),
    );
    info!("{:x} LEADER", leader_data.debug_id());
    let leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, Some(500)).unwrap();
    assert_eq!(leader_balance, 500);
    let leader_balance =
        send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, Some(1000)).unwrap();
    assert_eq!(leader_balance, 1000);

    let mut validators: Vec<(ReplicatedData, FullNode)> = (0..N)
        .into_iter()
        .map(|_| {
            let validator = TestNode::new();
            let rd = validator.data.clone();
            let val = FullNode::new(
                validator,
                false,
                InFile::Path(ledger_path.clone()),
                Some(leader_data.contact_info.ncp),
                Some(OutFile::Path(ledger_path.clone())),
            );
            info!("{:x} VALIDATOR", rd.debug_id());
            (rd, val)
        })
        .collect();

    for i in 0..N {
        //verify leader can do transfer
        let expected = ((i + 3) * 500) as i64;
        let leader_balance =
            send_tx_and_retry_get_balance(&leader_data, &alice, &bob_pubkey, Some(expected))
                .unwrap();
        assert_eq!(leader_balance, expected);
        //verify all validators have the same balance
        for i in 0..10 {
            let mut success = 0usize;
            let mut distance = 0i64;
            for server in validators.iter() {
                let mut client = mk_client(&server.0);
                let getbal = retry_get_balance(&mut client, &bob_pubkey, Some(expected));
                trace!(
                    "{:x} {} get_balance: {:?} expected: {}",
                    server.0.debug_id(),
                    i,
                    getbal,
                    expected
                );
                let bal = getbal.unwrap_or(0);
                distance += (expected - bal) / 500;
                if let Some(bal) = getbal {
                    if bal == leader_balance {
                        success += 1;
                    }
                }
            }
            if success == validators.len() {
                break;
            }
            sleep(Duration::from_millis(i * 100));
            info!(
                "SUCCESS {} out of {} distance: {}",
                success,
                validators.len(),
                distance
            );
        }

        let val = {
            let validator = TestNode::new();
            let rd = validator.data.clone();
            let val = FullNode::new(
                validator,
                false,
                InFile::Path(ledger_path.clone()),
                Some(leader_data.contact_info.ncp),
                Some(OutFile::Path(ledger_path.clone())),
            );
            info!("{:x} ADDED", rd.debug_id());
            (rd, val)
        };

        let old_val = mem::replace(&mut validators[i], val);

        // this should be almost true, or at least validators.len() - 1 while the other node catches up
        //assert!(success == validators.len());
        //kill a validator
        old_val.1.close().unwrap();
        info!("{:x} KILLED", old_val.0.debug_id());
        //add a new one
    }

    for (_, node) in validators {
        node.close().unwrap();
    }
    server.close().unwrap();

    std::fs::remove_file(ledger_path).unwrap();
}

fn mk_client(leader: &ReplicatedData) -> ThinClient {
    let requests_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    requests_socket
        .set_read_timeout(Some(Duration::new(1, 0)))
        .unwrap();
    let transactions_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    let daddr = "0.0.0.0:0".parse().unwrap();
    assert!(leader.contact_info.rpu != daddr);
    assert!(leader.contact_info.tpu != daddr);
    ThinClient::new(
        leader.contact_info.rpu,
        requests_socket,
        leader.contact_info.tpu,
        transactions_socket,
    )
}

fn retry_get_balance(
    client: &mut ThinClient,
    bob_pubkey: &PublicKey,
    expected: Option<i64>,
) -> Option<i64> {
    const LAST: usize = 20;
    for run in 0..(LAST + 1) {
        let out = client.poll_get_balance(bob_pubkey);
        if expected.is_none() || run == LAST {
            return out.ok().clone();
        }
        if let (Some(e), Ok(o)) = (expected, out) {
            if o == e {
                return Some(o);
            }
        }
    }
    None
}

fn send_tx_and_retry_get_balance(
    leader: &ReplicatedData,
    alice: &Mint,
    bob_pubkey: &PublicKey,
    expected: Option<i64>,
) -> Option<i64> {
    let mut client = mk_client(leader);
    trace!("getting leader last_id");
    let last_id = client.get_last_id();
    info!("executing leader transfer");
    let _sig = client
        .transfer(500, &alice.keypair(), *bob_pubkey, &last_id)
        .unwrap();
    retry_get_balance(&mut client, bob_pubkey, expected)
}
