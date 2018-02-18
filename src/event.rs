//! The `event` crate provides the foundational data structures for Proof-of-History

/// A Proof-of-History is an ordered log of events in time. Each entry contains three
/// pieces of data. The 'num_hashes' field is the number of hashes performed since the previous
/// entry.  The 'end_hash' field is the result of hashing 'end_hash' from the previous entry
/// 'num_hashes' times.  The 'data' field is an optional foreign key (a hash) pointing to some
/// arbitrary data that a client is looking to associate with the entry.
///
/// If you divide 'num_hashes' by the amount of time it takes to generate a new hash, you
/// get a duration estimate since the last event. Since processing power increases
/// over time, one should expect the duration 'num_hashes' represents to decrease proportionally.
/// Though processing power varies across nodes, the network gives priority to the
/// fastest processor. Duration should therefore be estimated by assuming that the hash
/// was generated by the fastest processor at the time the entry was logged.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Event {
    pub num_hashes: u64,
    pub end_hash: u64,
    pub data: EventData,
}

/// When 'data' is Tick, the event represents a simple clock tick, and exists for the
/// sole purpose of improving the performance of event log verification. A tick can
/// be generated in 'num_hashes' hashes and verified in 'num_hashes' hashes.  By logging
/// a hash alongside the tick, each tick and be verified in parallel using the 'end_hash'
/// of the preceding tick to seed its hashing.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum EventData {
    Tick,
    UserDataKey(u64),
}

impl Event {
    /// Creates an Event from the number of hashes 'num_hashes' since the previous event
    /// and that resulting 'end_hash'.
    pub fn new_tick(num_hashes: u64, end_hash: u64) -> Self {
        let data = EventData::Tick;
        Event {
            num_hashes,
            end_hash,
            data,
        }
    }

    /// Verifies self.end_hash is the result of hashing a 'start_hash' 'self.num_hashes' times.
    pub fn verify(self: &Self, start_hash: u64) -> bool {
        self.end_hash == next_tick(start_hash, self.num_hashes).end_hash
    }
}

/// Creates the next Tick Event 'num_hashes' after 'start_hash'.
pub fn next_tick(start_hash: u64, num_hashes: u64) -> Event {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut end_hash = start_hash;
    let mut hasher = DefaultHasher::new();
    for _ in 0..num_hashes {
        end_hash.hash(&mut hasher);
        end_hash = hasher.finish();
    }
    Event::new_tick(num_hashes, end_hash)
}

/// Verifies the hashes and counts of a slice of events are all consistent.
pub fn verify_slice(events: &[Event], start_hash: u64) -> bool {
    use rayon::prelude::*;
    let genesis = [Event::new_tick(0, start_hash)];
    let event_pairs = genesis.par_iter().chain(events).zip(events);
    event_pairs.all(|(x0, x1)| x1.verify(x0.end_hash))
}

/// Verifies the hashes and events serially. Exists only for reference.
pub fn verify_slice_seq(events: &[Event], start_hash: u64) -> bool {
    let genesis = [Event::new_tick(0, start_hash)];
    let mut event_pairs = genesis.iter().chain(events).zip(events);
    event_pairs.all(|(x0, x1)| x1.verify(x0.end_hash))
}

/// Create a vector of Ticks of length 'len' from 'start_hash' hash and 'num_hashes'.
pub fn create_ticks(start_hash: u64, num_hashes: u64, len: usize) -> Vec<Event> {
    use itertools::unfold;
    let mut events = unfold(start_hash, |state| {
        let event = next_tick(*state, num_hashes);
        *state = event.end_hash;
        return Some(event);
    });
    events.by_ref().take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_verify() {
        assert!(Event::new_tick(0, 0).verify(0)); // base case
        assert!(!Event::new_tick(0, 0).verify(1)); // base case, bad
        assert!(next_tick(0, 1).verify(0)); // inductive step
        assert!(!next_tick(0, 1).verify(1)); // inductive step, bad
    }

    #[test]
    fn test_next_tick() {
        assert_eq!(next_tick(0, 1).num_hashes, 1)
    }

    fn verify_slice_generic(verify_slice: fn(&[Event], u64) -> bool) {
        assert!(verify_slice(&vec![], 0)); // base case
        assert!(verify_slice(&vec![Event::new_tick(0, 0)], 0)); // singleton case 1
        assert!(!verify_slice(&vec![Event::new_tick(0, 0)], 1)); // singleton case 2, bad
        assert!(verify_slice(&create_ticks(0, 0, 2), 0)); // inductive step

        let mut bad_ticks = create_ticks(0, 0, 2);
        bad_ticks[1].end_hash = 1;
        assert!(!verify_slice(&bad_ticks, 0)); // inductive step, bad
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
    use event;

    #[bench]
    fn event_bench(bencher: &mut Bencher) {
        let start_hash = 0;
        let events = event::create_ticks(start_hash, 100_000, 8);
        bencher.iter(|| {
            assert!(event::verify_slice(&events, start_hash));
        });
    }

    #[bench]
    fn event_bench_seq(bencher: &mut Bencher) {
        let start_hash = 0;
        let events = event::create_ticks(start_hash, 100_000, 8);
        bencher.iter(|| {
            assert!(event::verify_slice_seq(&events, start_hash));
        });
    }
}
