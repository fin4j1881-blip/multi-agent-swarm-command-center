//! Filesystem access is funnelled through here so the agent can never escape
//! the selected project root.

use crate::models::SessionData;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".swarm",
    ".idea",
    ".cache",
    ".turbo",
    "coverage",
    "vendor",
    ".gradle",
    ".dart_tool",
    "Pods",
    ".terraform",
    "out",
];

const SKIP_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "pdf", "zip", "gz", "tar", "7z",
    "rar", "exe", "dll", "so", "dylib", "a", "o", "class", "jar", "wasm", "mp3", "mp4", "mov",
    "avi", "mkv", "wav", "ogg", "ttf", "otf", "woff", "woff2", "eot", "db", "sqlite", "sqlite3",
    "lock", "bin", "dat", "pdb", "map",
];

const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

const ALLOWED_DOT_DIRS: &[&str] = &[".github", ".vscode", ".config"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffKind {
    Added,
    Removed,
    Same,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

impl DiffLine {
    pub fn new(kind: DiffKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WriteReport {
    pub path: PathBuf,
    pub bytes: usize,
    pub backup: Option<PathBuf>,
    pub created: bool,
    pub dry_run: bool,
}

/// Scans key files in the project root to generate a summary bundle for AI auto-profiling.
pub async fn read_project_overview(root: &Path) -> String {
    let mut out = String::new();

    // 1. Project tree (top 70 files)
    let tree = scan_tree(root, 70);
    out.push_str("PROJE DOSYA AĞACI:\n");
    out.push_str(&tree);
    out.push_str("\n\n");

    // 2. Read key informational and manifest files
    let key_files = &[
        "README.md",
        "README",
        "Cargo.toml",
        "package.json",
        "go.mod",
        "pyproject.toml",
        "requirements.txt",
        "pom.xml",
        "build.gradle",
        "wrangler.jsonc",
        "wrangler.toml",
        "Makefile",
        "AGENTS.md",
        "CHANGELOG.md",
    ];

    for file in key_files {
        if let Ok(res) = read_text(root, file, 6000).await {
            if !res.text.trim().is_empty() {
                out.push_str(&format!("--- DOSYA: {file} ---\n{}\n\n", res.text.trim()));
            }
        }
    }

    out
}

pub fn normalize_rel(rel: &str) -> Result<String, String> {
    let cleaned = rel
        .trim()
        .trim_matches('`')
        .trim()
        .trim_start_matches("./")
        .replace('\\', "/");

    if cleaned.is_empty() {
        return Err("Dosya yolu boş.".into());
    }
    if cleaned.starts_with('/') || cleaned.starts_with('~') {
        return Err(format!("Güvenlik: mutlak yol kabul edilmiyor: `{rel}`"));
    }

    let path = Path::new(&cleaned);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(format!(
                    "Güvenlik: `{rel}` proje kökü dışına çıkamaz (.. veya kök bileşeni)."
                ))
            }
        }
    }

    let lower = cleaned.to_lowercase();
    // Any `.git` component, so nested repositories and submodules are protected too.
    if lower.split('/').any(|part| part == ".git") {
        return Err("Güvenlik: `.git` dizinine yazılamaz.".into());
    }
    if lower == ".swarm" || lower.starts_with(".swarm/") {
        return Err("Güvenlik: `.swarm` yedek dizini korunuyor.".into());
    }

    Ok(cleaned)
}

pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let cleaned = normalize_rel(rel)?;
    Ok(root.join(cleaned))
}

/// Compact, token-friendly project tree.
pub fn scan_tree(root: &Path, max_entries: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let limit = max_entries.max(50);

    let walker = WalkDir::new(root)
        .max_depth(8)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.file_type().is_dir() {
                if SKIP_DIRS.iter().any(|d| *d == name) {
                    return false;
                }
                if name.starts_with('.') && !ALLOWED_DOT_DIRS.contains(&name.as_str()) {
                    return false;
                }
                true
            } else {
                let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
                !SKIP_EXTS.contains(&ext.as_str())
            }
        });

    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if size > 512 * 1024 {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        lines.push(format!("{} ({} B)", rel.display(), size));
        if lines.len() >= limit {
            lines.push("… (ağaç listesi kısaltıldı)".to_string());
            break;
        }
    }

    if lines.is_empty() {
        "(boş proje)".to_string()
    } else {
        lines.join("\n")
    }
}

/// File content as seen by the models.
pub struct FileText {
    pub text: String,
    /// True when the file was longer than the requested limit.
    pub truncated: bool,
}

pub async fn read_text(root: &Path, rel: &str, max_chars: usize) -> Result<FileText, String> {
    let path = safe_join(root, rel)?;
    ensure_inside(root, &path).await?;

    let size = tokio::fs::metadata(&path)
        .await
        .map_err(|e| format!("`{rel}` okunamadı: {e}"))?
        .len();
    if size > MAX_READ_BYTES {
        return Err(format!("`{rel}` çok büyük (>4 MB), atlandı."));
    }

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("`{rel}` okunamadı: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    match text.char_indices().nth(max_chars) {
        None => Ok(FileText {
            text: text.into_owned(),
            truncated: false,
        }),
        Some((cut, _)) => Ok(FileText {
            text: text[..cut].to_string(),
            truncated: true,
        }),
    }
}

/// Like [`read_text`], but a missing file is `Ok(None)` (a new file) while any
/// other failure stays an error, so an unreadable file is never mistaken for
/// a new one and overwritten.
pub async fn read_existing(
    root: &Path,
    rel: &str,
    max_chars: usize,
) -> Result<Option<FileText>, String> {
    let path = safe_join(root, rel)?;
    match tokio::fs::metadata(&path).await {
        Ok(_) => read_text(root, rel, max_chars).await.map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("`{rel}` okunamadı: {e}")),
    }
}

/// Reads up to `max_files` extra files the architect flagged as relevant and
/// renders them as one prompt block. Unreadable files are skipped silently.
pub async fn read_context_files(
    root: &Path,
    files: &[String],
    skip: &str,
    max_files: usize,
    chars_each: usize,
) -> String {
    let mut block = String::new();
    let mut taken = 0usize;

    for raw in files {
        if taken >= max_files {
            break;
        }
        let Ok(rel) = normalize_rel(raw) else {
            continue;
        };
        if rel == skip {
            continue;
        }
        let Ok(file) = read_text(root, &rel, chars_each).await else {
            continue;
        };
        if file.text.trim().is_empty() {
            continue;
        }
        taken += 1;
        let note = if file.truncated {
            " (kısaltıldı)"
        } else {
            ""
        };
        block.push_str(&format!(
            "BAĞLAM DOSYASI `{rel}`{note} — yalnızca okuma amaçlı\n---8<---\n{}\n---8<---\n\n",
            file.text
        ));
    }
    block
}

/// Loads `.swarm/session.json` if present; returns [`SessionData::default`] if missing or unreadable.
pub async fn load_session(root: &Path) -> SessionData {
    let path = root.join(".swarm").join("session.json");
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return SessionData::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Atomically saves [`SessionData`] to `.swarm/session.json`, creating the `.swarm`
/// directory if needed.
pub async fn save_session(root: &Path, session: &SessionData) -> Result<(), String> {
    let swarm_dir = root.join(".swarm");
    if let Err(e) = tokio::fs::create_dir_all(&swarm_dir).await {
        return Err(format!(".swarm dizini oluşturulamadı: {e}"));
    }

    let target = swarm_dir.join("session.json");
    let bytes = serde_json::to_vec_pretty(session)
        .map_err(|e| format!("session verisi serileştirilemedi: {e}"))?;

    write_atomic(&target, &bytes)
        .await
        .map_err(|e| format!("session.json yazılamadı: {e}"))?;

    Ok(())
}

/// Writes the file atomically enough for an editor to pick up the change, after
/// snapshotting the previous version under `.swarm/backups/<stamp>/`.
pub async fn write_text(
    root: &Path,
    rel: &str,
    content: &str,
    dry_run: bool,
) -> Result<WriteReport, String> {
    let cleaned = normalize_rel(rel)?;
    let path = root.join(&cleaned);
    let existed = tokio::fs::metadata(&path).await.is_ok();

    let mut body = content.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let bytes = body.len();

    if dry_run {
        return Ok(WriteReport {
            path,
            bytes,
            backup: None,
            created: !existed,
            dry_run: true,
        });
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Klasör oluşturulamadı ({}): {e}", parent.display()))?;

        // Reject symlinked directories that resolve outside the project root.
        let real_root = tokio::fs::canonicalize(root)
            .await
            .unwrap_or_else(|_| root.to_path_buf());
        let real_parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|e| format!("Hedef klasör doğrulanamadı: {e}"))?;
        if !real_parent.starts_with(&real_root) {
            return Err(format!(
                "Güvenlik: `{}` proje kökü dışına işaret ediyor (symlink).",
                parent.display()
            ));
        }
    }

    if existed {
        ensure_inside(root, &path).await?;
    }

    let mut backup = None;
    if existed {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
        let backup_path = root
            .join(".swarm")
            .join("backups")
            .join(stamp)
            .join(&cleaned);
        if let Some(parent) = backup_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Yedek klasörü oluşturulamadı: {e}"))?;
        }
        tokio::fs::copy(&path, &backup_path)
            .await
            .map_err(|e| format!("Yedek alınamadı: {e}"))?;
        backup = Some(backup_path);
    }

    write_atomic(&path, body.as_bytes())
        .await
        .map_err(|e| format!("`{cleaned}` yazılamadı: {e}"))?;

    Ok(WriteReport {
        path,
        bytes,
        backup,
        created: !existed,
        dry_run: false,
    })
}

/// Locates the most recent backup file under `.swarm/backups/<stamp>/<rel>`.
pub async fn find_latest_backup(root: &Path, rel: &str) -> Result<Option<PathBuf>, String> {
    let cleaned = normalize_rel(rel)?;
    let backups_dir = root.join(".swarm").join("backups");
    let mut dir = match tokio::fs::read_dir(&backups_dir).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Yedek dizini okunamadı: {e}")),
    };

    let mut stamps = Vec::new();
    while let Ok(Some(entry)) = dir.next_entry().await {
        if let Ok(file_type) = entry.file_type().await {
            if file_type.is_dir() {
                stamps.push(entry.file_name().to_string_lossy().to_string());
            }
        }
    }

    stamps.sort();
    for stamp in stamps.into_iter().rev() {
        let candidate = backups_dir.join(stamp).join(&cleaned);
        if tokio::fs::metadata(&candidate).await.is_ok() {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

/// Calculates line-by-line differences between the latest backup and current file.
pub async fn calculate_file_diff(root: &Path, rel: &str) -> Result<Vec<DiffLine>, String> {
    let cleaned = normalize_rel(rel)?;
    let current_path = safe_join(root, &cleaned)?;
    ensure_inside(root, &current_path).await?;

    let backup_path = find_latest_backup(root, &cleaned).await?;

    let backup_text = match backup_path {
        Some(path) => {
            ensure_inside(root, &path).await?;
            match tokio::fs::read(&path).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(e) => return Err(format!("Yedek dosya okunamadı: {e}")),
            }
        }
        None => return Ok(Vec::new()),
    };

    let current_text = match tokio::fs::read(&current_path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("Güncel dosya okunamadı: {e}")),
    };

    let old_lines: Vec<&str> = backup_text.lines().collect();
    let new_lines: Vec<&str> = current_text.lines().collect();

    Ok(compute_line_diff(&old_lines, &new_lines))
}

fn compute_line_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffLine> {
    let n = old.len();
    let m = new.len();

    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            if old[i] == new[j] {
                dp[i + 1][j + 1] = dp[i][j] + 1;
            } else {
                dp[i + 1][j + 1] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    let mut diff = Vec::new();
    let mut i = n;
    let mut j = m;

    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old[i - 1] == new[j - 1] {
            diff.push(DiffLine {
                kind: DiffKind::Same,
                text: old[i - 1].to_string(),
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            diff.push(DiffLine {
                kind: DiffKind::Added,
                text: new[j - 1].to_string(),
            });
            j -= 1;
        } else if i > 0 && (j == 0 || dp[i][j - 1] < dp[i - 1][j]) {
            diff.push(DiffLine {
                kind: DiffKind::Removed,
                text: old[i - 1].to_string(),
            });
            i -= 1;
        }
    }

    diff.reverse();
    diff
}

/// Writes through a sibling temp file and renames it over the target, so an
/// editor or a crash never observes a half-written file. Permissions of an
/// existing target are carried over.
async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.swarm-tmp-{}", std::process::id()));

    let result = async {
        tokio::fs::write(&tmp, bytes).await?;
        if let Ok(meta) = tokio::fs::metadata(path).await {
            let _ = tokio::fs::set_permissions(&tmp, meta.permissions()).await;
        }
        tokio::fs::rename(&tmp, path).await
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

pub async fn restore(root: &Path, rel: &str, backup: &Path) -> Result<(), String> {
    let target = safe_join(root, rel)?;
    tokio::fs::copy(backup, &target)
        .await
        .map(|_| ())
        .map_err(|e| format!("Yedek geri yüklenemedi: {e}"))
}

pub async fn remove(root: &Path, rel: &str) -> Result<(), String> {
    let target = safe_join(root, rel)?;
    tokio::fs::remove_file(&target)
        .await
        .map_err(|e| format!("Dosya silinemedi: {e}"))
}

/// Optional post-write compile gate for Rust projects.
pub async fn cargo_check(root: &Path, timeout_secs: u64) -> Result<(), String> {
    if !root.join("Cargo.toml").exists() {
        return Ok(());
    }

    let mut cmd = tokio::process::Command::new("cargo");
    cmd.arg("check")
        .arg("--quiet")
        .arg("--message-format")
        .arg("short")
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| format!("cargo check {timeout_secs} sn içinde bitmedi."))?
        .map_err(|e| format!("cargo check başlatılamadı: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    let tail: String = combined
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Err(if tail.trim().is_empty() {
        "cargo check başarısız (çıktı yok).".to_string()
    } else {
        tail
    })
}

/// Rejects paths whose canonical location escapes the project root (symlinks).
async fn ensure_inside(root: &Path, path: &Path) -> Result<(), String> {
    let real_root = tokio::fs::canonicalize(root)
        .await
        .unwrap_or_else(|_| root.to_path_buf());
    let real_target = match tokio::fs::canonicalize(path).await {
        Ok(target) => target,
        Err(_) => return Ok(()), // does not exist yet; parent is validated on write
    };
    if real_target.starts_with(&real_root) {
        Ok(())
    } else {
        Err(format!(
            "Güvenlik: `{}` proje kökü dışına işaret ediyor (symlink).",
            path.display()
        ))
    }
}

/// Ensures .gitignore contains exclusions for sensitive or generated paths:
/// `.env`, `target/`, and `.swarm/`.
pub async fn ensure_gitignore(root: &Path) -> Result<(), String> {
    let gitignore_path = root.join(".gitignore");
    let content = if gitignore_path.exists() {
        tokio::fs::read_to_string(&gitignore_path)
            .await
            .map_err(|e| format!(".gitignore okunamadı: {e}"))?
    } else {
        String::new()
    };

    let required_entries = [".env", "target/", ".swarm/"];
    let mut to_add = Vec::new();

    for req in required_entries {
        let trimmed_req = req.trim_end_matches('/');
        let found = content.lines().any(|line| {
            let t = line.trim();
            t == req || t == trimmed_req || t == format!("{trimmed_req}/")
        });
        if !found {
            to_add.push(req);
        }
    }

    if !to_add.is_empty() {
        let mut new_content = content;
        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        for item in to_add {
            new_content.push_str(item);
            new_content.push('\n');
        }
        write_atomic(&gitignore_path, new_content.as_bytes())
            .await
            .map_err(|e| format!(".gitignore yazılamadı: {e}"))?;
    }

    Ok(())
}

/// Automatically commits changes at the end of a successful round with format
/// `swarm(tur-{tur}): {gorev}` and optionally pushes to `heroku main`.
/// Guarantees that `.env`, `target/`, and `.swarm/` remain untracked.
pub async fn git_commit_and_push(
    root: &Path,
    tur: u32,
    gorev: &str,
    heroku_push: bool,
) -> Result<String, String> {
    if !root.join(".git").exists() {
        return Err("Git deposu bulunamadı (.git dizini mevcut değil).".into());
    }

    // 1. Ensure exclusions in .gitignore
    ensure_gitignore(root).await?;

    // 2. Unstage ignored paths in case they were previously staged
    let mut rm_cmd = tokio::process::Command::new("git");
    rm_cmd
        .args(["rm", "--cached", "-r", "--ignore-unmatch", ".env", "target", ".swarm"])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let _ = rm_cmd.status().await;

    // 3. Stage changes
    let mut add_cmd = tokio::process::Command::new("git");
    add_cmd
        .arg("add")
        .arg("-A")
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let add_out = tokio::time::timeout(std::time::Duration::from_secs(15), add_cmd.output())
        .await
        .map_err(|_| "git add zaman aşımına uğradı.".to_string())?
        .map_err(|e| format!("git add başlatılamadı: {e}"))?;

    if !add_out.status.success() {
        let err = String::from_utf8_lossy(&add_out.stderr);
        return Err(format!("git add başarısız: {err}"));
    }

    // 4. Commit
    let commit_msg = format!("swarm(tur-{tur}): {gorev}");
    let mut commit_cmd = tokio::process::Command::new("git");
    commit_cmd
        .arg("commit")
        .arg("-m")
        .arg(&commit_msg)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let commit_out = tokio::time::timeout(std::time::Duration::from_secs(15), commit_cmd.output())
        .await
        .map_err(|_| "git commit zaman aşımına uğradı.".to_string())?
        .map_err(|e| format!("git commit başlatılamadı: {e}"))?;

    let commit_stdout = String::from_utf8_lossy(&commit_out.stdout);
    let commit_stderr = String::from_utf8_lossy(&commit_out.stderr);

    if !commit_out.status.success() {
        let combined = format!("{commit_stdout} {commit_stderr}");
        if combined.contains("nothing to commit") || combined.contains("working tree clean") {
            return Ok(format!("Değişiklik yok: {commit_msg}"));
        }
        return Err(format!("git commit başarısız: {}", commit_stderr.trim()));
    }

    // 5. Optional Heroku push
    if heroku_push {
        let mut push_cmd = tokio::process::Command::new("git");
        push_cmd
            .args(["push", "heroku", "main"])
            .current_dir(root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let push_out = tokio::time::timeout(std::time::Duration::from_secs(60), push_cmd.output())
            .await
            .map_err(|_| "git push heroku main zaman aşımına uğradı.".to_string())?
            .map_err(|e| format!("git push heroku main başlatılamadı: {e}"))?;

        if !push_out.status.success() {
            let err = String::from_utf8_lossy(&push_out.stderr);
            return Err(format!("git push heroku main başarısız: {err}"));
        }
    }

    Ok(commit_msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_relative_paths() {
        assert_eq!(normalize_rel("./src/main.rs").unwrap(), "src/main.rs");
        assert_eq!(normalize_rel("ui\\app.js").unwrap(), "ui/app.js");
        assert_eq!(normalize_rel("`README.md`").unwrap(), "README.md");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("swarm-symlink-{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("link")).unwrap();

        let result = write_text(&root, "link/evil.rs", "fn main() {}\n", false).await;
        assert!(result.is_err(), "symlink escape must be rejected");

        let result = write_text(&root, "safe/ok.rs", "fn main() {}\n", false).await;
        assert!(result.is_ok(), "normal write must work");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn normalize_rejects_escapes() {
        assert!(normalize_rel("../outside.txt").is_err());
        assert!(normalize_rel("/etc/passwd").is_err());
        assert!(normalize_rel("~/secret").is_err());
        assert!(normalize_rel(".git/config").is_err());
        assert!(normalize_rel(".swarm/backups/x").is_err());
        assert!(normalize_rel("src/../../x").is_err());
        assert!(normalize_rel("   ").is_err());
    }

    #[test]
    fn nested_git_dirs_are_protected() {
        assert!(normalize_rel("vendor/lib/.git/config").is_err());
        assert!(normalize_rel("src/.github/workflows/ci.yml").is_ok());
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("swarm-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[tokio::test]
    async fn write_is_atomic_backs_up_and_leaves_no_temp_files() {
        let root = temp_root("atomic");
        write_text(&root, "a.txt", "eski", false).await.unwrap();
        let report = write_text(&root, "a.txt", "yeni", false).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "yeni\n"
        );
        let backup = report.backup.expect("backup of the old version");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "eski\n");
        let leftovers = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("swarm-tmp"))
            .count();
        assert_eq!(leftovers, 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn read_existing_distinguishes_missing_from_truncated() {
        let root = temp_root("read");
        std::fs::write(root.join("big.txt"), "x".repeat(50)).unwrap();

        assert!(read_existing(&root, "yok.txt", 100)
            .await
            .unwrap()
            .is_none());

        let full = read_existing(&root, "big.txt", 100).await.unwrap().unwrap();
        assert!(!full.truncated);

        let clipped = read_existing(&root, "big.txt", 10).await.unwrap().unwrap();
        assert!(clipped.truncated);
        assert_eq!(clipped.text.len(), 10);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn context_files_are_inlined_and_skip_the_target() {
        let root = temp_root("ctx");
        std::fs::write(root.join("b.rs"), "fn b() {}").unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        let files = vec![
            "a.rs".to_string(),
            "b.rs".to_string(),
            "../etc/passwd".to_string(),
        ];

        let block = read_context_files(&root, &files, "a.rs", 3, 100).await;
        assert!(block.contains("fn b()"));
        assert!(!block.contains("fn a()"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn session_load_returns_default_when_missing() {
        let root = temp_root("session-missing");
        let session = load_session(&root).await;
        assert_eq!(session, SessionData::default());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn session_save_and_load_roundtrip() {
        let root = temp_root("session-rt");
        let mut session = SessionData::default();
        session.stats.tur = 3;
        session.stats.uygulandi = 2;

        save_session(&root, &session).await.expect("save session");

        assert!(root.join(".swarm").join("session.json").exists());

        let loaded = load_session(&root).await;
        assert_eq!(loaded.stats.tur, 3);
        assert_eq!(loaded.stats.uygulandi, 2);
        assert_eq!(loaded, session);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn calculate_file_diff_detects_changes() {
        let root = temp_root("diff-changes");
        write_text(&root, "code.rs", "satir1\nsatir2\n", false)
            .await
            .unwrap();
        write_text(&root, "code.rs", "satir1\nsatir2 guncel\nsatir3\n", false)
            .await
            .unwrap();

        let diff = calculate_file_diff(&root, "code.rs").await.unwrap();
        assert!(!diff.is_empty());
        assert!(diff.iter().any(|d| d.kind == DiffKind::Removed && d.text == "satir2"));
        assert!(diff.iter().any(|d| d.kind == DiffKind::Added && d.text == "satir2 guncel"));
        assert!(diff.iter().any(|d| d.kind == DiffKind::Added && d.text == "satir3"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn calculate_file_diff_returns_empty_when_no_backup() {
        let root = temp_root("diff-empty");
        write_text(&root, "initial.rs", "satir1\n", false)
            .await
            .unwrap();

        let diff = calculate_file_diff(&root, "initial.rs").await.unwrap();
        assert!(diff.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_gitignore_adds_missing_entries() {
        let root = temp_root("gitignore-test");
        std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        ensure_gitignore(&root).await.unwrap();

        let content = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(content.contains(".env"));
        assert!(content.contains("target/"));
        assert!(content.contains(".swarm/"));

        // Idempotent test
        ensure_gitignore(&root).await.unwrap();
        let content2 = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(content, content2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn git_commit_and_push_creates_commit_with_format() {
        let root = temp_root("git-commit-test");
        // Initialize dummy git repo
        let _ = tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&root)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(&root)
            .output()
            .await;

        std::fs::write(root.join("hello.txt"), "hello").unwrap();
        std::fs::write(root.join(".env"), "SECRET=123").unwrap();

        let res = git_commit_and_push(&root, 1, "test commit", false).await;
        assert!(res.is_ok(), "git_commit_and_push failed: {:?}", res);

        let log_out = tokio::process::Command::new("git")
            .args(["log", "-1", "--pretty=%B"])
            .current_dir(&root)
            .output()
            .await
            .unwrap();
        let log_msg = String::from_utf8_lossy(&log_out.stdout);
        assert!(log_msg.contains("swarm(tur-1): test commit"));

        // Verify .env is untracked
        let status_out = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output()
            .await
            .unwrap();
        let status = String::from_utf8_lossy(&status_out.stdout);
        assert!(!status.contains(".env"));

        std::fs::remove_dir_all(&root).ok();
    }
}
