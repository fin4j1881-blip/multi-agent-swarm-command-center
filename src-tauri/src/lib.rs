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
use tauri::{AppHandle, State};
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

    {
        let mut guard = state.lock().await;
        if guard.running {
            return Err("Komutanlık zaten çalışıyor.".into());
        }
        guard.running = true;
        guard.stop.store(false, Ordering::SeqCst);
        guard.logs.clear();
        guard.stats = Default::default();
        guard.stats.started_at = Some(now_iso());
    }

    emit_status(&app, true);
    emit_log(
        &app,
        state.inner(),
        LogEvent::new(
            "info",
            "system",
            format!("Komutanlık başlatıldı → {}", root.display()),
        ),
    )
    .await;

    let shared: SharedState = state.inner().clone();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        orchestrator::run(handle, shared, config).await;
    });

    Ok(root.to_string_lossy().to_string())
}

/// Requests a cooperative stop. The current model call finishes first.
#[tauri::command]
async fn stop_command_center(app: AppHandle, state: State<'_, SharedState>) -> Result<(), String> {
    let running = {
        let guard = state.lock().await;
        guard.stop.store(true, Ordering::SeqCst);
        guard.running
    };

    if running {
        emit_log(
            &app,
            state.inner(),
            LogEvent::new(
                "warn",
                "system",
                "Durdurma isteği alındı; mevcut adım bitince duracak.",
            ),
        )
        .await;
    }
    Ok(())
}

#[tauri::command]
async fn get_status(state: State<'_, SharedState>) -> Result<StatusPayload, String> {
    let guard = state.lock().await;
    Ok(StatusPayload {
        running: guard.running,
        stats: guard.stats.clone(),
        logs: guard.logs.iter().cloned().collect(),
    })
}

/// Lightweight variant of `get_status` for frequent polling: no log history.
#[tauri::command]
async fn get_stats(state: State<'_, SharedState>) -> Result<StatsPayload, String> {
    let guard = state.lock().await;
    Ok(StatsPayload {
        running: guard.running,
        stats: guard.stats.clone(),
    })
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
            open_new_window
        ])
        .run(tauri::generate_context!())
        .expect("Tauri uygulaması başlatılamadı");
}
