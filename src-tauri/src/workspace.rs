//! Filesystem access is funnelled through here so the agent can never escape
//! the selected project root.

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

const ALLOWED_DOT_DIRS: &[&str] = &[".github", ".vscode", ".config"];

#[derive(Debug, Clone)]
pub struct WriteReport {
    pub path: PathBuf,
    pub bytes: usize,
    pub backup: Option<PathBuf>,
    pub created: bool,
    pub dry_run: bool,
}

/// Normalizes an LLM supplied relative path and rejects escapes.
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
    if lower == ".git" || lower.starts_with(".git/") {
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

pub async fn read_text(root: &Path, rel: &str, max_chars: usize) -> Result<String, String> {
    let path = safe_join(root, rel)?;
    ensure_inside(root, &path).await?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("`{rel}` okunamadı: {e}"))?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(format!("`{rel}` çok büyük (>4 MB), atlandı."));
    }
    let text = String::from_utf8_lossy(&bytes).to_string();
    Ok(truncate_chars(&text, max_chars))
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

    tokio::fs::write(&path, body.as_bytes())
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

pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}\n… (kısaltıldı)")
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
}
