use std::{collections::HashMap, sync::Mutex};

// NOTE: ~x2 performance gain over a naive `RwLock<HashMap>`!
use dashmap::DashMap;

use crate::{
    MemoryLocation, MemoryValue, ReadOrigin, ReadSet, TxIdx, TxIncarnation, TxVersion, WriteSet,
};

#[derive(Clone)]
enum MemoryEntry {
    Data(TxIncarnation, MemoryValue),
    // When an incarnation is aborted due to a validation failure, the
    // entries in the multi-version data structure corresponding to its
    // write set are replaced with this special ESTIMATE marker.
    // This signifies that the next incarnation is estimated to write to the
    // same memory locations. An incarnation stops and is immediately aborted
    // whenever it reads a value marked as an ESTIMATE written by a lower
    // transaction, instead of potentially wasting a full execution and aborting
    // during validation.
    // The ESTIMATE markers that are not overwritten are removed by the next
    // incarnation.
    Estimate,
}

pub(crate) enum ReadMemoryResult {
    NotFound,
    ReadError {
        blocking_tx_idx: TxIdx,
    },
    Ok {
        version: TxVersion,
        value: MemoryValue,
    },
}

// The MvMemory contains shared memory in a form of a multi-version data
// structure for values written and read by different transactions. It stores
// multiple writes for each memory location, along with a value and an associated
// version of a corresponding transaction.
// TODO: Better concurrency control if possible.
pub(crate) struct MvMemory {
    data: DashMap<
        MemoryLocation,
        // TODO: Use an id hasher for performance.
        DashMap<TxIdx, MemoryEntry>,
    >,
    last_written_locations: Vec<Mutex<Vec<MemoryLocation>>>,
    last_read_set: Vec<Mutex<ReadSet>>,
}

impl MvMemory {
    pub(crate) fn new(block_size: usize) -> Self {
        Self {
            data: DashMap::new(),
            last_written_locations: (0..block_size).map(|_| Mutex::new(Vec::new())).collect(),
            last_read_set: (0..block_size).map(|_| Mutex::new(Vec::new())).collect(),
        }
    }

    // Return the value written by the highest transaction for every location
    // that was written to by some transaction.
    //
    // TODO: More properly type this, especially depending on who needs it.
    // TODO: We can easily parallize this.
    pub(crate) fn snapshot(&self) -> HashMap<MemoryLocation, MemoryValue> {
        let mut ret = HashMap::new();
        for entry in self.data.iter() {
            let location = entry.key();
            // TODO: Replace MAX with no. transactions?
            // TODO: Inline the relevant part from `self.read`?
            if let ReadMemoryResult::Ok { value, .. } = self.read(location, std::usize::MAX) {
                ret.insert(location.clone(), value);
            }
        }
        ret
    }

    // Apply a new pair of read & write sets to the multi-version data structure.
    // Return whether a write occurred to a memory location not written to by
    // the previous incarnation of the same transaction. This determines whether
    // the executed higher transactions need re-validation.
    pub(crate) fn record(
        &self,
        tx_version: &TxVersion,
        read_set: ReadSet,
        write_set: WriteSet,
    ) -> bool {
        // TODO: Better error handling
        *self.last_read_set[tx_version.tx_idx].lock().unwrap() = read_set;

        for (location, value) in write_set.iter() {
            // TODO: Cleaner insert/upsert here
            let entry = MemoryEntry::Data(tx_version.tx_incarnation, value.clone());
            match self.data.get_mut(location) {
                Some(location_map) => {
                    location_map.insert(tx_version.tx_idx, entry);
                }
                _ => {
                    // TODO: Fine-tune the number of shards
                    let location_map = DashMap::with_shard_amount(2);
                    location_map.insert(tx_version.tx_idx, entry);
                    self.data.insert(location.clone(), location_map);
                }
            }
        }

        // TODO: Better error handling
        let mut last_written_locations = self.last_written_locations[tx_version.tx_idx]
            .lock()
            .unwrap();
        let prev_locations = last_written_locations.clone();

        let new_locations: Vec<MemoryLocation> = write_set.iter().map(|(l, _)| l.clone()).collect();
        *last_written_locations = new_locations.clone();

        for prev_location in prev_locations.iter() {
            // TODO: Faster "difference" function when there are many locations
            if !new_locations.contains(prev_location) {
                if let Some(location_map) = self.data.get_mut(prev_location) {
                    location_map.remove(&tx_version.tx_idx);
                }
            }
        }

        for new_location in new_locations.iter() {
            if !prev_locations.contains(new_location) {
                return true;
            }
        }
        false
    }

    // Obtain the last read set recorded by an execution of `tx_idx` and check
    // that re-reading each memory location in the read set still yields the
    // same value. For every value that was read, the read set stores a read
    // origin containing the version of the transaction (during the execution
    // of which the value was written), or if the value was read from storage
    // (i.e., not written by a smaller transaction). The incarnation numbers
    // are monotonically increasing, so it is sufficient to validate the read set
    // by comparing the corresponding origin.
    // This is invoked during validation, when the incarnation being validated is
    // already executed and has recorded the read set. However, if the thread
    // performing a validation for incarnation i of a transaction is slow, it is
    // possible that this function invocation observes a read set recorded by a
    // latter (> i) incarnation. In this case, incarnation i is guaranteed to be
    // already aborted (else higher incarnations would never start), and the
    // validation task will have no effect on the system regardless of the outcome
    // (only validations that successfully abort affect the state and each
    // incarnation can be aborted at most once).
    pub(crate) fn validate_read_set(&self, tx_idx: TxIdx) -> bool {
        // TODO: Better error handling
        for (location, prior_origin) in self.last_read_set[tx_idx].lock().unwrap().iter() {
            match self.read(location, tx_idx) {
                ReadMemoryResult::ReadError { .. } => return false,
                ReadMemoryResult::NotFound => {
                    if *prior_origin != ReadOrigin::Storage {
                        return false;
                    }
                }
                ReadMemoryResult::Ok { version, .. } => match prior_origin {
                    // TODO: Verify this logic as it's not explicitly described
                    // in the paper
                    ReadOrigin::Storage => return false,
                    ReadOrigin::MvMemory(v) => {
                        if *v != version {
                            return false;
                        }
                    }
                },
            }
        }
        true
    }

    // Replace the write set of the aborted version in the shared memory data
    // structure with special ESTIMATE markers to quickly abort higher transactions
    // that read them.
    pub(crate) fn convert_writes_to_estimates(&self, tx_idx: TxIdx) {
        // TODO: Better error handling
        for location in self.last_written_locations[tx_idx].lock().unwrap().iter() {
            if let Some(location_map) = self.data.get_mut(location) {
                location_map.insert(tx_idx, MemoryEntry::Estimate);
            }
        }
    }

    // Find the highest transaction index among lower transactions of an input
    // transaction that has written to a memory location. This is the best guess
    // for reading speculatively, that no transaction between the highest
    // transaction found and the input transaction writes to the same memory
    // location.
    // If the entry corresponding to the highest transaction index is an ESTIMTE
    // marker, we return an error to tell the caller to postpone the execution of
    // the input transaction until the next incarnation of this highest blocking
    // index transaction completes, since it is expected to write to the ESTIMATE
    // locations again.
    // When no lower transaction has written to the memory location, a read returns
    // a not found status, implying that the value cannot be obtained from previous
    // transactions. The caller can then complete the speculative read by reading
    // from storage.
    // TODO: Refactor & make this much faster
    pub(crate) fn read(&self, location: &MemoryLocation, tx_idx: TxIdx) -> ReadMemoryResult {
        let mut result: Option<(usize, MemoryEntry)> = None;
        if let Some(location_map) = self.data.get(location) {
            for entry in location_map.iter() {
                let idx = entry.key();
                if *idx < tx_idx {
                    // TODO: Cleaner code please...
                    if result.is_none() || result.clone().unwrap().0 < *idx {
                        result = Some((*idx, entry.value().clone()));
                    }
                }
            }
        }
        match result {
            None => ReadMemoryResult::NotFound,
            Some((blocking_tx_idx, MemoryEntry::Estimate)) => {
                ReadMemoryResult::ReadError { blocking_tx_idx }
            }
            Some((max_idx, MemoryEntry::Data(tx_incarnation, value))) => ReadMemoryResult::Ok {
                version: TxVersion {
                    tx_idx: max_idx,
                    tx_incarnation,
                },
                value,
            },
        }
    }
}