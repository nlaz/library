//! Atomic JSON sidecar files.
//!
//! `data/` keeps small user-owned records (collections, titles, ingest
//! status, cards, annotations) as plain JSON next to the stores. Writers
//! must be crash-safe and readers must never see a torn file, so every
//! write goes tmp + rename. Readers are forgiving: a missing or corrupt
//! file reads as "nothing", and the next write rewrites it whole.

use std::io;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Serialize `v` as pretty JSON via tmp + rename, so concurrent readers
/// never see a torn file.
pub fn write_json_atomic<T: Serialize>(path: &Path, v: &T) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(v).map_err(io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Read a JSON sidecar; missing or corrupt files read as `None`.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sidecar-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip() {
        let dir = tmp_dir("rt");
        let path = dir.join("v.json");
        assert_eq!(read_json::<Vec<String>>(&path), None);

        write_json_atomic(&path, &vec!["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(
            read_json::<Vec<String>>(&path),
            Some(vec!["a".to_string(), "b".to_string()])
        );

        // rewrite replaces whole file, no tmp left behind
        write_json_atomic(&path, &vec!["c".to_string()]).unwrap();
        assert_eq!(read_json::<Vec<String>>(&path), Some(vec!["c".to_string()]));
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_reads_as_none() {
        let dir = tmp_dir("corrupt");
        let path = dir.join("bad.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(read_json::<Vec<String>>(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
