//! Durable per-document ingest status: `data/status/<doc>.json`.
//!
//! One file per doc, written atomically (tmp + rename), so the app, the
//! background worker, and any CLI run share one crash-safe view of what
//! still needs ingesting. The filesystem is the source of truth: a PDF in
//! `data/pdfs/` whose status is not terminal is pending work — there is no
//! other queue.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocState {
    /// Accepted into `data/pdfs/`, no work started.
    Queued,
    /// A worker owns it (see the claim file) and is running prepare phases.
    Preparing,
    /// Prepared records are serialized under `data/staged/<doc>/` because
    /// the store was locked at commit time; whoever holds the store next
    /// commits them without recomputing.
    Staged,
    /// Text is committed and searchable; figures still pending.
    TextReady,
    /// Fully indexed. Terminal.
    Ready,
    /// Ingest errored (`error` says why). Terminal until re-queued.
    Failed,
    /// Tombstone: the PDF stays in `data/pdfs/` but the doc is out of the
    /// library. Terminal until the same doc is re-added.
    Deleted,
}

impl DocState {
    /// Terminal states are not pending work.
    pub fn terminal(self) -> bool {
        matches!(self, DocState::Ready | DocState::Failed | DocState::Deleted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocStatus {
    pub state: DocState,
    /// Ingest stage while `preparing`: same strings the app's progress
    /// events use (`ocr`/`clean`/`embed`/`figures`/`clip`/`indexing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: u64,
    /// Unix seconds of the last transition.
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DocStatus {
    pub fn new(state: DocState) -> Self {
        DocStatus { state, stage: None, done: 0, total: 0, updated: now(), error: None }
    }

    pub fn failed(error: String) -> Self {
        DocStatus { error: Some(error), ..DocStatus::new(DocState::Failed) }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn dir(data: &Path) -> PathBuf {
    data.join("status")
}

fn path(data: &Path, doc: &str) -> PathBuf {
    dir(data).join(format!("{doc}.json"))
}

/// Serialize `v` as pretty JSON via tmp + rename, so concurrent readers
/// never see a torn file.
pub fn write_json_atomic<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read(data: &Path, doc: &str) -> Option<DocStatus> {
    let bytes = std::fs::read(path(data, doc)).ok()?;
    // a corrupt status file means "unknown" — the doc just counts as
    // pending again, and the next transition rewrites it
    serde_json::from_slice(&bytes).ok()
}

pub fn write(data: &Path, doc: &str, st: &DocStatus) -> Result<()> {
    std::fs::create_dir_all(dir(data))?;
    write_json_atomic(&path(data, doc), st)
}

pub fn remove(data: &Path, doc: &str) {
    let _ = std::fs::remove_file(path(data, doc));
}

/// Every doc with a status file. Claim files (`.lock`) and strays are
/// skipped.
pub fn scan(data: &Path) -> BTreeMap<String, DocStatus> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir(data)) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(doc) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if let Ok(bytes) = std::fs::read(&p)
            && let Ok(st) = serde_json::from_slice(&bytes)
        {
            out.insert(doc, st);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("status-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_and_scan() {
        let data = tmp("rt");
        assert!(read(&data, "a").is_none());

        write(&data, "a", &DocStatus::new(DocState::Queued)).unwrap();
        write(&data, "b", &DocStatus::failed("boom".into())).unwrap();
        // claim files must not show up in a scan
        std::fs::write(dir(&data).join("a.lock"), "123").unwrap();

        assert_eq!(read(&data, "a").unwrap().state, DocState::Queued);
        let b = read(&data, "b").unwrap();
        assert_eq!(b.state, DocState::Failed);
        assert_eq!(b.error.as_deref(), Some("boom"));

        let all = scan(&data);
        assert_eq!(all.keys().collect::<Vec<_>>(), vec!["a", "b"]);

        remove(&data, "a");
        assert!(read(&data, "a").is_none());
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn corrupt_status_reads_as_none() {
        let data = tmp("corrupt");
        std::fs::create_dir_all(dir(&data)).unwrap();
        std::fs::write(dir(&data).join("bad.json"), b"{not json").unwrap();
        assert!(read(&data, "bad").is_none());
        assert!(scan(&data).is_empty());
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn states_serialize_snake_case() {
        // the TS side matches on these exact strings
        let s = serde_json::to_string(&DocState::TextReady).unwrap();
        assert_eq!(s, "\"text_ready\"");
    }
}
