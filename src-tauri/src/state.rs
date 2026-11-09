//! Shared, thread-safe runtime state plus the log/event plumbing.

use crate::models::SessionData;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
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
#[derive(Debug, Default)]
pub struct ProjectState {
    pub running: bool,
    pub stop: Arc<AtomicBool>,
    pub logs: VecDeque<LogEvent>,
    pub stats: RunStats,
    pub session: SessionData,
}

#[allow(dead_code)]
impl ProjectState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_session(session: SessionData) -> Self {
        let stats: RunStats = session.stats.clone().into();
        let logs: VecDeque<LogEvent> = session
            .logs
            .iter()
            .cloned()
            .map(LogEvent::from)
            .collect();
        Self {
            running: false,
            stop: Arc::new(AtomicBool::new(false)),
            logs,
            stats,
            session,
        }
    }

    pub fn push_log(&mut self, event: LogEvent) {
        if self.logs.len() >= MAX_LOGS {
            self.logs.pop_front();
        }
        self.logs.push_back(event.clone());

        self.session.logs.push(event.into());
        if self.session.logs.len() > MAX_LOGS {
            self.session.logs.drain(0..self.session.logs.len() - MAX_LOGS);
        }
        self.session.stats = self.stats.clone().into();
    }

    pub fn update_stats(&mut self, apply: impl FnOnce(&mut RunStats)) {
        apply(&mut self.stats);
        self.session.stats = self.stats.clone().into();
    }

    pub fn sync_session(&mut self) {
        self.session.stats = self.stats.clone().into();
    }
}

#[derive(Debug, Default)]
pub struct AppState {
    pub session: SessionData,
    pub projects: HashMap<String, ProjectState>,
}

#[allow(dead_code)]
impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_session(session: SessionData) -> Self {
        Self {
            session,
            projects: HashMap::new(),
        }
    }

    /// Başlangıçta veya çalışma alanı seçildiğinde oturum verisini yükler.
    pub async fn load_workspace(&mut self, root: &Path) -> &SessionData {
        let session = crate::workspace::load_session(root).await;
        self.session = session.clone();
        let key = root.to_string_lossy().to_string();
        self.projects.insert(key, ProjectState::from_session(session));
        &self.session
    }

    /// Çalışma alanından AppState başlatır.
    pub async fn from_workspace(root: &Path) -> Self {
        let mut state = Self::new();
        state.load_workspace(root).await;
        state
    }

    /// Belirtilen projenin veya genel oturumu .swarm/session.json dosyasına atomik kaydeder.
    pub async fn save_session(&self, root: &Path) -> Result<(), String> {
        let key = root.to_string_lossy().to_string();
        let session = self
            .projects
            .get(&key)
            .map(|p| &p.session)
            .unwrap_or(&self.session);
        crate::workspace::save_session(root, session).await
    }

    /// Proje yolu string olarak verildiğinde oturumu kaydeder.
    pub async fn save_project_session(&self, project_path: &str) -> Result<(), String> {
        let path = Path::new(project_path);
        self.save_session(path).await
    }

    /// Sayaçları güvenli şekilde günceller ve oturum verisiyle senkronize eder.
    pub fn update_stats(&mut self, project_path: &str, apply: impl FnOnce(&mut RunStats)) {
        let p = self.projects.entry(project_path.to_string()).or_default();
        p.update_stats(apply);
        let stats = p.stats.clone();
        self.session.stats = stats.into();
    }

    /// Sayaçları güncelledikten sonra oturumu diske yazar.
    pub async fn update_stats_and_save(
        &mut self,
        project_path: &str,
        apply: impl FnOnce(&mut RunStats),
    ) -> Result<(), String> {
        self.update_stats(project_path, apply);
        self.save_project_session(project_path).await
    }

    /// Log ekler ve oturum verisini günceller.
    pub fn push_log(&mut self, project_path: &str, event: LogEvent) {
        let p = self.projects.entry(project_path.to_string()).or_default();
        p.push_log(event.clone());
        let session_stats = p.session.stats.clone();
        self.session.logs.push(event.into());
        if self.session.logs.len() > MAX_LOGS {
            self.session.logs.drain(0..self.session.logs.len() - MAX_LOGS);
        }
        self.session.stats = session_stats;
    }

    /// Log ekler ve oturumu diske kaydeder.
    pub async fn push_log_and_save(
        &mut self,
        project_path: &str,
        event: LogEvent,
    ) -> Result<(), String> {
        self.push_log(project_path, event);
        self.save_project_session(project_path).await
    }

    /// Projenin oturum verisini getirir.
    pub fn get_session(&self, project_path: &str) -> &SessionData {
        self.projects
            .get(project_path)
            .map(|p| &p.session)
            .unwrap_or(&self.session)
    }
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

/// Appends to the project's in-memory ring buffer, syncs session, streams to the UI,
/// and asynchronously persists to .swarm/session.json when project_path is provided.
pub async fn emit_log(app: &AppHandle, shared: &SharedState, event: LogEvent) {
    let key = event.project_path.clone();
    let to_save = {
        let mut guard = shared.lock().await;
        guard.push_log(&key, event.clone());

        if !key.is_empty() {
            guard.projects.get(&key).map(|p| (key.clone(), p.session.clone()))
        } else {
            None
        }
    };

    if let Some((path_str, session)) = to_save {
        let _ = crate::workspace::save_session(Path::new(&path_str), &session).await;
    }

    let _ = app.emit("agent-log", &event);
}

pub fn emit_status(app: &AppHandle, project_path: &str, running: bool) {
    let _ = app.emit(
        "agent-status",
        serde_json::json!({ "projectPath": project_path, "running": running }),
    );
}

/// Asynchronously persists current session data for a project to .swarm/session.json.
#[allow(dead_code)]
pub async fn sync_and_save_session(shared: &SharedState, project_path: &str) -> Result<(), String> {
    let (path_buf, session) = {
        let mut guard = shared.lock().await;
        if let Some(p) = guard.projects.get_mut(project_path) {
            p.sync_session();
            let session = p.session.clone();
            guard.session.stats = session.stats.clone();
            (std::path::PathBuf::from(project_path), session)
        } else {
            (std::path::PathBuf::from(project_path), guard.session.clone())
        }
    };

    if !project_path.is_empty() {
        crate::workspace::save_session(&path_buf, &session).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{LogEntry, SessionStats};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(prefix: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("swarm-state-test-{prefix}-{stamp}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn app_state_initializes_with_default_session() {
        let state = AppState::new();
        assert_eq!(state.session.stats, SessionStats::default());
        assert!(state.session.logs.is_empty());
        assert!(state.projects.is_empty());
    }

    #[tokio::test]
    async fn app_state_load_from_workspace_restores_session() {
        let dir = temp_test_dir("load-ws");
        let mut session = SessionData::default();
        session.stats.tur = 5;
        session.stats.uygulandi = 4;
        session.stats.red = 1;
        session.logs.push(LogEntry::new("info", "system", "Oturum yuklendi"));

        crate::workspace::save_session(&dir, &session).await.unwrap();

        let state = AppState::from_workspace(&dir).await;
        assert_eq!(state.session.stats.tur, 5);
        assert_eq!(state.session.stats.uygulandi, 4);
        assert_eq!(state.session.stats.red, 1);
        assert_eq!(state.session.logs.len(), 1);

        let key = dir.to_string_lossy().to_string();
        let project = state.projects.get(&key).expect("project state exists");
        assert_eq!(project.stats.rounds, 5);
        assert_eq!(project.stats.tasks_applied, 4);
        assert_eq!(project.logs.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn update_stats_and_save_persists_to_disk() {
        let dir = temp_test_dir("stats-save");
        let dir_str = dir.to_string_lossy().to_string();

        let mut state = AppState::new();
        state
            .update_stats_and_save(&dir_str, |s| {
                s.rounds = 3;
                s.tasks_applied = 2;
                s.files_written = 4;
            })
            .await
            .unwrap();

        assert_eq!(state.session.stats.tur, 3);
        assert_eq!(state.session.stats.uygulandi, 2);
        assert_eq!(state.session.stats.dosya, 4);

        let loaded = crate::workspace::load_session(&dir).await;
        assert_eq!(loaded.stats.tur, 3);
        assert_eq!(loaded.stats.uygulandi, 2);
        assert_eq!(loaded.stats.dosya, 4);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn push_log_and_save_persists_logs_to_disk() {
        let dir = temp_test_dir("log-save");
        let dir_str = dir.to_string_lossy().to_string();

        let mut state = AppState::new();
        let event = LogEvent::new("ok", "coder", "Dosya guncellendi").with_project(&dir_str);
        state.push_log_and_save(&dir_str, event).await.unwrap();

        assert_eq!(state.session.logs.len(), 1);
        assert_eq!(state.session.logs[0].message, "Dosya guncellendi");

        let loaded = crate::workspace::load_session(&dir).await;
        assert_eq!(loaded.logs.len(), 1);
        assert_eq!(loaded.logs[0].message, "Dosya guncellendi");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn thread_safe_shared_state_concurrency() {
        let dir = temp_test_dir("concurrency");
        let dir_str = dir.to_string_lossy().to_string();

        let shared: SharedState = Arc::new(Mutex::new(AppState::new()));
        let mut handles = vec![];

        for _ in 0..10 {
            let shared_clone = Arc::clone(&shared);
            let d = dir_str.clone();
            handles.push(tokio::spawn(async move {
                let mut guard = shared_clone.lock().await;
                guard.update_stats(&d, |s| {
                    s.tasks_applied += 1;
                });
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let guard = shared.lock().await;
        assert_eq!(guard.session.stats.uygulandi, 10);
        let p = guard.projects.get(&dir_str).unwrap();
        assert_eq!(p.stats.tasks_applied, 10);
        assert_eq!(p.session.stats.uygulandi, 10);
        drop(guard);

        sync_and_save_session(&shared, &dir_str).await.unwrap();
        let loaded = crate::workspace::load_session(&dir).await;
        assert_eq!(loaded.stats.uygulandi, 10);

        std::fs::remove_dir_all(&dir).ok();
    }
}
