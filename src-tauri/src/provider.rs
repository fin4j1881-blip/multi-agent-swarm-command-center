//! The single abstraction every model source implements.

use crate::api_provider::ApiProvider;
use crate::config::{ProviderConfig, ProviderSource};
use crate::ide_bridge::IdeBridgeProvider;
use async_trait::async_trait;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Human readable label used in logs, e.g. `API · OpenRouter · deepseek-chat`.
    fn label(&self) -> String;

    /// Single-turn completion. `system_prompt` carries the role contract,
    /// `prompt` carries the concrete work order.
    async fn complete(&self, prompt: &str, system_prompt: &str) -> Result<String, String>;
}

/// A provider that wraps an ordered chain of models (primary + fallbacks).
///
/// When an attempt fails with a quota/limit, rate-limit, insufficient balance,
/// overloaded server or capacity error, it automatically falls back to the next
/// model in the chain and remembers the working model for subsequent requests.
pub struct FallbackProvider {
    providers: Vec<Box<dyn ModelProvider>>,
    current: AtomicUsize,
    role: String,
}

impl FallbackProvider {
    pub fn new(
        role: impl Into<String>,
        providers: Vec<Box<dyn ModelProvider>>,
    ) -> Result<Self, String> {
        if providers.is_empty() {
            return Err("En az bir model sağlayıcısı gerekli.".into());
        }
        Ok(Self {
            providers,
            current: AtomicUsize::new(0),
            role: role.into(),
        })
    }

    /// True if the error indicates a model quota/rate-limit/capacity issue
    /// where trying a fallback model is meaningful.
    pub fn is_fallback_error(err: &str) -> bool {
        let lower = err.to_lowercase();
        // HTTP status codes
        contains_code(&lower, "429")
            || lower.contains("too many requests")
            || lower.contains("rate limit")
            || lower.contains("rate_limit")
            || lower.contains("ratelimit")
            // Quota / balance / credit
            || lower.contains("quota")
            || lower.contains("insufficient_quota")
            || lower.contains("exceeded your current quota")
            || lower.contains("balance")
            || lower.contains("credit")
            || lower.contains("billing")
            || lower.contains("payment")
            // Capacity / overload
            || lower.contains("overloaded")
            || lower.contains("server is overloaded")
            || lower.contains("capacity")
            || lower.contains("out of capacity")
            || lower.contains("resource_exhausted")
            || lower.contains("model is not available")
            || lower.contains("not supported")
            // Context/token limits of the specific model
            || lower.contains("maximum context length")
            || lower.contains("context_length_exceeded")
            || lower.contains("token limit")
            // Gateway 503 / 504 / 502
            || contains_code(&lower, "503")
            || contains_code(&lower, "502")
            || contains_code(&lower, "504")
            || lower.contains("temporarily unavailable")
    }
}

/// True if `code` appears as a standalone number (so "port 15029" is not a 502).
fn contains_code(haystack: &str, code: &str) -> bool {
    haystack.match_indices(code).any(|(start, _)| {
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + code.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_digit()) && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

#[async_trait]
impl ModelProvider for FallbackProvider {
    fn label(&self) -> String {
        let idx = self
            .current
            .load(Ordering::Relaxed)
            .min(self.providers.len() - 1);
        let current_label = self.providers[idx].label();
        if self.providers.len() > 1 {
            format!(
                "{current_label} (zincir: {}/{})",
                idx + 1,
                self.providers.len()
            )
        } else {
            current_label
        }
    }

    async fn complete(&self, prompt: &str, system_prompt: &str) -> Result<String, String> {
        let start_idx = self
            .current
            .load(Ordering::Relaxed)
            .min(self.providers.len() - 1);
        let total = self.providers.len();
        let mut last_error = String::new();

        // Try from current index up to end of chain
        for i in start_idx..total {
            let provider = &self.providers[i];
            let provider_label = provider.label();

            match provider.complete(prompt, system_prompt).await {
                Ok(response) => {
                    if i != start_idx {
                        self.current.store(i, Ordering::Relaxed);
                    }
                    return Ok(response);
                }
                Err(err) => {
                    last_error = err.clone();
                    let is_recoverable = Self::is_fallback_error(&err);
                    let has_next = i + 1 < total;

                    if is_recoverable && has_next {
                        // The orchestrator notices the label change and logs it.
                        self.current.store(i + 1, Ordering::Relaxed);
                        continue;
                    }

                    // Not a fallback-eligible error, or we reached the end of the chain
                    return Err(format!(
                        "[{}] {provider_label} başarısız oldu: {err}",
                        self.role
                    ));
                }
            }
        }

        Err(format!(
            "[{}] Tüm modeller ({total} adet) denendi fakat yanıt alınamadı. Son hata: {last_error}",
            self.role
        ))
    }
}

/// Builds the right provider (with fallback chain) for a role from its config.
/// `role` only appears in error messages ("Komutan", "İşçi").
pub fn build(
    cfg: &ProviderConfig,
    project_dir: &Path,
    role: &str,
) -> Result<Box<dyn ModelProvider>, String> {
    let models = cfg.all_models();

    let chain: Vec<Box<dyn ModelProvider>> = match cfg.source {
        ProviderSource::Api => {
            let mut primary = cfg.clone();
            if let Some(first) = models.first() {
                primary.model = first.clone();
            }
            let base = ApiProvider::new(&primary)?;
            let mut chain: Vec<Box<dyn ModelProvider>> = vec![Box::new(base.clone())];
            chain.extend(
                models
                    .iter()
                    .skip(1)
                    .map(|m| Box::new(base.with_model(m)) as Box<dyn ModelProvider>),
            );
            chain
        }
        ProviderSource::Ide => {
            let mut primary = cfg.clone();
            if let Some(first) = models.first() {
                primary.model = first.clone();
            }
            let base = IdeBridgeProvider::new(&primary, project_dir)?;
            let mut chain: Vec<Box<dyn ModelProvider>> = Vec::new();
            for m in models.iter().skip(1) {
                chain.push(Box::new(base.with_model(m)));
            }
            chain.insert(0, Box::new(base));
            chain
        }
    };

    if chain.len() == 1 {
        return Ok(chain.into_iter().next().expect("one provider"));
    }
    Ok(Box::new(FallbackProvider::new(role, chain)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    struct MockProvider {
        label: String,
        calls: Arc<AtomicU32>,
        response: Result<String, String>,
    }

    #[async_trait]
    impl ModelProvider for MockProvider {
        fn label(&self) -> String {
            self.label.clone()
        }
        async fn complete(&self, _prompt: &str, _sys: &str) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.response.clone()
        }
    }

    #[tokio::test]
    async fn falls_back_on_429() {
        let calls1 = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::new(AtomicU32::new(0));

        let p1 = Box::new(MockProvider {
            label: "Model 1".into(),
            calls: Arc::clone(&calls1),
            response: Err("HTTP 429: Too Many Requests (rate limit exceeded)".into()),
        });
        let p2 = Box::new(MockProvider {
            label: "Model 2".into(),
            calls: Arc::clone(&calls2),
            response: Ok("Başarılı yanıt".into()),
        });

        let chain = FallbackProvider::new("TestRole", vec![p1, p2]).unwrap();
        let res = chain.complete("hi", "sys").await.unwrap();
        assert_eq!(res, "Başarılı yanıt");
        assert_eq!(calls1.load(Ordering::SeqCst), 1);
        assert_eq!(calls2.load(Ordering::SeqCst), 1);

        // Next call should stay on Model 2 directly
        let res2 = chain.complete("hi 2", "sys").await.unwrap();
        assert_eq!(res2, "Başarılı yanıt");
        assert_eq!(calls1.load(Ordering::SeqCst), 1); // Not called again
        assert_eq!(calls2.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn falls_back_on_quota_error() {
        let calls1 = Arc::new(AtomicU32::new(0));
        let calls2 = Arc::new(AtomicU32::new(0));

        let p1 = Box::new(MockProvider {
            label: "Model 1".into(),
            calls: Arc::clone(&calls1),
            response: Err("insufficient_quota: you exceeded your current quota".into()),
        });
        let p2 = Box::new(MockProvider {
            label: "Model 2".into(),
            calls: Arc::clone(&calls2),
            response: Ok("OK".into()),
        });

        let chain = FallbackProvider::new("TestRole", vec![p1, p2]).unwrap();
        assert_eq!(chain.complete("hi", "sys").await.unwrap(), "OK");
    }

    #[test]
    fn fallback_error_detector() {
        assert!(FallbackProvider::is_fallback_error("HTTP 429: rate limit"));
        assert!(FallbackProvider::is_fallback_error("insufficient_quota"));
        assert!(FallbackProvider::is_fallback_error("overloaded"));
        assert!(FallbackProvider::is_fallback_error(
            "503 Service Unavailable"
        ));
        assert!(FallbackProvider::is_fallback_error(
            "maximum context length exceeded"
        ));
        assert!(!FallbackProvider::is_fallback_error(
            "Invalid JSON format in prompt"
        ));
        assert!(!FallbackProvider::is_fallback_error(
            "bağlantı 127.0.0.1:15029 reddedildi"
        ));
    }
}
