//! Tauri v2 entry point and IPC surface.

mod api_provider;
mod config;
mod guardrail;
mod ide_bridge;
mod models;
mod orchestrator;
mod prompts;
mod protocol;
mod provider;
mod state;
mod util;
mod workspace;

use config::{AgentConfig, ProviderConfig};
use state::{
    emit_log, emit_status, now_iso, AppState, LogEvent, SharedState, StatsPayload, StatusPayload,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Mutex;

/// Opens the native folder picker and returns the absolute path.
#[tauri::command]
async fn select_folder(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |folder| {
        let _ = tx.send(folder.map(|path| path.to_string()));
    });
    rx.await
        .map_err(|_| "Klasör seçimi iptal edildi.".to_string())
}

/// Validates the config, flips the runtime state and spawns the agent loop.
#[tauri::command]
async fn start_command_center(
    app: AppHandle,
    state: State<'_, SharedState>,
    config: AgentConfig,
) -> Result<String, String> {
    let root = config.validate()?;

    let project_key = root.to_string_lossy().to_string();

    {
        let mut guard = state.lock().await;
        let p = guard.projects.entry(project_key.clone()).or_default();
        if p.running {
            return Err("Bu proje için komutanlık zaten çalışıyor.".into());
        }
        p.running = true;
        p.stop.store(false, Ordering::SeqCst);
        p.logs.clear();
        p.stats = Default::default();
        p.stats.started_at = Some(now_iso());
    }

    emit_status(&app, &project_key, true);
    emit_log(
        &app,
        state.inner(),
        LogEvent::new(
            "info",
            "system",
            format!("Komutanlık başlatıldı → {}", root.display()),
        )
        .with_project(&project_key),
    )
    .await;

    let shared: SharedState = state.inner().clone();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        orchestrator::run(handle, shared, config).await;
    });

    Ok(project_key)
}

/// Requests a cooperative stop for a given project (or all running projects if omitted).
#[tauri::command]
async fn stop_command_center(
    app: AppHandle,
    state: State<'_, SharedState>,
    project_path: Option<String>,
) -> Result<(), String> {
    let mut guard = state.lock().await;
    let targets: Vec<String> = if let Some(path) = project_path.filter(|p| !p.trim().is_empty()) {
        vec![path]
    } else {
        guard.projects.keys().cloned().collect()
    };

    for target in targets {
        if let Some(p) = guard.projects.get_mut(&target) {
            if p.running {
                p.stop.store(true, Ordering::SeqCst);
                let _ = app.emit(
                    "agent-log",
                    &LogEvent::new(
                        "warn",
                        "system",
                        "Durdurma isteği alındı; mevcut adım bitince duracak.",
                    )
                    .with_project(&target),
                );
            }
        }
    }
    Ok(())
}

#[tauri::command]
async fn get_status(
    state: State<'_, SharedState>,
    project_path: Option<String>,
) -> Result<StatusPayload, String> {
    let guard = state.lock().await;
    let key = project_path.unwrap_or_default();
    if let Some(p) = guard.projects.get(&key) {
        Ok(StatusPayload {
            running: p.running,
            stats: p.stats.clone(),
            logs: p.logs.iter().cloned().collect(),
        })
    } else {
        Ok(StatusPayload {
            running: false,
            stats: Default::default(),
            logs: Vec::new(),
        })
    }
}

/// Lightweight variant of `get_status` for frequent polling: no log history.
#[tauri::command]
async fn get_stats(
    state: State<'_, SharedState>,
    project_path: Option<String>,
) -> Result<StatsPayload, String> {
    let guard = state.lock().await;
    let key = project_path.unwrap_or_default();
    if let Some(p) = guard.projects.get(&key) {
        Ok(StatsPayload {
            running: p.running,
            stats: p.stats.clone(),
        })
    } else {
        Ok(StatsPayload {
            running: false,
            stats: Default::default(),
        })
    }
}

/// Directory model/CLI probes run in: the project if given, else the cwd.
fn probe_dir(project_path: Option<String>) -> std::path::PathBuf {
    project_path
        .filter(|p| !p.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// One-shot connectivity probe for either provider type (UI "Test" button).
#[tauri::command]
async fn check_provider(
    config: ProviderConfig,
    project_path: Option<String>,
) -> Result<String, String> {
    config.validate("Sağlayıcı")?;

    let provider = provider::build(&config, &probe_dir(project_path), "Test")?;
    let answer = provider
        .complete(
            "Bağlantı testi. Sadece OK yaz.",
            "Sen bir bağlantı testisin. Tek kelime yanıt ver.",
        )
        .await?;

    let preview: String = answer.trim().chars().take(200).collect();
    Ok(format!("{} → {preview}", provider.label()))
}

/// Lists models for a provider so the UI can render a dropdown instead of a
/// free-text field. Also used to auto-fill endpoint information.
#[tauri::command]
async fn list_models(
    config: ProviderConfig,
    project_path: Option<String>,
) -> Result<models::ModelCatalog, String> {
    models::list_models(&config, &probe_dir(project_path)).await
}

/// Opens a new native application window from within the same Tauri process.
/// This completely avoids multi-process WebKit/NVIDIA buffer collisions and flickering.
#[tauri::command]
async fn open_new_window(app: AppHandle) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(2);

    let idx = COUNTER.fetch_add(1, Ordering::Relaxed);
    let label = format!("window-{idx}");

    tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App("index.html".into()))
        .title("Swarm Command Center")
        .inner_size(1320.0, 880.0)
        .min_inner_size(1000.0, 640.0)
        .resizable(true)
        .center()
        .build()
        .map_err(|e| format!("Yeni pencere açılamadı: {e}"))?;

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoProfileResult {
    pub vision: String,
    pub instructions: String,
}

/// Automatically reads the project tree and key manifest/readme files, then
/// asks the architect/selected provider to propose a vision and instructions.
#[tauri::command]
async fn analyze_project(
    project_path: String,
    config: ProviderConfig,
) -> Result<AutoProfileResult, String> {
    let raw = project_path.trim();
    if raw.is_empty() {
        return Err("Proje klasörü seçilmedi.".into());
    }
    let root = std::path::Path::new(raw);
    if !root.is_dir() {
        return Err(format!("Proje klasörü bulunamadı: {raw}"));
    }
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("Klasör açılamadı: {e}"))?;

    let overview = workspace::read_project_overview(&canonical).await;
    let provider = provider::build(&config, &canonical, "Mimar")?;

    let system_prompt = r#"Sen kıdemli bir yazılım mimarısın. Verilen proje dosya ağacı ve ana dosyaları inceleyerek projeyi otonom yönetecek multi-agent sistemi için iki metin hazırla.
1. VİZYON (vision): Projenin ne olduğunu, teknolojilerini, mimarisini, amaçlarını anlatan 2-4 cümlelik net ve zengin bir vizyon paragrafı.
2. GENEL TALİMATLAR (instructions): Projede kod yazarken dikkat edilecek hayati kurallar, mimari prensipler, korunan kütüphaneler ve test doğrulama adımları (madde imli • şeklinde).

Yanıtını YALNIZCA şu JSON formatında ver, başka hiçbir metin veya markdown bloğu yazma:
{"vision": "...", "instructions": "..."}"#;

    let prompt = format!(
        "Aşağıdaki proje özetini incele ve JSON formatında vision ve instructions üret:\n\n{}",
        overview
    );

    let raw_reply = provider.complete(&prompt, system_prompt).await?;
    if let Some(val) = protocol::extract_json(&raw_reply) {
        let vision = val
            .get("vision")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let instructions = val
            .get("instructions")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !vision.is_empty() {
            return Ok(AutoProfileResult {
                vision,
                instructions,
            });
        }
    }

    Ok(AutoProfileResult {
        vision: raw_reply.trim().chars().take(1200).collect(),
        instructions: "• Kodlama standartlarını koru.\n• Her adımda derleme ve testleri doğrula."
            .into(),
    })
}

/// Computes line-by-line differences between the latest backup and current file.
#[tauri::command]
async fn get_file_diff(
    file_path: String,
    project_path: Option<String>,
) -> Result<String, String> {
    let root = probe_dir(project_path);
    let rel_path = if let Ok(rel) = std::path::Path::new(&file_path).strip_prefix(&root) {
        rel.to_string_lossy().to_string()
    } else {
        file_path
    };

    let diff = workspace::calculate_file_diff(&root, &rel_path).await?;
    serde_json::to_string(&diff).map_err(|e| format!("Diff serileştirilemedi: {e}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shared: SharedState = Arc::new(Mutex::new(AppState::default()));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            select_folder,
            start_command_center,
            stop_command_center,
            get_status,
            get_stats,
            check_provider,
            list_models,
            open_new_window,
            analyze_project,
            get_file_diff
        ])
        .run(tauri::generate_context!())
        .expect("Tauri uygulaması başlatılamadı");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_file_diff_returns_json() {
        let dir = std::env::temp_dir().join(format!("swarm-diff-test-{}", std::process::id()));
        let _ = tokio::fs::create_dir_all(&dir).await;

        let res = get_file_diff("code.rs".into(), Some(dir.to_string_lossy().to_string())).await;
        assert!(res.is_ok());
        let val = res.unwrap();
        assert_eq!(val, "[]");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
