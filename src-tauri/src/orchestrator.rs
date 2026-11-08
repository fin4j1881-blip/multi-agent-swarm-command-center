//! The autonomous loop: plan → code → guardrail → review → write.
//!
//! Stopping is cooperative but prompt: a "Durdur" click abandons an in-flight
//! model call (HTTP request or CLI process) immediately. Files are only ever
//! written between model calls, so a stop can never leave a half-written file.

use crate::config::AgentConfig;
use crate::guardrail;
use crate::prompts;
use crate::protocol::{self, PlanTask};
use crate::provider::{self, ModelProvider};
use crate::state::{emit_log, emit_status, now_iso, LogEvent, RunStats, SharedState};
use crate::util::truncate_chars;
use crate::workspace::{self, FileText};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;

/// How many of the architect's `contextFiles` are shown to the coder, and how
/// much of each.
const CONTEXT_FILES: usize = 3;
const CONTEXT_FILE_CHARS: usize = 6_000;
const CARGO_CHECK_TIMEOUT_SECS: u64 = 300;

enum TaskOutcome {
    Applied,
    Rejected(String),
    /// Could not be attempted; reported back to the architect.
    Skipped(String),
    /// Approved, but nothing was written (dry-run / auto-apply off).
    NotWritten,
    Stopped,
}

enum CallError {
    Stopped,
    Failed(String),
}

pub async fn run(app: AppHandle, shared: SharedState, cfg: AgentConfig) {
    let result = run_inner(&app, &shared, &cfg).await;

    if let Err(error) = result {
        emit_log(
            &app,
            &shared,
            LogEvent::new("error", "system", format!("Komutanlık durdu: {error}")),
        )
        .await;
    }

    {
        let mut guard = shared.lock().await;
        guard.running = false;
        guard.stats.finished_at = Some(now_iso());
    }
    emit_status(&app, false);
}

struct Runner<'a> {
    app: &'a AppHandle,
    shared: &'a SharedState,
    cfg: &'a AgentConfig,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    architect: Box<dyn ModelProvider>,
    coder: Box<dyn ModelProvider>,
    verify_build: bool,
}

async fn run_inner(app: &AppHandle, shared: &SharedState, cfg: &AgentConfig) -> Result<(), String> {
    let root = cfg.root()?;
    let stop = shared.lock().await.stop.clone();

    let architect = provider::build(&cfg.architect, &root, "Komutan")
        .map_err(|e| format!("Komutan sağlayıcısı hazırlanamadı: {e}"))?;
    let coder = provider::build(&cfg.coder, &root, "İşçi")
        .map_err(|e| format!("İşçi sağlayıcısı hazırlanamadı: {e}"))?;

    let mut runner = Runner {
        app,
        shared,
        cfg,
        root,
        stop,
        architect,
        coder,
        verify_build: cfg.verify_build,
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

    runner.check_build_baseline().await;
    runner.run_rounds().await
}

impl Runner<'_> {
    async fn log(&self, level: &str, phase: &str, message: impl Into<String>) {
        emit_log(self.app, self.shared, LogEvent::new(level, phase, message)).await;
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
            LogEvent::new(level, phase, message).detail(detail),
        )
        .await;
    }

    fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    async fn update_stats(&self, apply: impl FnOnce(&mut RunStats)) {
        apply(&mut self.shared.lock().await.stats);
    }

    /// Runs `work` until it finishes or the user presses "Durdur".
    /// Dropping the future cancels HTTP requests and kills CLI children.
    async fn until_stopped<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            output = work => Some(output),
            _ = wait_for_stop(&self.stop) => None,
        }
    }

    /// One model call, cancellable, with a log line when the fallback chain
    /// moves to another model.
    async fn call(
        &self,
        phase: &str,
        provider: &dyn ModelProvider,
        prompt: &str,
        system: &str,
    ) -> Result<String, CallError> {
        let before = provider.label();
        let result = self.until_stopped(provider.complete(prompt, system)).await;
        let after = provider.label();
        if before != after {
            self.log("warn", phase, format!("Model değişti: {before} → {after}"))
                .await;
        }
        match result {
            None => Err(CallError::Stopped),
            Some(Ok(text)) => Ok(text),
            Some(Err(error)) => Err(CallError::Failed(error)),
        }
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

        // Without writes the project never changes, so more rounds would only
        // make the architect plan the same tasks again.
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
                    self.architect.as_ref(),
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

            for task in tasks {
                if self.is_stopped() {
                    return Ok(());
                }
                self.update_stats(|s| s.tasks_total += 1).await;

                match self.execute_task(&task).await {
                    TaskOutcome::Applied => {
                        completed.push(format!("{} → {}", task.title, task.file));
                    }
                    TaskOutcome::Rejected(reason) | TaskOutcome::Skipped(reason) => {
                        failures.push(format!("{} ({}): {reason}", task.title, task.file));
                    }
                    TaskOutcome::NotWritten => {
                        self.update_stats(|s| s.tasks_skipped += 1).await;
                    }
                    TaskOutcome::Stopped => return Ok(()),
                }
            }
        }

        let stats = self.shared.lock().await.stats.clone();
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

        // A missing file is a new file; anything else unreadable is an error —
        // treating it as "new" could overwrite a file we failed to read.
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

        // The coder rewrites whole files. Showing it a clipped file would make
        // it silently delete everything past the cut.
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
                    self.coder.as_ref(),
                    &prompt,
                    &prompts::coder_system(),
                )
                .await
            {
                Ok(raw) => raw,
                Err(CallError::Stopped) => return TaskOutcome::Stopped,
                Err(CallError::Failed(error)) => {
                    self.log("error", "coder", format!("İşçi çağrısı başarısız: {error}"))
                        .await;
                    feedback = format!(
                        "Önceki çağrı teknik olarak başarısız oldu: {error}. Aynı görevi yeniden dene."
                    );
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
                Ok(proposal) => proposal,
                Err(error) => {
                    self.log("warn", "coder", format!("Kod çözümlenemedi: {error}"))
                        .await;
                    feedback = format!(
                        "Yanıt formatı geçersiz: {error}. `### FILE:`, `### SUMMARY:` ve tek bir fenced kod bloğu kullan."
                    );
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
                continue;
            }

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

            let review_prompt =
                prompts::review_prompt(&cfg.vision, task, &proposal, current_text, &pre.warnings);

            let verdict_raw = match self
                .call(
                    "review",
                    self.architect.as_ref(),
                    &review_prompt,
                    &prompts::reviewer_system(),
                )
                .await
            {
                Ok(raw) => raw,
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
                continue;
            }

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
                WriteStep::Done => return TaskOutcome::Applied,
                WriteStep::NotWritten => return TaskOutcome::NotWritten,
                WriteStep::Retry => continue,
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
                    return WriteStep::Retry;
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
                    // Unverified code must not survive a stop.
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
                    *feedback = format!(
                        "`cargo check` hataları:\n{error}\nBunları düzelterek dosyanın tamamını yeniden üret."
                    );
                    return WriteStep::Retry;
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
    Retry,
    Stopped,
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
}
