//! Shared, thread-safe runtime state plus the log/event plumbing.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    #[serde(default)]
    pub project_path: String,
    /// info | ok | warn | error | architect | coder | review | write
    pub level: String,
    /// system | architect | coder | review | write
    pub phase: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub timestamp: String,
}

impl LogEvent {
    pub fn new(level: &str, phase: &str, message: impl Into<String>) -> Self {
        Self {
            project_path: String::new(),
            level: level.to_string(),
            phase: phase.to_string(),
            message: message.into(),
            detail: None,
            timestamp: now_hms(),
        }
    }

    pub fn with_project(mut self, path: impl Into<String>) -> Self {
        self.project_path = path.into();
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        let text = detail.into();
        if !text.trim().is_empty() {
            self.detail = Some(text);
        }
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStats {
    pub rounds: u32,
    pub tasks_total: u32,
    pub tasks_applied: u32,
    pub tasks_rejected: u32,
    pub tasks_skipped: u32,
    pub files_written: u32,
    pub revisions: u32,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// Ring-buffer size of the log replayed to a UI that (re)connects.
const MAX_LOGS: usize = 800;

/// Tracks state for a single running or completed project.
#[derive(Default)]
pub struct ProjectState {
    pub running: bool,
    pub stop: Arc<AtomicBool>,
    pub logs: VecDeque<LogEvent>,
    pub stats: RunStats,
}

impl ProjectState {
    pub fn push_log(&mut self, event: LogEvent) {
        if self.logs.len() >= MAX_LOGS {
            self.logs.pop_front();
        }
        self.logs.push_back(event);
    }
}

#[derive(Default)]
pub struct AppState {
    pub projects: HashMap<String, ProjectState>,
}

pub type SharedState = Arc<Mutex<AppState>>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusPayload {
    pub running: bool,
    pub stats: RunStats,
    pub logs: Vec<LogEvent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsPayload {
    pub running: bool,
    pub stats: RunStats,
}

/// Appends to the project's in-memory ring buffer and streams the event to the UI.
pub async fn emit_log(app: &AppHandle, shared: &SharedState, event: LogEvent) {
    let key = event.project_path.clone();
    {
        let mut guard = shared.lock().await;
        let p_state = guard.projects.entry(key.clone()).or_default();
        p_state.push_log(event.clone());
    }
    let _ = app.emit("agent-log", &event);
}

pub fn emit_status(app: &AppHandle, project_path: &str, running: bool) {
    let _ = app.emit(
        "agent-status",
        serde_json::json!({ "projectPath": project_path, "running": running }),
    );
}
