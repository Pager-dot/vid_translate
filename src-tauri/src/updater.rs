//! In-app updates for the self-distributed builds, and an honest explanation everywhere else.
//!
//! The same binary ships through three channels that each own updates differently:
//!
//!   * GitHub Releases — ours to update, via `tauri-plugin-updater`.
//!   * Microsoft Store (MSIX) — the Store updates it; a self-updater is grounds for
//!     certification failure, and Program Files is read-only to us anyway.
//!   * Flathub — the Flatpak image is immutable and Flathub forbids self-updating.
//!
//! So the plugin is always registered (see the note on capabilities below) and the *check*
//! is gated at runtime. Note the gating is deliberately not a cargo feature: `tauri_build`
//! resolves the permission strings in `capabilities/default.json` at compile time, so a
//! capability naming `updater:default` fails to build the moment the crate is absent, and
//! keeping two capability files in sync per flavour is worse than one runtime branch.
//!
//! The frontend only ever calls the three commands below — never the updater plugin
//! directly — which is why `capabilities/default.json` needs no new entries: `core:default`
//! already covers invoking our own commands.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

pub const RELEASES_URL: &str = "https://github.com/Pager-dot/vid_translate/releases";

/// Only one download at a time. Mirrors how `PipelineState.stop_flag` guards the capture
/// pipeline — two concurrent installers would race over the same half-written bundle.
static INSTALLING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Downloaded from GitHub Releases — we own updates.
    Github,
    /// Microsoft Store MSIX package.
    MsStore,
    /// Flatpak (Flathub or any other remote).
    Flatpak,
    /// A Linux package install (.deb) — owned by the system package manager. The updater
    /// only supports AppImage on Linux, and /usr isn't ours to write to.
    LinuxPackage,
}

impl Channel {
    fn as_str(self) -> &'static str {
        match self {
            Channel::Github => "github",
            Channel::MsStore => "msstore",
            Channel::Flatpak => "flatpak",
            Channel::LinuxPackage => "linux-package",
        }
    }
}

/// Where this build came from.
///
/// `VT_CHANNEL` is baked in at compile time by the packaging workflows, because some
/// containers cannot be detected reliably from inside. Runtime sniffing then runs anyway, as
/// defence in depth: a build mislabelled `github` that finds itself inside a Flatpak must
/// still refuse to update itself.
pub fn channel() -> Channel {
    match option_env!("VT_CHANNEL") {
        Some("msstore") => return Channel::MsStore,
        Some("flatpak") => return Channel::Flatpak,
        _ => {}
    }

    // Flatpak always mounts this file into the sandbox, and sets FLATPAK_ID.
    if std::path::Path::new("/.flatpak-info").exists() || std::env::var_os("FLATPAK_ID").is_some() {
        return Channel::Flatpak;
    }

    #[cfg(target_os = "windows")]
    {
        // MSIX-packaged apps run from under C:\Program Files\WindowsApps.
        let packaged = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().to_ascii_lowercase().contains("\\windowsapps\\"))
            .unwrap_or(false);
        if packaged || std::env::var_os("MSIX_PACKAGE_FAMILY_NAME").is_some() {
            return Channel::MsStore;
        }
    }

    // On Linux the updater can only replace an AppImage, which exports $APPIMAGE to the app
    // it runs. Without it we're the .deb, installed under /usr by dpkg.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if std::env::var_os("APPIMAGE").is_none() {
            return Channel::LinuxPackage;
        }
    }

    Channel::Github
}

pub fn updates_supported() -> bool {
    matches!(channel(), Channel::Github)
}

#[derive(Serialize)]
pub struct AppInfo {
    version: String,
    channel: &'static str,
    os: &'static str,
    updates_supported: bool,
    releases_url: &'static str,
}

/// Version, channel and platform, so the About pane can say who updates this copy — and so
/// the audio permission screen can word itself for the right OS.
#[tauri::command]
pub fn app_info() -> AppInfo {
    let ch = channel();
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        channel: ch.as_str(),
        os: std::env::consts::OS,
        updates_supported: matches!(ch, Channel::Github),
        releases_url: RELEASES_URL,
    }
}

#[derive(Serialize)]
pub struct AvailableUpdate {
    version: String,
    notes: Option<String>,
    pub_date: Option<String>,
}

/// `None` means "you're up to date". An `Err` means the check itself failed.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<Option<AvailableUpdate>, String> {
    if !updates_supported() {
        // Belt and braces: the UI never shows a check button on these channels, so reaching
        // here means something went wrong — fail rather than quietly touching the network.
        return Err(format!(
            "updates on this build are managed by {}",
            channel().as_str()
        ));
    }

    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(AvailableUpdate {
            version: update.version.clone(),
            notes: update.body.clone(),
            pub_date: update.date.map(|d| d.to_string()),
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Mirrors the model downloads: `{kind, status, downloaded, total, error}` on a dedicated
/// event, so the settings pane can reuse the same progress widget verbatim.
#[derive(Clone, Serialize)]
struct UpdateProgress {
    kind: &'static str,
    status: &'static str,
    downloaded: u64,
    total: Option<u64>,
    error: Option<String>,
}

fn emit(app: &AppHandle, status: &'static str, downloaded: u64, total: Option<u64>, error: Option<String>) {
    let _ = app.emit(
        "update_progress",
        UpdateProgress { kind: "app", status, downloaded, total, error },
    );
}

/// Download and install the pending update, then relaunch.
///
/// Re-checks rather than reusing a handle from `check_for_update`: `Update` isn't `Clone` and
/// stashing it in managed state across two commands buys nothing but a lifetime problem.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    if !updates_supported() {
        return Err(format!(
            "updates on this build are managed by {}",
            channel().as_str()
        ));
    }
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("an update is already being installed".into());
    }

    let result = install_inner(&app).await;

    if let Err(ref e) = result {
        INSTALLING.store(false, Ordering::SeqCst);
        emit(&app, "error", 0, None, Some(e.clone()));
    }
    result?;

    // Windows exits on its own as the installer takes over; everywhere else we ask for it.
    // `restart` diverges, so nothing after this line runs.
    app.restart()
}

async fn install_inner(app: &AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no update available".to_string())?;

    emit(app, "downloading", 0, None, None);

    let mut downloaded: u64 = 0;
    let mut total: Option<u64> = None;
    update
        .download_and_install(
            |chunk, content_length| {
                downloaded += chunk as u64;
                total = content_length;
                emit(app, "downloading", downloaded, content_length, None);
            },
            || {
                emit(app, "installing", 0, None, None);
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let _ = total;
    emit(app, "done", downloaded, total, None);
    Ok(())
}
