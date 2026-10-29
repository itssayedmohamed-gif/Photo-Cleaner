//! Persistent review decisions, keyed by stable MediaStore photo IDs.
//!
//! Storage is an append-only text log, one op per line:
//!   `K <id>` keep, `D <id>` mark for deletion, `U <id>` undo, `R <id>` forget
//!   (used once the photo is really deleted). Replaying the log rebuilds state.
//! No array indexes are ever stored, so progress survives restarts, new photos,
//! and batch deletions.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Keep,
    Delete,
}

pub struct Store {
    path: Option<PathBuf>,
    file: Option<File>,
    state: HashMap<i64, Decision>,
    /// Decided ids in decision order (last = most recent) for undo.
    order: Vec<i64>,
    /// Cached: ids marked for deletion, in decision order.
    pending: Vec<i64>,
    log_lines: usize,
}

fn parse_line(line: &str) -> Option<(char, i64)> {
    let mut it = line.split_whitespace();
    let op = it.next()?.chars().next()?;
    let id = it.next()?.parse().ok()?;
    if matches!(op, 'K' | 'D' | 'U' | 'R') {
        Some((op, id))
    } else {
        None
    }
}

impl Store {
    pub fn in_memory() -> Self {
        Store {
            path: None,
            file: None,
            state: HashMap::new(),
            order: Vec::new(),
            pending: Vec::new(),
            log_lines: 0,
        }
    }

    pub fn open(path: &Path) -> Self {
        let mut s = Self::in_memory();
        s.path = Some(path.to_path_buf());
        let mut needs_newline = false;
        if let Ok(text) = fs::read_to_string(path) {
            needs_newline = !text.is_empty() && !text.ends_with('\n');
            for line in text.lines() {
                if let Some((op, id)) = parse_line(line) {
                    s.apply(op, id);
                    s.log_lines += 1;
                }
            }
        }
        s.refresh_pending();
        if s.log_lines > s.order.len() * 2 + 1000 {
            s.compact();
            needs_newline = false;
        }
        s.file = OpenOptions::new().create(true).append(true).open(path).ok();
        if needs_newline {
            if let Some(f) = s.file.as_mut() {
                let _ = f.write_all(b"\n"); // terminate a line cut off by a crash
            }
        }
        s
    }

    fn apply(&mut self, op: char, id: i64) {
        match op {
            'K' | 'D' => {
                let d = if op == 'K' { Decision::Keep } else { Decision::Delete };
                if self.state.insert(id, d).is_some() {
                    self.order.retain(|x| *x != id);
                }
                self.order.push(id);
            }
            'U' | 'R' => {
                if self.state.remove(&id).is_some() {
                    if self.order.last() == Some(&id) {
                        self.order.pop();
                    } else {
                        self.order.retain(|x| *x != id);
                    }
                }
            }
            _ => {}
        }
    }

    fn append(&mut self, op: char, id: i64) {
        self.apply(op, id);
        self.log_lines += 1;
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{op} {id}");
            let _ = f.flush();
        }
    }

    fn refresh_pending(&mut self) {
        let p: Vec<i64> = self
            .order
            .iter()
            .copied()
            .filter(|id| self.state.get(id) == Some(&Decision::Delete))
            .collect();
        self.pending = p;
    }

    fn compact(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let tmp = path.with_extension("tmp");
        let mut body = String::new();
        for id in &self.order {
            let op = match self.state.get(id) {
                Some(Decision::Keep) => 'K',
                _ => 'D',
            };
            body.push_str(&format!("{op} {id}\n"));
        }
        if fs::write(&tmp, body).is_ok() && fs::rename(&tmp, &path).is_ok() {
            self.log_lines = self.order.len();
        }
    }

    pub fn record(&mut self, d: Decision, id: i64) {
        self.append(if d == Decision::Keep { 'K' } else { 'D' }, id);
        self.refresh_pending();
    }

    /// Undo the most recent decision; returns its id.
    pub fn undo(&mut self) -> Option<i64> {
        let id = *self.order.last()?;
        self.append('U', id);
        self.refresh_pending();
        Some(id)
    }

    pub fn can_undo(&self) -> bool {
        !self.order.is_empty()
    }

    /// Forget ids entirely (photos that were really deleted).
    pub fn forget(&mut self, ids: &[i64]) {
        for &id in ids {
            self.append('R', id);
        }
        self.refresh_pending();
    }

    /// Drop pending-deletion marks for photos that no longer exist.
    pub fn prune_missing(&mut self, present: &HashSet<i64>) {
        let gone: Vec<i64> = self.pending.iter().copied().filter(|id| !present.contains(id)).collect();
        if !gone.is_empty() {
            self.forget(&gone);
        }
    }

    pub fn is_decided(&self, id: i64) -> bool {
        self.state.contains_key(&id)
    }

    pub fn decision(&self, id: i64) -> Option<Decision> {
        self.state.get(&id).copied()
    }

    pub fn pending(&self) -> &[i64] {
        &self.pending
    }

    pub fn pending_batch(&self, max: usize) -> Vec<i64> {
        self.pending.iter().take(max).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("pc-{}-{}.log", name, std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn record_and_undo_in_order() {
        let mut s = Store::in_memory();
        s.record(Decision::Keep, 10);
        s.record(Decision::Delete, 11);
        assert_eq!(s.pending(), &[11]);
        assert_eq!(s.undo(), Some(11));
        assert!(!s.is_decided(11));
        assert_eq!(s.undo(), Some(10));
        assert_eq!(s.undo(), None);
    }

    #[test]
    fn persists_across_reopen_by_id() {
        let p = tmp("persist");
        {
            let mut s = Store::open(&p);
            s.record(Decision::Keep, 5);
            s.record(Decision::Delete, 9);
            s.record(Decision::Keep, 7);
            s.undo();
        }
        let s = Store::open(&p);
        assert_eq!(s.decision(5), Some(Decision::Keep));
        assert_eq!(s.decision(9), Some(Decision::Delete));
        assert_eq!(s.decision(7), None);
        assert_eq!(s.pending(), &[9]);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn batch_delete_does_not_reset_progress() {
        let p = tmp("batch");
        let mut s = Store::open(&p);
        for id in 1..=6 {
            s.record(if id % 2 == 0 { Decision::Delete } else { Decision::Keep }, id);
        }
        let batch = s.pending_batch(200);
        assert_eq!(batch, vec![2, 4, 6]);
        s.forget(&batch);
        assert!(s.pending().is_empty());
        for id in [1, 3, 5] {
            assert_eq!(s.decision(id), Some(Decision::Keep)); // kept stay decided
        }
        drop(s);
        let s2 = Store::open(&p);
        assert!(s2.pending().is_empty());
        assert!(s2.is_decided(5) && !s2.is_decided(6));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn prune_missing_pending() {
        let mut s = Store::in_memory();
        s.record(Decision::Delete, 1);
        s.record(Decision::Delete, 2);
        let present: HashSet<i64> = [2].into_iter().collect();
        s.prune_missing(&present);
        assert_eq!(s.pending(), &[2]);
    }

    #[test]
    fn truncated_last_line_is_survivable() {
        let p = tmp("trunc");
        fs::write(&p, "K 1\nD 2\nK 3").unwrap(); // crash cut "K 33" -> "K 3" w/o newline
        {
            let mut s = Store::open(&p);
            s.record(Decision::Keep, 4);
        }
        let s = Store::open(&p);
        assert!(s.is_decided(1) && s.is_decided(2) && s.is_decided(4));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn compaction_preserves_state() {
        let p = tmp("compact");
        {
            let mut s = Store::open(&p);
            s.record(Decision::Keep, 1);
            s.record(Decision::Delete, 2);
            for _ in 0..1100 {
                s.record(Decision::Keep, 99);
                s.undo();
            }
        }
        let s = Store::open(&p); // triggers compaction
        assert_eq!(s.decision(1), Some(Decision::Keep));
        assert_eq!(s.pending(), &[2]);
        assert!(!s.is_decided(99));
        let lines = fs::read_to_string(&p).unwrap().lines().count();
        assert!(lines <= 3, "log not compacted: {lines}");
        let _ = fs::remove_file(&p);
    }
}
