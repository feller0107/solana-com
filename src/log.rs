//! The `log` crate provides the foundational data structures for Proof-of-History,
//! an ordered log of events in time.

/// Each log entry contains three pieces of data. The 'num_hashes' field is the number
/// of hashes performed since the previous entry.  The 'end_hash' field is the result
/// of hashing 'end_hash' from the previous entry 'num_hashes' times.  The 'event'
/// field points to an Event that took place shortly after 'end_hash' was generated.
///
/// If you divide 'num_hashes' by the amount of time it takes to generate a new hash, you
/// get a duration estimate since the last event. Since processing power increases
/// over time, one should expect the duration 'num_hashes' represents to decrease proportionally.
/// Though processing power varies across nodes, the network gives priority to the
/// fastest processor. Duration should therefore be estimated by assuming that the hash
/// was generated by the fastest processor at the time the entry was logged.

use digest::generic_array::GenericArray;
use digest::generic_array::typenum::U32;
pub type Sha256Hash = GenericArray<u8, U32>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Entry {
    pub num_hashes: u64,
    pub end_hash: Sha256Hash,
    pub event: Event,
}

/// When 'event' is Tick, the event represents a simple clock tick, and exists for the
/// sole purpose of improving the performance of event log verification. A tick can
/// be generated in 'num_hashes' hashes and verified in 'num_hashes' hashes.  By logging
/// a hash alongside the tick, each tick and be verified in parallel using the 'end_hash'
/// of the preceding tick to seed its hashing.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Event {
    Tick,
    UserDataKey(Sha256Hash),
}

impl Entry {
    /// Creates a Entry from the number of hashes 'num_hashes' since the previous event
    /// and that resulting 'end_hash'.
    pub fn new_tick(num_hashes: u64, end_hash: &Sha256Hash) -> Self {
        Entry {
            num_hashes,
            end_hash: *end_hash,
            event: Event::Tick,
        }
    }

    /// Verifies self.end_hash is the result of hashing a 'start_hash' 'self.num_hashes' times.
    /// If the event is a UserDataKey, then hash that as well.
    pub fn verify(self: &Self, start_hash: &Sha256Hash) -> bool {
        self.end_hash == next_hash(start_hash, self.num_hashes, &self.event)
    }
}

pub fn hash(val: &[u8]) -> Sha256Hash {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::default();
    hasher.input(val);
    hasher.result()
}

/// Return the hash of the given hash extended with the given value.
pub fn extend_and_hash(end_hash: &Sha256Hash, val: &[u8]) -> Sha256Hash {
    let mut hash_data = end_hash.to_vec();
    hash_data.extend_from_slice(val);
    hash(&hash_data)
}

pub fn next_hash(start_hash: &Sha256Hash, num_hashes: u64, event: &Event) -> Sha256Hash {
    let mut end_hash = *start_hash;
    for _ in 0..num_hashes {
        end_hash = hash(&end_hash);
    }
    if let Event::UserDataKey(key) = *event {
        return extend_and_hash(&end_hash, &key);
    }
    end_hash
}

/// Creates the next Tick Entry 'num_hashes' after 'start_hash'.
pub fn next_entry(start_hash: &Sha256Hash, num_hashes: u64, event: Event) -> Entry {
    Entry {
        num_hashes,
        end_hash: next_hash(start_hash, num_hashes, &event),
        event,
    }
}

/// Creates the next Tick Entry 'num_hashes' after 'start_hash'.
pub fn next_tick(start_hash: &Sha256Hash, num_hashes: u64) -> Entry {
    next_entry(start_hash, num_hashes, Event::Tick)
}

/// Verifies the hashes and counts of a slice of events are all consistent.
pub fn verify_slice(events: &[Entry], start_hash: &Sha256Hash) -> bool {
    use rayon::prelude::*;
    let genesis = [Entry::new_tick(Default::default(), start_hash)];
    let event_pairs = genesis.par_iter().chain(events).zip(events);
    event_pairs.all(|(x0, x1)| x1.verify(&x0.end_hash))
}

/// Verifies the hashes and events serially. Exists only for reference.
pub fn verify_slice_seq(events: &[Entry], start_hash: &Sha256Hash) -> bool {
    let genesis = [Entry::new_tick(0, start_hash)];
    let mut event_pairs = genesis.iter().chain(events).zip(events);
    event_pairs.all(|(x0, x1)| x1.verify(&x0.end_hash))
}

/// Create a vector of Ticks of length 'len' from 'start_hash' hash and 'num_hashes'.
pub fn create_ticks(start_hash: &Sha256Hash, num_hashes: u64, len: usize) -> Vec<Entry> {
    use std::iter;
    let mut end_hash = *start_hash;
    iter::repeat(Event::Tick)
        .take(len)
        .map(|event| {
            let entry = next_entry(&end_hash, num_hashes, event);
            end_hash = entry.end_hash;
            entry
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_verify() {
        let zero = Sha256Hash::default();
        let one = hash(&zero);
        assert!(Entry::new_tick(0, &zero).verify(&zero)); // base case
        assert!(!Entry::new_tick(0, &zero).verify(&one)); // base case, bad
        assert!(next_tick(&zero, 1).verify(&zero)); // inductive step
        assert!(!next_tick(&zero, 1).verify(&one)); // inductive step, bad
    }

    #[test]
    fn test_next_tick() {
        let zero = Sha256Hash::default();
        assert_eq!(next_tick(&zero, 1).num_hashes, 1)
    }

    fn verify_slice_generic(verify_slice: fn(&[Entry], &Sha256Hash) -> bool) {
        let zero = Sha256Hash::default();
        let one = hash(&zero);
        assert!(verify_slice(&vec![], &zero)); // base case
        assert!(verify_slice(&vec![Entry::new_tick(0, &zero)], &zero)); // singleton case 1
        assert!(!verify_slice(&vec![Entry::new_tick(0, &zero)], &one)); // singleton case 2, bad
        assert!(verify_slice(&create_ticks(&zero, 0, 2), &zero)); // inductive step

        let mut bad_ticks = create_ticks(&zero, 0, 2);
        bad_ticks[1].end_hash = one;
        assert!(!verify_slice(&bad_ticks, &zero)); // inductive step, bad
    }

    #[test]
    fn test_verify_slice() {
        verify_slice_generic(verify_slice);
    }

    #[test]
    fn test_verify_slice_seq() {
        verify_slice_generic(verify_slice_seq);
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
        let events = create_ticks(&start_hash, 10_000, 8);
        bencher.iter(|| {
            assert!(verify_slice(&events, &start_hash));
        });
    }

    #[bench]
    fn event_bench_seq(bencher: &mut Bencher) {
        let start_hash = Default::default();
        let events = create_ticks(&start_hash, 10_000, 8);
        bencher.iter(|| {
            assert!(verify_slice_seq(&events, &start_hash));
        });
    }
}
