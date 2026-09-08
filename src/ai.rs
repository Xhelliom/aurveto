//! AI review of the PKGBUILD diff via a configurable provider
//! (Groq / OpenAI / a local llama.cpp server: chat-completions format;
//! Anthropic: messages format). We ask the model for a structured JSON verdict.

use crate::config::{AiConfig, Provider};
use crate::t;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Zero temperature: we want the most deterministic verdict possible.
const TEMPERATURE: f32 = 0.0;
/// Token ceiling for the response (the JSON verdict is short).
const MAX_TOKENS: u32 = 512;
/// Anthropic Messages API version.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Binary of the local runtime, looked up in the PATH.
pub const LOCAL_RUNTIME_BIN: &str = "llama-server";
/// Package providing `llama-server`: `llama-cpp` in the Arch official repos
/// (note the dash — the upstream project is spelled `llama.cpp`).
pub const LOCAL_RUNTIME_PACKAGE: &str = "llama-cpp";
/// Path suffix of the chat endpoint, swapped for `/models` when probing.
const CHAT_PATH: &str = "/chat/completions";
/// Listing route every OpenAI-compatible runtime exposes; needs no body.
const MODELS_PATH: &str = "/models";
/// A probe must not stall the UI: the server is on localhost or nowhere.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
/// How much of a provider's error body is kept: enough for its message, not so
/// much that a stack of HTML floods the frontends' chain step.
const MAX_ERROR_BODY: usize = 300;

#[derive(Debug, Clone, Deserialize)]
pub struct Verdict {
    /// true if the diff looks safe.
    pub safe: bool,
    /// low / medium / high / critical
    pub severity: String,
    /// Short explanation.
    pub summary: String,
}

const SYSTEM_PROMPT: &str =
    "You are a security auditor specialised in Arch Linux PKGBUILDs and the AUR. \
You are given the diff (or the contents) of a PKGBUILD and its scripts. Your role is to \
detect a supply-chain COMPROMISE, not to critique packaging style. \
\
NORMAL and NOT suspicious in itself (do NOT flag): version number bump (pkgver, \
pkgrel), checksum updates (sha256sums/sha512sums/b2sums) accompanying a new version, \
extracting a .deb/.tar, using sed/ln/install/desktop-file to place files, symlinks to \
/usr/bin, downloading from the vendor's usual official domain already present in the \
previous version. \
\
TRULY suspicious (flag, safe=false): a new source pointing to an unusual domain different \
from the vendor, addition of a curl|bash or wget|sh, execution of a downloaded binary/ELF, \
a new pre/post install hook running remote code, obfuscated or encoded code \
(base64/eval/xxd), exfiltration (sending files, env variables, keys) over the network, \
unexpected addition of npm/pip dependencies installed at build time with lifecycle hooks. \
\
Rely only on what the diff shows. Reply ONLY with a JSON object, with no surrounding text: \
{\"safe\": bool, \"severity\": \"low|medium|high|critical\", \"summary\": \"...\"}. \
Set safe=false only if there is a real indicator from the \"truly suspicious\" list.";

/// AI review of a diff with multi-vote confirmation.
///
/// Two vote policies, because the two kinds of provider fail differently:
///
/// - **Cloud** — every call is billed, so a "safe" 1st verdict stops there.
///   Only a block is put to the vote, and a strict majority confirms it; this
///   removes the false positives caused by the model's non-determinism.
/// - **Local** — inference is free, so "safe" verdicts are voted on too and
///   **unanimity is required** to allow the update. A smaller local model's
///   false negative (missing a real compromise) is the failure mode that
///   matters here, and the majority rule would let it through on the 1st call.
///   Fail-closed: one dissenting vote blocks.
pub fn review_diff(cfg: &AiConfig, pkg: &str, diff: &str) -> Result<Verdict> {
    let votes = cfg.confirm_votes.max(1);
    let first = review_once(cfg, pkg, diff)?;
    let is_local = cfg.provider.is_local();

    // Nothing to confirm: multi-vote disabled, or a cloud "safe" verdict.
    if !needs_confirmation(is_local, first.safe, votes) {
        return Ok(first);
    }

    let mut total = 1u32;
    let mut unsafe_count = u32::from(!first.safe);
    let mut last_unsafe = if first.safe {
        None
    } else {
        Some(first.clone())
    };
    for _ in 1..votes {
        match review_once(cfg, pkg, diff) {
            Ok(v) => {
                total += 1;
                if !v.safe {
                    unsafe_count += 1;
                    last_unsafe = Some(v);
                }
            }
            // A failed vote does not count but does not abort the procedure.
            Err(e) => eprintln!("  (AI vote failed for {pkg}: {e})"),
        }
    }

    match (blocked_by_votes(is_local, unsafe_count, total), last_unsafe) {
        (true, Some(mut v)) => {
            v.summary = if is_local {
                t!(
                    "{} — blocked, {}/{} local votes flagged it (unanimity required)",
                    v.summary,
                    unsafe_count,
                    total
                )
            } else {
                t!(
                    "{} — block confirmed by {}/{} votes",
                    v.summary,
                    unsafe_count,
                    total
                )
            };
            Ok(v)
        }
        // Every local vote agreed: keep the 1st verdict as it stands.
        (_, None) => Ok(first),
        // The vote overturned the initial block.
        (false, Some(_)) => Ok(Verdict {
            safe: true,
            severity: "low".to_string(),
            summary: t!(
                "initial block NOT confirmed ({}/{} suspicious votes) — allowed",
                unsafe_count,
                total
            ),
        }),
    }
}

/// Why the local AI review cannot run right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalStatus {
    /// Not applicable (cloud provider or AI review disabled), or the server answers.
    Ready,
    /// `llama-server` is nowhere in the PATH.
    NotInstalled,
    /// The binary exists but nothing answers on the configured endpoint.
    NotRunning,
}

/// Checks that a local review can actually happen. Like a missing `aur-scan`
/// binary, an unreachable local server would otherwise only show up as a failed
/// review buried in each decision chain.
pub fn local_status(cfg: &AiConfig) -> LocalStatus {
    if !cfg.enabled || !cfg.provider.is_local() {
        return LocalStatus::Ready;
    }
    if probe_local(cfg) {
        return LocalStatus::Ready;
    }
    if std::process::Command::new(LOCAL_RUNTIME_BIN)
        .arg("--version")
        .output()
        .is_ok()
    {
        LocalStatus::NotRunning
    } else {
        LocalStatus::NotInstalled
    }
}

/// GET on the runtime's `/models` route, next to the configured chat endpoint.
fn probe_local(cfg: &AiConfig) -> bool {
    let endpoint = cfg.endpoint();
    let url = match endpoint.strip_suffix(CHAT_PATH) {
        Some(base) => format!("{base}{MODELS_PATH}"),
        None => endpoint,
    };
    let mut req = ureq::get(&url).timeout(PROBE_TIMEOUT);
    // A server started with --api-key answers 401 without this.
    if let Some(key) = crate::config::resolve_api_key(cfg).filter(|k| !k.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    req.call().is_ok()
}

/// Does the 1st verdict have to be put to the vote? A cloud "safe" verdict is
/// taken at face value (each extra call is billed); a local one is not, since
/// local inference costs nothing and a weaker model's false negative is what we
/// are guarding against.
fn needs_confirmation(is_local: bool, first_safe: bool, votes: u32) -> bool {
    votes > 1 && (!first_safe || is_local)
}

/// Final decision once the votes are in. Local: fail-closed, one dissenting
/// vote is enough to block. Cloud: a strict majority confirms the block.
fn blocked_by_votes(is_local: bool, unsafe_count: u32, total: u32) -> bool {
    if is_local {
        unsafe_count > 0
    } else {
        unsafe_count * 2 > total
    }
}

/// A single call to the model, returning a Verdict.
fn review_once(cfg: &AiConfig, pkg: &str, diff: &str) -> Result<Verdict> {
    // A local runtime needs no account; a key is only used if the server was
    // started with one.
    let api_key = crate::config::resolve_api_key(cfg);
    if api_key.is_none() && cfg.provider.needs_api_key() {
        return Err(anyhow!(
            "{:?} API key not found (neither in ${} nor in secrets.toml)",
            cfg.provider,
            cfg.key_env_or_default()
        ));
    }
    let model = cfg.model_or_default();

    let user_msg = format!(
        "Package: {pkg}\nAnalyse this PKGBUILD diff and return your JSON verdict:\n\n{diff}"
    );

    let endpoint = cfg.endpoint();
    let raw = match cfg.provider {
        Provider::Anthropic => {
            call_anthropic(&endpoint, &api_key.unwrap_or_default(), &model, &user_msg)?
        }
        Provider::Groq | Provider::Openai | Provider::Local => {
            call_openai_compatible(&endpoint, api_key.as_deref(), &model, &user_msg)?
        }
    };

    parse_verdict(&raw).with_context(|| format!("unusable AI response: {raw}"))
}

/// Chat-completions format (Groq, OpenAI and llama.cpp share the same schema).
/// `api_key` is optional: a local server usually runs without one.
fn call_openai_compatible(
    endpoint: &str,
    api_key: Option<&str>,
    model: &str,
    user_msg: &str,
) -> Result<String> {
    let body = serde_json::json!({
        "model": model,
        "temperature": TEMPERATURE,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_msg}
        ]
    });
    let mut req = ureq::post(endpoint).set("Content-Type", "application/json");
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let resp: serde_json::Value = req
        .send_json(body)
        .map_err(|e| http_error("chat-completions API call", e))?
        .into_json()
        .context("parsing chat-completions response")?;
    resp["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing content field in the response"))
}

/// Anthropic Messages format.
fn call_anthropic(endpoint: &str, api_key: &str, model: &str, user_msg: &str) -> Result<String> {
    let body = serde_json::json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "temperature": TEMPERATURE,
        "system": SYSTEM_PROMPT,
        "messages": [
            {"role": "user", "content": user_msg}
        ]
    });
    let resp: serde_json::Value = ureq::post(endpoint)
        .set("x-api-key", api_key)
        .set("anthropic-version", ANTHROPIC_VERSION)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| http_error("Anthropic API call", e))?
        .into_json()
        .context("parsing Anthropic response")?;
    resp["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing text field in the Anthropic response"))
}

/// Turns a provider failure into an error carrying the server's own words. A
/// bare status ("status 429") never says whether the key expired, the credits
/// ran out or the model was decommissioned — which is exactly what the user
/// needs to read in the frontends when the review stops happening.
fn http_error(what: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let body: String = body.trim().chars().take(MAX_ERROR_BODY).collect();
            anyhow!("{what}: HTTP {code} — {body}")
        }
        e => anyhow!("{what}: {e}"),
    }
}

/// Extracts the first valid JSON object from the text returned by the model.
fn parse_verdict(raw: &str) -> Result<Verdict> {
    let start = raw.find('{').ok_or_else(|| anyhow!("no JSON"))?;
    let end = raw.rfind('}').ok_or_else(|| anyhow!("unterminated JSON"))?;
    let json = &raw[start..=end];
    let v: Verdict = serde_json::from_str(json)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_trusts_a_safe_verdict_but_local_does_not() {
        // A billed provider stops at the 1st "safe" call; the local one votes.
        assert!(!needs_confirmation(false, true, 3));
        assert!(needs_confirmation(true, true, 3));
        // A block is always put to the vote, wherever it came from.
        assert!(needs_confirmation(false, false, 3));
        assert!(needs_confirmation(true, false, 3));
        // Multi-vote disabled: a single call, both ways.
        assert!(!needs_confirmation(true, true, 1));
        assert!(!needs_confirmation(false, false, 1));
    }

    #[test]
    fn local_blocks_on_any_dissent_cloud_needs_a_majority() {
        // Local: unanimity required to allow.
        assert!(blocked_by_votes(true, 1, 3));
        assert!(!blocked_by_votes(true, 0, 3));
        // Cloud: 1/3 is overturned, 2/3 confirms.
        assert!(!blocked_by_votes(false, 1, 3));
        assert!(blocked_by_votes(false, 2, 3));
        // A tie is not a majority: the initial block does not survive it.
        assert!(!blocked_by_votes(false, 1, 2));
    }

    #[test]
    fn verdict_is_extracted_from_a_chatty_answer() {
        // Small local models wrap the JSON in prose or a markdown fence.
        let raw = "Here is my verdict:\n```json\n{\"safe\": false, \
\"severity\": \"critical\", \"summary\": \"curl|bash\"}\n```";
        let v = parse_verdict(raw).unwrap();
        assert!(!v.safe);
        assert_eq!(v.severity, "critical");
    }
}
