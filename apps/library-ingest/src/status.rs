//! Durable per-document ingest status, in the `docs` table of `meta.db`.
//!
//! This was one JSON file per doc under `data/status/`, written tmp+rename
//! so the app and any CLI run shared a crash-safe view of what still needed
//! ingesting. The rows live in SQLite now — same states, same transitions,
//! but a transition updates six columns instead of rewriting a file, and
//! two processes can write different docs at once.
//!
//! The state machine itself is unchanged: a source file whose status is not
//! terminal is pending work, and there is no other queue.

use std::collections::BTreeMap;

use anyhow::Result;
use library_core::meta::Meta;
use rusqlite::OptionalExtension;
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
    /// Tombstone: the source file stays in `data/pdfs/` but the doc is out of the
    /// library. Terminal until the same doc is re-added.
    Deleted,
}

impl DocState {
    /// Terminal states are not pending work.
    pub fn terminal(self) -> bool {
        matches!(self, DocState::Ready | DocState::Failed | DocState::Deleted)
    }

    /// The stored form. Identical to the serde representation because the
    /// same strings cross the wire to the frontend — `states_match_serde`
    /// pins that.
    pub fn as_str(self) -> &'static str {
        match self {
            DocState::Queued => "queued",
            DocState::Preparing => "preparing",
            DocState::Staged => "staged",
            DocState::TextReady => "text_ready",
            DocState::Ready => "ready",
            DocState::Failed => "failed",
            DocState::Deleted => "deleted",
        }
    }

    fn parse(s: &str) -> Option<DocState> {
        Some(match s {
            "queued" => DocState::Queued,
            "preparing" => DocState::Preparing,
            "staged" => DocState::Staged,
            "text_ready" => DocState::TextReady,
            "ready" => DocState::Ready,
            "failed" => DocState::Failed,
            "deleted" => DocState::Deleted,
            _ => return None,
        })
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
    /// Ingest performance (stage timings, counts, legibility), written at
    /// the `text_ready`/`ready` transitions. Docs from before this existed
    /// have `None`; the perf view's lazy backfill fills in legibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<library_core::perf::IngestMetrics>,
}

impl DocStatus {
    pub fn new(state: DocState) -> Self {
        DocStatus {
            state,
            stage: None,
            done: 0,
            total: 0,
            updated: now(),
            error: None,
            metrics: None,
        }
    }

    pub fn failed(error: String) -> Self {
        DocStatus {
            error: Some(error),
            ..DocStatus::new(DocState::Failed)
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a `DocStatus` from a `docs` row selected in this column order:
/// state, stage, done, total, error, metrics, updated_at.
fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<DocStatus>> {
    let state: Option<String> = row.get(0)?;
    // a row that exists only to hold a title has no ingest state yet
    let Some(state) = state.as_deref().and_then(DocState::parse) else {
        return Ok(None);
    };
    let metrics: Option<String> = row.get(5)?;
    Ok(Some(DocStatus {
        state,
        stage: row.get(1)?,
        done: row.get::<_, i64>(2)? as u64,
        total: row.get::<_, i64>(3)? as u64,
        error: row.get(4)?,
        // a metrics blob we can't parse costs the perf view one row, not
        // the doc's status
        metrics: metrics.and_then(|m| serde_json::from_str(&m).ok()),
        updated: row.get::<_, i64>(6)? as u64,
    }))
}

/// The columns `from_row` expects, in its order. Just the list — callers
/// add their own `SELECT`/`FROM`, so neither can accidentally inherit the
/// other's clauses.
const COLS: &str = "state, stage, done, total, error, metrics, updated_at";

pub fn read(meta: &Meta, doc: &str) -> Option<DocStatus> {
    meta.read(|c| {
        c.query_row(
            &format!("SELECT {COLS} FROM docs WHERE id = ?1"),
            [doc],
            from_row,
        )
        .optional()
        .map(Option::flatten)
    })
}

pub fn write(meta: &Meta, doc: &str, st: &DocStatus) -> Result<()> {
    let metrics = match &st.metrics {
        Some(m) => Some(serde_json::to_string(m)?),
        None => None,
    };
    meta.write(|c| {
        c.execute(
            "INSERT INTO docs (id, state, stage, done, total, error, metrics, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
               state = excluded.state, stage = excluded.stage,
               done = excluded.done, total = excluded.total,
               error = excluded.error, metrics = excluded.metrics,
               updated_at = excluded.updated_at",
            rusqlite::params![
                doc,
                st.state.as_str(),
                st.stage,
                st.done as i64,
                st.total as i64,
                st.error,
                metrics,
                st.updated as i64,
            ],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// Forget a doc's ingest state without touching its title — the row may
/// still be carrying one.
pub fn remove(meta: &Meta, doc: &str) {
    let _ = meta.write(|c| {
        c.execute(
            "UPDATE docs SET state = NULL, stage = NULL, done = 0, total = 0,
                             error = NULL, metrics = NULL WHERE id = ?1",
            [doc],
        )?;
        Ok(())
    });
}

/// Every doc that has an ingest state.
pub fn scan(meta: &Meta) -> BTreeMap<String, DocStatus> {
    meta.read(|c| {
        let mut q = c.prepare(&format!(
            "SELECT {COLS}, id FROM docs WHERE state IS NOT NULL"
        ))?;
        let rows = q.query_map([], |row| {
            let id: String = row.get(7)?;
            Ok(from_row(row)?.map(|st| (id, st)))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            if let Some((id, st)) = row? {
                out.insert(id, st);
            }
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta {
        Meta::open_in_memory().unwrap()
    }

    #[test]
    fn round_trip_and_scan() {
        let m = meta();
        assert!(read(&m, "a").is_none());

        write(&m, "a", &DocStatus::new(DocState::Queued)).unwrap();
        write(&m, "b", &DocStatus::failed("boom".into())).unwrap();

        assert_eq!(read(&m, "a").unwrap().state, DocState::Queued);
        let b = read(&m, "b").unwrap();
        assert_eq!(b.state, DocState::Failed);
        assert_eq!(b.error.as_deref(), Some("boom"));

        let all = scan(&m);
        assert_eq!(all.keys().collect::<Vec<_>>(), vec!["a", "b"]);

        remove(&m, "a");
        assert!(read(&m, "a").is_none());
        assert_eq!(scan(&m).keys().collect::<Vec<_>>(), vec!["b"]);
    }

    /// `Meta::doc_status_json` rebuilds this struct's JSON shape from the
    /// columns, in a crate that can't see the struct. Two independent
    /// spellings of one wire format — the frontend reads whichever the host
    /// happens to serve, so they must agree exactly.
    #[test]
    fn status_json_matches_docstatus() {
        let m = meta();
        let mut st = DocStatus::new(DocState::Preparing);
        st.stage = Some("ocr".into());
        st.done = 12;
        st.total = 406;
        st.metrics = Some(library_core::perf::IngestMetrics {
            pages: Some(406),
            ..Default::default()
        });
        write(&m, "a", &st).unwrap();
        assert_eq!(m.doc_status_json("a"), serde_json::to_value(&st).unwrap());

        // and the sparse case: no stage, no error, no metrics
        let bare = DocStatus::new(DocState::Ready);
        write(&m, "b", &bare).unwrap();
        assert_eq!(m.doc_status_json("b"), serde_json::to_value(&bare).unwrap());

        // a failure carries its message
        let failed = DocStatus::failed("boom".into());
        write(&m, "c", &failed).unwrap();
        assert_eq!(
            m.doc_status_json("c"),
            serde_json::to_value(&failed).unwrap()
        );
    }

    #[test]
    fn a_transition_replaces_the_prior_state() {
        let m = meta();
        write(&m, "a", &DocStatus::failed("boom".into())).unwrap();
        write(&m, "a", &DocStatus::new(DocState::Ready)).unwrap();

        let st = read(&m, "a").unwrap();
        assert_eq!(st.state, DocState::Ready);
        assert_eq!(st.error, None, "the old error must not linger");
        assert_eq!(scan(&m).len(), 1, "one row per doc, not one per write");
    }

    #[test]
    fn a_title_only_row_has_no_status() {
        // set_title creates the docs row; that must not read as a doc
        // waiting to be ingested
        let m = meta();
        m.set_title("a", Some("Some Book")).unwrap();
        assert!(read(&m, "a").is_none());
        assert!(scan(&m).is_empty());
    }

    #[test]
    fn removing_status_keeps_the_title() {
        let m = meta();
        m.set_title("a", Some("Some Book")).unwrap();
        write(&m, "a", &DocStatus::new(DocState::Ready)).unwrap();
        remove(&m, "a");
        assert_eq!(m.titles().get("a").map(String::as_str), Some("Some Book"));
    }

    #[test]
    fn metrics_survive_the_round_trip() {
        let m = meta();
        let mut st = DocStatus::new(DocState::Ready);
        st.metrics = Some(library_core::perf::IngestMetrics {
            pages: Some(406),
            ..Default::default()
        });
        write(&m, "a", &st).unwrap();
        assert_eq!(read(&m, "a").unwrap().metrics.unwrap().pages, Some(406));
    }

    #[test]
    fn states_match_serde() {
        // the TS side matches on these exact strings, and so does the
        // stored column — the two must not drift apart
        for s in [
            DocState::Queued,
            DocState::Preparing,
            DocState::Staged,
            DocState::TextReady,
            DocState::Ready,
            DocState::Failed,
            DocState::Deleted,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(DocState::parse(s.as_str()), Some(s));
        }
    }
}
