//! Direct cloud API provider: OpenAI-compatible + Anthropic Messages API.

use crate::config::{ApiKind, ProviderConfig};
use crate::provider::ModelProvider;
use crate::util::snippet;
use async_trait::async_trait;
use reqwest::RequestBuilder;
use serde_json::{json, Value};
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 3;

#[derive(Clone)]
pub struct ApiProvider {
    client: reqwest::Client,
    kind: ApiKind,
    model: String,
    api_key: String,
    base_url: String,
    temperature: f32,
    max_tokens: u32,
    label: String,
}

impl ApiProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self, String> {
        let model = cfg.model.trim().to_string();
        if model.is_empty() {
            return Err("API modunda model adı zorunlu.".into());
        }
        let api_key = cfg.resolve_api_key().unwrap_or_default();
        let base_url = cfg
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| cfg.api_kind.default_base_url().to_string());

        if base_url.is_empty() {
            return Err("Base URL boş. Custom API için tam endpoint gerekli.".into());
        }
        if api_key.is_empty() && cfg.api_kind != ApiKind::Custom && !cfg.is_custom_endpoint() {
            return Err(format!(
                "{} için API anahtarı yok. Alanı doldur veya {} ortam değişkenini tanımla.",
                cfg.api_kind.as_str(),
                cfg.api_kind.env_key()
            ));
        }

        let client = reqwest::Client::builder()
            .user_agent("swarm-command-center/0.1")
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(cfg.timeout_secs.max(30)))
            .build()
            .map_err(|e| format!("HTTP istemcisi oluşturulamadı: {e}"))?;

        let label = Self::make_label(cfg.api_kind, &model);

        Ok(Self {
            client,
            kind: cfg.api_kind,
            model,
            api_key,
            base_url,
            temperature: cfg.temperature.clamp(0.0, 2.0),
            max_tokens: cfg.max_tokens.max(256),
            label,
        })
    }

    fn make_label(kind: ApiKind, model: &str) -> String {
        format!("API · {} · {}", kind.as_str(), model)
    }

    /// Clones this provider with an alternate model name for fallback execution.
    pub fn with_model(&self, new_model: &str) -> Self {
        let model = new_model.trim().to_string();
        Self {
            label: Self::make_label(self.kind, &model),
            model,
            ..self.clone()
        }
    }

    fn chat_endpoint(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        match self.kind {
            ApiKind::Anthropic => {
                if base.ends_with("/messages") {
                    base.to_string()
                } else if base.ends_with("/v1") {
                    format!("{base}/messages")
                } else {
                    format!("{base}/v1/messages")
                }
            }
            _ => {
                if base.ends_with("/chat/completions") {
                    base.to_string()
                } else if base.ends_with("/v1") {
                    format!("{base}/chat/completions")
                } else {
                    format!("{base}/v1/chat/completions")
                }
            }
        }
    }

    /// OpenAI's reasoning models (o-series, gpt-5) reject `temperature`.
    fn omits_temperature(&self) -> bool {
        if self.kind != ApiKind::Openai {
            return false;
        }
        let model = self.model.to_lowercase();
        model.starts_with("gpt-5")
            || model
                .strip_prefix('o')
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    }

    /// Sends the request, retrying 429 / 5xx / network failures with backoff,
    /// and returns the body of the first successful response.
    async fn post_with_retry(
        &self,
        label: &str,
        build: impl Fn() -> RequestBuilder,
    ) -> Result<String, String> {
        let mut last_error = String::from("bilinmeyen hata");

        for attempt in 0..MAX_ATTEMPTS {
            let retries_left = attempt + 1 < MAX_ATTEMPTS;

            match build().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok());
                    let text = resp.text().await.unwrap_or_default();

                    if status.is_success() {
                        return Ok(text);
                    }
                    last_error = format!("HTTP {}: {}", status.as_u16(), snippet(&text, 700));
                    let transient = status.as_u16() == 429 || status.is_server_error();
                    if !(transient && retries_left) {
                        return Err(format!("{label} isteği başarısız → {last_error}"));
                    }
                    sleep_backoff(attempt, retry_after).await;
                }
                Err(e) => {
                    last_error = format!("ağ hatası: {e}");
                    if !retries_left {
                        break;
                    }
                    sleep_backoff(attempt, None).await;
                }
            }
        }
        Err(format!("{label} çağrısı başarısız → {last_error}"))
    }

    async fn openai_compatible(&self, prompt: &str, system_prompt: &str) -> Result<String, String> {
        let url = self.chat_endpoint();
        let mut body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": prompt }
            ],
        });
        if !self.omits_temperature() {
            body["temperature"] = json!(self.temperature);
        }
        // OpenAI deprecated `max_tokens`; every other OpenAI-compatible server
        // (Ollama, DeepSeek, OpenRouter...) still expects it.
        let token_field = if self.kind == ApiKind::Openai {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body[token_field] = json!(self.max_tokens);

        let text = self
            .post_with_retry(self.kind.as_str(), || {
                let mut req = self.client.post(&url).json(&body);
                if !self.api_key.is_empty() {
                    req = req.bearer_auth(&self.api_key);
                }
                if self.kind == ApiKind::Openrouter {
                    req = req
                        .header("HTTP-Referer", "https://localhost/swarm-command-center")
                        .header("X-Title", "Swarm Command Center");
                }
                req
            })
            .await?;
        parse_openai_body(&text)
    }

    async fn anthropic(&self, prompt: &str, system_prompt: &str) -> Result<String, String> {
        let url = self.chat_endpoint();
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "system": system_prompt,
            "messages": [
                { "role": "user", "content": prompt }
            ]
        });

        let text = self
            .post_with_retry("Anthropic", || {
                let mut req = self
                    .client
                    .post(&url)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body);
                if !self.api_key.is_empty() {
                    req = req.header("x-api-key", &self.api_key);
                }
                req
            })
            .await?;
        parse_anthropic_body(&text)
    }
}

#[async_trait]
impl ModelProvider for ApiProvider {
    fn label(&self) -> String {
        self.label.clone()
    }

    async fn complete(&self, prompt: &str, system_prompt: &str) -> Result<String, String> {
        match self.kind {
            ApiKind::Anthropic => self.anthropic(prompt, system_prompt).await,
            _ => self.openai_compatible(prompt, system_prompt).await,
        }
    }
}

/// Exponential backoff (1s, 2s, 4s...), or the server's `Retry-After` (capped).
async fn sleep_backoff(attempt: u32, retry_after: Option<u64>) {
    let secs = retry_after
        .map(|s| s.clamp(1, 30))
        .unwrap_or(1u64 << attempt.min(3));
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

const TRUNCATED: &str = "Yanıt çıktı token sınırında (max_tokens) kesildi; dosya eksik gelirdi. \
     Gelişmiş ayarlardan çıktı token sınırını artır veya görevi daha küçük bir dosyaya böl.";

fn parse_openai_body(text: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("Yanıt JSON değil ({e}): {}", snippet(text, 400)))?;

    if let Some(err) = value.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return Err(format!("API hatası: {message}"));
    }

    // A cut-off answer would be written to disk as a broken file.
    if value["choices"][0]["finish_reason"].as_str() == Some("length") {
        return Err(TRUNCATED.to_string());
    }

    let message = &value["choices"][0]["message"];

    if let Some(content) = message["content"].as_str() {
        if !content.trim().is_empty() {
            return Ok(content.to_string());
        }
    }
    if let Some(parts) = message["content"].as_array() {
        let joined = parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        if !joined.trim().is_empty() {
            return Ok(joined);
        }
    }
    for key in ["text", "output_text", "content", "response"] {
        if let Some(s) = value[key].as_str() {
            if !s.trim().is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    Err(format!("Yanıtta içerik bulunamadı: {}", snippet(text, 400)))
}

fn parse_anthropic_body(text: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("Anthropic yanıtı JSON değil ({e}): {}", snippet(text, 400)))?;

    if let Some(err) = value.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return Err(format!("Anthropic hatası: {message}"));
    }

    if value["stop_reason"].as_str() == Some("max_tokens") {
        return Err(TRUNCATED.to_string());
    }

    if let Some(parts) = value["content"].as_array() {
        let joined = parts
            .iter()
            .filter(|p| p["type"].as_str().unwrap_or("text") == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        if !joined.trim().is_empty() {
            return Ok(joined);
        }
    }
    if let Some(s) = value["completion"].as_str() {
        if !s.trim().is_empty() {
            return Ok(s.to_string());
        }
    }
    Err("Anthropic yanıtında metin bloğu yok.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderSource;

    fn serve_once(body: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn openai_compatible_roundtrip() {
        let base = serve_once(r#"{"choices":[{"message":{"content":"MERHABA"}}]}"#);
        let cfg = ProviderConfig {
            source: ProviderSource::Api,
            api_kind: ApiKind::Custom,
            model: "test-model".into(),
            base_url: Some(format!("{base}/v1")),
            ..Default::default()
        };
        let provider = ApiProvider::new(&cfg).expect("provider");
        assert_eq!(provider.complete("hi", "sys").await.unwrap(), "MERHABA");
    }

    #[tokio::test]
    async fn anthropic_roundtrip() {
        let base = serve_once(r#"{"content":[{"type":"text","text":"CLAUDE"}]}"#);
        let cfg = ProviderConfig {
            source: ProviderSource::Api,
            api_kind: ApiKind::Anthropic,
            model: "claude-test".into(),
            api_key: Some("dummy".into()),
            base_url: Some(base),
            ..Default::default()
        };
        let provider = ApiProvider::new(&cfg).expect("provider");
        assert_eq!(provider.complete("hi", "sys").await.unwrap(), "CLAUDE");
    }

    #[test]
    fn endpoint_normalization() {
        let base = |kind: ApiKind, url: &str| {
            let cfg = ProviderConfig {
                api_kind: kind,
                model: "m".into(),
                api_key: Some("k".into()),
                base_url: Some(url.into()),
                ..Default::default()
            };
            ApiProvider::new(&cfg).unwrap().chat_endpoint()
        };

        assert_eq!(
            base(ApiKind::Openai, "https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            base(ApiKind::Deepseek, "https://api.deepseek.com"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            base(ApiKind::Anthropic, "https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            base(
                ApiKind::Custom,
                "http://127.0.0.1:11434/v1/chat/completions"
            ),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn truncated_answers_are_errors() {
        let openai = r#"{"choices":[{"finish_reason":"length","message":{"content":"yarım"}}]}"#;
        assert!(parse_openai_body(openai)
            .unwrap_err()
            .contains("max_tokens"));
        let claude = r#"{"stop_reason":"max_tokens","content":[{"type":"text","text":"yarım"}]}"#;
        assert!(parse_anthropic_body(claude)
            .unwrap_err()
            .contains("max_tokens"));
        let fine = r#"{"choices":[{"finish_reason":"stop","message":{"content":"tam"}}]}"#;
        assert_eq!(parse_openai_body(fine).unwrap(), "tam");
    }

    #[test]
    fn reasoning_models_skip_temperature() {
        let provider = |model: &str| {
            let cfg = ProviderConfig {
                api_kind: ApiKind::Openai,
                model: model.into(),
                api_key: Some("k".into()),
                ..Default::default()
            };
            ApiProvider::new(&cfg).unwrap()
        };
        assert!(provider("o4-mini").omits_temperature());
        assert!(provider("gpt-5").omits_temperature());
        assert!(!provider("gpt-4.1-mini").omits_temperature());
        assert!(!provider("omni-moderation").omits_temperature());
    }
}
