//! Dosya sistemi gunlugu: bir komutanlik kosusunu arayuzden bagimsiz izlemek icin.
//!
//! Her kosu `.swarm/runs/run-<zaman>/` altinda kendi klasorunu alir:
//! - `run.json`     : kosunun durumu, baslangic/bitis zamani, secilen modeller, istatistikler
//! - `events.jsonl` : ajan olaylari (plan, gorev, yazma, dogrulama, inceleme, hata)
//!
//! Ayrica `.swarm/last-run.json` her zaman en son kosunun ozetini tutar; boylece
//! kosu surerken ve bittikten sonra durum tek dosyadan okunabilir.
//!
//! Gizli bilgi yazilmaz: API anahtarlari ve istem icerigi bu gunluge girmez.

use crate::config::{AgentConfig, ProviderConfig};
use crate::state::{now_iso, LogEvent, RunStats};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::sync::Mutex;

type RunMap = Mutex<HashMap<String, PathBuf>>;

fn runs() -> &'static RunMap {
    static MAP: OnceLock<RunMap> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runs_dir(root: &Path) -> PathBuf {
    root.join(".swarm").join("runs")
}

fn last_run_path(root: &Path) -> PathBuf {
    root.join(".swarm").join("last-run.json")
}

/// Modelin kimligini (anahtar olmadan) ozetler.
fn provider_summary(cfg: &ProviderConfig) -> serde_json::Value {
    serde_json::json!({
        "source": cfg.source,
        "apiKind": cfg.api_kind,
        "ideKind": cfg.ide_kind,
        "model": cfg.model,
        "hasApiKey": cfg.api_key.as_deref().map(|v| !v.trim().is_empty()).unwrap_or(false),
    })
}

async fn write_json(path: &Path, value: &serde_json::Value) {
    if let Ok(bytes) = serde_json::to_vec_pretty(value) {
        let _ = tokio::fs::write(path, bytes).await;
    }
}

/// Yeni bir kosu baslatir. `.swarm/runs` olusturulamazsa `None` doner ve
/// gunluk sessizce devre disi kalir; komutanlik calismaya devam eder.
pub async fn begin(root: &Path, cfg: &AgentConfig) -> Option<String> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let run_id = format!("run-{stamp}");
    let dir = runs_dir(root).join(&run_id);
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return None;
    }

    let meta = serde_json::json!({
        "runId": run_id,
        "status": "running",
        "project": root.to_string_lossy(),
        "startedAt": now_iso(),
        "finishedAt": serde_json::Value::Null,
        "architect": provider_summary(&cfg.architect),
        "coder": provider_summary(&cfg.coder),
        "limits": {
            "maxAttempts": cfg.max_attempts,
            "maxTasks": cfg.max_tasks,
            "maxRounds": cfg.max_rounds,
            "autoApply": cfg.auto_apply,
            "verifyBuild": cfg.verify_build,
            "dryRun": cfg.dry_run,
        },
        "stats": serde_json::Value::Null,
    });
    write_json(&dir.join("run.json"), &meta).await;
    write_json(&last_run_path(root), &meta).await;

    runs()
        .lock()
        .await
        .insert(root.to_string_lossy().to_string(), dir);
    Some(run_id)
}

/// Bir ajan olayini kosunun `events.jsonl` dosyasina ekler.
pub async fn record(event: &LogEvent) {
    if event.project_path.is_empty() {
        return;
    }
    let dir = {
        let map = runs().lock().await;
        map.get(&event.project_path).cloned()
    };
    let Some(dir) = dir else {
        return;
    };

    let line = serde_json::json!({
        "at": now_iso(),
        "level": event.level,
        "phase": event.phase,
        "message": event.message,
        "detail": event.detail,
    });
    let Ok(text) = serde_json::to_string(&line) else {
        return;
    };
    let path = dir.join("events.jsonl");
    if let Ok(mut file) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        use tokio::io::AsyncWriteExt;
        let _ = file.write_all(format!("{text}\n").as_bytes()).await;
        let _ = file.flush().await;
    }
}

/// Kosuyu kapatir; `run.json` ve `.swarm/last-run.json` guncellenir.
pub async fn finish(project: &str, status: &str, stats: &RunStats) {
    let dir = {
        let mut map = runs().lock().await;
        map.remove(project)
    };
    let Some(dir) = dir else {
        return;
    };

    let mut meta = tokio::fs::read(dir.join("run.json"))
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({ "project": project }));

    if let Some(obj) = meta.as_object_mut() {
        obj.insert("status".into(), serde_json::json!(status));
        obj.insert("finishedAt".into(), serde_json::json!(now_iso()));
        obj.insert("stats".into(), serde_json::to_value(stats).unwrap_or_default());
    }
    write_json(&dir.join("run.json"), &meta).await;

    let root = Path::new(project);
    write_json(&last_run_path(root), &meta).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, ProviderConfig};

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "swarm-journal-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_for(root: &Path) -> AgentConfig {
        let provider = ProviderConfig {
            model: "test-model".to_string(),
            api_key: Some("sk-gizli-anahtar".to_string()),
            ..Default::default()
        };
        AgentConfig {
            project_path: root.to_string_lossy().to_string(),
            vision: "test".to_string(),
            instructions: String::new(),
            architect: provider.clone(),
            coder: provider,
            max_attempts: 3,
            max_tasks: 5,
            max_rounds: 2,
            context_chars: 8000,
            tree_entries: 120,
            auto_apply: true,
            dry_run: false,
            verify_build: false,
            auto_git: false,
            heroku_deploy: false,
        }
    }

    #[tokio::test]
    async fn writes_run_meta_events_and_finishes() {
        let root = temp_root("meta");
        let cfg = config_for(&root);
        let run_id = begin(&root, &cfg).await.expect("kosu baslamali");
        let dir = runs_dir(&root).join(&run_id);
        assert!(dir.join("run.json").is_file());

        let mut event = LogEvent::new("info", "system", "deneme");
        event.project_path = root.to_string_lossy().to_string();
        record(&event).await;

        let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(events.contains("deneme"));

        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(meta["status"], "running");
        // API anahtari gunluge sizmamali.
        assert!(!std::fs::read_to_string(dir.join("run.json"))
            .unwrap()
            .contains("sk-gizli"));

        let stats = RunStats {
            tasks_total: 2,
            tasks_applied: 1,
            ..Default::default()
        };
        finish(&root.to_string_lossy(), "done", &stats).await;

        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(meta["status"], "done");
        assert_eq!(meta["stats"]["tasksTotal"], 2);
        let last: serde_json::Value =
            serde_json::from_slice(&std::fs::read(last_run_path(&root)).unwrap()).unwrap();
        assert_eq!(last["status"], "done");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn record_without_begin_is_a_no_op() {
        let mut event = LogEvent::new("info", "system", "yalniz");
        event.project_path = "/tmp/olmayan-proje".to_string();
        record(&event).await;
    }
}
