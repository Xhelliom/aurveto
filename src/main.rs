//! aurveto — security gate for AUR updates.
//!
//! Per-package decision chain: whitelist -> delay (LastModified) ->
//! static scan (aur-scan) -> AI review of the PKGBUILD diff.

use anyhow::Result;
use aurveto::aur::SECS_PER_DAY;
use aurveto::pipeline::{Decision, Outcome};
use aurveto::{ai, aur, config, pipeline, scan, t};
use clap::{Parser, Subcommand};
use std::process::Command;

/// Length of an abbreviated commit hash for display.
const SHORT_HASH_LEN: usize = 7;

#[derive(Parser)]
#[command(
    name = "aurveto",
    version,
    about = "Secure AUR updates: delay, whitelist, static scan and AI review"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report: evaluate available updates without installing anything (default).
    Check {
        /// Show the AI reviewer's explanation for each reviewed package.
        #[arg(long)]
        explain: bool,
    },
    /// Install AUR packages judged safe (does not touch official repos).
    Apply {
        /// Restrict to these package names (a subset of those cleared for
        /// install). Empty = install everything the chain cleared.
        packages: Vec<String>,
        /// Do not install; only show the command that would run.
        #[arg(long)]
        dry_run: bool,
        /// Show the AI reviewer's explanation for each reviewed package.
        #[arg(long)]
        explain: bool,
    },
    /// Update EVERYTHING: official repos (pacman -Syu) then safe AUR packages.
    Upgrade {
        /// Show the AI reviewer's explanation for each reviewed package.
        #[arg(long)]
        explain: bool,
    },
    /// Show the age (last AUR modification) of every installed AUR package.
    Status,
    /// Show the config file path (and create it if missing).
    Config,
    /// Install the desktop entry, icon, translations and notification timer.
    Install,
    /// (internal) Emit a desktop notification of pending updates (run by the timer).
    #[command(hide = true)]
    Notify {
        /// Send a fixed test notification instead of the real update summary.
        #[arg(long)]
        test: bool,
    },
    /// (debug) Run the AI review on a local PKGBUILD file.
    ReviewFile {
        /// Path of the PKGBUILD (or diff) to analyse.
        path: String,
    },
    /// (debug) Check whether a revision has been reverted/cleaned since.
    RevertCheck {
        /// Package pkgbase.
        pkgbase: String,
        /// Target commit hash.
        commit: String,
    },
    /// Open the settings UI in the terminal (TUI).
    #[cfg(feature = "tui")]
    ConfigUi,
}

fn main() {
    aurveto::i18n::init();
    if let Err(e) = run() {
        eprintln!("{}: {e:#}", t!("error"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Check { explain: false }) {
        Cmd::Check { explain } => cmd_check(explain),
        Cmd::Apply {
            dry_run,
            packages,
            explain,
        } => cmd_apply(dry_run, &packages, explain),
        Cmd::Upgrade { explain } => cmd_upgrade(explain),
        Cmd::Status => cmd_status(),
        Cmd::Config => cmd_config(),
        Cmd::Install => cmd_install(),
        Cmd::Notify { test } => {
            if test {
                aurveto::deploy::send_test_notification();
                Ok(())
            } else {
                let cfg = config::Config::load_or_init()?;
                aurveto::deploy::send_notification(&cfg)
            }
        }
        Cmd::ReviewFile { path } => cmd_review_file(&path),
        Cmd::RevertCheck { pkgbase, commit } => {
            match aur::reverted_since(&pkgbase, &commit)? {
                Some(reason) => println!("{}", t!("⛔ SUSPICIOUS — {}", reason)),
                None => println!(
                    "{}",
                    t!("✅ revision not reverted since (nothing suspicious)")
                ),
            }
            Ok(())
        }
        #[cfg(feature = "tui")]
        Cmd::ConfigUi => {
            let cfg = config::Config::load_or_init()?;
            aurveto::tui::run(cfg)
        }
    }
}

fn cmd_review_file(path: &str) -> Result<()> {
    let cfg = config::Config::load_or_init()?;
    let content = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
    println!(
        "{}\n",
        t!(
            "AI review of {} (provider {}, up to {} confirmation votes)",
            path,
            format!("{:?}", cfg.ai.provider),
            cfg.ai.confirm_votes
        )
    );
    let v = ai::review_diff(&cfg.ai, path, &content)?;
    println!("  {:<10}: {}", t!("safe"), v.safe);
    println!("  {:<10}: {}", t!("severity"), v.severity);
    println!("  {:<10}: {}", t!("summary"), v.summary);
    Ok(())
}

fn cmd_check(explain: bool) -> Result<()> {
    let cfg = config::Config::load_or_init()?;
    print_official_summary();
    let outcomes = pipeline::evaluate(&cfg)?;
    print_report(&cfg, &outcomes, explain);
    Ok(())
}

/// List the pending official-repo updates (signed, out of aurveto's scope but
/// shown for a complete picture). Each line is `name old -> new`.
fn print_official_summary() {
    let updates = aur::official_updates();
    if updates.is_empty() {
        return;
    }
    println!(
        "{}",
        t!(
            "Official repositories: {} signed updates (handled by `aurveto upgrade`)",
            updates.len()
        )
    );
    for line in &updates {
        println!("  {line}");
    }
    println!();
}

/// Update the official repos then the safe AUR packages.
fn cmd_upgrade(explain: bool) -> Result<()> {
    println!("=== {} ===", t!("Official repositories (pacman -Syu)"));
    let status = Command::new("sudo").args(["pacman", "-Syu"]).status()?;
    if !status.success() {
        anyhow::bail!(t!("pacman -Syu failed — AUR update not started"));
    }
    println!("\n=== {} ===", t!("AUR packages (aurveto security chain)"));
    cmd_apply(false, &[], explain)
}

/// Restricts the cleared set to the explicitly requested package names.
///
/// Selection only ever *narrows* what the decision chain already cleared: a
/// requested package the chain did not allow (delayed, blocked, or not even a
/// pending update) is reported and skipped — never forced. Fail-closed.
fn select_requested<'a>(
    outcomes: &'a [Outcome],
    allow: Vec<&'a Outcome>,
    requested: &[String],
) -> Vec<&'a Outcome> {
    requested
        .iter()
        .filter_map(|name| {
            if let Some(o) = allow.iter().find(|o| &o.update.name == name) {
                Some(*o)
            } else {
                let known = outcomes.iter().any(|o| &o.update.name == name);
                if known {
                    eprintln!(
                        "{}",
                        t!(
                            "Skipping {} — not cleared by the security chain (see report above)",
                            name
                        )
                    );
                } else {
                    eprintln!("{}", t!("Skipping {} — no pending update", name));
                }
                None
            }
        })
        .collect()
}

fn cmd_apply(dry_run: bool, only: &[String], explain: bool) -> Result<()> {
    let cfg = config::Config::load_or_init()?;
    let outcomes = pipeline::evaluate(&cfg)?;
    print_report(&cfg, &outcomes, explain);

    let mut allow: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.decision == Decision::Allow)
        .collect();
    if !only.is_empty() {
        allow = select_requested(&outcomes, allow, only);
    }
    if allow.is_empty() {
        println!("\n{}", t!("Nothing to install."));
        return Ok(());
    }

    // Split lag revisions (built locally via makepkg) from latest versions
    // (whitelist/hold, installed by the helper).
    let lag: Vec<&Outcome> = allow.iter().copied().filter(|o| o.lag.is_some()).collect();
    let latest: Vec<String> = allow
        .iter()
        .filter(|o| o.lag.is_none())
        .map(|o| o.update.name.clone())
        .collect();

    if dry_run {
        for o in &lag {
            let target = o.lag.as_ref().unwrap();
            println!(
                "{}",
                t!(
                    "(dry-run) build {} {} (revision D-{}, commit {})",
                    o.update.name,
                    target.version,
                    cfg.delay_days,
                    target.commit[..target.commit.len().min(SHORT_HASH_LEN)].to_string()
                )
            );
        }
        if !latest.is_empty() {
            println!("(dry-run) {} -S {}", cfg.helper, latest.join(" "));
        }
        return Ok(());
    }

    for o in &lag {
        let target = o.lag.as_ref().unwrap();
        println!(
            "{}",
            t!(
                "→ building {} {} (revision D-{})",
                o.update.name,
                target.version,
                cfg.delay_days
            )
        );
        match aur::install_lagged(target) {
            Ok(true) => {}
            Ok(false) => eprintln!("  {}", t!("makepkg failed for {}", o.update.name)),
            Err(e) => eprintln!("  {}", t!("error {}: {}", o.update.name, e)),
        }
    }
    if !latest.is_empty() {
        let status = Command::new(&cfg.helper).arg("-S").args(&latest).status()?;
        if !status.success() {
            anyhow::bail!(t!("the helper returned an error"));
        }
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let cfg = config::Config::load_or_init()?;
    let pkgs_raw = match Command::new("pacman").args(["-Qmq"]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => String::new(),
    };
    let names: Vec<String> = pkgs_raw.lines().map(|l| l.trim().to_string()).collect();
    let last_mod = aur::last_modified(&names)?;
    let now = aur::now_secs();
    let threshold = cfg.delay_days * SECS_PER_DAY;

    println!(
        "{}",
        t!(
            "Age of {} installed AUR packages (delay = {}d):",
            names.len(),
            cfg.delay_days
        )
    );
    let mut rows: Vec<(String, Option<u64>)> = names
        .iter()
        .map(|n| {
            (
                n.clone(),
                last_mod
                    .get(n)
                    .map(|lm| now.saturating_sub(*lm) / SECS_PER_DAY),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, age) in rows {
        match age {
            Some(d) => {
                let flag = if d * SECS_PER_DAY < threshold {
                    "⏳"
                } else {
                    "  "
                };
                println!("  {flag} {name:<34} {d:>4}j");
            }
            None => println!("     {name:<34}    ?"),
        }
    }
    Ok(())
}

fn cmd_config() -> Result<()> {
    let cfg = config::Config::load_or_init()?;
    println!("{}: {}", t!("Config"), config::Config::path()?.display());
    println!(
        "  {:<18}: {} {} ({:?})",
        t!("delay"),
        cfg.delay_days,
        t!("days"),
        cfg.delay_mode
    );
    println!("  {:<18}: {}", t!("helper"), cfg.helper);
    println!("  {:<18}: {}", t!("aur-scan"), cfg.use_aur_scan);
    println!(
        "  {:<18}: {} ({}: {:?}, {}: {})",
        t!("AI review"),
        cfg.ai.enabled,
        t!("provider"),
        cfg.ai.provider,
        t!("model"),
        cfg.ai.model_or_default()
    );
    println!(
        "  {:<18}: {}",
        t!("confirm votes"),
        if cfg.ai.provider.is_local() {
            // Local inference is free, so "safe" verdicts are voted on too.
            t!(
                "{} (every verdict, unanimity required)",
                cfg.ai.confirm_votes
            )
        } else {
            t!("{} (triggered only on a block)", cfg.ai.confirm_votes)
        }
    );
    // Only probed for a local provider: it is the one that can be silently down.
    match ai::local_status(&cfg.ai) {
        ai::LocalStatus::Ready => {}
        ai::LocalStatus::NotInstalled => println!(
            "  {:<18}: {}",
            t!("local runtime"),
            t!("{} is not installed", ai::LOCAL_RUNTIME_PACKAGE)
        ),
        ai::LocalStatus::NotRunning => println!(
            "  {:<18}: {}",
            t!("local runtime"),
            t!("no server answering at {}", cfg.ai.endpoint())
        ),
    }
    println!(
        "  {:<18}: {}",
        t!("whitelist"),
        t!("{} packages", cfg.whitelist.len())
    );
    Ok(())
}

/// Install desktop integration: menu entry, icon, translations and the
/// notification timer (its active state follows `config.notify.enabled`).
fn cmd_install() -> Result<()> {
    let cfg = config::Config::load_or_init()?;

    let gui_available = aurveto::deploy::install_binaries()?;
    println!("{}", t!("Binaries installed in ~/.local/bin."));

    // The menu entry launches the GUI: only install it when the GUI is
    // available, otherwise the shortcut would point to nothing.
    if gui_available {
        aurveto::deploy::install_desktop_entry()?;
        println!("{}", t!("Desktop entry and icon installed."));
    } else {
        println!(
            "{}",
            t!("GUI binary not found — desktop entry skipped (build with `--features gui`).")
        );
    }

    aurveto::deploy::install_locales()?;
    println!("{}", t!("Translations installed."));

    aurveto::deploy::apply_notify(&cfg.notify)?;
    if cfg.notify.enabled {
        println!(
            "{}",
            t!(
                "Notification timer enabled (every {}h).",
                cfg.notify.interval_hours
            )
        );
    } else {
        println!(
            "{}",
            t!("Notification timer installed but disabled (enable it in settings).")
        );
    }
    Ok(())
}

fn print_report(cfg: &config::Config, outcomes: &[Outcome], explain: bool) {
    if outcomes.is_empty() {
        println!("{}", t!("No AUR updates available."));
        return;
    }
    let ai_state = if cfg.ai.enabled {
        t!("enabled")
    } else {
        t!("disabled")
    };
    println!(
        "{}\n",
        t!(
            "AUR updates: {} (delay {}d, AI review {})",
            outcomes.len(),
            cfg.delay_days,
            ai_state
        )
    );
    let (mut allow, mut delay, mut block) = (0, 0, 0);
    for o in outcomes {
        let tag = match &o.decision {
            Decision::Allow => {
                allow += 1;
                let base = if o.whitelisted {
                    t!("✅ (whitelist)")
                } else {
                    t!("✅ allowed")
                };
                if matches!(o.scan, scan::ScanResult::Clean) {
                    format!("{base} {}", t!("[scan ok]"))
                } else {
                    base
                }
            }
            Decision::Delayed(d) => {
                delay += 1;
                t!("⏳ delayed ({}d)", d)
            }
            Decision::Blocked(reason) => {
                block += 1;
                t!("⛔ BLOCKED — {}", reason)
            }
        };
        let tag = if o.ai_note.is_some() {
            format!("{tag} {}", t!("[AI reviewed]"))
        } else {
            tag
        };
        let ver = match &o.lag {
            // Lag mode: show the target version (revision D-N), not the latest.
            Some(target) if !o.update.old_ver.is_empty() => format!(
                "{} → {} (D-{})",
                o.update.old_ver, target.version, cfg.delay_days
            ),
            Some(target) => format!("→ {} (D-{})", target.version, cfg.delay_days),
            None => match (o.update.old_ver.is_empty(), o.update.new_ver.is_empty()) {
                (false, false) => format!("{} → {}", o.update.old_ver, o.update.new_ver),
                _ => o
                    .age_days
                    .map(|d| t!("modified {}d ago", d))
                    .unwrap_or_default(),
            },
        };
        println!("  {:<28} {:<26} {tag}", o.update.name, ver);
        if explain {
            if let Some(note) = &o.ai_note {
                println!("      {} {note}", t!("↳ AI:"));
            }
        }
    }
    println!(
        "\n{}",
        t!("Safe: {} | delayed: {} | blocked: {}", allow, delay, block)
    );
}
