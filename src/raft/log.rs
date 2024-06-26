use super::{NodeID, Term};
use crate::encoding::{self, bincode, Key as _, Value as _};
use crate::error::Result;
use crate::storage;
use crate::{asserterr, errassert};

use serde::{Deserialize, Serialize};

/// A log index. Starts at 1, indicates no index if 0.
pub type Index = u64;

/// A log entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The entry index.
    pub index: Index,
    /// The term in which the entry was added.
    pub term: Term,
    /// The state machine command. None (noop) commands are used during leader
    /// election to commit old entries, see section 5.4.2 in the Raft paper.
    pub command: Option<Vec<u8>>,
}

impl encoding::Value for Entry {}

/// A log storage key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Key {
    /// A log entry, storing the term and command.
    Entry(Index),
    /// Stores the current term and vote (if any).
    TermVote,
    /// Stores the current commit index (if any).
    CommitIndex,
}

impl encoding::Key<'_> for Key {}

/// Log key prefixes used for prefix scans. Must match the Key structure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum KeyPrefix {
    Entry,
    TermVote,
    CommitIndex,
}

impl encoding::Key<'_> for KeyPrefix {}

/// The Raft log stores a sequence of arbitrary commands (typically writes) that
/// are replicated across nodes and applied sequentially to the local state
/// machine. Each entry contains an index, command, and the term in which the
/// leader proposed it. Commands may be noops (None), which are added when a
/// leader is elected (see section 5.4.2 in the Raft paper). For example:
///
/// Index | Term | Command
/// ------|------|------------------------------------------------------
///   1   |   1  | None
///   2   |   1  | CREATE TABLE table (id INT PRIMARY KEY, value STRING)
///   3   |   1  | INSERT INTO table VALUES (1, 'foo')
///   4   |   2  | None
///   5   |   2  | UPDATE table SET value = 'bar' WHERE id = 1
///   6   |   2  | DELETE FROM table WHERE id = 1
///
/// A key/value store is used to store the log entries on disk, keyed by index,
/// along with a few other metadata keys (e.g. who we voted for in this term).
///
/// In the steady state, the log is append-only: when a client submits a
/// command, the leader appends it to its own log (via [`Log::append`]) and
/// replicates it to followers who append it to their logs (via
/// [`Log::splice`]). When an index has been replicated to a majority of nodes
/// it becomes committed, making the log immutable up to that index and
/// guaranteeing that all nodes will eventually contain it. Nodes keep track of
/// the commit index via [`Log::commit`] and apply committed commands to the
/// state machine.
///
/// However, uncommitted entries can be replaced or removed. A leader may append
/// entries to its log, but then be unable to reach consensus on them (e.g.
/// because it is unable to communicate with a majority of nodes). If a
/// different leader is elected and writes different commands to those same
/// indexes, then the uncommitted entries will be replaced with entries from the
/// new leader once the old leader (or a follower) discovers it.
///
/// The Raft log has the following invariants:
///
/// * Entry indexes are contiguous starting at 1 (no index gaps).
/// * Entry terms never decrease from the previous entry.
/// * Appended entries are durable (flushed to disk).
/// * Committed entries are never changed or removed (no log truncation).
/// * Committed entries will eventually be replicated to all nodes.
/// * Entries with the same index/term contain the same command.
/// * If two logs contain a matching index/term, all previous entries
///   are identical (see section 5.3 in the Raft paper).
pub struct Log {
    /// The underlying storage engine. Uses a trait object instead of generics,
    /// to allow runtime selection of the engine and avoid propagating the
    /// generic type parameters throughout Raft.
    engine: Box<dyn storage::Engine>,
    /// The index of the last stored entry.
    last_index: Index,
    /// The term of the last stored entry.
    last_term: Term,
    /// The index of the last committed entry.
    commit_index: Index,
    /// The term of the last committed entry.
    commit_term: Term,
}

impl Log {
    /// Initializes a log using the given storage engine.
    pub fn new(mut engine: impl storage::Engine + 'static) -> Result<Self> {
        let (last_index, last_term) = engine
            .scan_prefix(&KeyPrefix::Entry.encode()?)
            .last()
            .transpose()?
            .map(|(_, v)| Entry::decode(&v))
            .transpose()?
            .map(|e| (e.index, e.term))
            .unwrap_or((0, 0));
        let (commit_index, commit_term) = engine
            .get(&Key::CommitIndex.encode()?)?
            .map(|v| bincode::deserialize(&v))
            .transpose()?
            .unwrap_or((0, 0));
        Ok(Self { engine: Box::new(engine), last_index, last_term, commit_index, commit_term })
    }

    /// Returns the commit index and term.
    pub fn get_commit_index(&self) -> (Index, Term) {
        (self.commit_index, self.commit_term)
    }

    /// Returns the last log index and term.
    pub fn get_last_index(&self) -> (Index, Term) {
        (self.last_index, self.last_term)
    }

    /// Returns the last known term (0 if none) and cast vote (if any).
    pub fn get_term(&mut self) -> Result<(Term, Option<NodeID>)> {
        Ok(self
            .engine
            .get(&Key::TermVote.encode()?)?
            .map(|v| bincode::deserialize(&v))
            .transpose()?
            .unwrap_or((0, None)))
    }

    /// Stores the most recent term and cast vote (if any). Enforces that the
    /// term does not regress, and that we only vote for one node in a term.
    pub fn set_term(&mut self, term: Term, vote: Option<NodeID>) -> Result<()> {
        match self.get_term()? {
            (t, _) if term < t => return errassert!("term regression {t} → {term}"),
            (t, _) if term > t => {} // below, term == t
            (0, _) => return errassert!("can't set term 0"),
            (t, v) if t == term && v == vote => return Ok(()),
            (_, None) => {}
            (_, v) if vote != v => return errassert!("can't change vote {v:?} → {vote:?}"),
            (_, _) => {}
        };
        self.engine.set(&Key::TermVote.encode()?, bincode::serialize(&(term, vote))?)?;
        self.engine.flush()?;
        Ok(())
    }

    /// Appends a command to the log and flushes it to disk, returning its
    /// index. None implies a noop command, typically after Raft leader changes.
    /// The term must be equal to or greater than the previous entry.
    pub fn append(&mut self, term: Term, command: Option<Vec<u8>>) -> Result<Index> {
        match self.get(self.last_index)? {
            Some(e) if term < e.term => return errassert!("term regression {} → {term}", e.term),
            None if self.last_index > 0 => return errassert!("log gap at {}", self.last_index),
            None if term == 0 => return errassert!("can't append entry with term 0"),
            Some(_) | None => {}
        }
        // We could omit the index in the encoded value, since it's also stored
        // in the key, but we keep it simple.
        let entry = Entry { index: self.last_index + 1, term, command };
        self.engine.set(&Key::Entry(entry.index).encode()?, entry.encode()?)?;
        self.engine.flush()?;
        self.last_index = entry.index;
        self.last_term = entry.term;
        Ok(entry.index)
    }

    /// Commits entries up to and including the given index. The index must
    /// exist, be at or after the current commit index, and have the given term.
    pub fn commit(&mut self, index: Index, term: Term) -> Result<Index> {
        match self.get(index)? {
            Some(e) if e.index < self.commit_index => {
                return errassert!("commit index regression {} → {}", self.commit_index, e.index);
            }
            Some(e) if e.term != term => return errassert!("commit term {term} != {}", e.term),
            Some(e) if e.index == self.commit_index => return Ok(index),
            Some(_) => {}
            None => return errassert!("commit index {index} does not exist"),
        };
        self.engine.set(&Key::CommitIndex.encode()?, bincode::serialize(&(index, term))?)?;
        // NB: the commit index doesn't need to be fsynced, since the entries
        // are fsynced and the commit index can be recovered from a log quorum.
        self.commit_index = index;
        self.commit_term = term;
        Ok(index)
    }

    /// Fetches an entry at an index, or None if it does not exist.
    pub fn get(&mut self, index: Index) -> Result<Option<Entry>> {
        self.engine.get(&Key::Entry(index).encode()?)?.map(|v| Entry::decode(&v)).transpose()
    }

    /// Checks if the log contains an entry with the given index and term.
    pub fn has(&mut self, index: Index, term: Term) -> Result<bool> {
        Ok(self.get(index)?.map(|e| e.term == term).unwrap_or(false))
    }

    /// Iterates over log entries in the given index range.
    pub fn scan(
        &mut self,
        range: impl std::ops::RangeBounds<Index>,
    ) -> Result<impl Iterator<Item = Result<Entry>> + '_> {
        use std::ops::Bound;
        let from = match range.start_bound() {
            Bound::Excluded(&index) => Bound::Excluded(Key::Entry(index).encode()?),
            Bound::Included(&index) => Bound::Included(Key::Entry(index).encode()?),
            Bound::Unbounded => Bound::Included(Key::Entry(0).encode()?),
        };
        let to = match range.end_bound() {
            Bound::Excluded(&index) => Bound::Excluded(Key::Entry(index).encode()?),
            Bound::Included(&index) => Bound::Included(Key::Entry(index).encode()?),
            Bound::Unbounded => Bound::Included(Key::Entry(Index::MAX).encode()?),
        };
        Ok(self.engine.scan_dyn((from, to)).map(|r| r.and_then(|(_, v)| Entry::decode(&v))))
    }

    /// Splices a set of entries into the log and flushes it to disk. The
    /// entries must have contiguous indexes and equal/increasing terms, and the
    /// first entry must be in the range [1,last_index+1] with a term at or
    /// equal to the previous (base) entry's term. New indexes will be appended.
    /// Overlapping indexes with the same term must be equal and will be
    /// ignored. Overlapping indexes with different terms will truncate the
    /// existing log at the first conflict and then splice the new entries.
    pub fn splice(&mut self, entries: Vec<Entry>) -> Result<Index> {
        let (Some(first), Some(last)) = (entries.first(), entries.last()) else {
            return Ok(self.last_index); // empty input is noop
        };

        // Check that the entries are well-formed.
        if first.index == 0 || first.term == 0 {
            return errassert!("spliced entry has index or term 0");
        }
        if !entries.windows(2).all(|w| w[0].index + 1 == w[1].index) {
            return errassert!("spliced entries are not contiguous");
        }
        if !entries.windows(2).all(|w| w[0].term <= w[1].term) {
            return errassert!("spliced entries have term regression");
        }

        // Check that the entries connect to the existing log (if any), and that the
        // term doesn't regress.
        match self.get(first.index - 1)? {
            Some(base) if first.term < base.term => {
                return errassert!("splice term regression {} → {}", base.term, first.term)
            }
            Some(_) => {}
            None if first.index == 1 => {}
            None => return errassert!("first index {} must touch existing log", first.index),
        }

        // Skip entries that are already in the log.
        let mut entries = entries.as_slice();
        let mut scan = self.scan(first.index..=last.index)?;
        while let Some(entry) = scan.next().transpose()? {
            // [0] is ok, because the scan has the same size as entries.
            asserterr!(entry.index == entries[0].index, "index mismatch at {entry:?}");
            if entry.term != entries[0].term {
                break;
            }
            asserterr!(entry.command == entries[0].command, "command mismatch at {entry:?}");
            entries = &entries[1..];
        }
        drop(scan);

        // If all entries already exist then we're done.
        let Some(first) = entries.first() else {
            return Ok(self.last_index);
        };

        // Write the entries that weren't already in the log, and remove the
        // tail of the old log if any. We can't write below the commit index,
        // since these entries must be immutable.
        asserterr!(first.index > self.commit_index, "spliced entries below commit index");

        for entry in entries {
            self.engine.set(&Key::Entry(entry.index).encode()?, entry.encode()?)?;
        }
        for index in last.index + 1..=self.last_index {
            self.engine.delete(&Key::Entry(index).encode()?)?;
        }
        self.engine.flush()?;

        self.last_index = last.index;
        self.last_term = last.term;
        Ok(self.last_index)
    }

    /// Returns log engine status.
    pub fn status(&mut self) -> Result<storage::Status> {
        self.engine.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam::channel::Receiver;
    use std::{error::Error, result::Result};
    use test_each_file::test_each_path;

    // Run goldenscript tests in src/raft/testscripts/log.
    test_each_path! { in "src/raft/testscripts/log" as scripts => test_goldenscript }

    fn test_goldenscript(path: &std::path::Path) {
        goldenscript::run(&mut TestRunner::new(), path).expect("goldenscript failed")
    }

    /// Runs Raft log goldenscript tests. For available commands, see run().
    struct TestRunner {
        log: Log,
        op_rx: Receiver<storage::debug::Operation>,
    }

    impl goldenscript::Runner for TestRunner {
        fn run(&mut self, command: &goldenscript::Command) -> Result<String, Box<dyn Error>> {
            let mut output = String::new();
            match command.name.as_str() {
                // append TERM [COMMAND] [oplog=BOOL]
                "append" => {
                    let mut args = command.consume_args();
                    let term = args.next_pos().ok_or("term not given")?.parse()?;
                    let command = args.next_pos().map(|a| a.value.as_bytes().to_vec());
                    let oplog = args.lookup_parse("oplog")?.unwrap_or(false);
                    args.reject_rest()?;
                    let index = self.log.append(term, command)?;
                    let entry = self.log.get(index)?.expect("entry not found");
                    self.maybe_oplog(oplog, &mut output);
                    output.push_str(&format!("append → {}\n", Self::format_entry(&entry)));
                }

                // commit INDEX@TERM [oplog=BOOL]
                "commit" => {
                    let mut args = command.consume_args();
                    let (index, term) = Self::parse_index_term(
                        &args.next_pos().ok_or("index/term not given")?.value,
                    )?;
                    let oplog = args.lookup_parse("oplog")?.unwrap_or(false);
                    args.reject_rest()?;
                    let index = self.log.commit(index, term)?;
                    let entry = self.log.get(index)?.expect("entry not found");
                    self.maybe_oplog(oplog, &mut output);
                    output.push_str(&format!("commit → {}\n", Self::format_entry(&entry)));
                }

                // dump
                "dump" => {
                    command.consume_args().reject_rest()?;
                    let range = (std::ops::Bound::Unbounded, std::ops::Bound::Unbounded);
                    let mut scan = self.log.engine.scan_dyn(range);
                    while let Some((key, value)) = scan.next().transpose()? {
                        output.push_str(&Self::format_key_value(&key, &value));
                        output.push('\n');
                    }
                }

                // get INDEX...
                "get" => {
                    let mut args = command.consume_args();
                    let indexes: Vec<Index> =
                        args.rest_pos().iter().map(|a| a.parse()).collect::<Result<_, _>>()?;
                    args.reject_rest()?;
                    for index in indexes {
                        let entry = self
                            .log
                            .get(index)?
                            .as_ref()
                            .map(Self::format_entry)
                            .unwrap_or("None".to_string());
                        output.push_str(&format!("{entry}\n"));
                    }
                }

                // get_term
                "get_term" => {
                    command.consume_args().reject_rest()?;
                    let (term, vote) = self.log.get_term()?;
                    output.push_str(&format!(
                        "term={term} vote={}\n",
                        vote.map(|v| v.to_string()).unwrap_or("None".to_string())
                    ));
                }

                // has INDEX@TERM...
                "has" => {
                    let mut args = command.consume_args();
                    let indexes: Vec<(Index, Term)> = args
                        .rest_pos()
                        .iter()
                        .map(|a| Self::parse_index_term(&a.value))
                        .collect::<Result<_, _>>()?;
                    args.reject_rest()?;
                    for (index, term) in indexes {
                        let has = self.log.has(index, term)?;
                        output.push_str(&format!("{has}\n"));
                    }
                }

                // scan [RANGE]
                "scan" => {
                    let mut args = command.consume_args();
                    let range = Self::parse_index_range(
                        args.next_pos().map_or("..", |a| a.value.as_str()),
                    )?;
                    args.reject_rest()?;
                    let mut scan = self.log.scan(range)?;
                    while let Some(entry) = scan.next().transpose()? {
                        output.push_str(&format!("{}\n", Self::format_entry(&entry)));
                    }
                    if output.is_empty() {
                        output.push_str("<empty>");
                    }
                }

                // set_term TERM [VOTE] [oplog=true]
                "set_term" => {
                    let mut args = command.consume_args();
                    let term = args.next_pos().ok_or("term not given")?.parse()?;
                    let vote = args.next_pos().map(|a| a.parse()).transpose()?;
                    let oplog = args.lookup_parse("oplog")?.unwrap_or(false);
                    args.reject_rest()?;
                    self.log.set_term(term, vote)?;
                    self.maybe_oplog(oplog, &mut output);
                }

                // splice [INDEX@TERM=COMMAND...] [oplog=BOOL]
                "splice" => {
                    let mut args = command.consume_args();
                    let oplog = args.lookup_parse("oplog")?.unwrap_or(false);
                    let mut entries = Vec::new();
                    for arg in args.rest_key() {
                        let (index, term) = Self::parse_index_term(arg.key.as_deref().unwrap())?;
                        let command = match arg.value.as_str() {
                            "" => None,
                            value => Some(value.as_bytes().to_vec()),
                        };
                        entries.push(Entry { index, term, command });
                    }
                    args.reject_rest()?;
                    let index = self.log.splice(entries)?;
                    let entry = self.log.get(index)?.expect("entry not found");
                    self.maybe_oplog(oplog, &mut output);
                    output.push_str(&format!("splice → {}\n", Self::format_entry(&entry)));
                }

                // status [engine=BOOL]
                "status" => {
                    let mut args = command.consume_args();
                    let engine = args.lookup_parse("engine")?.unwrap_or(false);
                    args.reject_rest()?;
                    let (commit_index, commit_term) = self.log.get_commit_index();
                    let (last_index, last_term) = self.log.get_last_index();
                    output.push_str(&format!(
                        "last={last_index}@{last_term} commit={commit_index}@{commit_term}"
                    ));
                    if engine {
                        output.push_str(&format!(" engine={:#?}", self.log.status()?));
                    }
                    output.push('\n');
                }

                name => return Err(format!("unknown command {name}").into()),
            }
            Ok(output)
        }

        fn end_command(&mut self, _: &goldenscript::Command) -> Result<String, Box<dyn Error>> {
            // Drain the oplog, to avoid it leaking to another command.
            while self.op_rx.try_recv().is_ok() {}
            Ok(String::new())
        }
    }

    impl TestRunner {
        fn new() -> Self {
            let engine = storage::Debug::new(storage::Memory::new());
            let op_rx = engine.op_rx();
            let log = Log::new(engine).expect("log init failed");
            Self { log, op_rx }
        }

        /// Formats a log entry.
        fn format_entry(entry: &Entry) -> String {
            let command = match entry.command.as_ref() {
                Some(raw) => std::str::from_utf8(raw).expect("invalid command"),
                None => "None",
            };
            format!("{}@{} {command}", entry.index, entry.term)
        }

        /// Formats a raw key.
        fn format_key(key: &[u8]) -> String {
            format!("{:?} 0x{}", Key::decode(key).expect("invalid key"), hex::encode(key))
        }

        /// Formats a raw key/value pair.
        fn format_key_value(key: &[u8], value: &[u8]) -> String {
            format!("{} = 0x{}", Self::format_key(key), hex::encode(value))
        }

        /// Outputs the oplog if requested.
        fn maybe_oplog(&self, maybe: bool, output: &mut String) {
            if !maybe {
                return;
            }
            while let Ok(op) = self.op_rx.try_recv() {
                use storage::debug::Operation;
                let s = match op {
                    Operation::Delete(k) => format!("delete {}", Self::format_key(&k)),
                    Operation::Flush => "flush".to_string(),
                    Operation::Set(k, v) => format!("set {}", Self::format_key_value(&k, &v)),
                };
                output.push_str(&format!("engine: {s}\n"));
            }
        }

        /// Parses an index@term pair.
        fn parse_index_term(s: &str) -> Result<(Index, Term), Box<dyn Error>> {
            let re = regex::Regex::new(r"^(\d+)@(\d+)$").expect("invalid regex");
            let groups = re.captures(s).ok_or_else(|| format!("invalid index/term {s}"))?;
            let index = groups.get(1).unwrap().as_str().parse()?;
            let term = groups.get(2).unwrap().as_str().parse()?;
            Ok((index, term))
        }

        /// Parses an index range, in Rust range syntax.
        fn parse_index_range(s: &str) -> Result<impl std::ops::RangeBounds<Index>, Box<dyn Error>> {
            let mut bound =
                (std::ops::Bound::<Index>::Unbounded, std::ops::Bound::<Index>::Unbounded);
            let re = regex::Regex::new(r"^(\d+)?\.\.(=)?(\d+)?").expect("invalid regex");
            let groups = re.captures(s).ok_or_else(|| format!("invalid range {s}"))?;
            if let Some(start) = groups.get(1) {
                bound.0 = std::ops::Bound::Included(start.as_str().parse()?);
            }
            if let Some(end) = groups.get(3) {
                let end = end.as_str().parse()?;
                if groups.get(2).is_some() {
                    bound.1 = std::ops::Bound::Included(end)
                } else {
                    bound.1 = std::ops::Bound::Excluded(end)
                }
            }
            Ok(bound)
        }
    }
}
