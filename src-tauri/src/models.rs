//! Model discovery.
//!
//! The UI never asks the user to type a model name: it calls this module, which
//! asks the provider's `/models` endpoint or the local IDE/CLI for the list.
//! Endpoint URLs are derived from the selected provider kind.

use crate::config::{ApiKind, IdeKind, ProviderConfig, ProviderSource};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

const NON_CHAT: &[&str] = &[
    "embed",
    "whisper",
    "tts",
    "dall-e",
    "moderation",
    "rerank",
    "text-to-speech",
    "speech",
    "transcribe",
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub models: Vec<ModelInfo>,
    /// api | http | cli | static
    pub source: String,
    pub note: String,
}

pub async fn list_models(cfg: &ProviderConfig, project_dir: &Path) -> Result<ModelCatalog, String> {
    match cfg.source {
        ProviderSource::Api => api_catalog(cfg).await,
        ProviderSource::Ide => ide_catalog(cfg, project_dir).await,
    }
}

async fn api_catalog(cfg: &ProviderConfig) -> Result<ModelCatalog, String> {
    let base = cfg
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cfg.api_kind.default_base_url().to_string());

    if base.is_empty() {
        return Err("Custom API için base URL gerekli.".into());
    }

    let key = cfg.resolve_api_key();
    if key.is_none() && cfg.api_kind != ApiKind::Custom && !cfg.is_custom_endpoint() {
        return Err(format!(
            "Model listesi için {} API anahtarı gerekli.",
            cfg.api_kind.as_str()
        ));
    }

    let url = models_endpoint(cfg.api_kind, &base);
    let client = reqwest::Client::builder()
        .user_agent("swarm-command-center/0.1")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .build()
        .map_err(|e| format!("HTTP istemcisi oluşturulamadı: {e}"))?;

    let mut req = client.get(&url);
    match cfg.api_kind {
        ApiKind::Anthropic => {
            req = req.header("anthropic-version", "2023-06-01");
            if let Some(key) = key.as_deref() {
                req = req.header("x-api-key", key);
            }
        }
        _ => {
            if let Some(key) = key.as_deref() {
                req = req.bearer_auth(key);
            }
        }
    }

    let response = req
        .send()
        .await
        .map_err(|e| format!("Model listesi alınamadı ({url}): {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(format!(
            "Model listesi HTTP {}: {}",
            status.as_u16(),
            snippet(&text, 300)
        ));
    }

    let models = parse_models(&text)?;
    if models.is_empty() {
        return Err("Sağlayıcı boş model listesi döndürdü.".into());
    }

    Ok(ModelCatalog {
        note: format!("{} model · {url}", models.len()),
        models,
        source: "api".into(),
    })
}

async fn ide_catalog(cfg: &ProviderConfig, project_dir: &Path) -> Result<ModelCatalog, String> {
    if let Some(endpoint) = cfg
        .ide_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            let models = http_models(endpoint, cfg).await?;
            return Ok(ModelCatalog {
                note: format!("{}/models", endpoint.trim_end_matches('/')),
                models,
                source: "http".into(),
            });
        }
    }

    match cfg.ide_kind {
        IdeKind::Opencode => {
            let program = crate::ide_bridge::resolve_program(IdeKind::Opencode);
            match cli_models(&program, &["models"], project_dir).await {
                Ok(models) if !models.is_empty() => Ok(ModelCatalog {
                    note: format!("{program} models · {} model", models.len()),
                    models,
                    source: "cli".into(),
                }),
                Ok(_) => Ok(empty_catalog(
                    "cli",
                    "`opencode models` boş liste döndürdü. Model alanını boş bırakırsan CLI varsayılanı kullanılır.",
                )),
                Err(error) => Ok(empty_catalog(
                    "cli",
                    format!("{error} Model alanını boş bırakabilirsin; CLI varsayılanı kullanılır."),
                )),
            }
        }
        IdeKind::ClaudeCode => {
            let bin = crate::ide_bridge::resolve_program(IdeKind::ClaudeCode);
            let exists = !bin.is_empty() && Path::new(&bin).is_file();
            let note = if exists {
                format!("Claude CLI bulundu: {bin}")
            } else {
                "Claude CLI bulunamadı. Kurulumdan sonra `CLAUDE_BIN` ortam değişkeniyle yolunu belirtebilirsin.".into()
            };

            let models = [
                ("claude-opus-5-5", "Claude Opus 5.5"),
                ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
                ("claude-opus-4-6", "Claude Opus 4.6"),
                ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
                ("claude-3-7-sonnet", "Claude 3.7 Sonnet"),
                ("claude-3-5-sonnet", "Claude 3.5 Sonnet"),
                ("claude-3-5-haiku", "Claude 3.5 Haiku"),
                ("opus", "Opus (alias)"),
                ("sonnet", "Sonnet (alias)"),
                ("haiku", "Haiku (alias)"),
            ]
            .iter()
            .map(|(id, name)| ModelInfo {
                id: (*id).to_string(),
                name: (*name).to_string(),
                context: None,
            })
            .collect();
            Ok(ModelCatalog {
                note,
                models,
                source: "static".into(),
            })
        }
        IdeKind::CursorAgent => {
            let program = crate::ide_bridge::resolve_program(IdeKind::CursorAgent);
            let exists = !program.is_empty() && Path::new(&program).is_file();

            let models = [
                ("claude-3-7-sonnet", "Claude 3.7 Sonnet"),
                ("claude-3-5-sonnet", "Claude 3.5 Sonnet"),
                ("gpt-4o", "GPT-4o"),
                ("o3-mini", "o3-mini"),
                ("cursor-small", "Cursor Small"),
            ]
            .iter()
            .map(|(id, name)| ModelInfo {
                id: (*id).to_string(),
                name: (*name).to_string(),
                context: None,
            })
            .collect();

            let note = if exists {
                format!("Cursor bulundu: {program}")
            } else {
                "Cursor CLI bulunamadı. Yolu için `CURSOR_AGENT_BIN` veya Custom CLI kullanabilirsin.".into()
            };

            Ok(ModelCatalog {
                note,
                models,
                source: "static".into(),
            })
        }
        IdeKind::CustomCli => Ok(empty_catalog(
            "cli",
            "Custom CLI için otomatik liste yok. Boş bırakırsan CLI varsayılanı kullanılır; istersen özel model adı girebilirsin.",
        )),
        IdeKind::CustomHttp => Ok(empty_catalog(
            "http",
            "HTTP köprüsü endpoint'ini girip yenile.",
        )),
    }
}

/// `base` → `.../models`, accepting base URLs with or without `/v1`.
pub fn models_endpoint(_kind: ApiKind, base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let base = base
        .strip_suffix("/chat/completions")
        .or_else(|| base.strip_suffix("/messages"))
        .unwrap_or(base);

    if base.ends_with("/models") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

fn parse_models(text: &str) -> Result<Vec<ModelInfo>, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("Model listesi JSON değil ({e}): {}", snippet(text, 300)))?;

    let array = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .or_else(|| value.as_array())
        .ok_or("Model listesi beklenen formatta değil (data/models dizisi yok).")?;

    let mut models = Vec::new();
    let mut seen = HashSet::new();

    for item in array {
        let (id, name, context) = match item {
            Value::String(id) => (id.clone(), id.clone(), None),
            Value::Object(map) => {
                let id = ["id", "slug", "model", "name"]
                    .iter()
                    .find_map(|key| map.get(*key).and_then(Value::as_str))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                let name = ["name", "display_name", "displayName", "title"]
                    .iter()
                    .find_map(|key| map.get(*key).and_then(Value::as_str))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| id.clone());
                let context = [
                    "context_length",
                    "context_window",
                    "max_context_length",
                    "maxContextLength",
                ]
                .iter()
                .find_map(|key| map.get(*key).and_then(Value::as_u64));
                (id, name, context)
            }
            _ => continue,
        };

        let lower = id.to_lowercase();
        if NON_CHAT.iter().any(|needle| lower.contains(needle)) {
            continue;
        }
        if seen.insert(id.clone()) {
            models.push(ModelInfo { id, name, context });
        }
    }

    models.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });

    Ok(models)
}

async fn http_models(endpoint: &str, cfg: &ProviderConfig) -> Result<Vec<ModelInfo>, String> {
    let url = format!("{}/models", endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .user_agent("swarm-command-center/0.1")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("HTTP istemcisi oluşturulamadı: {e}"))?;

    let mut req = client.get(&url);
    if let Some(token) = cfg.resolve_api_key() {
        req = req.bearer_auth(token);
    }

    let response = req
        .send()
        .await
        .map_err(|e| format!("Köprü model listesi alınamadı ({url}): {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(format!(
            "Köprü model listesi HTTP {}: {}",
            status.as_u16(),
            snippet(&text, 300)
        ));
    }

    let models = parse_models(&text)?;
    if models.is_empty() {
        return Err("Köprü boş model listesi döndürdü.".into());
    }
    Ok(models)
}

async fn cli_models(program: &str, args: &[&str], cwd: &Path) -> Result<Vec<ModelInfo>, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| {
            format!(
                "`{program} {}` 60 saniye içinde yanıt vermedi.",
                args.join(" ")
            )
        })?
        .map_err(|e| format!("`{program}` çalıştırılamadı: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`{program} {}` hata verdi: {}",
            args.join(" "),
            snippet(&stderr, 300)
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut models = Vec::new();
    let mut seen = HashSet::new();

    for line in text.lines() {
        let line = line.trim().trim_start_matches(['-', '*', '•']).trim();
        if line.is_empty() || line.len() < 2 || line.len() > 160 {
            continue;
        }
        if line.contains(char::is_whitespace) || line.ends_with(':') {
            continue;
        }
        if !line.chars().any(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        if seen.insert(line.to_string()) {
            models.push(ModelInfo {
                id: line.to_string(),
                name: line.to_string(),
                context: None,
            });
        }
    }

    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(models)
}

fn empty_catalog(source: &str, note: impl Into<String>) -> ModelCatalog {
    ModelCatalog {
        models: Vec::new(),
        source: source.to_string(),
        note: note.into(),
    }
}

fn snippet(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_variants() {
        assert_eq!(
            models_endpoint(ApiKind::Openai, "https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_endpoint(ApiKind::Openrouter, "https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/models"
        );
        assert_eq!(
            models_endpoint(ApiKind::Deepseek, "https://api.deepseek.com"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            models_endpoint(ApiKind::Anthropic, "https://api.anthropic.com"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_endpoint(
                ApiKind::Custom,
                "http://127.0.0.1:11434/v1/chat/completions"
            ),
            "http://127.0.0.1:11434/v1/models"
        );
    }

    #[test]
    fn parses_openai_shape() {
        let raw = r#"{"object":"list","data":[
            {"id":"gpt-4.1-mini","object":"model"},
            {"id":"text-embedding-3-small","object":"model"}
        ]}"#;
        let models = parse_models(raw).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-4.1-mini");
    }

    #[test]
    fn parses_openrouter_shape_with_context() {
        let raw = r#"{"data":[
            {"id":"anthropic/claude-sonnet-4","name":"Anthropic: Claude Sonnet 4","context_length":200000},
            {"id":"deepseek/deepseek-chat","name":"DeepSeek: Chat","context_length":64000}
        ]}"#;
        let models = parse_models(raw).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].context, Some(200000));
        assert!(models.iter().any(|m| m.id == "deepseek/deepseek-chat"));
    }

    #[test]
    fn parses_anthropic_shape() {
        let raw = r#"{"data":[
            {"type":"model","id":"claude-sonnet-4-20250514","display_name":"Claude Sonnet 4"}
        ],"has_more":false}"#;
        let models = parse_models(raw).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "Claude Sonnet 4");
    }

    #[test]
    fn parses_plain_array_and_dedupes() {
        let raw = r#"["gpt-4o","gpt-4o","o4-mini"]"#;
        let models = parse_models(raw).unwrap();
        assert_eq!(models.len(), 2);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_models("not json").is_err());
        assert!(parse_models(r#"{"foo":1}"#).is_err());
    }

    #[tokio::test]
    #[ignore = "requires the local opencode CLI"]
    async fn local_opencode_catalog_smoke() {
        let cfg = ProviderConfig {
            source: ProviderSource::Ide,
            ide_kind: IdeKind::Opencode,
            ..Default::default()
        };
        let catalog = ide_catalog(&cfg, std::path::Path::new(".")).await.unwrap();
        println!(
            "source={} note={} models={}",
            catalog.source,
            catalog.note,
            catalog.models.len()
        );
        assert!(!catalog.models.is_empty(), "note: {}", catalog.note);
    }
}
