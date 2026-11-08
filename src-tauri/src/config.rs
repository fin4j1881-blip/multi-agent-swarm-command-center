//! Configuration types shared between the Tauri IPC layer and the agent runtime.
//!
//! Everything the UI sends is camelCase JSON; enums use snake_case strings so the
//! frontend can simply use the values of a `<select>` element.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_max_attempts() -> u32 {
    3
}
fn default_max_tasks() -> u32 {
    6
}
fn default_max_rounds() -> u32 {
    3
}
fn default_temperature() -> f32 {
    0.2
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_true() -> bool {
    true
}
fn default_context_chars() -> usize {
    24_000
}
fn default_timeout_secs() -> u64 {
    600
}
fn default_tree_entries() -> usize {
    500
}

/// Where a model call is routed to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSource {
    /// Direct cloud HTTP API.
    #[default]
    Api,
    /// Local IDE / CLI session bridge.
    Ide,
}

/// Supported cloud API dialects.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    #[default]
    Openrouter,
    Anthropic,
    Openai,
    Deepseek,
    /// Any OpenAI-compatible endpoint (Ollama, LM Studio, vLLM, gateway...).
    Custom,
}

impl ApiKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openrouter => "OpenRouter",
            Self::Anthropic => "Anthropic",
            Self::Openai => "OpenAI",
            Self::Deepseek => "DeepSeek",
            Self::Custom => "Custom OpenAI-compatible",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Openrouter => "https://openrouter.ai/api/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Openai => "https://api.openai.com/v1",
            Self::Deepseek => "https://api.deepseek.com/v1",
            Self::Custom => "",
        }
    }

    pub fn env_key(self) -> &'static str {
        match self {
            Self::Openrouter => "OPENROUTER_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Openai => "OPENAI_API_KEY",
            Self::Deepseek => "DEEPSEEK_API_KEY",
            Self::Custom => "LLM_API_KEY",
        }
    }
}

/// Supported local IDE / CLI bridges.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IdeKind {
    /// `opencode run --model ... "prompt"`
    #[default]
    Opencode,
    /// `claude -p --model ...` with the prompt on stdin
    ClaudeCode,
    /// `cursor-agent -p --model ... "prompt"`
    CursorAgent,
    /// Any HTTP bridge listening on localhost that speaks the generic contract.
    CustomHttp,
    /// Any CLI program that accepts a prompt.
    CustomCli,
}

impl IdeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opencode => "OpenCode CLI",
            Self::ClaudeCode => "Claude Code CLI",
            Self::CursorAgent => "Cursor Agent CLI",
            Self::CustomHttp => "Custom HTTP bridge",
            Self::CustomCli => "Custom CLI",
        }
    }

    /// (program, fixed args) for the built-in CLI bridges.
    pub fn default_cli(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::Opencode => Some(("opencode", &["run"])),
            Self::ClaudeCode => Some(("claude", &["-p"])),
            Self::CursorAgent => Some(("cursor-agent", &["-p"])),
            _ => None,
        }
    }

    /// CLI bridges that read the prompt from stdin instead of argv.
    pub fn prompt_via_stdin(self) -> bool {
        matches!(self, Self::ClaudeCode)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default)]
    pub source: ProviderSource,
    #[serde(default)]
    pub api_kind: ApiKind,
    #[serde(default)]
    pub ide_kind: IdeKind,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub ide_endpoint: Option<String>,
    #[serde(default)]
    pub ide_command: Option<String>,
    #[serde(default)]
    pub ide_args: Vec<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl std::fmt::Debug for ProviderConfig {
    /// Hand-written so the API key can never end up in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("source", &self.source)
            .field("api_kind", &self.api_kind)
            .field("ide_kind", &self.ide_kind)
            .field("model", &self.model)
            .field("fallback_models", &self.fallback_models)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("ide_endpoint", &self.ide_endpoint)
            .field("ide_command", &self.ide_command)
            .field("ide_args", &self.ide_args)
            .finish_non_exhaustive()
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            source: ProviderSource::Api,
            api_kind: ApiKind::Openrouter,
            ide_kind: IdeKind::Opencode,
            model: String::new(),
            fallback_models: Vec::new(),
            api_key: None,
            base_url: None,
            ide_endpoint: None,
            ide_command: None,
            ide_args: Vec::new(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

impl ProviderConfig {
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(key) = self.explicit_api_key() {
            return Some(key);
        }
        std::env::var(self.api_kind.env_key())
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// The key typed into the UI, without any environment fallback. Local
    /// bridges must only ever receive a key the user explicitly gave them, never
    /// e.g. `OPENROUTER_API_KEY` picked up from the environment.
    pub fn explicit_api_key(&self) -> Option<String> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_string)
    }

    pub fn is_custom_endpoint(&self) -> bool {
        self.base_url
            .as_deref()
            .map(|b| !b.trim().is_empty())
            .unwrap_or(false)
    }

    /// All configured models in order: primary model followed by unique fallback models.
    pub fn all_models(&self) -> Vec<String> {
        let mut models = Vec::new();
        let primary = self.model.trim();
        if !primary.is_empty() {
            models.push(primary.to_string());
        }
        for fb in &self.fallback_models {
            let fb = fb.trim();
            if !fb.is_empty() && !models.iter().any(|m| m == fb) {
                models.push(fb.to_string());
            }
        }
        models
    }

    pub fn validate(&self, role: &str) -> Result<(), String> {
        match self.source {
            ProviderSource::Api => {
                if self.model.trim().is_empty() {
                    return Err(format!("{role}: API modunda model adı zorunlu."));
                }
                if self.api_kind != ApiKind::Custom
                    && !self.is_custom_endpoint()
                    && self.resolve_api_key().is_none()
                {
                    return Err(format!(
                        "{role}: {} için API anahtarı gerekli (veya özel base URL gir).",
                        self.api_kind.as_str()
                    ));
                }
                if self.api_kind == ApiKind::Custom && !self.is_custom_endpoint() {
                    return Err(format!(
                        "{role}: Custom API için base URL zorunlu (örn. http://127.0.0.1:11434/v1)."
                    ));
                }
            }
            ProviderSource::Ide => match self.ide_kind {
                IdeKind::CustomHttp => {
                    let endpoint = self.ide_endpoint.as_deref().unwrap_or("").trim();
                    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
                        return Err(format!(
                            "{role}: HTTP köprüsü için http(s):// ile başlayan endpoint gerekli."
                        ));
                    }
                }
                IdeKind::CustomCli
                    if self
                        .ide_command
                        .as_deref()
                        .map_or(true, |c| c.trim().is_empty()) =>
                {
                    return Err(format!("{role}: Custom CLI için komut adı gerekli."));
                }
                // Built-in CLI bridges need nothing else; a given endpoint makes
                // them behave as an HTTP bridge.
                _ => {}
            },
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    pub project_path: String,
    pub vision: String,
    #[serde(default)]
    pub instructions: String,
    pub architect: ProviderConfig,
    pub coder: ProviderConfig,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_max_tasks")]
    pub max_tasks: u32,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    #[serde(default = "default_context_chars")]
    pub context_chars: usize,
    #[serde(default = "default_tree_entries")]
    pub tree_entries: usize,
    #[serde(default = "default_true")]
    pub auto_apply: bool,
    #[serde(default)]
    pub dry_run: bool,
    /// Run `cargo check` after each write and revert the file on failure.
    #[serde(default)]
    pub verify_build: bool,
}

impl AgentConfig {
    pub fn root(&self) -> Result<PathBuf, String> {
        let raw = self.project_path.trim();
        if raw.is_empty() {
            return Err("Proje klasörü seçilmedi.".into());
        }
        let path = Path::new(raw);
        if !path.is_dir() {
            return Err(format!("Proje klasörü bulunamadı: {raw}"));
        }
        path.canonicalize()
            .map_err(|e| format!("Proje klasörü açılamadı: {e}"))
    }

    pub fn validate(&self) -> Result<PathBuf, String> {
        let root = self.root()?;
        if self.vision.trim().chars().count() < 20 {
            return Err("Proje vizyonu en az 20 karakter olmalı.".into());
        }
        self.architect.validate("Komutan")?;
        self.coder.validate("İşçi")?;
        if self.max_attempts == 0 {
            return Err("maxAttempts en az 1 olmalı.".into());
        }
        if self.max_rounds == 0 || self.max_tasks == 0 {
            return Err("maxRounds ve maxTasks en az 1 olmalı.".into());
        }
        if self.context_chars < 1_000 {
            return Err("contextChars en az 1000 olmalı.".into());
        }
        Ok(root)
    }
}
