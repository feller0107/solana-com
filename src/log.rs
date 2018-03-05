//! The `log` crate provides the foundational data structures for Proof-of-History,
//! an ordered log of events in time.

/// Each log entry contains three pieces of data. The 'num_hashes' field is the number
/// of hashes performed since the previous entry.  The 'id' field is the result
/// of hashing 'id' from the previous entry 'num_hashes' times.  The 'event'
/// field points to an Event that took place shortly after 'id' was generated.
///
/// If you divide 'num_hashes' by the amount of time it takes to generate a new hash, you
/// get a duration estimate since the last event. Since processing power increases
/// over time, one should expect the duration 'num_hashes' represents to decrease proportionally.
/// Though processing power varies across nodes, the network gives priority to the
/// fastest processor. Duration should therefore be estimated by assuming that the hash
/// was generated by the fastest processor at the time the entry was logged.

use generic_array::GenericArray;
use generic_array::typenum::U32;
use serde::Serialize;
use event::{get_signature, verify_event, Event};
use sha2::{Digest, Sha256};
use rayon::prelude::*;

pub type Sha256Hash = GenericArray<u8, U32>;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Entry<T> {
    pub num_hashes: u64,
    pub id: Sha256Hash,
    pub event: Event<T>,
}

impl<T> Entry<T> {
    /// Creates a Entry from the number of hashes 'num_hashes' since the previous event
    /// and that resulting 'id'.
    pub fn new_tick(num_hashes: u64, id: &Sha256Hash) -> Self {
        Entry {
            num_hashes,
            id: *id,
            event: Event::Tick,
        }
    }
}

/// Return a Sha256 hash for the given data.
pub fn hash(val: &[u8]) -> Sha256Hash {
    let mut hasher = Sha256::default();
    hasher.input(val);
    hasher.result()
}

/// Return the hash of the given hash extended with the given value.
pub fn extend_and_hash(id: &Sha256Hash, val: &[u8]) -> Sha256Hash {
    let mut hash_data = id.to_vec();
    hash_data.extend_from_slice(val);
    hash(&hash_data)
}

/// Creates the hash 'num_hashes' after start_hash. If the event contains
/// signature, the final hash will be a hash of both the previous ID and
/// the signature.
pub fn next_hash<T: Serialize>(
    start_hash: &Sha256Hash,
    num_hashes: u64,
    event: &Event<T>,
) -> Sha256Hash {
    let mut id = *start_hash;
    let sig = get_signature(event);
    let start_index = if sig.is_some() { 1 } else { 0 };
    for _ in start_index..num_hashes {
        id = hash(&id);
    }
    if let Some(sig) = sig {
        id = extend_and_hash(&id, &sig);
    }
    id
}

/// Creates the next Entry 'num_hashes' after 'start_hash'.
pub fn create_entry<T: Serialize>(
    start_hash: &Sha256Hash,
    cur_hashes: u64,
    event: Event<T>,
) -> Entry<T> {
    let sig = get_signature(&event);
    let num_hashes = cur_hashes + if sig.is_some() { 1 } else { 0 };
    let id = next_hash(start_hash, 0, &event);
    Entry {
        num_hashes,
        id,
        event,
    }
}

/// Creates the next Tick Entry 'num_hashes' after 'start_hash'.
pub fn create_entry_mut<T: Serialize>(
    start_hash: &mut Sha256Hash,
    cur_hashes: &mut u64,
    event: Event<T>,
) -> Entry<T> {
    let entry = create_entry(start_hash, *cur_hashes, event);
    *start_hash = entry.id;
    *cur_hashes = 0;
    entry
}

/// Creates the next Tick Entry 'num_hashes' after 'start_hash'.
pub fn next_tick<T: Serialize>(start_hash: &Sha256Hash, num_hashes: u64) -> Entry<T> {
    let event = Event::Tick;
    Entry {
        num_hashes,
        id: next_hash(start_hash, num_hashes, &event),
        event,
    }
}

/// Verifies self.id is the result of hashing a 'start_hash' 'self.num_hashes' times.
/// If the event is not a Tick, then hash that as well.
pub fn verify_entry<T: Serialize>(entry: &Entry<T>, start_hash: &Sha256Hash) -> bool {
    if !verify_event(&entry.event) {
        return false;
    }
    entry.id == next_hash(start_hash, entry.num_hashes, &entry.event)
}

/// Verifies the hashes and counts of a slice of events are all consistent.
pub fn verify_slice(events: &[Entry<Sha256Hash>], start_hash: &Sha256Hash) -> bool {
    let genesis = [Entry::new_tick(Default::default(), start_hash)];
    let event_pairs = genesis.par_iter().chain(events).zip(events);
    event_pairs.all(|(x0, x1)| verify_entry(&x1, &x0.id))
}

/// Verifies the hashes and counts of a slice of events are all consistent.
pub fn verify_slice_u64(events: &[Entry<u64>], start_hash: &Sha256Hash) -> bool {
    let genesis = [Entry::new_tick(Default::default(), start_hash)];
    let event_pairs = genesis.par_iter().chain(events).zip(events);
    event_pairs.all(|(x0, x1)| verify_entry(&x1, &x0.id))
}

/// Verifies the hashes and events serially. Exists only for reference.
pub fn verify_slice_seq<T: Serialize>(events: &[Entry<T>], start_hash: &Sha256Hash) -> bool {
    let genesis = [Entry::new_tick(0, start_hash)];
    let mut event_pairs = genesis.iter().chain(events).zip(events);
    event_pairs.all(|(x0, x1)| verify_entry(&x1, &x0.id))
}

pub fn create_entries<T: Serialize>(
    start_hash: &Sha256Hash,
    events: Vec<Event<T>>,
) -> Vec<Entry<T>> {
    let mut id = *start_hash;
    events
        .into_iter()
        .map(|event| create_entry_mut(&mut id, &mut 0, event))
        .collect()
}

/// Create a vector of Ticks of length 'len' from 'start_hash' hash and 'num_hashes'.
pub fn next_ticks(start_hash: &Sha256Hash, num_hashes: u64, len: usize) -> Vec<Entry<Sha256Hash>> {
    let mut id = *start_hash;
    let mut ticks = vec![];
    for _ in 0..len {
        let entry = next_tick(&id, num_hashes);
        id = entry.id;
        ticks.push(entry);
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;
    use event::{generate_keypair, get_pubkey, sign_claim_data, sign_transaction_data};

    #[test]
    fn test_event_verify() {
        let zero = Sha256Hash::default();
        let one = hash(&zero);
        assert!(verify_entry::<u8>(&Entry::new_tick(0, &zero), &zero)); // base case
        assert!(!verify_entry::<u8>(&Entry::new_tick(0, &zero), &one)); // base case, bad
        assert!(verify_entry::<u8>(&next_tick(&zero, 1), &zero)); // inductive step
        assert!(!verify_entry::<u8>(&next_tick(&zero, 1), &one)); // inductive step, bad
    }

    #[test]
    fn test_next_tick() {
        let zero = Sha256Hash::default();
        assert_eq!(next_tick::<Sha256Hash>(&zero, 1).num_hashes, 1)
    }

    fn verify_slice_generic(verify_slice: fn(&[Entry<Sha256Hash>], &Sha256Hash) -> bool) {
        let zero = Sha256Hash::default();
        let one = hash(&zero);
        assert!(verify_slice(&vec![], &zero)); // base case
        assert!(verify_slice(&vec![Entry::new_tick(0, &zero)], &zero)); // singleton case 1
        assert!(!verify_slice(&vec![Entry::new_tick(0, &zero)], &one)); // singleton case 2, bad
        assert!(verify_slice(&next_ticks(&zero, 0, 2), &zero)); // inductive step

        let mut bad_ticks = next_ticks(&zero, 0, 2);
        bad_ticks[1].id = one;
        assert!(!verify_slice(&bad_ticks, &zero)); // inductive step, bad
    }

    #[test]
    fn test_verify_slice() {
        verify_slice_generic(verify_slice);
    }

    #[test]
    fn test_verify_slice_seq() {
        verify_slice_generic(verify_slice_seq::<Sha256Hash>);
    }

    #[test]
    fn test_reorder_attack() {
        let zero = Sha256Hash::default();
        let one = hash(&zero);

        // First, verify entries
        let keypair = generate_keypair();
        let event0 = Event::new_claim(
            get_pubkey(&keypair),
            zero,
            zero,
            sign_claim_data(&zero, &keypair, &zero),
        );
        let event1 = Event::new_claim(
            get_pubkey(&keypair),
            one,
            zero,
            sign_claim_data(&one, &keypair, &zero),
        );
        let events = vec![event0, event1];
        let mut entries = create_entries(&zero, events);
        assert!(verify_slice(&entries, &zero));

        // Next, swap two events and ensure verification fails.
        let event0 = entries[0].event.clone();
        let event1 = entries[1].event.clone();
        entries[0].event = event1;
        entries[1].event = event0;
        assert!(!verify_slice(&entries, &zero));
    }

    #[test]
    fn test_claim() {
        let keypair = generate_keypair();
        let data = hash(b"hello, world");
        let zero = Sha256Hash::default();
        let event0 = Event::new_claim(
            get_pubkey(&keypair),
            data,
            zero,
            sign_claim_data(&data, &keypair, &zero),
        );
        let entries = create_entries(&zero, vec![event0]);
        assert!(verify_slice(&entries, &zero));
    }

    #[test]
    fn test_wrong_data_claim_attack() {
        let keypair = generate_keypair();
        let zero = Sha256Hash::default();
        let event0 = Event::new_claim(
            get_pubkey(&keypair),
            hash(b"goodbye cruel world"),
            zero,
            sign_claim_data(&hash(b"hello, world"), &keypair, &zero),
        );
        let entries = create_entries(&zero, vec![event0]);
        assert!(!verify_slice(&entries, &zero));
    }

    #[test]
    fn test_transfer() {
        let zero = Sha256Hash::default();
        let keypair0 = generate_keypair();
        let keypair1 = generate_keypair();
        let pubkey1 = get_pubkey(&keypair1);
        let data = hash(b"hello, world");
        let event0 = Event::Transaction {
            from: get_pubkey(&keypair0),
            to: pubkey1,
            data,
            last_id: zero,
            sig: sign_transaction_data(&data, &keypair0, &pubkey1, &zero),
        };
        let entries = create_entries(&zero, vec![event0]);
        assert!(verify_slice(&entries, &zero));
    }

    #[test]
    fn test_wrong_data_transfer_attack() {
        let keypair0 = generate_keypair();
        let keypair1 = generate_keypair();
        let pubkey1 = get_pubkey(&keypair1);
        let data = hash(b"hello, world");
        let zero = Sha256Hash::default();
        let event0 = Event::Transaction {
            from: get_pubkey(&keypair0),
            to: pubkey1,
            data: hash(b"goodbye cruel world"), // <-- attack!
            last_id: zero,
            sig: sign_transaction_data(&data, &keypair0, &pubkey1, &zero),
        };
        let entries = create_entries(&zero, vec![event0]);
        assert!(!verify_slice(&entries, &zero));
    }

    #[test]
    fn test_transfer_hijack_attack() {
        let keypair0 = generate_keypair();
        let keypair1 = generate_keypair();
        let thief_keypair = generate_keypair();
        let pubkey1 = get_pubkey(&keypair1);
        let data = hash(b"hello, world");
        let zero = Sha256Hash::default();
        let event0 = Event::Transaction {
            from: get_pubkey(&keypair0),
            to: get_pubkey(&thief_keypair), // <-- attack!
            data: hash(b"goodbye cruel world"),
            last_id: zero,
            sig: sign_transaction_data(&data, &keypair0, &pubkey1, &zero),
        };
        let entries = create_entries(&zero, vec![event0]);
        assert!(!verify_slice(&entries, &zero));
    }
}

#[cfg(all(feature = "unstable", test))]
mod bench {
    extern crate test;
    use self::test::Bencher;
    use log::*;

    #[bench]
    fn event_bench(bencher: &mut Bencher) {
        let start_hash = Default::default();
        let events = next_ticks(&start_hash, 10_000, 8);
        bencher.iter(|| {
            assert!(verify_slice(&events, &start_hash));
        });
    }

    #[bench]
    fn event_bench_seq(bencher: &mut Bencher) {
        let start_hash = Default::default();
        let events = next_ticks(&start_hash, 10_000, 8);
        bencher.iter(|| {
            assert!(verify_slice_seq(&events, &start_hash));
        });
    }
}
