//! System integration: desktop entry, icon, translation catalogs and the
//! systemd `--user` timer that triggers update notifications.
//!
//! All installation I/O (writing files to `~/.local/share`,
//! `~/.config/systemd/user`, calls to `systemctl`/`msgfmt`/`notify-send`) is
//! centralised here: the frontends only call these functions and present the
//! result.

use crate::config::{Config, NotifyConfig};
use crate::{aur, t};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Application identifier (desktop entry, icon, metadata).
const APP_ID: &str = "fr.xhelliom.AurVeto";
/// Name of the CLI binary.
const BIN_CLI: &str = "aurveto";
/// Name of the GUI binary (launched by the desktop entry).
const BIN_GUI: &str = "aurveto-gui";
/// Permissions of installed binaries (rwxr-xr-x).
const BIN_MODE: u32 = 0o755;
/// gettext domain (must match `i18n::init`).
const GETTEXT_DOMAIN: &str = "aurveto";
/// Base name of the notification systemd units (`.service` / `.timer`).
const NOTIFY_UNIT: &str = "aurveto-notify";
/// Delay after boot before the first check.
const NOTIFY_BOOT_DELAY: &str = "2min";
/// How often the `sudo` timestamp is refreshed during an install run. Well
/// under the default 15-minute `timestamp_timeout`, so even a long build never
/// lets the session expire.
const SUDO_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);

/// Desktop entry and icon, embedded in the binary so the `install` command is
/// self-contained (no need for the source tree at runtime).
const DESKTOP_BYTES: &[u8] = include_bytes!("../data/fr.xhelliom.AurVeto.desktop");
const ICON_BYTES: &[u8] = include_bytes!("../data/fr.xhelliom.AurVeto.svg");

/// Source translation catalogs, compiled at install time via `msgfmt`.
/// `(language_code, po_content)`.
const CATALOGS: &[(&str, &str)] = &[("fr", include_str!("../po/fr.po"))];

/// Root of user data (`$XDG_DATA_HOME` or `~/.local/share`).
fn data_home() -> Result<PathBuf> {
    dirs::data_dir().context("cannot resolve ~/.local/share")
}

/// User systemd units directory (`~/.config/systemd/user`).
fn systemd_user_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("cannot resolve ~/.config")?;
    Ok(base.join("systemd/user"))
}

/// Absolute path of the current binary, to bake into the systemd unit.
fn current_exe() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| GETTEXT_DOMAIN.to_string())
}

/// Copies the binaries (`aurveto`, and `aurveto-gui` if it was built) into
/// the user executables directory (`~/.local/bin`).
///
/// Returns `true` if the GUI binary is available afterwards (freshly copied or
/// already present): the caller only installs the menu entry in that case, so as
/// not to create a shortcut pointing to a missing binary.
pub fn install_binaries() -> Result<bool> {
    let dest_dir = dirs::executable_dir().context("cannot resolve ~/.local/bin")?;
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let src_dir = std::env::current_exe()
        .context("resolving the current binary")?
        .parent()
        .map(|p| p.to_path_buf())
        .context("the current binary has no parent directory")?;

    install_one_binary(&src_dir, &dest_dir, BIN_CLI)?;
    let gui_dest = dest_dir.join(BIN_GUI);
    install_one_binary(&src_dir, &dest_dir, BIN_GUI)?;
    Ok(gui_dest.exists())
}

/// Copies a binary from `src_dir` to `dest_dir` if it exists at the source and
/// is not already the same file (copying a binary onto itself would truncate it).
/// Missing at the source = nothing to do (not an error).
fn install_one_binary(
    src_dir: &std::path::Path,
    dest_dir: &std::path::Path,
    name: &str,
) -> Result<()> {
    let src = src_dir.join(name);
    if !src.exists() {
        return Ok(());
    }
    let dest = dest_dir.join(name);
    // Same path (we are already running from ~/.local/bin): copy nothing.
    if std::fs::canonicalize(&src).ok() == std::fs::canonicalize(&dest).ok() && dest.exists() {
        return Ok(());
    }
    // Temporary write then atomic rename: directly replacing a running binary
    // would fail (ETXTBSY); the rename only touches the directory entry, the
    // running process keeps its old inode.
    let tmp = dest_dir.join(format!(".{name}.new"));
    std::fs::copy(&src, &tmp)
        .with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(BIN_MODE))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp); // best-effort cleanup
        return Err(e).with_context(|| format!("installing {}", dest.display()));
    }
    Ok(())
}

/// Absolute path of a binary installed in `~/.local/bin`, or its bare name as a
/// last resort (binary missing / `~/.local/bin` unresolved). Graphical launchers
/// — and non-interactive `bash -c` — often do not have `~/.local/bin` in their
/// PATH: a bare name would not resolve there, hence the preference for absolute.
fn installed_binary(name: &str) -> String {
    dirs::executable_dir()
        .map(|d| d.join(name))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| name.to_string())
}

/// Path of `name` sitting next to the running binary, if it is there.
fn sibling_binary(name: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe_name = exe.file_name()?.to_str()?;
    let path = exe.parent()?.join(sibling_name(exe_name, name));
    path.exists().then(|| path.display().to_string())
}

/// A renamed GUI build keeps its suffix on the CLI it pairs with:
/// `aurveto-gui-dev` drives `aurveto-dev`, not the packaged `aurveto` that a
/// bare name would resolve to — the GUI would silently run another version.
fn sibling_name(gui_exe_name: &str, name: &str) -> String {
    let suffix = gui_exe_name.strip_prefix(BIN_GUI).unwrap_or("");
    format!("{name}{suffix}")
}

/// Command to use to launch the `aurveto` CLI binary, as an absolute path.
/// Intended for frontends that start the CLI in an external terminal whose PATH
/// does not include `~/.local/bin`.
///
/// The CLI **next to the running frontend** wins: a GUI must drive the CLI of
/// its own build. Resolving through `~/.local/bin` or the PATH first lets a
/// freshly built GUI silently talk to an older installed CLI, which then
/// rejects the flags the GUI sends it — a failure that surfaces as an
/// incomprehensible usage error in the terminal, long after the click.
pub fn cli_command() -> String {
    sibling_binary(BIN_CLI).unwrap_or_else(|| installed_binary(BIN_CLI))
}

/// Installs the desktop entry and icon into `~/.local/share`.
///
/// Makes the application visible in the menu and associates its icon. The
/// embedded `Exec` line (bare `aurveto-gui`) is rewritten with the **absolute
/// path** of the installed binary: graphical launchers often do not have
/// `~/.local/bin` in their PATH, so a bare command would not resolve there.
pub fn install_desktop_entry() -> Result<()> {
    let share = data_home()?;
    let desktop = share.join("applications").join(format!("{APP_ID}.desktop"));

    let gui_path = installed_binary(BIN_GUI);
    let content = String::from_utf8_lossy(DESKTOP_BYTES)
        .replace(&format!("Exec={BIN_GUI}"), &format!("Exec={gui_path}"));
    write_file(&desktop, content.as_bytes())?;

    let icon = share
        .join("icons/hicolor/scalable/apps")
        .join(format!("{APP_ID}.svg"));
    write_file(&icon, ICON_BYTES)?;
    Ok(())
}

/// Compiles and installs the translation catalogs (`msgfmt`).
///
/// A missing `msgfmt` is not fatal: we warn and continue (the strings then fall
/// back to the English source).
pub fn install_locales() -> Result<()> {
    let share = data_home()?;
    for (lang, po) in CATALOGS {
        let dest = share
            .join("locale")
            .join(lang)
            .join("LC_MESSAGES")
            .join(format!("{GETTEXT_DOMAIN}.mo"));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // `msgfmt - -o <dest>` reads the .po from stdin: no temporary file.
        let res = Command::new("msgfmt")
            .arg("-")
            .arg("-o")
            .arg(&dest)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(stdin) = child.stdin.take() {
                    let mut stdin = stdin;
                    stdin.write_all(po.as_bytes())?;
                }
                child.wait()
            });
        match res {
            Ok(status) if status.success() => {}
            Ok(_) => eprintln!("{}", t!("msgfmt failed for locale {}", lang)),
            Err(_) => {
                eprintln!("{}", t!("msgfmt not found — skipping translations"));
                break;
            }
        }
    }
    Ok(())
}

/// Writes (or refreshes) the notification systemd units then enables or disables
/// the timer depending on `cfg.enabled`.
///
/// The service runs `<exe> notify`: all the notification logic is in Rust,
/// nothing is hard-baked into a shell string.
pub fn apply_notify(cfg: &NotifyConfig) -> Result<()> {
    let dir = systemd_user_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let exe = current_exe();

    let service = format!(
        "[Unit]\n\
         Description=aurveto update notification\n\n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe} notify\n"
    );
    let interval = cfg.interval_hours.max(1);
    let timer = format!(
        "[Unit]\n\
         Description=aurveto periodic update check\n\n\
         [Timer]\n\
         OnBootSec={NOTIFY_BOOT_DELAY}\n\
         OnUnitActiveSec={interval}h\n\
         Persistent=true\n\n\
         [Install]\n\
         WantedBy=timers.target\n"
    );
    write_file(
        &dir.join(format!("{NOTIFY_UNIT}.service")),
        service.as_bytes(),
    )?;
    write_file(&dir.join(format!("{NOTIFY_UNIT}.timer")), timer.as_bytes())?;

    run_systemctl(&["daemon-reload"]);
    let timer_unit = format!("{NOTIFY_UNIT}.timer");
    if cfg.enabled {
        run_systemctl(&["enable", "--now", &timer_unit]);
    } else {
        run_systemctl(&["disable", "--now", &timer_unit]);
    }
    Ok(())
}

/// Computes the update counts and sends a desktop notification.
///
/// Lightweight by design: counts the *available* official and AUR updates without
/// triggering a scan or AI review (hence no API cost on the timer).
pub fn send_notification(cfg: &Config) -> Result<()> {
    let official = aur::official_updates().len();
    let aur = aur::list_updates(&cfg.helper).map(|u| u.len()).unwrap_or(0);

    if official > 0 || aur > 0 {
        notify_send(
            "normal",
            &t!("Updates available"),
            &t!("{} repo + {} AUR (aurveto upgrade)", official, aur),
        );
    } else if !cfg.notify.silent_when_up_to_date {
        notify_send("low", &t!("System up to date"), &t!("No updates"));
    }
    Ok(())
}

/// Immediately sends a test notification, **always visible** (whatever the state
/// of the updates), to check that `notify-send` and the desktop notification
/// daemon work.
pub fn send_test_notification() {
    notify_send(
        "normal",
        "aurveto",
        &t!("Test notification — if you see this, notifications work."),
    );
}

/// Opens a `sudo` session covering a whole install run: asks for the password
/// **once**, then keeps the timestamp warm in the background so no later
/// `pacman`, `makepkg -si` or AUR helper call prompts again. An update that
/// asks for the password three times is an update the user stops reading.
///
/// The refresher is a detached thread, started at most once per process: it
/// lives exactly as long as the run. Returns `false` if `sudo` is missing or
/// authentication failed — the caller decides whether to go on.
pub fn open_sudo_session() -> bool {
    let authenticated = Command::new("sudo")
        .arg("-v")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !authenticated {
        return false;
    }
    static REFRESHER: std::sync::Once = std::sync::Once::new();
    REFRESHER.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(SUDO_REFRESH);
            // `-n` never prompts: a lost timestamp just fails quietly here and
            // the next real command asks for the password itself.
            let _ = Command::new("sudo")
                .args(["-n", "-v"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    });
    true
}

/// Writes a file, creating its parent directories as needed.
fn write_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

/// Calls `systemctl --user <args>`; a failure is logged, never fatal.
fn run_systemctl(args: &[&str]) {
    let res = Command::new("systemctl").arg("--user").args(args).status();
    if let Ok(status) = res {
        if !status.success() {
            eprintln!("{}", t!("systemctl --user {} failed", args.join(" ")));
        }
    } else {
        eprintln!(
            "{}",
            t!("systemctl not found — notification timer not applied")
        );
    }
}

/// Calls `notify-send`; a missing tool is not fatal.
fn notify_send(urgency: &str, title: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-u", urgency, title, body])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_renamed_gui_pairs_with_the_same_renamed_cli() {
        assert_eq!(sibling_name("aurveto-gui-dev", BIN_CLI), "aurveto-dev");
        assert_eq!(sibling_name("aurveto-gui", BIN_CLI), "aurveto");
    }
}
