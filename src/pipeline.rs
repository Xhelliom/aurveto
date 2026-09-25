//! Orchestration: for each AUR update, applies the decision chain
//! whitelist -> delay (hold or lag) -> static scan -> AI review.

use crate::ai;
use crate::aur::{self, LagTarget, PkgInfo, RevisionTree, Update};
use crate::config::{Config, DelayMode};
use crate::scan::{self, ScanResult};
use crate::t;
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

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
    let pkgbase = info
        .map(|i| i.package_base.clone())
        .unwrap_or_else(|| upd.name.clone());
    let age_days = info.map(|i| now.saturating_sub(i.last_modified) / aur::SECS_PER_DAY);

    // Trusted package: target the LATEST version, delay skipped, but the
    // scan + AI review still apply.
    if whitelisted {
        return decide_latest(cfg, upd, &pkgbase, age_days, true);
    }

    match cfg.delay_mode {
        DelayMode::Hold => {
            let fresh = info
                .map(|i| now.saturating_sub(i.last_modified) < threshold)
                .unwrap_or(false);
            if fresh {
                return delayed(upd, age_days, (eligible_at(info, threshold), None));
            }
            decide_latest(cfg, upd, &pkgbase, age_days, false)
        }
        DelayMode::Lag => evaluate_lag(cfg, upd, &pkgbase, info, age_days, now, threshold),
    }
}

/// Lag mode: targets the revision that was HEAD `threshold` seconds ago.
fn evaluate_lag(
    cfg: &Config,
    upd: Update,
    pkgbase: &str,
    info: Option<&PkgInfo>,
    age_days: Option<u64>,
    now: u64,
    threshold: u64,
) -> Outcome {
    let before = now.saturating_sub(threshold);

    let target = match aur::lagged_target(pkgbase, before) {
        // Package too young to have existed N days ago: it will mature → datable.
        Ok(None) => {
            let elig = lag_eligible(pkgbase, &upd.old_ver, threshold, info);
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
        let elig = lag_eligible(pkgbase, &upd.old_ver, threshold, info);
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
    let scan = scan_revision(cfg, &upd.name, pkgbase, &upd.old_ver, &target.commit);
    let diff = if cfg.ai.enabled {
        aur::diff_against_installed(&upd.name, &upd.old_ver, &target.pkgbuild)
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
fn decide_latest(
    cfg: &Config,
    upd: Update,
    pkgbase: &str,
    age_days: Option<u64>,
    whitelisted: bool,
) -> Outcome {
    let scan = scan_latest(cfg, &upd.name, pkgbase, &upd.old_ver);
    let diff = if cfg.ai.enabled {
        aur::pkgbuild_diff(&upd.name, &upd.old_ver).unwrap_or_default()
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
        let (step, note) = second_opinion(cfg, name, diff, detail);
        steps.push(step);
        return (Decision::Blocked(t!("aur-scan: {}", detail)), note, steps);
    }
    // A guard enabled in the config but unable to run returned no verdict: the
    // revision stays uninspected, and an uninspected revision is a doubt.
    let scan_unavailable = cfg.use_aur_scan && !scan.inspected();
    let mut ai_unavailable = false;

    if cfg.ai.enabled && !diff.trim().is_empty() {
        match ai::review_diff(&cfg.ai, name, diff, None) {
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
                eprintln!("  (AI review unavailable for {name}: {e:#})");
                steps.push(ChainStep::new(
                    t!("AI review"),
                    StepStatus::Skipped,
                    t!("review unavailable: {}", format!("{e:#}")),
                ));
                ai_unavailable = true;
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

    // Fail-closed: reaching here means nothing actually inspected this
    // revision's contents. A guard skipped by choice is the user's call; one
    // that was asked for and could not run must never read as "safe".
    if !scan.inspected() && (scan_unavailable || ai_unavailable) {
        return (
            Decision::Blocked(t!("unverified — no guard could inspect this revision")),
            None,
            steps,
        );
    }
    (Decision::Allow, None, steps)
}

/// The AI review as a **second opinion** on a static-scan block: it receives the
/// scanner's findings and says whether the PKGBUILD supports them, but it can
/// never lift the block. A pattern matcher has false positives (a substring hit
/// on a legitimate domain, a `-bin` package declaring `provides`), and the user
/// needs to read an argument before overriding by hand — while an automatic
/// override would mean a model that can be talked out of a detection, which is
/// exactly what an attacker would aim for. Fail-closed stays fail-closed.
/// Returns the chain step and, when a verdict actually came back, the
/// reviewer's own words for `Outcome::ai_note` — so `--explain` and the
/// "[AI reviewed]" tag stay truthful on a blocked package too.
fn second_opinion(
    cfg: &Config,
    name: &str,
    diff: &str,
    findings: &str,
) -> (ChainStep, Option<String>) {
    let step_name = t!("AI review");
    let skipped = |note| {
        (
            ChainStep::new(step_name.clone(), StepStatus::Skipped, note),
            None,
        )
    };
    if !cfg.ai.enabled {
        return skipped(t!("disabled"));
    }
    if diff.trim().is_empty() {
        return skipped(t!("no diff to review"));
    }
    match ai::review_diff(&cfg.ai, name, diff, Some(findings)) {
        // The model corroborates: the block is not a scanner artefact.
        Ok(v) if !v.safe => (
            ChainStep::new(
                step_name,
                StepStatus::Failed,
                t!("confirms the block — {}", v.summary),
            ),
            Some(v.summary),
        ),
        // The model disagrees: informational only, hence the neutral status.
        Ok(v) => (
            ChainStep::new(
                step_name,
                StepStatus::Skipped,
                t!("2nd opinion (does not lift the block) — {}", v.summary),
            ),
            Some(v.summary),
        ),
        Err(e) => {
            eprintln!("  (AI second opinion unavailable for {name}: {e:#})");
            skipped(t!("review unavailable: {}", format!("{e:#}")))
        }
    }
}

/// Builds the static-scan chain step from its result.
fn scan_step(cfg: &Config, scan: &ScanResult) -> ChainStep {
    let (status, note) = match scan {
        ScanResult::Clean => (StepStatus::Passed, t!("aur-scan: nothing to report")),
        ScanResult::Known(detail) => (
            StepStatus::Passed,
            t!(
                "aur-scan: nothing new since the installed version ({})",
                detail
            ),
        ),
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

/// Static scan of the exact revision `commit` of `pkgbase` — the one that will
/// be installed — judged against the installed version's revision(s).
fn scan_revision(
    cfg: &Config,
    name: &str,
    pkgbase: &str,
    installed: &str,
    commit: &str,
) -> ScanResult {
    if !cfg.use_aur_scan {
        return ScanResult::Skipped;
    }
    let target = match aur::export_revision(pkgbase, commit) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  (cannot export {name} for the scan: {e:#})");
            return ScanResult::Skipped;
        }
    };
    let baselines = installed_trees(name, pkgbase, installed);
    let paths: Vec<&Path> = baselines.iter().map(RevisionTree::path).collect();
    scan::scan_revision(target.path(), &paths, true)
}

/// Static scan for the latest version: the current AUR HEAD, plus its AUR
/// dependency tree (which has no installed baseline). Only findings absent
/// from the `installed` version block.
pub fn scan_latest(cfg: &Config, name: &str, pkgbase: &str, installed: &str) -> ScanResult {
    if !cfg.use_aur_scan {
        return ScanResult::Skipped;
    }
    match aur::head_commit(pkgbase) {
        Ok(head) => scan::merge(
            scan_revision(cfg, name, pkgbase, installed, &head),
            scan::scan_dependencies(name, true),
        ),
        Err(e) => {
            eprintln!("  (git unavailable for {name}, not scanned: {e:#})");
            ScanResult::Skipped
        }
    }
}

/// Exported trees of every revision carrying the installed version. All or
/// nothing: a partial set would shrink the intersection's constraints and so
/// widen what counts as "already installed".
fn installed_trees(name: &str, pkgbase: &str, installed: &str) -> Vec<RevisionTree> {
    aur::installed_revisions(pkgbase, installed)
        .and_then(|commits| {
            commits
                .iter()
                .map(|c| aur::export_revision(pkgbase, c))
                .collect::<Result<Vec<_>>>()
        })
        .unwrap_or_else(|e| {
            eprintln!("  (no installed baseline for {name}, every finding counts: {e:#})");
            Vec::new()
        })
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

    /// Config with the AI review off, so `vet` never reaches the network.
    fn offline_cfg(use_aur_scan: bool) -> Config {
        let mut cfg = Config {
            use_aur_scan,
            ..Config::default()
        };
        cfg.ai.enabled = false;
        cfg
    }

    #[test]
    fn vet_blocks_when_an_enabled_guard_could_not_run() {
        let (decision, _, _) = vet(&offline_cfg(true), "pkg", &ScanResult::Skipped, "");
        assert!(matches!(decision, Decision::Blocked(_)));
    }

    #[test]
    fn vet_allows_when_every_guard_is_off_by_choice() {
        let (decision, _, _) = vet(&offline_cfg(false), "pkg", &ScanResult::Skipped, "");
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn vet_allows_when_the_scan_cleared_the_revision() {
        let (decision, _, _) = vet(&offline_cfg(true), "pkg", &ScanResult::Clean, "");
        assert_eq!(decision, Decision::Allow);
    }
}
