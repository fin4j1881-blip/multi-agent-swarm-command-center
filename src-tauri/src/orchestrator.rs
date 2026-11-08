//! The autonomous loop: plan → code → guardrail → review → write.
//!
//! Every step is cancellable through `AppState::stop`; the flag is checked
//! between model calls so a "Durdur" click never kills a half-written file.

use crate::config::AgentConfig;
use crate::guardrail;
use crate::prompts;
use crate::protocol::{self, PlanTask};
use crate::provider::{self, ModelProvider};
use crate::state::{emit_log, emit_status, now_iso, LogEvent, SharedState};
use crate::workspace::{self, truncate_chars};
use std::path::Path;
use tauri::AppHandle;

enum TaskOutcome {
    Applied,
    Rejected(String),
    Skipped(String),
    Stopped,
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

async fn run_inner(app: &AppHandle, shared: &SharedState, cfg: &AgentConfig) -> Result<(), String> {
    let root = cfg.root()?;
    emit_log(
        app,
        shared,
        LogEvent::new("info", "system", format!("Proje kökü: {}", root.display())),
    )
    .await;

    let architect = provider::build(&cfg.architect, &root)
        .map_err(|e| format!("Komutan sağlayıcısı hazırlanamadı: {e}"))?;
    let coder = provider::build(&cfg.coder, &root)
        .map_err(|e| format!("İşçi sağlayıcısı hazırlanamadı: {e}"))?;

    emit_log(
        app,
        shared,
        LogEvent::new("info", "system", format!("Komutan → {}", architect.label())),
    )
    .await;
    emit_log(
        app,
        shared,
        LogEvent::new("info", "system", format!("İşçi → {}", coder.label())),
    )
    .await;

    let mut completed: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let rounds = cfg.max_rounds.max(1);

    for round in 1..=rounds {
        if is_stopped(shared).await {
            emit_log(app, shared, LogEvent::new("warn", "system", "Durduruldu.")).await;
            return Ok(());
        }

        {
            let mut guard = shared.lock().await;
            guard.stats.rounds = round;
        }

        emit_log(
            app,
            shared,
            LogEvent::new(
                "architect",
                "architect",
                format!("Tur {round}/{rounds} — proje ağacı taranıyor"),
            ),
        )
        .await;

        let tree = workspace::scan_tree(&root, cfg.tree_entries);
        let prompt = prompts::plan_prompt(&cfg.vision, &tree, &completed, &failures, round, rounds);

        let raw_plan = architect
            .complete(&prompt, &prompts::architect_system(cfg.max_tasks))
            .await
            .map_err(|e| format!("Komutan plan üretemedi: {e}"))?;

        emit_log(
            app,
            shared,
            LogEvent::new("architect", "architect", "Komutan görev planını üretti")
                .detail(truncate_chars(&raw_plan, 6000)),
        )
        .await;

        let plan = match protocol::parse_plan(&raw_plan) {
            Ok(plan) => plan,
            Err(error) => {
                emit_log(
                    app,
                    shared,
                    LogEvent::new("warn", "architect", format!("Plan çözümlenemedi: {error}")),
                )
                .await;
                failures.push(format!("Tur {round}: plan JSON'u geçersiz"));
                continue;
            }
        };

        if plan.is_complete() || plan.tasks.is_empty() {
            emit_log(
                app,
                shared,
                LogEvent::new("ok", "architect", "Komutan projeyi tamamlandı ilan etti")
                    .detail(plan.summary),
            )
            .await;
            break;
        }

        let task_count = plan.tasks.len();
        emit_log(
            app,
            shared,
            LogEvent::new("ok", "architect", format!("{task_count} görev planlandı")),
        )
        .await;

        for task in plan.tasks.into_iter().take(cfg.max_tasks.max(1) as usize) {
            if is_stopped(shared).await {
                return Ok(());
            }
            {
                let mut guard = shared.lock().await;
                guard.stats.tasks_total += 1;
            }

            let outcome = execute_task(
                app,
                shared,
                cfg,
                &root,
                architect.as_ref(),
                coder.as_ref(),
                &task,
            )
            .await;

            match outcome {
                TaskOutcome::Applied => completed.push(format!("{} → {}", task.title, task.file)),
                TaskOutcome::Rejected(reason) => {
                    failures.push(format!("{} ({}): {reason}", task.title, task.file))
                }
                TaskOutcome::Skipped(reason) => {
                    failures.push(format!("{} ({}): {reason}", task.title, task.file))
                }
                TaskOutcome::Stopped => return Ok(()),
            }
        }
    }

    let stats = {
        let guard = shared.lock().await;
        guard.stats.clone()
    };
    emit_log(
        app,
        shared,
        LogEvent::new(
            "ok",
            "system",
            format!(
                "Komutanlık bitti — {} görev uygulandı, {} reddedildi, {} dosya yazıldı",
                stats.tasks_applied, stats.tasks_rejected, stats.files_written
            ),
        ),
    )
    .await;

    Ok(())
}

async fn execute_task(
    app: &AppHandle,
    shared: &SharedState,
    cfg: &AgentConfig,
    root: &Path,
    architect: &dyn ModelProvider,
    coder: &dyn ModelProvider,
    task: &PlanTask,
) -> TaskOutcome {
    let file = task.normalized_file();
    let current = workspace::read_text(root, &file, cfg.context_chars)
        .await
        .ok();

    emit_log(
        app,
        shared,
        LogEvent::new("coder", "coder", format!("Görev: {}", task.title)).detail(format!(
            "Dosya: {file}\nAmaç: {}\nKriterler: {}",
            task.objective,
            if task.acceptance_criteria.is_empty() {
                "-".to_string()
            } else {
                task.acceptance_criteria.join(" | ")
            }
        )),
    )
    .await;

    let mut feedback = String::new();
    let max_attempts = cfg.max_attempts.max(1);

    for attempt in 1..=max_attempts {
        if is_stopped(shared).await {
            return TaskOutcome::Stopped;
        }

        emit_log(
            app,
            shared,
            LogEvent::new(
                "coder",
                "coder",
                format!("{} — deneme {attempt}/{max_attempts}", task.title),
            ),
        )
        .await;

        let prompt = prompts::coder_prompt(
            &cfg.vision,
            task,
            current.as_deref(),
            &feedback,
            attempt,
            max_attempts,
        );

        let raw = match coder.complete(&prompt, &prompts::coder_system()).await {
            Ok(raw) => raw,
            Err(error) => {
                emit_log(
                    app,
                    shared,
                    LogEvent::new("error", "coder", format!("İşçi çağrısı başarısız: {error}")),
                )
                .await;
                feedback = format!(
                    "Önceki çağrı teknik olarak başarısız oldu: {error}. Aynı görevi yeniden dene."
                );
                continue;
            }
        };

        emit_log(
            app,
            shared,
            LogEvent::new("coder", "coder", "İşçi kod üretti").detail(truncate_chars(&raw, 8000)),
        )
        .await;

        let proposal = match protocol::parse_proposal(&raw, &file) {
            Ok(proposal) => proposal,
            Err(error) => {
                emit_log(
                    app,
                    shared,
                    LogEvent::new("warn", "coder", format!("Kod çözümlenemedi: {error}")),
                )
                .await;
                feedback = format!(
                    "Yanıt formatı geçersiz: {error}. `### FILE:`, `### SUMMARY:` ve tek bir fenced kod bloğu kullan."
                );
                continue;
            }
        };

        let pre = guardrail::precheck(root, task, &proposal, current.as_deref());
        if !pre.issues.is_empty() {
            let joined = pre.issues.join("\n- ");
            emit_log(
                app,
                shared,
                LogEvent::new(
                    "warn",
                    "review",
                    format!("Yerel guardrail reddetti (deneme {attempt})"),
                )
                .detail(format!("- {joined}")),
            )
            .await;
            feedback = format!(
                "Yerel guardrail şu sorunları buldu:\n- {joined}\nDosyanın tamamını, sorunları gidererek yeniden üret."
            );
            bump_revision(shared).await;
            continue;
        }

        if !pre.warnings.is_empty() {
            emit_log(
                app,
                shared,
                LogEvent::new("warn", "review", "Guardrail uyarıları")
                    .detail(pre.warnings.join("\n")),
            )
            .await;
        }

        emit_log(
            app,
            shared,
            LogEvent::new("review", "review", "Komutan denetliyor…"),
        )
        .await;

        let review_prompt = prompts::review_prompt(
            &cfg.vision,
            task,
            &proposal,
            current.as_deref(),
            &pre.warnings,
        );

        let verdict_raw = match architect
            .complete(&review_prompt, &prompts::reviewer_system())
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                emit_log(
                    app,
                    shared,
                    LogEvent::new(
                        "error",
                        "review",
                        format!("Denetim çağrısı başarısız: {error}"),
                    ),
                )
                .await;
                feedback = format!("Denetim teknik olarak başarısız oldu: {error}. Yeniden üret.");
                continue;
            }
        };

        let verdict = protocol::parse_verdict(&verdict_raw);
        emit_log(
            app,
            shared,
            LogEvent::new(
                if verdict.approved { "ok" } else { "warn" },
                "review",
                if verdict.approved {
                    format!("Komutan ONAYLADI (puan {:.0}/10)", verdict.score)
                } else {
                    format!("Komutan REDDETTİ (puan {:.0}/10)", verdict.score)
                },
            )
            .detail(format!(
                "{}\n\n{}",
                verdict.summary,
                if verdict.issues.is_empty() {
                    String::new()
                } else {
                    format!("Sorunlar:\n- {}", verdict.issues.join("\n- "))
                }
            )),
        )
        .await;

        if !verdict.approved {
            bump_revision(shared).await;
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
            emit_log(
                app,
                shared,
                LogEvent::new(
                    "warn",
                    "write",
                    "Otomatik uygulama kapalı; dosya yazılmadı.",
                ),
            )
            .await;
            return TaskOutcome::Skipped("otomatik uygulama kapalı".into());
        }

        match workspace::write_text(root, &proposal.file, &proposal.content, cfg.dry_run).await {
            Ok(report) => {
                if report.dry_run {
                    emit_log(
                        app,
                        shared,
                        LogEvent::new(
                            "warn",
                            "write",
                            format!(
                                "Dry-run: `{}` yazılmadı ({} bayt olacaktı).",
                                report.path.display(),
                                report.bytes
                            ),
                        ),
                    )
                    .await;
                    return TaskOutcome::Skipped("dry-run".into());
                }

                emit_log(
                    app,
                    shared,
                    LogEvent::new(
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
                            if proposal.summary.trim().is_empty() {
                                String::new()
                            } else {
                                format!(" — {}", proposal.summary.trim())
                            }
                        ),
                    ),
                )
                .await;

                if let Some(backup) = report.backup.as_ref() {
                    emit_log(
                        app,
                        shared,
                        LogEvent::new("info", "write", format!("Yedek: {}", backup.display())),
                    )
                    .await;
                }

                if cfg.verify_build {
                    emit_log(
                        app,
                        shared,
                        LogEvent::new("info", "write", "cargo check çalışıyor…"),
                    )
                    .await;

                    if let Err(error) = workspace::cargo_check(root, 300).await {
                        emit_log(
                            app,
                            shared,
                            LogEvent::new(
                                "error",
                                "write",
                                "Derleme doğrulaması başarısız; değişiklik geri alınıyor",
                            )
                            .detail(error.clone()),
                        )
                        .await;

                        if let Some(backup) = report.backup.as_ref() {
                            let _ = workspace::restore(root, &proposal.file, backup).await;
                        } else {
                            let _ = workspace::remove(root, &proposal.file).await;
                        }

                        bump_revision(shared).await;
                        feedback = format!(
                            "`cargo check` hataları:\n{error}\nBunları düzelterek dosyanın tamamını yeniden üret."
                        );
                        continue;
                    }

                    emit_log(
                        app,
                        shared,
                        LogEvent::new("ok", "write", "cargo check temiz."),
                    )
                    .await;
                }

                {
                    let mut guard = shared.lock().await;
                    guard.stats.tasks_applied += 1;
                    guard.stats.files_written += 1;
                }
                return TaskOutcome::Applied;
            }
            Err(error) => {
                emit_log(
                    app,
                    shared,
                    LogEvent::new("error", "write", format!("Dosya yazılamadı: {error}")),
                )
                .await;
                feedback = format!("Dosya yazılamadı: {error}. Yolu ve içeriği kontrol et.");
                continue;
            }
        }
    }

    {
        let mut guard = shared.lock().await;
        guard.stats.tasks_rejected += 1;
    }
    emit_log(
        app,
        shared,
        LogEvent::new(
            "error",
            "review",
            format!(
                "{}: {max_attempts} denemede onaylanmadı, atlandı.",
                task.title
            ),
        ),
    )
    .await;

    TaskOutcome::Rejected(format!("{max_attempts} denemede onaylanmadı"))
}

async fn is_stopped(shared: &SharedState) -> bool {
    let guard = shared.lock().await;
    guard.should_stop()
}

async fn bump_revision(shared: &SharedState) {
    let mut guard = shared.lock().await;
    guard.stats.revisions += 1;
}
