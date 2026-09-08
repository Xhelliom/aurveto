//! Integration with `aur-scan` (ks-aur-scanner) for static analysis.
//! We delegate to the external tool when present; otherwise we return "skipped".

use std::path::Path;
use std::process::Command;

/// Minimum severity that makes `aur-scan` exit non-zero. Below it, the AUR is
/// full of benign findings (SKIP checksums, packaging style) that would block
/// every package; at or above it, the detection is worth stopping for.
const FAIL_ON_SEVERITY: &str = "high";
/// Severity tag `aur-scan` prefixes each finding with. Every other line of its
/// transcript (banner, dependency tree, totals) is noise for the frontends.
const SEVERITY_TAGS: [&str; 4] = ["[CRITICAL]", "[HIGH]", "[MEDIUM]", "[LOW]"];
/// Bullet `aur-scan check` puts in front of a per-package finding.
const FINDING_BULLET: &str = "· ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanResult {
    /// aur-scan missing or disabled.
    Skipped,
    /// No critical alert.
    Clean,
    /// At least one blocking detection, with the raw detail.
    Flagged(String),
}

/// Is `aur-scan` available in the PATH?
pub fn available() -> bool {
    Command::new("aur-scan")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Scans an AUR package before installation: `aur-scan check <name>`.
/// Convention: a non-zero exit code => blocking detection. `--no-confirm` is
/// mandatory: without it `check` prompts, aborts for want of a tty, and its
/// exit code then reports a missing terminal rather than a detection.
pub fn scan_package(name: &str, enabled: bool) -> ScanResult {
    if !enabled || !available() {
        return ScanResult::Skipped;
    }
    interpret(
        Command::new("aur-scan")
            .args(["check", "--no-confirm", "--fail-on", FAIL_ON_SEVERITY, name])
            .output(),
    )
}

/// Scans a local PKGBUILD file: `aur-scan scan <path>`.
pub fn scan_pkgbuild_file(path: &Path, enabled: bool) -> ScanResult {
    if !enabled || !available() {
        return ScanResult::Skipped;
    }
    let path = match path.to_str() {
        Some(p) => p,
        None => return ScanResult::Skipped,
    };
    interpret(
        Command::new("aur-scan")
            .args(["scan", "--fail-on", FAIL_ON_SEVERITY, path])
            .output(),
    )
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
}
