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
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
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

/// What [`install`] did. Declining is a normal outcome with a reason, not a
/// failure — callers log the reason and carry on.
pub enum Install {
    /// Loaded into launchd, or already exactly right.
    Ready(PathBuf),
    Skipped(String),
}

/// Whether this binary lives somewhere launchd will still find next week.
///
/// A path under `/Volumes` is a mounted image or an external disk — which is
/// what "opened the app straight from the DMG" looks like, and the single
/// most common way a first launch happens. An `AppTranslocation` path is the
/// randomised read-only copy Gatekeeper runs a quarantined app from. Baking
/// either into a plist installs an agent that wakes every fifteen minutes,
/// forever, pointed at a path that stopped existing when they ejected.
pub fn is_stable_location(bin: &Path) -> bool {
    let p = bin.to_string_lossy();
    !p.starts_with("/Volumes/") && !p.contains("/AppTranslocation/")
}

/// Parse `launchctl print-disabled gui/$UID` for one label. Lines look like
/// `"com.example.thing" => disabled`.
fn parse_disabled(printed: &str, label: &str) -> bool {
    printed
        .lines()
        .find(|l| l.contains(&format!("\"{label}\"")))
        .is_some_and(|l| l.split("=>").nth(1).is_some_and(|v| v.trim() == "disabled"))
}

/// Whether the user has switched this agent off in System Settings →
/// General → Login Items → Allow in the Background.
///
/// launchd remembers that decision across reinstalls, and it is the one
/// place a person can say no to us. Bootstrapping over it on every launch
/// would make their switch appear not to stick, which is worse than not
/// having offered the switch at all.
pub fn user_disabled() -> bool {
    let uid = unsafe { libc::getuid() };
    Command::new("launchctl")
        .args(["print-disabled", &format!("gui/{uid}")])
        .output()
        .ok()
        .is_some_and(|out| parse_disabled(&String::from_utf8_lossy(&out.stdout), LABEL))
}

/// Write the plist (atomically) and (re)load it into the user's launchd
/// session. Safe to call repeatedly; a no-op when nothing changed.
pub fn install(bin: &Path, data: &Path) -> Result<Install> {
    if !bin.is_file() {
        bail!("worker binary not found at {}", bin.display());
    }
    if !is_stable_location(bin) {
        return Ok(Install::Skipped(format!(
            "the app is running from {} — move it to /Applications for background indexing",
            bin.display()
        )));
    }
    if user_disabled() {
        return Ok(Install::Skipped(
            "background indexing is switched off in Login Items — leaving it off".into(),
        ));
    }
    let path = plist_path()?;
    if installed_matches(bin, data) {
        return Ok(Install::Ready(path));
    }
    std::fs::create_dir_all(path.parent().expect("plist path always has a parent"))?;
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
    Ok(Install::Ready(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dmg_or_translocated_app_is_not_a_stable_location() {
        for p in [
            "/Volumes/The Library/The Library.app/Contents/MacOS/library-ingest",
            "/private/var/folders/x/AppTranslocation/ABC-123/d/The Library.app/Contents/MacOS/library-ingest",
        ] {
            assert!(!is_stable_location(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn an_installed_app_is_a_stable_location() {
        for p in [
            "/Applications/The Library.app/Contents/MacOS/library-ingest",
            "/Users/someone/Applications/The Library.app/Contents/MacOS/library-ingest",
            "/Users/someone/code/library/target/debug/library-ingest",
        ] {
            assert!(is_stable_location(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn disabled_is_read_from_launchctl_output() {
        let printed = "\tdisabled services = {\n\t\t\"com.other.thing\" => enabled\n\t\t\"computer.flower.library.ingest\" => disabled\n\t}\n";
        assert!(parse_disabled(printed, LABEL));
    }

    #[test]
    fn enabled_and_absent_both_mean_not_disabled() {
        let enabled = "\t\t\"computer.flower.library.ingest\" => enabled\n";
        assert!(!parse_disabled(enabled, LABEL));
        // never toggled: launchd doesn't list it at all
        assert!(!parse_disabled(
            "\t\t\"com.other.thing\" => disabled\n",
            LABEL
        ));
    }

    #[test]
    fn a_label_that_merely_contains_ours_is_not_ours() {
        let other = "\t\t\"computer.flower.library.ingest.helper\" => disabled\n";
        assert!(!parse_disabled(other, LABEL));
    }
}
