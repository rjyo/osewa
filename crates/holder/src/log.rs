use std::collections::VecDeque;
use std::rc::Rc;

use osewa_wire as wire;

/// Record kinds, matching the frame that carries each.
pub(crate) const REC_OUTPUT: u8 = wire::TYPE_OUTPUT;
pub(crate) const REC_EVENT: u8 = wire::TYPE_EVENT;

#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub start: u64,
    pub kind: u8,
    /// Sized to the record, never a slice of a bigger buffer: the log keeps
    /// records for megabytes, and each would pin its read buffer.
    pub data: Rc<[u8]>,
    /// Clipboard records go only to the owner's streams.
    pub owner_only: bool,
    /// The one stream (by uid) the record is for: state restored after a
    /// catch-up screen.
    pub only: Option<u64>,
}

impl Record {
    pub fn end(&self) -> u64 {
        self.start + wire::record_size(self.data.len())
    }

    /// Counted bytes: the payload plus the 8-byte end.
    pub fn cost(&self) -> usize {
        8 + self.data.len()
    }
}

/// The session's ring of records, capped by position bytes.
#[derive(Default)]
pub(crate) struct OutputLog {
    recs: VecDeque<Record>,
    /// Positions held, the sum of record sizes.
    pub size: u64,
    pub end: u64,
    pub cap: u64,
}

impl OutputLog {
    pub fn new(cap: u64) -> OutputLog {
        OutputLog { cap, ..Default::default() }
    }

    pub fn append(&mut self, kind: u8, data: &[u8], owner_only: bool, only: Option<u64>) -> Record {
        let r = Record { start: self.end, kind, data: Rc::from(data), owner_only, only };
        self.end = r.end();
        self.size += wire::record_size(data.len());
        self.recs.push_back(r.clone());
        // Keep at least the newest record so a resume from just before it
        // works.
        while self.size > self.cap && self.recs.len() > 1 {
            let old = self.recs.pop_front().unwrap();
            self.size -= wire::record_size(old.data.len());
        }
        r
    }

    /// The oldest position still held.
    pub fn start(&self) -> u64 {
        self.recs.front().map_or(self.end, |r| r.start)
    }

    /// The records from position p, or None when p is evicted, in the future
    /// or not on a record boundary.
    pub fn since(&self, p: u64) -> Option<Vec<Record>> {
        if p == self.end {
            return Some(Vec::new());
        }
        if p < self.start() || p > self.end {
            return None;
        }
        let i = self.recs.partition_point(|r| r.start < p);
        if i == self.recs.len() || self.recs[i].start != p {
            return None;
        }
        Some(self.recs.range(i..).cloned().collect())
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.recs.len()
    }
}
