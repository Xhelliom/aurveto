//! Integration with `aur-scan` (ks-aur-scanner) for static analysis.
//! We delegate to the external tool when present; otherwise we return "skipped".
//! A revision is judged against the installed one: only new findings block.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Minimum severity that makes `aur-scan` exit non-zero. Below it, the AUR is
/// full of benign findings (SKIP checksums, packaging style) that would block
/// every package; at or above it, the detection is worth stopping for.
const FAIL_ON_SEVERITY: &str = "high";
/// Severity tag `aur-scan` prefixes each finding with. Every other line of its
/// transcript (banner, dependency tree, totals) is noise for the frontends.
const SEVERITY_TAGS: [&str; 4] = ["[CRITICAL]", "[HIGH]", "[MEDIUM]", "[LOW]"];
/// The tags among `SEVERITY_TAGS` at or above `FAIL_ON_SEVERITY`.
const BLOCKING_TAGS: [&str; 2] = ["[CRITICAL]", "[HIGH]"];
/// Bullet `aur-scan check` puts in front of a per-package finding.
const FINDING_BULLET: &str = "· ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanResult {
    /// aur-scan missing or disabled.
    Skipped,
    /// No blocking alert.
    Clean,
    /// Blocking alerts, but every one was already present in the installed
    /// revision — nothing new to stop for. Carries the findings, for display.
    Known(String),
    /// At least one blocking detection, with the raw detail.
    Flagged(String),
}

impl ScanResult {
    /// Did the scanner actually inspect the revision (whatever it found)?
    pub fn inspected(&self) -> bool {
        !matches!(self, ScanResult::Skipped)
    }
}

/// Is `aur-scan` available in the PATH?
pub fn available() -> bool {
    Command::new("aur-scan")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Severities (as `aur-scan --format json` spells them) at which a finding
/// blocks — the same threshold as `FAIL_ON_SEVERITY`.
const BLOCKING_SEVERITIES: [&str; 2] = ["critical", "high"];

/// One `aur-scan` finding, as reported by `--format json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Finding {
    pub id: String,
    pub severity: String,
    pub title: String,
    pub description: String,
    pub location: Location,
}

/// Where a finding matched. `snippet` is absent for whole-file checks
/// (checksum counts, metadata).
#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    pub file: String,
    pub snippet: Option<String>,
}

#[derive(Deserialize)]
struct Report {
    findings: Vec<Finding>,
}

impl Finding {
    /// Line-independent identity: the rule, the file and the matched code (or,
    /// for a whole-file check, the description that carries its specifics).
    /// Line numbers shift with every unrelated edit; the code does not.
    fn fingerprint(&self) -> String {
        let evidence = self
            .location
            .snippet
            .as_deref()
            .unwrap_or(&self.description);
        format!("{}\0{}\0{}", self.id, self.location.file, evidence.trim())
    }

    fn blocks(&self) -> bool {
        BLOCKING_SEVERITIES.contains(&self.severity.as_str())
    }

    fn summary(&self) -> String {
        format!(
            "[{}] {} {} ({})",
            self.severity.to_uppercase(),
            self.id,
            self.title,
            self.location.file
        )
    }
}

/// Scans the exported tree of the revision about to be installed and blocks
/// only on the blocking findings that are **new** relative to the installed
/// revision(s) in `baselines`.
///
/// Why: `aur-scan` is a context-free pattern matcher, and most of its hits on
/// mainstream packages are stable packaging idioms (a cron dir being removed,
/// a `profile.d` snippet) that the user already built and ran. An injected
/// payload, by contrast, is necessarily new code. Fail-closed: an empty
/// `baselines` (first install, VCS version, history unavailable) or a baseline
/// the scanner could not read suppresses nothing.
pub fn scan_revision(target: &Path, baselines: &[&Path], enabled: bool) -> ScanResult {
    if !enabled || !available() {
        return ScanResult::Skipped;
    }
    let found = match scan_tree(target) {
        Ok(f) => f,
        Err(ScanError::NotRun) => return ScanResult::Skipped,
        Err(ScanError::Unreadable(raw)) => return ScanResult::Flagged(findings(&raw)),
    };
    let known: Vec<Vec<Finding>> = baselines
        .iter()
        .map(|b| scan_tree(b).unwrap_or_default())
        .collect();
    let fresh = new_findings(&found, &known);
    let summarize = |list: &[&Finding]| {
        list.iter()
            .map(|f| f.summary())
            .collect::<Vec<_>>()
            .join("\n")
    };
    if !fresh.is_empty() {
        return ScanResult::Flagged(summarize(&fresh));
    }
    if found.is_empty() {
        return ScanResult::Clean;
    }
    ScanResult::Known(summarize(&found.iter().collect::<Vec<_>>()))
}

enum ScanError {
    /// `aur-scan` could not be spawned: no verdict at all.
    NotRun,
    /// It ran but its report could not be parsed: the raw output.
    Unreadable(String),
}

/// The blocking findings of `aur-scan scan --format json <dir>`.
fn scan_tree(dir: &Path) -> Result<Vec<Finding>, ScanError> {
    let out = Command::new("aur-scan")
        .args(["scan", "--format", "json"])
        .arg(dir)
        .output()
        .map_err(|_| ScanError::NotRun)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: Report = serde_json::from_str(json_part(&stdout)).map_err(|_| {
        let mut raw = stdout.to_string();
        raw.push_str(&String::from_utf8_lossy(&out.stderr));
        ScanError::Unreadable(raw)
    })?;
    // Report paths relative to the scanned tree: each revision is exported to
    // its own directory, and the fingerprints must match across them.
    let prefix = format!("{}/", dir.display());
    Ok(report
        .findings
        .into_iter()
        .filter(Finding::blocks)
        .map(|mut f| {
            if let Some(rel) = f.location.file.strip_prefix(&prefix) {
                f.location.file = rel.to_string();
            }
            f
        })
        .collect())
}

/// The JSON document of an `aur-scan --format json` transcript: the scanner
/// may print warnings (e.g. a missing `install=` file) on stdout before it.
fn json_part(stdout: &str) -> &str {
    stdout
        .find("\n{")
        .map(|i| &stdout[i + 1..])
        .unwrap_or(stdout)
}

/// The findings of `target` not accounted for by the baselines. A finding is
/// known only if **every** baseline carries it (several baselines = several
/// commits share the installed version, and we cannot tell which one was
/// built), and each known occurrence absorbs one target occurrence — a
/// payload that duplicates an existing line is still new.
fn new_findings<'a>(target: &'a [Finding], baselines: &[Vec<Finding>]) -> Vec<&'a Finding> {
    let counts = |list: &[Finding]| {
        let mut m: HashMap<String, usize> = HashMap::new();
        for f in list {
            *m.entry(f.fingerprint()).or_default() += 1;
        }
        m
    };
    let per_baseline: Vec<HashMap<String, usize>> = baselines.iter().map(|b| counts(b)).collect();
    let mut allowance: HashMap<String, usize> = HashMap::new();
    if let Some((first, rest)) = per_baseline.split_first() {
        for (fp, n) in first {
            let n = rest
                .iter()
                .map(|m| m.get(fp).copied().unwrap_or(0))
                .fold(*n, usize::min);
            allowance.insert(fp.clone(), n);
        }
    }
    target
        .iter()
        .filter(|f| match allowance.get_mut(&f.fingerprint()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// Scans the AUR **dependencies** of `name` (`aur-scan check`, which resolves
/// the tree from the AUR). The package's own lines are dropped: its revision
/// is judged by `scan_revision`, against the installed baseline. A dependency
/// has no baseline, so any blocking hit on it blocks.
pub fn scan_dependencies(name: &str, enabled: bool) -> ScanResult {
    if !enabled || !available() {
        return ScanResult::Skipped;
    }
    let result = interpret(
        Command::new("aur-scan")
            .args(["check", "--no-confirm", "--fail-on", FAIL_ON_SEVERITY, name])
            .output(),
    );
    match result {
        ScanResult::Flagged(detail) => dependency_findings(name, &detail),
        other => other,
    }
}

/// Keeps the blocking lines of a `check` transcript that concern a package
/// other than `name`.
fn dependency_findings(name: &str, detail: &str) -> ScanResult {
    // A failure with no parseable finding is a scanner error: keep it whole.
    if !detail
        .lines()
        .any(|l| SEVERITY_TAGS.iter().any(|t| l.contains(t)))
    {
        return ScanResult::Flagged(detail.to_string());
    }
    let own = format!("{name} ");
    let deps: Vec<&str> = detail
        .lines()
        .filter(|l| !l.starts_with(&own))
        .filter(|l| BLOCKING_TAGS.iter().any(|t| l.contains(t)))
        .collect();
    if deps.is_empty() {
        ScanResult::Clean
    } else {
        ScanResult::Flagged(deps.join("\n"))
    }
}

/// Combines two scans of the same update: a detection wins, then a scan that
/// could not run (no verdict = doubt), and only two clean results are clean.
pub fn merge(a: ScanResult, b: ScanResult) -> ScanResult {
    use ScanResult::*;
    match (a, b) {
        (Flagged(x), Flagged(y)) => Flagged(format!("{x}\n{y}")),
        (Flagged(x), _) | (_, Flagged(x)) => Flagged(x),
        (Skipped, _) | (_, Skipped) => Skipped,
        (Known(x), _) | (_, Known(x)) => Known(x),
        (Clean, Clean) => Clean,
    }
}

fn interpret(output: std::io::Result<std::process::Output>) -> ScanResult {
    match output {
        Ok(out) => {
            if out.status.success() {
                ScanResult::Clean
            } else {
                let mut detail = String::from_utf8_lossy(&out.stdout).to_string();
                detail.push_str(&String::from_utf8_lossy(&out.stderr));
                ScanResult::Flagged(findings(&detail))
            }
        }
        Err(_) => ScanResult::Skipped,
    }
}

/// Keeps only the finding lines of an `aur-scan` transcript, so a frontend
/// shows `pkg [HIGH] Checksum count mismatch` instead of the whole report.
/// Falls back to the raw text when nothing matches: a scanner error message is
/// more useful than an empty reason.
fn findings(raw: &str) -> String {
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| SEVERITY_TAGS.iter().any(|tag| l.contains(tag)))
        .map(|l| l.strip_prefix(FINDING_BULLET).unwrap_or(l))
        .collect();
    if lines.is_empty() {
        raw.trim().to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_keep_only_the_detections() {
        let raw = "\nAUR Security Scanner | Pre-Install Check\n====================\n\nResolving dependency tree...\n  1 AUR package(s) to scan, 7 repo/virtual dependencies\n\nScanning: cursor-bin (aur) 0C/1H\n    · cursor-bin [HIGH] Checksum count mismatch\n\nDependency tree (review before installing):\n[AUR] cursor-bin !! 0C/1H\n[repo] nodejs\n\n====================\nTree totals: 1 HIGH \n";
        assert_eq!(findings(raw), "cursor-bin [HIGH] Checksum count mismatch");
    }

    #[test]
    fn findings_fall_back_to_the_raw_text() {
        // A scanner crash carries no severity tag, but its message is the reason.
        assert_eq!(
            findings("  error: AUR unreachable\n"),
            "error: AUR unreachable"
        );
    }

    fn finding(id: &str, snippet: Option<&str>) -> Finding {
        Finding {
            id: id.into(),
            severity: "high".into(),
            title: "t".into(),
            description: "d".into(),
            location: Location {
                file: "./PKGBUILD".into(),
                snippet: snippet.map(Into::into),
            },
        }
    }

    #[test]
    fn a_finding_already_installed_is_not_new() {
        let target = [finding("PERSIST-003", Some("rm -r cron"))];
        let installed = vec![vec![finding("PERSIST-003", Some("rm -r cron"))]];
        assert!(new_findings(&target, &installed).is_empty());
    }

    #[test]
    fn without_baseline_everything_is_new() {
        let target = [finding("SRC-004", Some("url"))];
        assert_eq!(new_findings(&target, &[]).len(), 1);
    }

    #[test]
    fn changed_code_under_a_known_rule_is_new() {
        let target = [finding("PRIV-001", Some("sudo rm -rf /"))];
        let installed = vec![vec![finding("PRIV-001", Some("'sudo'"))]];
        assert_eq!(new_findings(&target, &installed).len(), 1);
    }

    #[test]
    fn a_duplicated_known_line_is_new() {
        let line = || finding("INSTALL-002", Some("/opt/x/run"));
        let target = [line(), line()];
        let installed = vec![vec![line()]];
        assert_eq!(new_findings(&target, &installed).len(), 1);
    }

    #[test]
    fn known_means_known_by_every_candidate_revision() {
        let target = [finding("ENV-003", Some("profile.d"))];
        let installed = vec![vec![finding("ENV-003", Some("profile.d"))], vec![]];
        assert_eq!(new_findings(&target, &installed).len(), 1);
    }

    #[test]
    fn dependency_findings_drop_the_package_itself() {
        let detail =
            "app [HIGH] Checksum count mismatch\ndep [CRITICAL] Sudo usage\ndep [MEDIUM] Style";
        assert_eq!(
            dependency_findings("app", detail),
            ScanResult::Flagged("dep [CRITICAL] Sudo usage".into())
        );
        assert_eq!(
            dependency_findings("app", "app [HIGH] x"),
            ScanResult::Clean
        );
    }

    #[test]
    fn json_part_skips_leading_warnings() {
        assert_eq!(json_part("WARN missing\n{\"a\":1}"), "{\"a\":1}");
        assert_eq!(json_part("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn merge_prefers_detection_then_doubt() {
        use ScanResult::*;
        assert_eq!(merge(Clean, Flagged("x".into())), Flagged("x".into()));
        assert_eq!(merge(Known("k".into()), Skipped), Skipped);
        assert_eq!(merge(Clean, Known("k".into())), Known("k".into()));
    }
}
