//! The autonomous loop: plan → code → guardrail → review → write.
//!
//! Stopping is cooperative but prompt: a "Durdur" click abandons an in-flight
//! model call (HTTP request or CLI process) immediately. Files are only ever
//! written between model calls, so a stop can never leave a half-written file.

use crate::config::{AgentConfig, ProviderConfig};
use crate::guardrail;
use crate::prompts;
use crate::protocol::{self, PlanTask};
use crate::provider::{self, ModelProvider};
use crate::state::{emit_log, emit_status, now_iso, LogEvent, RunStats, SharedState};
use crate::systems;
use crate::util::{condense_cargo_errors, truncate_chars};
use crate::workspace::{self, FileText};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// How many of the architect's `contextFiles` are shown to the coder, and how
/// much of each.
const CONTEXT_FILES: usize = 3;
const CONTEXT_FILE_CHARS: usize = 6_000;
const CARGO_CHECK_TIMEOUT_SECS: u64 = 300;
const MAX_CONSECUTIVE_ERRORS: u32 = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum TaskOutcome {
    Applied,
    Rejected(String),
    /// Could not be attempted; reported back to the architect.
    Skipped(String),
    /// Approved, but nothing was written (dry-run / auto-apply off).
    NotWritten,
    Stopped,
    /// Paused automatically due to exceeding the consecutive error limit.
    Paused,
}

enum CallError {
    Stopped,
    Failed(String),
}

/// Checks whether an error represents a 429 (rate limit), timeout, connection,
/// or temporary server overload issue suitable for automatic failover.
pub fn is_failover_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    contains_code(&lower, "429")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("ratelimit")
        || lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("timedout")
        || lower.contains("zaman aşımı")
        || lower.contains("deadline has elapsed")
        || lower.contains("connection")
        || lower.contains("connect")
        || lower.contains("bağlantı")
        || lower.contains("network")
        || lower.contains("econnrefused")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("reset by peer")
        || lower.contains("broken pipe")
        || lower.contains("dns")
        || lower.contains("quota")
        || lower.contains("insufficient_quota")
        || lower.contains("balance")
        || lower.contains("credit")
        || lower.contains("overloaded")
        || lower.contains("capacity")
        || contains_code(&lower, "502")
        || contains_code(&lower, "503")
        || contains_code(&lower, "504")
}

fn contains_code(haystack: &str, code: &str) -> bool {
    haystack.match_indices(code).any(|(start, _)| {
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + code.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_digit()) && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

/// Ordered failover chain of models: Primary -> Fallback 1 -> Fallback 2 -> others.
pub struct FailoverChain {
    #[allow(dead_code)]
    pub role: String,
    pub providers: Vec<Box<dyn ModelProvider>>,
    pub current: AtomicUsize,
}

impl FailoverChain {
    pub fn new(
        role: impl Into<String>,
        providers: Vec<Box<dyn ModelProvider>>,
    ) -> Result<Self, String> {
        if providers.is_empty() {
            return Err("En az bir model sağlayıcısı gerekli.".into());
        }
        Ok(Self {
            role: role.into(),
            providers,
            current: AtomicUsize::new(0),
        })
    }

    pub fn label(&self) -> String {
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
}

/// Builds the ordered model chain: [Primary, fallback_1, fallback_2, fallback_models...].
pub fn build_failover_chain(
    cfg: &ProviderConfig,
    root: &Path,
    role: &str,
) -> Result<FailoverChain, String> {
    let model_names = cfg.all_models();

    if model_names.is_empty() {
        return Err(format!("{role}: En az bir model adı yapılandırılmalıdır."));
    }

    let mut providers: Vec<Box<dyn ModelProvider>> = Vec::new();
    for m in &model_names {
        let mut single_cfg = cfg.clone();
        single_cfg.model = m.clone();
        single_cfg.fallback_1 = None;
        single_cfg.fallback_2 = None;
        single_cfg.fallback_models = Vec::new();

        let prov = provider::build(&single_cfg, root, role)
            .map_err(|e| format!("{role} ({m}) sağlayıcısı hazırlanamadı: {e}"))?;
        providers.push(prov);
    }

    FailoverChain::new(role, providers)
}

pub async fn run(app: AppHandle, shared: SharedState, cfg: AgentConfig) {
    let project_key = cfg
        .root()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| cfg.project_path.clone());

    let result = run_inner(&app, &shared, &cfg, &project_key).await;

    let failed = result.is_err();
    if let Err(error) = &result {
        emit_log(
            &app,
            &shared,
            LogEvent::new("error", "system", format!("Komutanlık durdu: {error}"))
                .with_project(&project_key),
        )
        .await;
    }

    let (stopped, stats) = {
        let mut guard = shared.lock().await;
        match guard.projects.get_mut(&project_key) {
            Some(p) => {
                p.running = false;
                p.stats.finished_at = Some(now_iso());
                (p.stop.load(Ordering::SeqCst), p.stats.clone())
            }
            None => (false, RunStats::default()),
        }
    };
    // Kosu gunlugunu kapat: .swarm/runs/<id>/run.json ve .swarm/last-run.json
    let status = if failed {
        "failed"
    } else if stopped {
        "stopped"
    } else {
        "done"
    };
    crate::journal::finish(&project_key, status, &stats).await;
    emit_status(&app, &project_key, false);
}

struct Runner<'a> {
    app: &'a AppHandle,
    shared: &'a SharedState,
    cfg: &'a AgentConfig,
    root: PathBuf,
    project_key: String,
    stop: Arc<AtomicBool>,
    architect: FailoverChain,
    coder: FailoverChain,
    verify_build: bool,
    system_context: String,
}

async fn run_inner(
    app: &AppHandle,
    shared: &SharedState,
    cfg: &AgentConfig,
    project_key: &str,
) -> Result<(), String> {
    let root = cfg.root()?;
    let stop = {
        let mut guard = shared.lock().await;
        let p = guard.projects.entry(project_key.to_string()).or_default();
        p.stop.clone()
    };

    let architect = build_failover_chain(&cfg.architect, &root, "Komutan")
        .map_err(|e| format!("Komutan sağlayıcısı hazırlanamadı: {e}"))?;
    let coder = build_failover_chain(&cfg.coder, &root, "İşçi")
        .map_err(|e| format!("İşçi sağlayıcısı hazırlanamadı: {e}"))?;

    let system_context = systems::context_for(&root);
    let system_name = systems::match_system(&root).map(|def| def.name);

    let mut runner = Runner {
        app,
        shared,
        cfg,
        root,
        project_key: project_key.to_string(),
        stop,
        architect,
        coder,
        verify_build: cfg.verify_build,
        system_context,
    };

    runner
        .log(
            "info",
            "system",
            format!("Proje kökü: {}", runner.root.display()),
        )
        .await;
    runner
        .log(
            "info",
            "system",
            format!("Komutan → {}", runner.architect.label()),
        )
        .await;
    runner
        .log("info", "system", format!("İşçi → {}", runner.coder.label()))
        .await;
    if let Some(name) = system_name {
        runner
            .log(
                "info",
                "system",
                format!("Sistem tanımı yüklendi: {name}"),
            )
            .await;
    }

    runner.check_build_baseline().await;
    runner.run_rounds().await
}

impl Runner<'_> {
    async fn log(&self, level: &str, phase: &str, message: impl Into<String>) {
        emit_log(
            self.app,
            self.shared,
            LogEvent::new(level, phase, message).with_project(&self.project_key),
        )
        .await;
    }

    async fn log_detail(
        &self,
        level: &str,
        phase: &str,
        message: impl Into<String>,
        detail: impl Into<String>,
    ) {
        emit_log(
            self.app,
            self.shared,
            LogEvent::new(level, phase, message)
                .detail(detail)
                .with_project(&self.project_key),
        )
        .await;
    }

    fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    async fn update_stats(&self, apply: impl FnOnce(&mut RunStats)) {
        let mut guard = self.shared.lock().await;
        if let Some(p) = guard.projects.get_mut(&self.project_key) {
            apply(&mut p.stats);
        }
    }

    /// Runs `work` until it finishes or the user presses "Durdur".
    /// Dropping the future cancels HTTP requests and kills CLI children.
    async fn until_stopped<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            output = work => Some(output),
            _ = wait_for_stop(&self.stop) => None,
        }
    }

    /// Calls a model chain with automatic failover on 429, timeout, or connection error.
    async fn call(
        &self,
        phase: &str,
        chain: &FailoverChain,
        prompt: &str,
        system: &str,
    ) -> Result<String, CallError> {
        let start_idx = chain
            .current
            .load(Ordering::Relaxed)
            .min(chain.providers.len() - 1);
        let total = chain.providers.len();
        let mut last_error = String::new();

        for i in start_idx..total {
            let provider = &chain.providers[i];
            let before = provider.label();

            if i != start_idx {
                self.log(
                    "warn",
                    phase,
                    format!("Yedek modele geçildi ({}/{}): {before}", i + 1, total),
                )
                .await;
            }

            let result = self.until_stopped(provider.complete(prompt, system)).await;

            match result {
                None => return Err(CallError::Stopped),
                Some(Ok(text)) => {
                    chain.current.store(i, Ordering::Relaxed);
                    return Ok(text);
                }
                Some(Err(error)) => {
                    last_error = error.clone();
                    let has_next = i + 1 < total;
                    if has_next && is_failover_error(&error) {
                        let next_provider = &chain.providers[i + 1];
                        let next_label = next_provider.label();
                        self.log(
                            "warn",
                            phase,
                            format!(
                                "Model çağrısı başarısız ({before}): {error}. Sıradaki yedek modele geçiliyor → {next_label}",
                            ),
                        )
                        .await;
                        chain.current.store(i + 1, Ordering::Relaxed);
                        continue;
                    }

                    return Err(CallError::Failed(error));
                }
            }
        }

        Err(CallError::Failed(format!(
            "Tüm modeller ({total} adet) denendi fakat yanıt alınamadı. Son hata: {last_error}"
        )))
    }

    /// Automatically pauses autonomous execution after 4 consecutive failures.
    async fn pause_task(&self, task: &PlanTask, reason: &str) -> TaskOutcome {
        let message = format!(
            "`{}` görevinde üst üste {MAX_CONSECUTIVE_ERRORS} hata alındı ({reason}). Sonsuz döngüyü önlemek için otonom döngü duraklatıldı (Paused).",
            task.title
        );
        self.log("error", "system", &message).await;
        self.log_detail(
            "warn",
            "system",
            "Otonom döngü duraklatıldı (Paused)",
            format!(
                "Görev: {}\nSebep: {reason}\nArdışık hata limiti: {MAX_CONSECUTIVE_ERRORS}",
                task.title
            ),
        )
        .await;

        self.stop.store(true, Ordering::SeqCst);
        {
            let mut guard = self.shared.lock().await;
            if let Some(p) = guard.projects.get_mut(&self.project_key) {
                p.running = false;
            }
        }
        emit_status(self.app, &self.project_key, false);
        let _ = self.app.emit(
            "agent-paused",
            serde_json::json!({
                "projectPath": self.project_key,
                "task": task.title,
                "reason": message,
                "status": "paused"
            }),
        );

        TaskOutcome::Paused
    }

    /// A project that does not compile before we touch it would make every
    /// write look like a regression and get reverted; disable the gate then.
    async fn check_build_baseline(&mut self) {
        if !self.verify_build || !self.root.join("Cargo.toml").exists() {
            return;
        }
        self.log(
            "info",
            "system",
            "Derleme tabanı kontrol ediliyor (cargo check)…",
        )
        .await;

        let root = self.root.clone();
        let outcome = self
            .until_stopped(workspace::cargo_check(&root, CARGO_CHECK_TIMEOUT_SECS))
            .await;

        match outcome {
            Some(Err(error)) => {
                self.verify_build = false;
                self.log_detail(
                    "warn",
                    "system",
                    "Proje zaten derlenmiyor; yazma sonrası doğrulama bu çalıştırmada kapatıldı",
                    error,
                )
                .await;
            }
            Some(Ok(())) => {
                self.log("ok", "system", "Derleme tabanı temiz.").await;
            }
            None => {}
        }
    }

    async fn run_rounds(&self) -> Result<(), String> {
        let cfg = self.cfg;
        let mut completed: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        let mut rounds = cfg.max_rounds.max(1);
        if (cfg.dry_run || !cfg.auto_apply) && rounds > 1 {
            rounds = 1;
            self.log(
                "info",
                "system",
                "Dosya yazılmayacağı için tek tur çalıştırılıyor.",
            )
            .await;
        }

        for round in 1..=rounds {
            if self.is_stopped() {
                self.log("warn", "system", "Durduruldu.").await;
                return Ok(());
            }
            self.update_stats(|s| s.rounds = round).await;

            self.log(
                "architect",
                "architect",
                format!("Tur {round}/{rounds} — proje ağacı taranıyor"),
            )
            .await;

            let tree = {
                let root = self.root.clone();
                let limit = cfg.tree_entries;
                tokio::task::spawn_blocking(move || workspace::scan_tree(&root, limit))
                    .await
                    .map_err(|e| format!("Ağaç taraması çöktü: {e}"))?
            };
            let prompt = prompts::plan_prompt(
                &cfg.vision,
                &cfg.instructions,
                &self.system_context,
                &tree,
                &completed,
                &failures,
                round,
                rounds,
                cfg.max_tasks,
            );

            let raw_plan = match self
                .call(
                    "architect",
                    &self.architect,
                    &prompt,
                    &prompts::architect_system(cfg.max_tasks),
                )
                .await
            {
                Ok(raw) => raw,
                Err(CallError::Stopped) => return Ok(()),
                Err(CallError::Failed(e)) => return Err(format!("Komutan plan üretemedi: {e}")),
            };

            self.log_detail(
                "architect",
                "architect",
                "Komutan görev planını üretti",
                truncate_chars(&raw_plan, 6000),
            )
            .await;

            let plan = match protocol::parse_plan(&raw_plan) {
                Ok(plan) => plan,
                Err(error) => {
                    self.log("warn", "architect", format!("Plan çözümlenemedi: {error}"))
                        .await;
                    failures.push(format!("Tur {round}: plan JSON'u geçersiz"));
                    continue;
                }
            };

            if plan.is_complete() || plan.tasks.is_empty() {
                self.log_detail(
                    "ok",
                    "architect",
                    "Komutan projeyi tamamlandı ilan etti",
                    plan.summary,
                )
                .await;
                break;
            }

            let tasks: Vec<PlanTask> = plan
                .tasks
                .into_iter()
                .take(cfg.max_tasks.max(1) as usize)
                .collect();
            self.log(
                "ok",
                "architect",
                format!("{} görev planlandı", tasks.len()),
            )
            .await;

            let mut round_applied = Vec::new();
            for task in tasks {
                if self.is_stopped() {
                    return Ok(());
                }
                self.update_stats(|s| s.tasks_total += 1).await;

                match self.execute_task(&task).await {
                    TaskOutcome::Applied => {
                        completed.push(format!("{} → {}", task.title, task.file));
                        round_applied.push(task.title.clone());
                    }
                    TaskOutcome::Rejected(reason) | TaskOutcome::Skipped(reason) => {
                        failures.push(format!("{} ({}): {reason}", task.title, task.file));
                    }
                    TaskOutcome::NotWritten => {
                        self.update_stats(|s| s.tasks_skipped += 1).await;
                    }
                    TaskOutcome::Stopped => return Ok(()),
                    TaskOutcome::Paused => {
                        self.log(
                            "warn",
                            "system",
                            "Otonom döngü hata limiti nedeniyle duraklatıldı (Paused).",
                        )
                        .await;
                        return Ok(());
                    }
                }
            }

            if cfg.auto_git {
                let gorev = if !round_applied.is_empty() {
                    round_applied.join(", ")
                } else if !plan.summary.trim().is_empty() {
                    plan.summary.trim().to_string()
                } else {
                    "otonom geliştirme".to_string()
                };

                match workspace::git_commit_and_push(
                    &self.root,
                    round,
                    &gorev,
                    cfg.heroku_deploy,
                )
                .await
                {
                    Ok(msg) => {
                        self.log("ok", "system", format!("Otomatik Git: {msg}"))
                            .await;
                    }
                    Err(err) => {
                        self.log("warn", "system", format!("Otomatik Git hatası: {err}"))
                            .await;
                    }
                }
            }
        }

        let stats = {
            let guard = self.shared.lock().await;
            guard
                .projects
                .get(&self.project_key)
                .map(|p| p.stats.clone())
                .unwrap_or_default()
        };
        self.log(
            "ok",
            "system",
            format!(
                "Komutanlık bitti — {} görev uygulandı, {} reddedildi, {} yazılmadı, {} dosya yazıldı",
                stats.tasks_applied, stats.tasks_rejected, stats.tasks_skipped, stats.files_written
            ),
        )
        .await;

        Ok(())
    }

    async fn execute_task(&self, task: &PlanTask) -> TaskOutcome {
        let cfg = self.cfg;
        let root = &self.root;
        let file = task.normalized_file();

        let current: Option<FileText> =
            match workspace::read_existing(root, &file, cfg.context_chars).await {
                Ok(current) => current,
                Err(error) => {
                    self.log("error", "coder", format!("{}: {error}", task.title))
                        .await;
                    self.update_stats(|s| s.tasks_rejected += 1).await;
                    return TaskOutcome::Skipped(error);
                }
            };

        if current.as_ref().is_some_and(|c| c.truncated) {
            let reason = format!(
                "`{file}` dosyası bağlam sınırından ({} karakter) büyük; tamamı gösterilemeden \
                 yeniden yazılamaz. Gelişmiş ayarlardan dosya bağlamını artır.",
                cfg.context_chars
            );
            self.log("warn", "coder", format!("{}: {reason}", task.title))
                .await;
            self.update_stats(|s| s.tasks_rejected += 1).await;
            return TaskOutcome::Skipped(reason);
        }
        let current_text: Option<&str> = current.as_ref().map(|c| c.text.as_str());

        let context_block = workspace::read_context_files(
            root,
            &task.context_files,
            &file,
            CONTEXT_FILES,
            CONTEXT_FILE_CHARS,
        )
        .await;

        self.log_detail(
            "coder",
            "coder",
            format!("Görev: {}", task.title),
            format!(
                "Dosya: {file}\nAmaç: {}\nKriterler: {}",
                task.objective,
                if task.acceptance_criteria.is_empty() {
                    "-".to_string()
                } else {
                    task.acceptance_criteria.join(" | ")
                }
            ),
        )
        .await;

        let mut feedback = String::new();
        let max_attempts = cfg.max_attempts.max(1);
        let mut consecutive_errors: u32 = 0;

        for attempt in 1..=max_attempts {
            if self.is_stopped() {
                return TaskOutcome::Stopped;
            }

            self.log(
                "coder",
                "coder",
                format!("{} — deneme {attempt}/{max_attempts}", task.title),
            )
            .await;

            let prompt = prompts::coder_prompt(
                &cfg.vision,
                &cfg.instructions,
                &self.system_context,
                task,
                current_text,
                &context_block,
                &feedback,
                attempt,
                max_attempts,
            );

            let raw = match self
                .call(
                    "coder",
                    &self.coder,
                    &prompt,
                    &prompts::coder_system(),
                )
                .await
            {
                Ok(raw) => {
                    consecutive_errors = 0;
                    raw
                }
                Err(CallError::Stopped) => return TaskOutcome::Stopped,
                Err(CallError::Failed(error)) => {
                    self.log("error", "coder", format!("İşçi çağrısı başarısız: {error}"))
                        .await;
                    feedback = format!(
                        "Önceki çağrı teknik olarak başarısız oldu: {error}. Aynı görevi yeniden dene."
                    );
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return self.pause_task(task, "İşçi model çağrısı hatası").await;
                    }
                    continue;
                }
            };

            self.log_detail(
                "coder",
                "coder",
                "İşçi kod üretti",
                truncate_chars(&raw, 8000),
            )
            .await;

            let proposal = match protocol::parse_proposal(&raw, &file) {
                Ok(proposal) => {
                    consecutive_errors = 0;
                    proposal
                }
                Err(error) => {
                    self.log("warn", "coder", format!("Kod çözümlenemedi: {error}"))
                        .await;
                    feedback = format!(
                        "Yanıt formatı geçersiz: {error}. FILE ve SUMMARY protokol başlıklarını ve tek bir fenced kod bloğu kullan."
                    );
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return self.pause_task(task, "Kod ayrıştırma hatası").await;
                    }
                    continue;
                }
            };

            let pre = guardrail::precheck(task, &proposal, current_text);
            if !pre.issues.is_empty() {
                let joined = pre.issues.join("\n- ");
                self.log_detail(
                    "warn",
                    "review",
                    format!("Yerel guardrail reddetti (deneme {attempt})"),
                    format!("- {joined}"),
                )
                .await;
                feedback = format!(
                    "Yerel guardrail şu sorunları buldu:\n- {joined}\nDosyanın tamamını, sorunları gidererek yeniden üret."
                );
                self.update_stats(|s| s.revisions += 1).await;
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return self.pause_task(task, "Yerel guardrail hatası").await;
                }
                continue;
            }

            consecutive_errors = 0;

            if !pre.warnings.is_empty() {
                self.log_detail(
                    "warn",
                    "review",
                    "Guardrail uyarıları",
                    pre.warnings.join("\n"),
                )
                .await;
            }

            self.log("review", "review", "Komutan denetliyor…").await;

            let review_prompt = prompts::review_prompt(
                &cfg.vision,
                &self.system_context,
                task,
                &proposal,
                current_text,
                &pre.warnings,
            );

            let verdict_raw = match self
                .call(
                    "review",
                    &self.architect,
                    &review_prompt,
                    &prompts::reviewer_system(),
                )
                .await
            {
                Ok(raw) => {
                    consecutive_errors = 0;
                    raw
                }
                Err(CallError::Stopped) => return TaskOutcome::Stopped,
                Err(CallError::Failed(error)) => {
                    self.log(
                        "error",
                        "review",
                        format!("Denetim çağrısı başarısız: {error}"),
                    )
                    .await;
                    feedback =
                        format!("Denetim teknik olarak başarısız oldu: {error}. Yeniden üret.");
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return self.pause_task(task, "Denetim çağrısı hatası").await;
                    }
                    continue;
                }
            };

            let verdict = protocol::parse_verdict(&verdict_raw);
            let headline = if verdict.approved {
                format!("Komutan ONAYLADI (puan {:.0}/10)", verdict.score)
            } else {
                format!("Komutan REDDETTİ (puan {:.0}/10)", verdict.score)
            };
            self.log_detail(
                if verdict.approved { "ok" } else { "warn" },
                "review",
                headline,
                format!(
                    "{}\n\n{}",
                    verdict.summary,
                    if verdict.issues.is_empty() {
                        String::new()
                    } else {
                        format!("Sorunlar:\n- {}", verdict.issues.join("\n- "))
                    }
                ),
            )
            .await;

            if !verdict.approved {
                self.update_stats(|s| s.revisions += 1).await;
                feedback = format!(
                    "Komutan reddetti.\nGerekçe: {}\nSorunlar:\n- {}\nDüzeltme talimatı: {}",
                    verdict.summary,
                    if verdict.issues.is_empty() {
                        "belirtilmedi".to_string()
                    } else {
                        verdict.issues.join("\n- ")
                    },
                    if verdict.revision.trim().is_empty() {
                        "Sorunları gider ve dosyanın tamamını yeniden üret."
                    } else {
                        verdict.revision.as_str()
                    }
                );
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return self.pause_task(task, "Komutan revizyon / ret hatası").await;
                }
                continue;
            }

            consecutive_errors = 0;

            if !cfg.auto_apply {
                self.log(
                    "warn",
                    "write",
                    "Otomatik uygulama kapalı; dosya yazılmadı.",
                )
                .await;
                return TaskOutcome::NotWritten;
            }

            match self.write_and_verify(&proposal, &mut feedback).await {
                WriteStep::Done => {
                    return TaskOutcome::Applied;
                }
                WriteStep::NotWritten => {
                    return TaskOutcome::NotWritten;
                }
                WriteStep::BuildError => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return self
                            .pause_task(task, "Derleme (cargo check) hatası")
                            .await;
                    }
                    continue;
                }
                WriteStep::WriteError => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return self.pause_task(task, "Dosya yazma hatası").await;
                    }
                    continue;
                }
                WriteStep::Stopped => return TaskOutcome::Stopped,
            }
        }

        self.update_stats(|s| s.tasks_rejected += 1).await;
        self.log(
            "error",
            "review",
            format!(
                "{}: {max_attempts} denemede onaylanmadı, atlandı.",
                task.title
            ),
        )
        .await;

        TaskOutcome::Rejected(format!("{max_attempts} denemede onaylanmadı"))
    }

    /// Writes an approved proposal and, when enabled, gates it on `cargo check`.
    async fn write_and_verify(
        &self,
        proposal: &protocol::Proposal,
        feedback: &mut String,
    ) -> WriteStep {
        let root = &self.root;

        let report =
            match workspace::write_text(root, &proposal.file, &proposal.content, self.cfg.dry_run)
                .await
            {
                Ok(report) => report,
                Err(error) => {
                    self.log("error", "write", format!("Dosya yazılamadı: {error}"))
                        .await;
                    *feedback = format!("Dosya yazılamadı: {error}. Yolu ve içeriği kontrol et.");
                    return WriteStep::WriteError;
                }
            };

        if report.dry_run {
            self.log(
                "warn",
                "write",
                format!(
                    "Dry-run: `{}` yazılmadı ({} bayt olacaktı).",
                    report.path.display(),
                    report.bytes
                ),
            )
            .await;
            return WriteStep::NotWritten;
        }

        let summary = proposal.summary.trim();
        self.log(
            "write",
            "write",
            format!(
                "Yazıldı: `{}` ({} bayt, {}){}",
                report.path.display(),
                report.bytes,
                if report.created {
                    "yeni dosya"
                } else {
                    "güncellendi"
                },
                if summary.is_empty() {
                    String::new()
                } else {
                    format!(" — {summary}")
                }
            ),
        )
        .await;

        if let Some(backup) = report.backup.as_ref() {
            self.log("info", "write", format!("Yedek: {}", backup.display()))
                .await;
        }

        if self.verify_build {
            self.log("info", "write", "cargo check çalışıyor…").await;

            let checked = self
                .until_stopped(workspace::cargo_check(root, CARGO_CHECK_TIMEOUT_SECS))
                .await;

            match checked {
                None => {
                    self.revert(proposal, report.backup.as_deref()).await;
                    return WriteStep::Stopped;
                }
                Some(Err(error)) => {
                    self.log_detail(
                        "error",
                        "write",
                        "Derleme doğrulaması başarısız; değişiklik geri alınıyor",
                        error.clone(),
                    )
                    .await;
                    self.revert(proposal, report.backup.as_deref()).await;
                    self.update_stats(|s| s.revisions += 1).await;
                    *feedback = format_cargo_check_feedback(&error);
                    return WriteStep::BuildError;
                }
                Some(Ok(())) => {
                    self.log("ok", "write", "cargo check temiz.").await;
                }
            }
        }

        self.update_stats(|s| {
            s.tasks_applied += 1;
            s.files_written += 1;
        })
        .await;
        WriteStep::Done
    }

    async fn revert(&self, proposal: &protocol::Proposal, backup: Option<&std::path::Path>) {
        let result = match backup {
            Some(backup) => workspace::restore(&self.root, &proposal.file, backup).await,
            None => workspace::remove(&self.root, &proposal.file).await,
        };
        if let Err(error) = result {
            self.log(
                "error",
                "write",
                format!(
                    "Geri alma başarısız, `{}` elle kontrol edilmeli: {error}",
                    proposal.file
                ),
            )
            .await;
        }
    }
}

enum WriteStep {
    Done,
    NotWritten,
    BuildError,
    WriteError,
    Stopped,
}

/// Condenses cargo check errors for the coder model, falling back to the raw output
/// if no structured error blocks were extracted.
pub fn format_cargo_check_feedback(error: &str) -> String {
    let condensed = condense_cargo_errors(error);
    let final_err = if condensed.trim().is_empty() {
        error.trim()
    } else {
        condensed.trim()
    };
    format!(
        "`cargo check` hataları:\n{final_err}\nBunları düzelterek dosyanın tamamını yeniden üret."
    )
}

/// Resolves once the stop flag is raised.
async fn wait_for_stop(stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::sync::Mutex as TokioMutex;

    struct MockProvider {
        label_text: String,
        responses: TokioMutex<Vec<Result<String, String>>>,
    }

    impl MockProvider {
        fn new(label: &str, responses: Vec<Result<String, String>>) -> Self {
            Self {
                label_text: label.to_string(),
                responses: TokioMutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl ModelProvider for MockProvider {
        fn label(&self) -> String {
            self.label_text.clone()
        }

        async fn complete(&self, _prompt: &str, _system_prompt: &str) -> Result<String, String> {
            let mut list = self.responses.lock().await;
            if list.is_empty() {
                Ok("mock ok".to_string())
            } else {
                list.remove(0)
            }
        }
    }

    #[tokio::test]
    async fn wait_for_stop_resolves_after_flag() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.store(true, Ordering::SeqCst);
        });
        tokio::time::timeout(Duration::from_secs(3), wait_for_stop(&stop))
            .await
            .expect("stop flag must wake the waiter");
    }

    #[test]
    fn detects_failover_errors() {
        assert!(is_failover_error("HTTP 429: Too Many Requests"));
        assert!(is_failover_error("rate limit exceeded"));
        assert!(is_failover_error("request timed out after 30s"));
        assert!(is_failover_error("connection refused by peer"));
        assert!(is_failover_error("zaman aşımı hatası"));
        assert!(is_failover_error("HTTP 503 Service Unavailable"));
        assert!(!is_failover_error("Syntax error: missing semicolon"));
    }

    #[tokio::test]
    async fn failover_chain_advances_on_rate_limit_and_timeout() {
        let p1 = Box::new(MockProvider::new(
            "Primary",
            vec![Err("HTTP 429 Too Many Requests".to_string())],
        ));
        let p2 = Box::new(MockProvider::new(
            "Fallback1",
            vec![Err("Connection timed out".to_string())],
        ));
        let p3 = Box::new(MockProvider::new(
            "Fallback2",
            vec![Ok("success from fallback 2".to_string())],
        ));

        let chain = FailoverChain::new("TestRole", vec![p1, p2, p3]).unwrap();

        let start_idx = chain.current.load(Ordering::Relaxed);
        assert_eq!(start_idx, 0);

        let mut final_response = None;
        for i in start_idx..chain.providers.len() {
            let res = chain.providers[i].complete("prompt", "sys").await;
            match res {
                Ok(text) => {
                    chain.current.store(i, Ordering::Relaxed);
                    final_response = Some(text);
                    break;
                }
                Err(err) if is_failover_error(&err) && i + 1 < chain.providers.len() => {
                    chain.current.store(i + 1, Ordering::Relaxed);
                    continue;
                }
                Err(err) => panic!("Unexpected failure: {err}"),
            }
        }

        assert_eq!(final_response.as_deref(), Some("success from fallback 2"));
        assert_eq!(chain.current.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn format_cargo_check_feedback_condenses_and_falls_back() {
        let error_with_warnings = r#"
warning: unused variable `x`
 --> src/main.rs:2:5

error[E0425]: cannot find value `foo` in this scope
 --> src/main.rs:10:5
  |
10 |     foo();
  |     ^^^ not found in this scope

warning: 1 warning emitted
"#;
        let fb = format_cargo_check_feedback(error_with_warnings);
        assert!(!fb.contains("unused variable"));
        assert!(!fb.contains("1 warning emitted"));
        assert!(fb.contains("error[E0425]: cannot find value `foo` in this scope"));
        assert!(fb.contains("`cargo check` hataları:"));

        // Fallback test when no structured errors are present
        let unstructured_err = "failed to load manifest at /path/Cargo.toml";
        let fb_fallback = format_cargo_check_feedback(unstructured_err);
        assert!(fb_fallback.contains(unstructured_err));
        assert!(fb_fallback.contains("`cargo check` hataları:"));
    }
}
