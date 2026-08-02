//! Keeping The Library current.
//!
//! Driven from Rust rather than through the updater plugin's JavaScript
//! API, for three reasons: the endpoint can be pointed elsewhere, so the
//! whole download → verify → swap → relaunch path is testable before any
//! release exists; the failures worth explaining (an app installed
//! somewhere it cannot rewrite itself) can be turned into sentences here
//! instead of leaking a library's error text into the UI; and the webview
//! keeps its narrow capability set, since none of the plugin's commands
//! are exposed to it.
//!
//! Checks are manual. Nothing here runs unless someone presses a button.
//!
//! The update is fetched twice — once to describe it, once to install it —
//! rather than held between the two commands. That is one extra request on
//! a path a person is waiting on anyway, and it buys the guarantee that
//! what gets installed is what is current at the moment of installing,
//! with no stale handle to invalidate.

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

/// A release newer than the running build. No size: the manifest doesn't
/// carry one, and the only honest number arrives with the download itself,
/// on `update:progress`.
#[derive(Clone, Serialize)]
pub(crate) struct Release {
    pub version: String,
    /// The release notes, as written in `latest.json`.
    pub notes: Option<String>,
}

/// Download progress, for the bar in Settings. Emitted from the install
/// command; `total` is None until the server says how big the body is.
#[derive(Clone, Serialize)]
struct Progress {
    downloaded: u64,
    total: Option<u64>,
}

/// Whether this build can install anything over itself.
///
/// A debug build runs out of `target/debug`, not an app bundle. The
/// updater takes the executable's directory as what to replace, so
/// installing from one would rename `target/debug` aside and unpack an app
/// where the build output was — recoverable, but not something to offer.
/// The About section hides its controls when this is false.
#[tauri::command]
pub(crate) fn updates_supported() -> bool {
    !cfg!(debug_assertions)
}

/// Points the check at a `latest.json` other than the configured one. Used
/// to rehearse an update against a local server before publishing a real
/// release — see `scripts/verify-update.sh`.
const ENDPOINT_ENV: &str = "LIBRARY_UPDATE_ENDPOINT";

/// Where to ask.
///
/// The override is honoured in release builds too, and deliberately: a
/// rehearsal is only worth anything against the bundle that ships, and the
/// dev build can't install over itself to be rehearsed with. It is not the
/// security boundary being relaxed — the tarball still has to carry a
/// minisign signature that verifies against the public key compiled into
/// this binary, so an attacker who can set our environment gains nothing
/// they didn't already have by being able to set our environment.
fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let mut b = app.updater_builder();
    if let Ok(url) = std::env::var(ENDPOINT_ENV) {
        let parsed = url
            .parse()
            .map_err(|e| format!("bad {ENDPOINT_ENV}: {e}"))?;
        b = b.endpoints(vec![parsed]).map_err(|e| e.to_string())?;
    }
    b.build().map_err(|e| e.to_string())
}

/// Null when this is the newest build. Network failure is an error, not a
/// "you're up to date" — telling someone they are current when we could
/// not reach the question is the one answer here that is a lie.
#[tauri::command]
pub(crate) async fn check_update(app: AppHandle) -> Result<Option<Release>, String> {
    let found = updater(&app)?
        .check()
        .await
        .map_err(|e| format!("could not check for updates: {e}"))?;
    Ok(found.map(|u| Release {
        version: u.version,
        notes: u.body,
    }))
}

/// Download, verify and swap the bundle in place. Does not relaunch — the
/// caller decides when, because an update that restarts the app out from
/// under someone mid-sentence is worse than one that waits.
#[tauri::command]
pub(crate) async fn install_update(app: AppHandle) -> Result<(), String> {
    let Some(update) = updater(&app)?
        .check()
        .await
        .map_err(|e| format!("could not check for updates: {e}"))?
    else {
        return Err("this is already the newest version".into());
    };

    let mut downloaded: u64 = 0;
    let h = app.clone();
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = h.emit("update:progress", Progress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(install_error)
}

/// Turn the failure into something worth reading. The one that matters is
/// a bundle we cannot rewrite: an app still running from the disk image,
/// from a Downloads folder owned by someone else, or from a volume mounted
/// read-only. That is not a broken update, it is an app in the wrong
/// place, and the fix is a sentence rather than a retry.
fn install_error(e: tauri_plugin_updater::Error) -> String {
    let raw = e.to_string();
    let denied = matches!(&e, tauri_plugin_updater::Error::Io(io)
        if io.kind() == std::io::ErrorKind::PermissionDenied)
        || raw.contains("Permission denied")
        || raw.contains("Read-only file system");
    if denied {
        return "The Library can't replace itself where it is installed. \
                Move it to your Applications folder and try again."
            .into();
    }
    format!("the update could not be installed: {raw}")
}

/// Quit and come back as the version just installed.
///
/// `request_restart`, not `restart`: it routes through the main loop's
/// exit, which is where the single-instance plugin tears down its socket
/// before the replacement process is spawned. Without that the new
/// instance is taken for a duplicate and exits immediately.
///
/// The fjall stores need no shutdown of their own. Tauri's restart spawns
/// the new binary and then `exit(0)`s this one, so the OS drops the lock
/// files whether or not anything is dropped in Rust, and `init_engine`
/// already waits out a `Locked` store for 90 seconds — which is what
/// covers the moment where both processes are briefly alive.
#[tauri::command]
pub(crate) fn restart_app(app: AppHandle) {
    app.request_restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dev_build_does_not_offer_to_replace_itself() {
        // it runs from target/debug, not a bundle; there is nothing to swap
        assert_eq!(updates_supported(), !cfg!(debug_assertions));
    }

    #[test]
    fn a_bundle_we_cannot_write_is_explained_rather_than_reported() {
        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let msg = install_error(tauri_plugin_updater::Error::Io(denied));
        assert!(msg.contains("Applications folder"), "{msg}");
        assert!(
            !msg.contains("nope"),
            "the raw io error is not the message: {msg}"
        );
    }

    #[test]
    fn other_failures_keep_their_text() {
        let other = std::io::Error::other("the tarball was truncated");
        let msg = install_error(tauri_plugin_updater::Error::Io(other));
        assert!(msg.contains("truncated"), "{msg}");
    }
}
