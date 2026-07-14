//! launchd LaunchAgent for background ingestion (macOS).
//!
//! Installs `~/Library/LaunchAgents/computer.flower.library.ingest.plist`
//! running `library-ingest worker --data <dir>` whenever a file lands in
//! `data/pdfs` (WatchPaths), every 15 minutes (StartInterval), and at
//! login (RunAtLoad). The worker exits immediately when the app holds the
//! stores, so firing while the app is open is harmless — see
//! [`worker`](crate::worker) for the coordination rules.
//!
//! The app installs/repairs this on startup (it knows the real data dir);
//! `library-ingest install-agent` does the same from the CLI for dev use.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Matches the app's bundle identifier prefix (computer.flower.library).
pub const LABEL: &str = "computer.flower.library.ingest";

pub fn plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("no HOME")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

pub fn plist_body(bin: &Path, data: &Path) -> String {
    let bin = bin.display();
    let data = data.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>worker</string>
        <string>--data</string>
        <string>{data}</string>
    </array>
    <key>WatchPaths</key>
    <array>
        <string>{data}/pdfs</string>
    </array>
    <key>StartInterval</key>
    <integer>900</integer>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>ThrottleInterval</key>
    <integer>60</integer>
    <key>StandardOutPath</key>
    <string>{data}/logs/ingest.log</string>
    <key>StandardErrorPath</key>
    <string>{data}/logs/ingest.log</string>
</dict>
</plist>
"#
    )
}

/// Whether the installed plist already points at this binary + data dir.
pub fn installed_matches(bin: &Path, data: &Path) -> bool {
    match plist_path().and_then(|p| std::fs::read_to_string(&p).map_err(Into::into)) {
        Ok(current) => current == plist_body(bin, data),
        Err(_) => false,
    }
}

/// Write the plist (atomically) and (re)load it into the user's launchd
/// session. Safe to call repeatedly; a no-op when nothing changed.
pub fn install(bin: &Path, data: &Path) -> Result<PathBuf> {
    if !bin.is_file() {
        bail!("worker binary not found at {}", bin.display());
    }
    let path = plist_path()?;
    if installed_matches(bin, data) {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::create_dir_all(data.join("logs"))?;
    let tmp = path.with_extension("plist.tmp");
    std::fs::write(&tmp, plist_body(bin, data))?;
    std::fs::rename(&tmp, &path)?;

    let uid = unsafe { libc::getuid() };
    // bootout of a not-loaded agent fails; that's fine
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .output();
    let out = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}")])
        .arg(&path)
        .output()
        .context("running launchctl bootstrap")?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(path)
}
