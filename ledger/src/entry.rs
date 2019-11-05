//! The `entry` module is a fundamental building block of Proof of History. It contains a
//! unique ID that is the hash of the Entry before it, plus the hash of the
//! transactions within it. Entries cannot be reordered, and its field `num_hashes`
//! represents an approximate amount of time since the last Entry was created.
use crate::poh::Poh;
use log::*;
use rayon::prelude::*;
use rayon::ThreadPool;
use serde::{Deserialize, Serialize};
use solana_measure::measure::Measure;
use solana_merkle_tree::MerkleTree;
use solana_metrics::*;
use solana_perf::perf_libs;
use solana_rayon_threadlimit::get_thread_count;
use solana_sdk::hash::Hash;
use solana_sdk::timing;
use solana_sdk::transaction::Transaction;
use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::Instant;

thread_local!(static PAR_THREAD_POOL: RefCell<ThreadPool> = RefCell::new(rayon::ThreadPoolBuilder::new()
                    .num_threads(get_thread_count())
                    .build()
                    .unwrap()));

pub type EntrySender = Sender<Vec<Entry>>;
pub type EntryReceiver = Receiver<Vec<Entry>>;

/// Each Entry contains three pieces of data. The `num_hashes` field is the number
/// of hashes performed since the previous entry.  The `hash` field is the result
/// of hashing `hash` from the previous entry `num_hashes` times.  The `transactions`
/// field points to Transactions that took place shortly before `hash` was generated.
///
/// If you divide `num_hashes` by the amount of time it takes to generate a new hash, you
/// get a duration estimate since the last Entry. Since processing power increases
/// over time, one should expect the duration `num_hashes` represents to decrease proportionally.
/// An upper bound on Duration can be estimated by assuming each hash was generated by the
/// world's fastest processor at the time the entry was recorded. Or said another way, it
/// is physically not possible for a shorter duration to have occurred if one assumes the
/// hash was computed by the world's fastest processor at that time. The hash chain is both
/// a Verifiable Delay Function (VDF) and a Proof of Work (not to be confused with Proof of
/// Work consensus!)

#[derive(Serialize, Deserialize, Debug, Default, PartialEq, Eq, Clone)]
pub struct Entry {
    /// The number of hashes since the previous Entry ID.
    pub num_hashes: u64,

    /// The SHA-256 hash `num_hashes` after the previous Entry ID.
    pub hash: Hash,

    /// An unordered list of transactions that were observed before the Entry ID was
    /// generated. They may have been observed before a previous Entry ID but were
    /// pushed back into this list to ensure deterministic interpretation of the ledger.
    pub transactions: Vec<Transaction>,
}

impl Entry {
    /// Creates the next Entry `num_hashes` after `start_hash`.
    pub fn new(prev_hash: &Hash, mut num_hashes: u64, transactions: Vec<Transaction>) -> Self {
        // If you passed in transactions, but passed in num_hashes == 0, then
        // next_hash will generate the next hash and set num_hashes == 1
        if num_hashes == 0 && !transactions.is_empty() {
            num_hashes = 1;
        }

        let hash = next_hash(prev_hash, num_hashes, &transactions);
        Entry {
            num_hashes,
            hash,
            transactions,
        }
    }

    pub fn new_mut(
        start_hash: &mut Hash,
        num_hashes: &mut u64,
        transactions: Vec<Transaction>,
    ) -> Self {
        let entry = Self::new(start_hash, *num_hashes, transactions);
        *start_hash = entry.hash;
        *num_hashes = 0;

        entry
    }

    #[cfg(test)]
    pub fn new_tick(num_hashes: u64, hash: &Hash) -> Self {
        Entry {
            num_hashes,
            hash: *hash,
            transactions: vec![],
        }
    }

    /// Verifies self.hash is the result of hashing a `start_hash` `self.num_hashes` times.
    /// If the transaction is not a Tick, then hash that as well.
    pub fn verify(&self, start_hash: &Hash) -> bool {
        let ref_hash = next_hash(start_hash, self.num_hashes, &self.transactions);
        if self.hash != ref_hash {
            warn!(
                "next_hash is invalid expected: {:?} actual: {:?}",
                self.hash, ref_hash
            );
            return false;
        }
        true
    }

    pub fn is_tick(&self) -> bool {
        self.transactions.is_empty()
    }
}

pub fn hash_transactions(transactions: &[Transaction]) -> Hash {
    // a hash of a slice of transactions only needs to hash the signatures
    let signatures: Vec<_> = transactions
        .iter()
        .flat_map(|tx| tx.signatures.iter())
        .collect();
    let merkle_tree = MerkleTree::new(&signatures);
    if let Some(root_hash) = merkle_tree.get_root() {
        *root_hash
    } else {
        Hash::default()
    }
}

/// Creates the hash `num_hashes` after `start_hash`. If the transaction contains
/// a signature, the final hash will be a hash of both the previous ID and
/// the signature.  If num_hashes is zero and there's no transaction data,
///  start_hash is returned.
pub fn next_hash(start_hash: &Hash, num_hashes: u64, transactions: &[Transaction]) -> Hash {
    if num_hashes == 0 && transactions.is_empty() {
        return *start_hash;
    }

    let mut poh = Poh::new(*start_hash, None);
    poh.hash(num_hashes.saturating_sub(1));
    if transactions.is_empty() {
        poh.tick().unwrap().hash
    } else {
        poh.record(hash_transactions(transactions)).unwrap().hash
    }
}

pub struct EntryVerifyState {
    thread_h: Option<JoinHandle<u64>>,
    hashes: Option<Arc<Mutex<Vec<Hash>>>>,
    verified: bool,
    tx_hashes: Vec<Option<Hash>>,
    start_time_ms: u64,
}

impl EntryVerifyState {
    pub fn finish_verify(&mut self, entries: &[Entry]) -> bool {
        if self.hashes.is_some() {
            let gpu_time_ms = self.thread_h.take().unwrap().join().unwrap();

            let mut verify_check_time = Measure::start("verify_check");
            let hashes = self.hashes.take().expect("hashes.as_ref");
            let hashes = Arc::try_unwrap(hashes)
                .expect("unwrap Arc")
                .into_inner()
                .expect("into_inner");
            let res = PAR_THREAD_POOL.with(|thread_pool| {
                thread_pool.borrow().install(|| {
                    hashes
                        .into_par_iter()
                        .zip(&self.tx_hashes)
                        .zip(entries)
                        .all(|((hash, tx_hash), answer)| {
                            if answer.num_hashes == 0 {
                                hash == answer.hash
                            } else {
                                let mut poh = Poh::new(hash, None);
                                if let Some(mixin) = tx_hash {
                                    poh.record(*mixin).unwrap().hash == answer.hash
                                } else {
                                    poh.tick().unwrap().hash == answer.hash
                                }
                            }
                        })
                })
            });
            verify_check_time.stop();
            inc_new_counter_warn!(
                "entry_verify-duration",
                (gpu_time_ms + verify_check_time.as_ms() + self.start_time_ms) as usize
            );
            res
        } else {
            self.verified
        }
    }
}

// an EntrySlice is a slice of Entries
pub trait EntrySlice {
    /// Verifies the hashes and counts of a slice of transactions are all consistent.
    fn verify_cpu(&self, start_hash: &Hash) -> EntryVerifyState;
    fn start_verify(&self, start_hash: &Hash) -> EntryVerifyState;
    fn verify(&self, start_hash: &Hash) -> bool;
    /// Checks that each entry tick has the correct number of hashes. Entry slices do not
    /// necessarily end in a tick, so `tick_hash_count` is used to carry over the hash count
    /// for the next entry slice.
    fn verify_tick_hash_count(&self, tick_hash_count: &mut u64, hashes_per_tick: u64) -> bool;
    /// Counts tick entries
    fn tick_count(&self) -> u64;
}

impl EntrySlice for [Entry] {
    fn verify(&self, start_hash: &Hash) -> bool {
        self.start_verify(start_hash).finish_verify(self)
    }
    fn verify_cpu(&self, start_hash: &Hash) -> EntryVerifyState {
        let now = Instant::now();
        let genesis = [Entry {
            num_hashes: 0,
            hash: *start_hash,
            transactions: vec![],
        }];
        let entry_pairs = genesis.par_iter().chain(self).zip(self);
        let res = PAR_THREAD_POOL.with(|thread_pool| {
            thread_pool.borrow().install(|| {
                entry_pairs.all(|(x0, x1)| {
                    let r = x1.verify(&x0.hash);
                    if !r {
                        warn!(
                            "entry invalid!: x0: {:?}, x1: {:?} num txs: {}",
                            x0.hash,
                            x1.hash,
                            x1.transactions.len()
                        );
                    }
                    r
                })
            })
        });
        inc_new_counter_warn!(
            "entry_verify-duration",
            timing::duration_as_ms(&now.elapsed()) as usize
        );
        EntryVerifyState {
            thread_h: None,
            verified: res,
            hashes: None,
            tx_hashes: vec![],
            start_time_ms: 0,
        }
    }

    fn start_verify(&self, start_hash: &Hash) -> EntryVerifyState {
        let api = perf_libs::api();
        if api.is_none() {
            return self.verify_cpu(start_hash);
        }
        let api = api.unwrap();
        inc_new_counter_warn!("entry_verify-num_entries", self.len() as usize);

        let start = Instant::now();

        let genesis = [Entry {
            num_hashes: 0,
            hash: *start_hash,
            transactions: vec![],
        }];

        let hashes: Vec<Hash> = genesis
            .iter()
            .chain(self)
            .map(|entry| entry.hash)
            .take(self.len())
            .collect();

        let num_hashes_vec: Vec<u64> = self
            .iter()
            .map(|entry| entry.num_hashes.saturating_sub(1))
            .collect();

        let length = self.len();
        let hashes = Arc::new(Mutex::new(hashes));
        let hashes_clone = hashes.clone();

        let gpu_verify_thread = thread::spawn(move || {
            let mut hashes = hashes_clone.lock().unwrap();
            let gpu_wait = Instant::now();
            let res;
            unsafe {
                res = (api.poh_verify_many)(
                    hashes.as_mut_ptr() as *mut u8,
                    num_hashes_vec.as_ptr(),
                    length,
                    1,
                );
            }
            if res != 0 {
                panic!("GPU PoH verify many failed");
            }
            inc_new_counter_warn!(
                "entry_verify-gpu_thread",
                timing::duration_as_ms(&gpu_wait.elapsed()) as usize
            );
            timing::duration_as_ms(&gpu_wait.elapsed())
        });

        let tx_hashes = PAR_THREAD_POOL.with(|thread_pool| {
            thread_pool.borrow().install(|| {
                self.into_par_iter()
                    .map(|entry| {
                        if entry.transactions.is_empty() {
                            None
                        } else {
                            Some(hash_transactions(&entry.transactions))
                        }
                    })
                    .collect()
            })
        });

        EntryVerifyState {
            thread_h: Some(gpu_verify_thread),
            verified: false,
            tx_hashes,
            start_time_ms: timing::duration_as_ms(&start.elapsed()),
            hashes: Some(hashes),
        }
    }

    fn verify_tick_hash_count(&self, tick_hash_count: &mut u64, hashes_per_tick: u64) -> bool {
        // When hashes_per_tick is 0, hashing is disabled.
        if hashes_per_tick == 0 {
            return true;
        }

        for entry in self {
            *tick_hash_count += entry.num_hashes;
            if entry.is_tick() {
                if *tick_hash_count != hashes_per_tick {
                    warn!(
                        "invalid tick hash count!: entry: {:#?}, tick_hash_count: {}, hashes_per_tick: {}",
                        entry,
                        tick_hash_count,
                        hashes_per_tick
                    );
                    return false;
                }
                *tick_hash_count = 0;
            }
        }
        *tick_hash_count < hashes_per_tick
    }

    fn tick_count(&self) -> u64 {
        self.iter().filter(|e| e.is_tick()).count() as u64
    }
}

pub fn next_entry_mut(start: &mut Hash, num_hashes: u64, transactions: Vec<Transaction>) -> Entry {
    let entry = Entry::new(&start, num_hashes, transactions);
    *start = entry.hash;
    entry
}

pub fn create_ticks(num_ticks: u64, hashes_per_tick: u64, mut hash: Hash) -> Vec<Entry> {
    let mut ticks = Vec::with_capacity(num_ticks as usize);
    for _ in 0..num_ticks {
        let new_tick = next_entry_mut(&mut hash, hashes_per_tick, vec![]);
        ticks.push(new_tick);
    }

    ticks
}

/// Creates the next Tick or Transaction Entry `num_hashes` after `start_hash`.
pub fn next_entry(prev_hash: &Hash, num_hashes: u64, transactions: Vec<Transaction>) -> Entry {
    assert!(num_hashes > 0 || transactions.is_empty());
    Entry {
        num_hashes,
        hash: next_hash(prev_hash, num_hashes, &transactions),
        transactions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;
    use chrono::prelude::Utc;
    use solana_budget_api::budget_instruction;
    use solana_sdk::{
        hash::{hash, Hash},
        message::Message,
        signature::{Keypair, KeypairUtil},
        system_transaction,
        transaction::Transaction,
    };

    fn create_sample_payment(keypair: &Keypair, hash: Hash) -> Transaction {
        let pubkey = keypair.pubkey();
        let ixs = budget_instruction::payment(&pubkey, &pubkey, 1);
        Transaction::new_signed_instructions(&[keypair], ixs, hash)
    }

    fn create_sample_timestamp(keypair: &Keypair, hash: Hash) -> Transaction {
        let pubkey = keypair.pubkey();
        let ix = budget_instruction::apply_timestamp(&pubkey, &pubkey, &pubkey, Utc::now());
        Transaction::new_signed_instructions(&[keypair], vec![ix], hash)
    }

    fn create_sample_apply_signature(keypair: &Keypair, hash: Hash) -> Transaction {
        let pubkey = keypair.pubkey();
        let ix = budget_instruction::apply_signature(&pubkey, &pubkey, &pubkey);
        Transaction::new_signed_instructions(&[keypair], vec![ix], hash)
    }

    #[test]
    fn test_entry_verify() {
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        assert!(Entry::new_tick(0, &zero).verify(&zero)); // base case, never used
        assert!(!Entry::new_tick(0, &zero).verify(&one)); // base case, bad
        assert!(next_entry(&zero, 1, vec![]).verify(&zero)); // inductive step
        assert!(!next_entry(&zero, 1, vec![]).verify(&one)); // inductive step, bad
    }

    #[test]
    fn test_transaction_reorder_attack() {
        let zero = Hash::default();

        // First, verify entries
        let keypair = Keypair::new();
        let tx0 = system_transaction::transfer(&keypair, &keypair.pubkey(), 0, zero);
        let tx1 = system_transaction::transfer(&keypair, &keypair.pubkey(), 1, zero);
        let mut e0 = Entry::new(&zero, 0, vec![tx0.clone(), tx1.clone()]);
        assert!(e0.verify(&zero));

        // Next, swap two transactions and ensure verification fails.
        e0.transactions[0] = tx1; // <-- attack
        e0.transactions[1] = tx0;
        assert!(!e0.verify(&zero));
    }

    #[test]
    fn test_witness_reorder_attack() {
        let zero = Hash::default();

        // First, verify entries
        let keypair = Keypair::new();
        let tx0 = create_sample_timestamp(&keypair, zero);
        let tx1 = create_sample_apply_signature(&keypair, zero);
        let mut e0 = Entry::new(&zero, 0, vec![tx0.clone(), tx1.clone()]);
        assert!(e0.verify(&zero));

        // Next, swap two witness transactions and ensure verification fails.
        e0.transactions[0] = tx1; // <-- attack
        e0.transactions[1] = tx0;
        assert!(!e0.verify(&zero));
    }

    #[test]
    fn test_next_entry() {
        let zero = Hash::default();
        let tick = next_entry(&zero, 1, vec![]);
        assert_eq!(tick.num_hashes, 1);
        assert_ne!(tick.hash, zero);

        let tick = next_entry(&zero, 0, vec![]);
        assert_eq!(tick.num_hashes, 0);
        assert_eq!(tick.hash, zero);

        let keypair = Keypair::new();
        let tx0 = create_sample_timestamp(&keypair, zero);
        let entry0 = next_entry(&zero, 1, vec![tx0.clone()]);
        assert_eq!(entry0.num_hashes, 1);
        assert_eq!(entry0.hash, next_hash(&zero, 1, &vec![tx0]));
    }

    #[test]
    #[should_panic]
    fn test_next_entry_panic() {
        let zero = Hash::default();
        let keypair = Keypair::new();
        let tx = system_transaction::transfer(&keypair, &keypair.pubkey(), 0, zero);
        next_entry(&zero, 0, vec![tx]);
    }

    #[test]
    fn test_verify_slice() {
        solana_logger::setup();
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        assert!(vec![][..].verify(&zero)); // base case
        assert!(vec![Entry::new_tick(0, &zero)][..].verify(&zero)); // singleton case 1
        assert!(!vec![Entry::new_tick(0, &zero)][..].verify(&one)); // singleton case 2, bad
        assert!(vec![next_entry(&zero, 0, vec![]); 2][..].verify(&zero)); // inductive step

        let mut bad_ticks = vec![next_entry(&zero, 0, vec![]); 2];
        bad_ticks[1].hash = one;
        assert!(!bad_ticks.verify(&zero)); // inductive step, bad
    }

    #[test]
    fn test_verify_slice_with_hashes() {
        solana_logger::setup();
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        let two = hash(&one.as_ref());
        assert!(vec![][..].verify(&one)); // base case
        assert!(vec![Entry::new_tick(1, &two)][..].verify(&one)); // singleton case 1
        assert!(!vec![Entry::new_tick(1, &two)][..].verify(&two)); // singleton case 2, bad

        let mut ticks = vec![next_entry(&one, 1, vec![])];
        ticks.push(next_entry(&ticks.last().unwrap().hash, 1, vec![]));
        assert!(ticks.verify(&one)); // inductive step

        let mut bad_ticks = vec![next_entry(&one, 1, vec![])];
        bad_ticks.push(next_entry(&bad_ticks.last().unwrap().hash, 1, vec![]));
        bad_ticks[1].hash = one;
        assert!(!bad_ticks.verify(&one)); // inductive step, bad
    }

    #[test]
    fn test_verify_slice_with_hashes_and_transactions() {
        solana_logger::setup();
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        let two = hash(&one.as_ref());
        let alice_pubkey = Keypair::default();
        let tx0 = create_sample_payment(&alice_pubkey, one);
        let tx1 = create_sample_timestamp(&alice_pubkey, one);
        assert!(vec![][..].verify(&one)); // base case
        assert!(vec![next_entry(&one, 1, vec![tx0.clone()])][..].verify(&one)); // singleton case 1
        assert!(!vec![next_entry(&one, 1, vec![tx0.clone()])][..].verify(&two)); // singleton case 2, bad

        let mut ticks = vec![next_entry(&one, 1, vec![tx0.clone()])];
        ticks.push(next_entry(
            &ticks.last().unwrap().hash,
            1,
            vec![tx1.clone()],
        ));
        assert!(ticks.verify(&one)); // inductive step

        let mut bad_ticks = vec![next_entry(&one, 1, vec![tx0])];
        bad_ticks.push(next_entry(&bad_ticks.last().unwrap().hash, 1, vec![tx1]));
        bad_ticks[1].hash = one;
        assert!(!bad_ticks.verify(&one)); // inductive step, bad
    }

    #[test]
    fn test_verify_tick_hash_count() {
        let hashes_per_tick = 10;
        let keypairs: Vec<&Keypair> = Vec::new();
        let tx: Transaction =
            Transaction::new(&keypairs, Message::new(Vec::new()), Hash::default());
        let tx_entry = Entry::new(&Hash::default(), 1, vec![tx]);
        let full_tick_entry = Entry::new_tick(hashes_per_tick, &Hash::default());
        let partial_tick_entry = Entry::new_tick(hashes_per_tick - 1, &Hash::default());
        let no_hash_tick_entry = Entry::new_tick(0, &Hash::default());
        let single_hash_tick_entry = Entry::new_tick(1, &Hash::default());

        let no_ticks = vec![];
        let mut tick_hash_count = 0;
        assert!(no_ticks.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, 0);

        // validation is disabled when hashes_per_tick == 0
        let no_hash_tick = vec![no_hash_tick_entry.clone()];
        assert!(no_hash_tick.verify_tick_hash_count(&mut tick_hash_count, 0));
        assert_eq!(tick_hash_count, 0);

        // validation is disabled when hashes_per_tick == 0
        let tx_and_no_hash_tick = vec![tx_entry.clone(), no_hash_tick_entry];
        assert!(tx_and_no_hash_tick.verify_tick_hash_count(&mut tick_hash_count, 0));
        assert_eq!(tick_hash_count, 0);

        let single_tick = vec![full_tick_entry.clone()];
        assert!(single_tick.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, 0);
        assert!(!single_tick.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick - 1));
        assert_eq!(tick_hash_count, hashes_per_tick);
        tick_hash_count = 0;

        let ticks_and_txs = vec![tx_entry.clone(), partial_tick_entry.clone()];
        assert!(ticks_and_txs.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, 0);

        let partial_tick = vec![partial_tick_entry.clone()];
        assert!(!partial_tick.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, hashes_per_tick - 1);
        tick_hash_count = 0;

        let tx_entries: Vec<Entry> = (0..hashes_per_tick - 1).map(|_| tx_entry.clone()).collect();
        let tx_entries_and_tick = [tx_entries, vec![single_hash_tick_entry]].concat();
        assert!(tx_entries_and_tick.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, 0);

        let too_many_tx_entries: Vec<Entry> =
            (0..hashes_per_tick).map(|_| tx_entry.clone()).collect();
        assert!(!too_many_tx_entries.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, hashes_per_tick);
    }
}
