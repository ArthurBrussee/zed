//! One session's agent-to-client frames, numbered from 1, kept on disk on the
//! box with a bounded tail in memory.
//!
//! The format is the one thing a newer relay reads from an older one, so it
//! only ever grows: a header line, then one JSON object per line, either a
//! frame (`seq`, `line`) or the id of a request that has been answered.

use anyhow::{Context as _, Result};
use collections::{HashSet, VecDeque};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
};

pub const LOG_FORMAT_VERSION: u32 = 1;
const TAIL_FRAMES: usize = 4096;
const TAIL_BYTES: usize = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Header {
    acp_relay_log: u32,
    session: String,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Entry {
    Frame { seq: u64, line: String },
    Answered { answered: u64 },
}

pub struct SessionLog {
    path: Option<PathBuf>,
    file: Option<File>,
    next_seq: u64,
    tail: VecDeque<(u64, Arc<str>)>,
    tail_bytes: usize,
    answered: HashSet<u64>,
}

impl SessionLog {
    /// A log under `dir`, or one held only in memory when there is no
    /// directory to write to. A log that cannot be written keeps working from
    /// memory, since losing the disk copy only costs catching up from very far
    /// back.
    pub fn create(dir: Option<&Path>, session_id: &str) -> Self {
        let path = dir.map(|dir| dir.join(file_name(session_id)));
        let file = path.as_ref().and_then(|path| {
            let result = (|| -> Result<File> {
                std::fs::create_dir_all(path.parent().context("log path has no parent")?)?;
                let mut file = File::create(path)?;
                let header = Header {
                    acp_relay_log: LOG_FORMAT_VERSION,
                    session: session_id.to_string(),
                };
                writeln!(file, "{}", serde_json::to_string(&header)?)?;
                Ok(file)
            })();
            result
                .map_err(|error| log::warn!("quiet-ui relay: no log on disk for {session_id}: {error:#}"))
                .ok()
        });
        Self {
            path,
            file,
            next_seq: 1,
            tail: VecDeque::new(),
            tail_bytes: 0,
            answered: HashSet::default(),
        }
    }

    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    pub fn append(&mut self, line: &str) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.write(&Entry::Frame {
            seq,
            line: line.to_string(),
        });
        self.tail.push_back((seq, Arc::from(line)));
        self.tail_bytes += line.len();
        while self.tail.len() > TAIL_FRAMES || self.tail_bytes > TAIL_BYTES {
            let Some((_, evicted)) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes -= evicted.len();
        }
        seq
    }

    pub fn mark_answered(&mut self, request: u64) {
        if self.answered.insert(request) {
            self.write(&Entry::Answered { answered: request });
        }
    }

    pub fn is_answered(&self, request: u64) -> bool {
        self.answered.contains(&request)
    }

    /// The frame numbered `seq`, from memory or else from disk.
    pub fn frame(&self, seq: u64) -> Option<Arc<str>> {
        if let Some(index) = self.tail_index(seq) {
            return Some(self.tail[index].1.clone());
        }
        self.frames_from_disk(seq.saturating_sub(1))
            .into_iter()
            .find(|(frame_seq, _)| *frame_seq == seq)
            .map(|(_, line)| line)
    }

    /// Every frame after `seq`, in order.
    pub fn frames_after(&self, seq: u64) -> Vec<(u64, Arc<str>)> {
        let in_memory = self
            .tail
            .front()
            .is_some_and(|(first, _)| *first <= seq + 1)
            || self.last_seq() <= seq;
        if in_memory {
            return self
                .tail
                .iter()
                .filter(|(frame_seq, _)| *frame_seq > seq)
                .cloned()
                .collect();
        }
        self.frames_from_disk(seq)
    }

    fn tail_index(&self, seq: u64) -> Option<usize> {
        let first = self.tail.front()?.0;
        let index = usize::try_from(seq.checked_sub(first)?).ok()?;
        (index < self.tail.len()).then_some(index)
    }

    fn frames_from_disk(&self, after: u64) -> Vec<(u64, Arc<str>)> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        match read_frames(path, after) {
            Ok(frames) => frames,
            Err(error) => {
                log::warn!(
                    "quiet-ui relay: reading {} failed: {error:#}",
                    path.display()
                );
                Vec::new()
            }
        }
    }

    fn write(&mut self, entry: &Entry) {
        let Some(file) = &mut self.file else {
            return;
        };
        let result = serde_json::to_string(entry)
            .map_err(anyhow::Error::from)
            .and_then(|line| writeln!(file, "{line}").map_err(anyhow::Error::from));
        if let Err(error) = result {
            log::warn!("quiet-ui relay: log write failed, keeping memory only: {error:#}");
            self.file = None;
        }
    }

    pub fn delete(self) {
        if let Some(path) = self.path {
            std::fs::remove_file(path).ok();
        }
    }
}

fn read_frames(path: &Path, after: u64) -> Result<Vec<(u64, Arc<str>)>> {
    let file = OpenOptions::new().read(true).open(path)?;
    let mut frames = Vec::new();
    for line in BufReader::new(file).lines().skip(1) {
        if let Ok(Entry::Frame { seq, line }) = serde_json::from_str(&line?)
            && seq > after
        {
            frames.push((seq, Arc::from(line)));
        }
    }
    Ok(frames)
}

/// Session ids are the agent's own strings, so they are hashed rather than
/// trusted as file names.
fn file_name(session_id: &str) -> String {
    let readable = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(48)
        .collect::<String>();
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in session_id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{readable}-{hash:016x}.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_past_the_memory_tail_come_back_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(Some(dir.path()), "session/../1");
        let total = TAIL_FRAMES as u64 + 10;
        for index in 1..=total {
            assert_eq!(log.append(&format!("frame {index}")), index);
        }
        assert_eq!(log.tail.len(), TAIL_FRAMES);

        let all = log.frames_after(0);
        assert_eq!(all.len() as u64, total);
        assert_eq!(all[0], (1, Arc::from("frame 1")));
        assert_eq!(log.frame(3).as_deref(), Some("frame 3"));

        let recent = log.frames_after(total - 2);
        assert_eq!(
            recent.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            [total - 1, total]
        );
        assert!(log.frames_after(total).is_empty());

        log.mark_answered(7);
        assert!(log.is_answered(7));
        // Answer markers do not disturb the frame numbering on disk.
        assert_eq!(log.frames_after(0).len() as u64, total);
    }

    #[test]
    fn a_log_without_a_directory_keeps_its_tail() {
        let mut log = SessionLog::create(None, "s");
        log.append("a");
        log.append("b");
        assert_eq!(log.frames_after(1), vec![(2, Arc::from("b"))]);
    }
}
