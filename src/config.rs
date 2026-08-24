//! aurveto configuration: delay, whitelist and AI review settings.
//! The file lives at ~/.config/aurveto/config.toml and is created with
//! default values on first launch.

use crate::t;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Secrets file permissions (owner read/write only).
const SECRETS_MODE: u32 = 0o600;

/// AI provider for the PKGBUILD diff review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Groq,
    Anthropic,
    Openai,
}

impl Provider {
    /// HTTP endpoint of the provider's chat API.
    pub fn endpoint(&self) -> &'static str {
        match self {
            Provider::Groq => "https://api.groq.com/openai/v1/chat/completions",
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
            Provider::Openai => "https://api.openai.com/v1/chat/completions",
        }
    }

    /// Name of the environment variable holding the default API key.
    pub fn default_key_env(&self) -> &'static str {
        match self {
            Provider::Groq => "GROQ_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::Openai => "OPENAI_API_KEY",
        }
    }

    /// A reasonable default model for security analysis.
    pub fn default_model(&self) -> &'static str {
        match self {
            Provider::Groq => "openai/gpt-oss-120b",
            Provider::Anthropic => "claude-fable-5",
            Provider::Openai => "gpt-4o",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    /// Enables the AI review of the PKGBUILD diff.
    pub enabled: bool,
    /// Selected provider.
    pub provider: Provider,
    /// Model to use (empty => provider's default_model).
    #[serde(default)]
    pub model: String,
    /// Env variable holding the API key (empty => provider's default_key_env).
    #[serde(default)]
    pub api_key_env: String,
    /// Total number of votes (including the 1st call) to CONFIRM a block.
    /// A "safe" verdict on the 1st call triggers no extra vote (a single call);
    /// only blocks are confirmed by majority.
    #[serde(default = "default_confirm_votes")]
    pub confirm_votes: u32,
}

fn default_confirm_votes() -> u32 {
    3
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            enabled: true,
            provider: Provider::Groq,
            model: String::new(),
            api_key_env: String::new(),
            confirm_votes: default_confirm_votes(),
        }
    }
}

impl AiConfig {
    pub fn model_or_default(&self) -> String {
        if self.model.is_empty() {
            self.provider.default_model().to_string()
        } else {
            self.model.clone()
        }
    }

    pub fn key_env_or_default(&self) -> String {
        if self.api_key_env.is_empty() {
            self.provider.default_key_env().to_string()
        } else {
            self.api_key_env.clone()
        }
    }
}

/// Semantics of the security delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelayMode {
    /// Blocks any update whose latest version is less than `delay_days` old.
    /// Stays on the installed version (risk: frequently updated packages are
    /// never upgraded).
    Hold,
    /// Installs the revision that was the AUR git HEAD `delay_days` days ago.
    /// Updates always arrive, with a constant lag.
    Lag,
}

fn default_delay_mode() -> DelayMode {
    DelayMode::Lag
}

/// Default notification interval (hours): a trade-off between freshness of the
/// information and discretion.
fn default_notify_interval() -> u64 {
    6
}

/// Desktop notification settings (systemd `--user` timer).
///
/// The timer runs `aurveto notify`, which counts the available official and
/// AUR updates (without the AI review, hence at no cost) and calls `notify-send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyConfig {
    /// Whether the notification timer should be active.
    pub enabled: bool,
    /// How often to check, in hours.
    #[serde(default = "default_notify_interval")]
    pub interval_hours: u64,
    /// Send nothing when the system is up to date (otherwise a quiet notification).
    #[serde(default)]
    pub silent_when_up_to_date: bool,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        NotifyConfig {
            enabled: false,
            interval_hours: default_notify_interval(),
            silent_when_up_to_date: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Number of days a fresh update is delayed.
    pub delay_days: u64,
    /// Delay semantics (hold or lag).
    #[serde(default = "default_delay_mode")]
    pub delay_mode: DelayMode,
    /// AUR helper to use for listing and installing (yay/paru).
    pub helper: String,
    /// Enables calling `aur-scan` (static analysis) if installed.
    pub use_aur_scan: bool,
    /// The user's trusted packages: immediate update, delay skipped.
    /// (The AI review / static scan still apply.)
    pub whitelist: Vec<String>,
    pub ai: AiConfig,
    /// Desktop notification settings.
    #[serde(default)]
    pub notify: NotifyConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            delay_days: 14,
            delay_mode: DelayMode::Lag,
            helper: "yay".to_string(),
            use_aur_scan: true,
            whitelist: recommended_whitelist(),
            ai: AiConfig::default(),
            notify: NotifyConfig::default(),
        }
    }
}

/// Recommended whitelist: `-bin` packages that repackage a signed binary from a
/// reputable vendor. For them the delay would mostly hold back legitimate
/// security fixes; we update them quickly BUT keep the scan + AI review (since
/// the PKGBUILD remains a possible attack vector).
pub fn recommended_whitelist() -> Vec<String> {
    [
        "google-chrome",
        "google-chrome-beta",
        "microsoft-edge-stable-bin",
        "brave-bin",
        "zen-browser-bin",
        "android-studio",
        "cursor-bin",
        "visual-studio-code-bin",
        "vscodium-bin",
        "spotify",
        "discord",
        "slack-desktop",
        "zoom",
        "1password",
        "dropbox",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Config {
    /// Path of the configuration file.
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("cannot resolve ~/.config")?;
        Ok(base.join("aurveto").join("config.toml"))
    }

    /// Loads the config, creating a default file if missing.
    pub fn load_or_init() -> Result<Config> {
        let path = Self::path()?;
        if !path.exists() {
            let cfg = Config::default();
            cfg.save()?;
            eprintln!("{}", t!("Default config created: {}", path.display()));
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing TOML of {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn is_whitelisted(&self, pkg: &str) -> bool {
        self.whitelist.iter().any(|w| w == pkg)
    }
}

/// Provider API keys, stored outside `config.toml` in a file with restricted
/// permissions. **Never** versioned or logged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groq: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai: Option<String>,
}

impl Secrets {
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("cannot resolve ~/.config")?;
        Ok(base.join("aurveto").join("secrets.toml"))
    }

    /// Loads the secrets (empty struct if the file is missing).
    pub fn load() -> Secrets {
        let Ok(path) = Self::path() else {
            return Secrets::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Writes the secrets with `0600` permissions.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(SECRETS_MODE))?;
        }
        Ok(())
    }

    pub fn get(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::Groq => self.groq.as_deref(),
            Provider::Anthropic => self.anthropic.as_deref(),
            Provider::Openai => self.openai.as_deref(),
        }
    }

    pub fn set(&mut self, provider: Provider, key: Option<String>) {
        // An empty string clears the key.
        let key = key.filter(|k| !k.trim().is_empty());
        match provider {
            Provider::Groq => self.groq = key,
            Provider::Anthropic => self.anthropic = key,
            Provider::Openai => self.openai = key,
        }
    }
}

/// Resolves the API key to use: environment variable first, otherwise the
/// secrets file. Returns None if none is available.
pub fn resolve_api_key(ai: &AiConfig) -> Option<String> {
    if let Ok(key) = std::env::var(ai.key_env_or_default()) {
        if !key.is_empty() {
            return Some(key);
        }
    }
    Secrets::load().get(ai.provider).map(|s| s.to_string())
}
