//! Local, zero-cost guardrails that run before the architect review.
//!
//! These checks catch the classic failure modes of small/local models without
//! spending an extra API call: truncated files, placeholder comments, wrapped
//! markdown, unbalanced braces and no-op writes.

use crate::protocol::{PlanTask, Proposal};
use crate::workspace::normalize_rel;
use std::path::Path;

pub struct Precheck {
    pub issues: Vec<String>,
    pub warnings: Vec<String>,
}

const PLACEHOLDER_MARKERS: &[&str] = &[
    "... existing code",
    "...existing code",
    "... rest of",
    "...rest of",
    "// ...",
    "/* ... */",
    "your code here",
    "kalan kod",
    "kodun geri kalanı",
    "<truncated>",
    "todo: implement",
    "implement here",
    "buraya kod",
    "aynı kalır",
    "değişmeden kalır",
    "unchanged",
];

const CODE_EXTS: &[&str] = &[
    "rs", "js", "mjs", "cjs", "ts", "tsx", "jsx", "json", "css", "scss", "html", "go", "java",
    "kt", "kts", "swift", "c", "h", "cpp", "hpp", "cs", "php", "rb", "py", "sh", "toml", "yaml",
    "yml", "sql", "vue", "svelte",
];

const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "pdf", "zip", "gz", "tar", "exe", "dll",
    "so", "dylib", "wasm", "mp3", "mp4", "mov", "woff", "woff2", "ttf", "otf", "db", "sqlite",
];

pub fn precheck(
    root: &Path,
    task: &PlanTask,
    proposal: &Proposal,
    current: Option<&str>,
) -> Precheck {
    let mut issues: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let file = proposal.file.trim();
    if file.is_empty() {
        issues.push("Dosya yolu belirtilmemiş.".to_string());
    } else if let Err(error) = normalize_rel(file) {
        issues.push(error);
    }

    let content = &proposal.content;
    let trimmed = content.trim();

    if trimmed.is_empty() {
        issues.push("Üretilen içerik boş.".to_string());
        return Precheck { issues, warnings };
    }

    if trimmed.chars().count() < 20 {
        issues.push("Üretilen içerik anlamsız derecede kısa.".to_string());
    }

    if trimmed.starts_with("```") {
        issues.push(
            "İçerik markdown kod bloğu ile sarılmış; ham dosya içeriği bekleniyordu.".to_string(),
        );
    }

    if trimmed.contains("### FILE:") || trimmed.contains("### SUMMARY:") {
        issues.push("İçerikte protokol başlığı kaldı; sadece kod gövdesi yazılmalı.".to_string());
    }

    let lower = content.to_lowercase();
    for marker in PLACEHOLDER_MARKERS.iter().copied() {
        if lower.contains(marker) {
            issues.push(format!(
                "İçerikte eksik bırakma işareti var (`{marker}`). Dosya eksiksiz olmalı."
            ));
        }
    }

    let ext = file.rsplit('.').next().unwrap_or("").to_lowercase();

    if BINARY_EXTS.contains(&ext.as_str()) {
        issues.push(format!("İkili dosya uzantısına metin yazılamaz: .{ext}"));
    }

    if CODE_EXTS.contains(&ext.as_str()) {
        let open = content.matches('{').count() as i64;
        let close = content.matches('}').count() as i64;
        if open != close {
            warnings.push(format!(
                "Parantez dengesi şüpheli: {{ = {open}, }} = {close}. Kod eksik olabilir."
            ));
        }
        if ext == "json" {
            if let Err(error) = serde_json::from_str::<serde_json::Value>(content) {
                issues.push(format!("JSON dosyası geçerli değil: {error}"));
            }
        }
    }

    if let Some(current) = current {
        if !current.trim().is_empty() && current.trim() == trimmed {
            issues.push("Öneri mevcut dosyayla birebir aynı; hiçbir değişiklik yok.".to_string());
        }
        let current_len = current.chars().count();
        let new_len = content.chars().count();
        if current_len > 600 && new_len < current_len / 2 {
            warnings.push(format!(
                "Dosya ciddi biçimde küçüldü ({current_len} → {new_len} karakter). İşlev kaybı olabilir."
            ));
        }
    }

    if !task.file.trim().is_empty() && proposal.file.trim() != task.normalized_file() {
        warnings.push(format!(
            "Görev `{}` dosyasını istiyor, öneri `{}` dosyasına yazıyor.",
            task.normalized_file(),
            proposal.file
        ));
    }

    if file.starts_with('/') || file.contains("..") {
        issues.push("Dosya yolu proje kökü dışına çıkıyor.".to_string());
    }

    // Root itself is only used for the path validation above.
    let _ = root;

    Precheck { issues, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn task(file: &str) -> PlanTask {
        PlanTask {
            id: "T1".into(),
            title: "Test".into(),
            file: file.into(),
            objective: "test".into(),
            ..Default::default()
        }
    }

    fn proposal(file: &str, content: &str) -> Proposal {
        Proposal {
            file: file.into(),
            summary: String::new(),
            content: content.into(),
        }
    }

    #[test]
    fn flags_placeholder_and_truncation() {
        let root = PathBuf::from("/tmp");
        let result = precheck(
            &root,
            &task("src/main.rs"),
            &proposal("src/main.rs", "fn main() {\n    // ... existing code\n}\n"),
            None,
        );
        assert!(!result.issues.is_empty());
    }

    #[test]
    fn flags_noop_write() {
        let root = PathBuf::from("/tmp");
        let current = "fn main() {\n    println!(\"hi\");\n}\n";
        let result = precheck(
            &root,
            &task("src/main.rs"),
            &proposal("src/main.rs", current),
            Some(current),
        );
        assert!(result.issues.iter().any(|i| i.contains("birebir aynı")));
    }

    #[test]
    fn flags_wrapped_markdown() {
        let root = PathBuf::from("/tmp");
        let result = precheck(
            &root,
            &task("a.js"),
            &proposal("a.js", "```js\nconsole.log(1);\n```"),
            None,
        );
        assert!(result.issues.iter().any(|i| i.contains("markdown")));
    }

    #[test]
    fn accepts_clean_rust() {
        let root = PathBuf::from("/tmp");
        let content = "use std::io;\n\nfn main() {\n    println!(\"ok\");\n}\n";
        let result = precheck(
            &root,
            &task("src/main.rs"),
            &proposal("src/main.rs", content),
            None,
        );
        assert!(result.issues.is_empty(), "issues: {:?}", result.issues);
    }

    #[test]
    fn rejects_invalid_json_file() {
        let root = PathBuf::from("/tmp");
        let result = precheck(
            &root,
            &task("config.json"),
            &proposal("config.json", "{ \"a\": 1, }"),
            None,
        );
        assert!(result.issues.iter().any(|i| i.contains("JSON")));
    }
}
