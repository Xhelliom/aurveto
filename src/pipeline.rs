//! Orchestration: for each AUR update, applies the decision chain
//! whitelist -> delay (hold or lag) -> static scan -> AI review.

use crate::ai;
use crate::aur::{self, LagTarget, PkgInfo, Update};
use crate::config::{Config, DelayMode};
use crate::scan::{self, ScanResult};
use crate::t;
use anyhow::Result;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Update allowed.
    Allow,
    /// Delayed because too recent (age in days).
    Delayed(u64),
    /// Blocked by the static scan or the AI review, with the reason.
    Blocked(String),
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub update: Update,
    pub age_days: Option<u64>,
    pub whitelisted: bool,
    pub scan: ScanResult,
    pub decision: Decision,
    /// In lag mode: the deferred revision to install (None = latest version).
    pub lag: Option<LagTarget>,
    /// For a datable `Delayed` verdict: the Unix timestamp (seconds) at which the
    /// next revision will have matured enough to become installable. `None` when
    /// the deadline is meaningless (VCS package, git error) or outside the delay.
    pub eligible_at: Option<u64>,
    /// Version that will actually be installed at `eligible_at` (in lag mode, the
    /// maturing revision — not necessarily the latest published). `None` if unknown.
    pub eligible_version: Option<String>,
    /// The AI reviewer's own explanation, whenever it ran (allowed or blocked).
    /// `None` when the AI review was skipped (disabled, empty diff, or call error).
    pub ai_note: Option<String>,
    /// The decision chain's per-step breakdown (whitelist / delay / anti-revert /
    /// scan / AI), in order. Populated when the chain actually ran (allowed or
    /// blocked by scan/AI/revert); empty for a plain delay. The frontends render
    /// it verbatim and never re-derive why a package was cleared or blocked.
    pub steps: Vec<ChainStep>,
}

/// Status of one decision-chain link, for the frontends' per-package breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// The guard ran and cleared the package.
    Passed,
    /// The guard did not run (disabled, not applicable, or unavailable).
    Skipped,
    /// The guard rejected the package — this link produced the block.
    Failed,
}

/// One link of the decision chain, with a human-readable note. Built by the
/// pipeline so the frontends only present it.
#[derive(Debug, Clone)]
pub struct ChainStep {
    pub name: String,
    pub status: StepStatus,
    pub note: String,
}

impl ChainStep {
    fn new(name: String, status: StepStatus, note: String) -> Self {
        Self { name, status, note }
    }
}

/// Evaluates all available updates according to the config.
pub fn evaluate(cfg: &Config) -> Result<Vec<Outcome>> {
    let updates = aur::list_updates(&cfg.helper)?;
    if updates.is_empty() {
        return Ok(Vec::new());
    }

    let names: Vec<String> = updates.iter().map(|u| u.name.clone()).collect();
    let infos = aur::fetch_infos(&names).unwrap_or_default();
    let now = aur::now_secs();
    let threshold = cfg.delay_days * aur::SECS_PER_DAY;

    let mut outcomes = Vec::new();
    for upd in updates {
        outcomes.push(evaluate_one(cfg, upd, &infos, now, threshold));
    }
    Ok(outcomes)
}

fn evaluate_one(
    cfg: &Config,
    upd: Update,
    infos: &HashMap<String, PkgInfo>,
    now: u64,
    threshold: u64,
) -> Outcome {
    let whitelisted = cfg.is_whitelisted(&upd.name);
    let info = infos.get(&upd.name);
    let age_days = info.map(|i| now.saturating_sub(i.last_modified) / aur::SECS_PER_DAY);

    // Trusted package: target the LATEST version, delay skipped, but the
    // scan + AI review still apply.
    if whitelisted {
        return decide_latest(cfg, upd, age_days, true);
    }

    match cfg.delay_mode {
        DelayMode::Hold => {
            let fresh = info
                .map(|i| now.saturating_sub(i.last_modified) < threshold)
                .unwrap_or(false);
            if fresh {
                return delayed(upd, age_days, (eligible_at(info, threshold), None));
            }
            decide_latest(cfg, upd, age_days, false)
        }
        DelayMode::Lag => evaluate_lag(cfg, upd, info, age_days, now, threshold),
    }
}

/// Lag mode: targets the revision that was HEAD `threshold` seconds ago.
fn evaluate_lag(
    cfg: &Config,
    upd: Update,
    info: Option<&PkgInfo>,
    age_days: Option<u64>,
    now: u64,
    threshold: u64,
) -> Outcome {
    let pkgbase = info
        .map(|i| i.package_base.clone())
        .unwrap_or_else(|| upd.name.clone());
    let before = now.saturating_sub(threshold);

    let target = match aur::lagged_target(&pkgbase, before) {
        // Package too young to have existed N days ago: it will mature → datable.
        Ok(None) => {
            let elig = lag_eligible(&pkgbase, &upd.old_ver, threshold, info);
            return delayed(upd, age_days, elig);
        }
        Ok(Some(t)) => t,
        Err(e) => {
            // Transient git error: unknown deadline.
            eprintln!("  (git unavailable for {}: {e})", upd.name);
            return delayed(upd, age_days, (None, None));
        }
    };

    // Dynamic version (VCS): per-revision lag is meaningless, so there is no
    // installation deadline to announce.
    if target.version == aur::DYNAMIC_VERSION {
        return delayed(upd, age_days, (None, None));
    }

    // Are we already up to date (or ahead) relative to the D-N target? If so,
    // the only available update is more recent than the delay → datable.
    if aur::vercmp(&target.version, &upd.old_ver) <= 0 {
        let elig = lag_eligible(&pkgbase, &upd.old_ver, threshold, info);
        return delayed(upd, age_days, elig);
    }

    // Guard: has the target revision been reverted/cleaned since? (A poisoned
    // version stays in the git history after an in-place fix.)
    match aur::reverted_since(&target.pkgbase, &target.commit) {
        Ok(Some(reason)) => {
            let decision = Decision::Blocked(t!("revision reverted since — {}", reason));
            let mut o = outcome(
                upd,
                age_days,
                false,
                ScanResult::Skipped,
                Some(target),
                decision,
            );
            o.steps = vec![
                lag_delay_step(cfg),
                ChainStep::new(t!("Anti-revert"), StepStatus::Failed, reason),
            ];
            return o;
        }
        Ok(None) => {}
        Err(e) => eprintln!("  (revert-check unavailable for {}: {e})", upd.name),
    }

    // Static scan + AI review on THE REVISION we will install.
    let scan = scan_lagged(&upd.name, &target.pkgbuild, cfg.use_aur_scan);
    let diff = if cfg.ai.enabled {
        aur::diff_against_installed(&upd.name, &target.pkgbuild)
    } else {
        String::new()
    };
    let (decision, ai_note, vet_steps) = vet(cfg, &upd.name, &scan, &diff);
    let mut steps = Vec::with_capacity(vet_steps.len() + 2);
    steps.push(lag_delay_step(cfg));
    steps.push(ChainStep::new(
        t!("Anti-revert"),
        StepStatus::Passed,
        t!("no compromise trace in the history"),
    ));
    steps.extend(vet_steps);
    let mut o = outcome(upd, age_days, false, scan, Some(target), decision);
    o.ai_note = ai_note;
    o.steps = steps;
    o
}

/// Decision targeting the latest version (whitelist, or hold after maturation).
fn decide_latest(cfg: &Config, upd: Update, age_days: Option<u64>, whitelisted: bool) -> Outcome {
    let scan = scan::scan_package(&upd.name, cfg.use_aur_scan);
    let diff = if cfg.ai.enabled {
        aur::pkgbuild_diff(&upd.name).unwrap_or_default()
    } else {
        String::new()
    };
    let (decision, ai_note, vet_steps) = vet(cfg, &upd.name, &scan, &diff);
    let mut steps = Vec::with_capacity(vet_steps.len() + 1);
    steps.push(if whitelisted {
        ChainStep::new(
            t!("Whitelist"),
            StepStatus::Passed,
            t!("trusted package, delay skipped"),
        )
    } else {
        ChainStep::new(
            t!("Delay"),
            StepStatus::Passed,
            t!("matured past the {}-day hold", cfg.delay_days),
        )
    });
    steps.extend(vet_steps);
    let mut o = outcome(upd, age_days, whitelisted, scan, None, decision);
    o.ai_note = ai_note;
    o.steps = steps;
    o
}

/// The "delay" step for a lag-mode allowed package (it targets the deferred
/// revision rather than the latest publication).
fn lag_delay_step(cfg: &Config) -> ChainStep {
    ChainStep::new(
        t!("Delay"),
        StepStatus::Passed,
        t!(
            "installing the deferred revision ({}-day lag)",
            cfg.delay_days
        ),
    )
}

/// Common static-scan + AI-review step. Returns the final decision, plus the
/// AI reviewer's own explanation whenever it ran — hidden from the default
/// report, but kept so the frontends can offer it on demand (badge/expand).
fn vet(
    cfg: &Config,
    name: &str,
    scan: &ScanResult,
    diff: &str,
) -> (Decision, Option<String>, Vec<ChainStep>) {
    let mut steps = vec![scan_step(cfg, scan)];
    if let ScanResult::Flagged(detail) = scan {
        return (Decision::Blocked(t!("aur-scan: {}", detail)), None, steps);
    }
    if cfg.ai.enabled && !diff.trim().is_empty() {
        match ai::review_diff(&cfg.ai, name, diff) {
            Ok(v) if !v.safe => {
                steps.push(ChainStep::new(
                    t!("AI review"),
                    StepStatus::Failed,
                    v.summary.clone(),
                ));
                let note = v.summary.clone();
                return (
                    Decision::Blocked(t!("AI [{}]: {}", v.severity, v.summary)),
                    Some(note),
                    steps,
                );
            }
            Ok(v) => {
                steps.push(ChainStep::new(
                    t!("AI review"),
                    StepStatus::Passed,
                    v.summary.clone(),
                ));
                return (Decision::Allow, Some(v.summary), steps);
            }
            Err(e) => {
                eprintln!("  (AI review unavailable for {name}: {e})");
                steps.push(ChainStep::new(
                    t!("AI review"),
                    StepStatus::Skipped,
                    t!("review unavailable"),
                ));
            }
        }
    } else if cfg.ai.enabled {
        steps.push(ChainStep::new(
            t!("AI review"),
            StepStatus::Skipped,
            t!("no diff to review"),
        ));
    } else {
        steps.push(ChainStep::new(
            t!("AI review"),
            StepStatus::Skipped,
            t!("disabled"),
        ));
    }
    (Decision::Allow, None, steps)
}

/// Builds the static-scan chain step from its result.
fn scan_step(cfg: &Config, scan: &ScanResult) -> ChainStep {
    let (status, note) = match scan {
        ScanResult::Clean => (StepStatus::Passed, t!("aur-scan: nothing to report")),
        ScanResult::Flagged(detail) => (StepStatus::Failed, t!("aur-scan: {}", detail)),
        ScanResult::Skipped if cfg.use_aur_scan => {
            (StepStatus::Skipped, t!("aur-scan unavailable"))
        }
        ScanResult::Skipped => (StepStatus::Skipped, t!("disabled")),
    };
    ChainStep::new(t!("Static scan"), status, note)
}

/// Builds an `Outcome` (avoids repeating the struct literal).
fn outcome(
    update: Update,
    age_days: Option<u64>,
    whitelisted: bool,
    scan: ScanResult,
    lag: Option<LagTarget>,
    decision: Decision,
) -> Outcome {
    Outcome {
        update,
        age_days,
        whitelisted,
        scan,
        decision,
        lag,
        eligible_at: None,
        eligible_version: None,
        ai_note: None,
        steps: Vec::new(),
    }
}

/// Deadline based on the latest publication: `last_modified + delay`, or
/// `None` if the metadata is missing. Correct for **hold** mode (which
/// re-blocks on every new publication) and used as a fallback in lag mode.
fn eligible_at(info: Option<&PkgInfo>, threshold: u64) -> Option<u64> {
    info.map(|i| i.last_modified + threshold)
}

/// Deadline in **lag** mode, anchored on the git history: `(date, version)` of
/// the next revision more recent than `installed`, the deadline being
/// `date + delay`. Robust against later publications (a new version does not
/// push back an already-acquired deadline) and announces the version actually
/// installed. Falls back to `eligible_at` (without a version) if git is unavailable.
fn lag_eligible(
    pkgbase: &str,
    installed: &str,
    threshold: u64,
    info: Option<&PkgInfo>,
) -> (Option<u64>, Option<String>) {
    match aur::next_upgrade(pkgbase, installed) {
        Ok(Some(nu)) => (Some(nu.committed_at + threshold), Some(nu.version)),
        Ok(None) => (eligible_at(info, threshold), None),
        Err(e) => {
            eprintln!("  (git history unavailable for {pkgbase}: {e})");
            (eligible_at(info, threshold), None)
        }
    }
}

/// "Delayed" verdict. `(eligible_at, eligible_version)` carry the deadline and the
/// targeted version when the delay is datable; `(None, None)` otherwise.
fn delayed(upd: Update, age_days: Option<u64>, eligible: (Option<u64>, Option<String>)) -> Outcome {
    let decision = Decision::Delayed(age_days.unwrap_or(0));
    let mut o = outcome(upd, age_days, false, ScanResult::Skipped, None, decision);
    (o.eligible_at, o.eligible_version) = eligible;
    o
}

/// Static scan of a lag revision: we write the PKGBUILD to a temporary file
/// and pass it to `aur-scan scan`.
fn scan_lagged(name: &str, pkgbuild: &str, enabled: bool) -> ScanResult {
    if !enabled {
        return ScanResult::Skipped;
    }
    let path = std::env::temp_dir().join(format!("aurveto-{name}.PKGBUILD"));
    if std::fs::write(&path, pkgbuild).is_err() {
        return ScanResult::Skipped;
    }
    let res = scan::scan_pkgbuild_file(&path, enabled);
    let _ = std::fs::remove_file(&path);
    res
}

/// List of allowed names (all Allow decisions combined).
pub fn allowed_names(outcomes: &[Outcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|o| o.decision == Decision::Allow)
        .map(|o| o.update.name.clone())
        .collect()
}

/// Breakdown of AUR verdicts: feeds the frontends' KPIs and visualization bar.
/// A pure counting aggregate — no decision logic here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Packages cleared for installation (latest version or lag revision).
    pub allowed: usize,
    /// Delayed packages (too recent for the configured delay).
    pub delayed: usize,
    /// Blocked packages (scan, AI or reverted revision).
    pub blocked: usize,
}

/// Counts the verdicts per category.
pub fn summarize(outcomes: &[Outcome]) -> Summary {
    let mut s = Summary::default();
    for o in outcomes {
        match o.decision {
            Decision::Allow => s.allowed += 1,
            Decision::Delayed(_) => s.delayed += 1,
            Decision::Blocked(_) => s.blocked += 1,
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aur::Update;
    use crate::scan::ScanResult;

    fn outcome_with(decision: Decision) -> Outcome {
        Outcome {
            update: Update {
                name: "pkg".into(),
                old_ver: String::new(),
                new_ver: String::new(),
            },
            age_days: None,
            whitelisted: false,
            scan: ScanResult::Skipped,
            decision,
            lag: None,
            eligible_at: None,
            eligible_version: None,
            ai_note: None,
            steps: Vec::new(),
        }
    }

    #[test]
    fn summarize_counts_each_decision() {
        let outcomes = vec![
            outcome_with(Decision::Allow),
            outcome_with(Decision::Allow),
            outcome_with(Decision::Delayed(3)),
            outcome_with(Decision::Blocked("nope".into())),
        ];
        assert_eq!(
            summarize(&outcomes),
            Summary {
                allowed: 2,
                delayed: 1,
                blocked: 1,
            }
        );
    }

    #[test]
    fn summarize_empty_is_zeroed() {
        assert_eq!(summarize(&[]), Summary::default());
    }
}
