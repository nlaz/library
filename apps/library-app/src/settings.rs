//! Settings: persisted app configuration and the paths derived from it.

use std::path::PathBuf;

use library_ingest::IngestCtx;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::engine::{AppState, dev_root};

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub data: PathBuf,
    /// Rendered page-image width in pixels.
    #[serde(default = "default_width")]
    pub width: u32,
}

fn default_width() -> u32 {
    1600
}

/// Default data dir: the repo's ./data in dev builds, the per-user app-data
/// dir (~/Library/Application Support/dev.thelibrary/data) in release
/// bundles — a downloaded .app must never inherit the build machine's repo
/// path.
fn default_data(app: &AppHandle) -> PathBuf {
    pick_default_data(cfg!(debug_assertions), app.path().app_data_dir().ok())
}

fn pick_default_data(dev: bool, app_data: Option<PathBuf>) -> PathBuf {
    if dev {
        dev_root().join("data")
    } else {
        app_data
            .map(|d| d.join("data"))
            .unwrap_or_else(|| dev_root().join("data"))
    }
}

pub(crate) fn load_settings(app: &AppHandle) -> Settings {
    let file = app
        .path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("settings.json"));
    let saved: Option<Settings> = file
        .as_ref()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok());

    let mut s = saved.unwrap_or_else(|| Settings {
        data: default_data(app),
        width: default_width(),
    });
    if let Ok(data) = std::env::var("LIBRARY_DATA") {
        s.data = PathBuf::from(data);
    }
    // first run: the dir must exist before the stores open
    let _ = std::fs::create_dir_all(&s.data);
    s
}

fn save_settings(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

pub(crate) fn ingest_ctx(s: &Settings, ctx: &library_core::meta::Ctx) -> IngestCtx {
    // clean: false — the in-app ingest must never park the ~2GB on-device
    // model in memory implicitly; cached edits still get applied
    IngestCtx {
        data: s.data.clone(),
        meta: ctx.meta.clone(),
        width: s.width,
        clean: false,
        text_layer: true,
    }
}

#[tauri::command]
pub(crate) fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.clone()
}

#[tauri::command]
pub(crate) fn set_settings(app: AppHandle, s: Settings) -> Result<(), String> {
    // takes effect on next launch; live swap of the data dir isn't worth it
    save_settings(&app, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_json_roundtrip() {
        let s = Settings {
            data: PathBuf::from("/some/library/data"),
            width: 1200,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data, s.data);
        assert_eq!(back.width, s.width);
    }

    #[test]
    fn dev_default_data_is_repo_data() {
        let d = pick_default_data(true, Some(PathBuf::from("/ignored")));
        assert_eq!(d, dev_root().join("data"));
    }

    #[test]
    fn release_default_data_is_app_data_dir() {
        let d = pick_default_data(false, Some(PathBuf::from("/app/support")));
        assert_eq!(d, PathBuf::from("/app/support/data"));
    }

    #[test]
    fn release_default_data_falls_back_without_app_data_dir() {
        let d = pick_default_data(false, None);
        assert_eq!(d, dev_root().join("data"));
    }

    #[test]
    fn missing_width_gets_default() {
        let s: Settings = serde_json::from_str(r#"{"data":"/some/library/data"}"#).unwrap();
        assert_eq!(s.data, PathBuf::from("/some/library/data"));
        assert_eq!(s.width, default_width());
        assert_eq!(s.width, 1600);
    }
}
