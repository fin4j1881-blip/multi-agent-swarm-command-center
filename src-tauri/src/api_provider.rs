//! Direct cloud API provider: OpenAI-compatible + Anthropic Messages API.

use crate::config::{ApiKind, ProviderConfig};
use crate::provider::ModelProvider;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

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
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
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

        let label = format!("API · {} · {}", cfg.api_kind.as_str(), model);

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

    /// Clones this provider with an alternate model name for fallback execution.
    pub fn with_model(&self, new_model: &str) -> Self {
        let model = new_model.trim().to_string();
        let label = format!("API · {} · {}", self.kind.as_str(), model);
        Self {
            client: self.client.clone(),
            kind: self.kind,
            model,
            api_key: self.api_key.clone(),
            base_url: self.base_url.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            label,
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

    async fn openai_compatible(&self, prompt: &str, system_prompt: &str) -> Result<String, String> {
        let url = self.chat_endpoint();
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": prompt }
            ],
            "temperature": self.temperature,
            "max_tokens": self.max_tokens
        });

        let mut last_error = String::from("bilinmeyen hata");
        for attempt in 0..3u32 {
            let mut req = self.client.post(&url).json(&body);
            if !self.api_key.is_empty() {
                req = req.bearer_auth(&self.api_key);
            }
            if self.kind == ApiKind::Openrouter {
                req = req
                    .header("HTTP-Referer", "https://localhost/swarm-command-center")
                    .header("X-Title", "Swarm Command Center");
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        return parse_openai_body(&text);
                    }
                    last_error = format!("HTTP {}: {}", status.as_u16(), snippet(&text, 700));
                    if (status.as_u16() == 429 || status.is_server_error()) && attempt < 2 {
                        sleep_backoff(attempt).await;
                        continue;
                    }
                    return Err(format!(
                        "{} isteği başarısız → {last_error}",
                        self.kind.as_str()
                    ));
                }
                Err(e) => {
                    last_error = format!("ağ hatası: {e}");
                    if attempt < 2 {
                        sleep_backoff(attempt).await;
                        continue;
                    }
                }
            }
        }
        Err(format!(
            "{} çağrısı başarısız → {last_error}",
            self.kind.as_str()
        ))
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

        let mut last_error = String::from("bilinmeyen hata");
        for attempt in 0..3u32 {
            let mut req = self
                .client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .json(&body);
            if !self.api_key.is_empty() {
                req = req.header("x-api-key", &self.api_key);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        return parse_anthropic_body(&text);
                    }
                    last_error = format!("HTTP {}: {}", status.as_u16(), snippet(&text, 700));
                    if (status.as_u16() == 429 || status.is_server_error()) && attempt < 2 {
                        sleep_backoff(attempt).await;
                        continue;
                    }
                    return Err(format!("Anthropic isteği başarısız → {last_error}"));
                }
                Err(e) => {
                    last_error = format!("ağ hatası: {e}");
                    if attempt < 2 {
                        sleep_backoff(attempt).await;
                        continue;
                    }
                }
            }
        }
        Err(format!("Anthropic çağrısı başarısız → {last_error}"))
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

async fn sleep_backoff(attempt: u32) {
    let secs = 1u64 << attempt.min(3);
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

fn snippet(text: &str, max: usize) -> String {
    let clean = text.trim();
    if clean.chars().count() <= max {
        return clean.to_string();
    }
    let cut: String = clean.chars().take(max).collect();
    format!("{cut}…")
}

fn parse_openai_body(text: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("Yanıt JSON değil ({e}): {}", snippet(text, 400)))?;

    if let Some(err) = value.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(|m| m.to_string())
            .unwrap_or_else(|| err.to_string());
        return Err(format!("API hatası: {message}"));
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
            .map(|m| m.to_string())
            .unwrap_or_else(|| err.to_string());
        return Err(format!("Anthropic hatası: {message}"));
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
}
