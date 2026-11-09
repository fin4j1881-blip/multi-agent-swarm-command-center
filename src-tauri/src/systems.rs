//! Embedded system definitions.
//!
//! Each definition describes a whole system (stack, architecture, commands,
//! deploy flow, rules) rather than a single vision paragraph. Definitions are
//! compiled into the binary from `systems.json` and matched against the
//! selected project directory, then injected into every model prompt so the
//! agents know how the project is built, tested and deployed.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SystemCommand {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SystemDefinition {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub dir_names: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub stack: Vec<String>,
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub commands: Vec<SystemCommand>,
    #[serde(default)]
    pub deploy: String,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub forbidden: Vec<String>,
    #[serde(default)]
    pub vision: String,
    #[serde(default)]
    pub instructions: String,
}

impl SystemDefinition {
    /// Renders the definition as a prompt block. Empty sections are omitted so
    /// a partially filled definition still produces clean context.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("SİSTEM TANIMI — {}\n", self.name));
        out.push_str("================\n");

        if !self.summary.trim().is_empty() {
            out.push_str(&format!("ÖZET\n{}\n\n", self.summary.trim()));
        }
        if !self.stack.is_empty() {
            out.push_str("TEKNOLOJİ YIĞINI\n");
            for item in &self.stack {
                out.push_str(&format!("- {item}\n"));
            }
            out.push('\n');
        }
        if !self.architecture.trim().is_empty() {
            out.push_str(&format!("MİMARİ\n{}\n\n", self.architecture.trim()));
        }
        if !self.commands.is_empty() {
            out.push_str("KOMUTLAR\n");
            for cmd in &self.commands {
                out.push_str(&format!("- {}: `{}`\n", cmd.name, cmd.command));
            }
            out.push('\n');
        }
        if !self.deploy.trim().is_empty() {
            out.push_str(&format!("DEPLOY\n{}\n\n", self.deploy.trim()));
        }
        if !self.rules.is_empty() {
            out.push_str("KURALLAR\n");
            for rule in &self.rules {
                out.push_str(&format!("- {rule}\n"));
            }
            out.push('\n');
        }
        if !self.forbidden.is_empty() {
            out.push_str("YASAKLAR\n");
            for item in &self.forbidden {
                out.push_str(&format!("- {item}\n"));
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

/// All embedded definitions. Parsed once and reused.
pub fn all() -> &'static [SystemDefinition] {
    static DEFS: OnceLock<Vec<SystemDefinition>> = OnceLock::new();
    DEFS.get_or_init(|| {
        serde_json::from_str(include_str!("../systems.json")).unwrap_or_else(|e| {
            eprintln!("systems.json okunamadı: {e}");
            Vec::new()
        })
    })
}

/// Finds the definition matching a project directory.
///
/// Matching order: exact (canonicalized) path, then directory name. Directory
/// name matching lets a cloned project still resolve when it lives elsewhere.
pub fn match_system(root: &Path) -> Option<SystemDefinition> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let dir_name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_string());

    all()
        .iter()
        .find(|def| {
            def.paths.iter().any(|p| {
                let candidate = Path::new(p);
                let candidate = candidate.canonicalize().unwrap_or_else(|_| candidate.to_path_buf());
                candidate == canonical
            })
        })
        .or_else(|| {
            let name = dir_name.as_deref()?;
            all()
                .iter()
                .find(|def| def.dir_names.iter().any(|d| d == name))
        })
        .cloned()
}

/// Rendered prompt block for a project directory, or an empty string.
pub fn context_for(root: &Path) -> String {
    match_system(root).map(|def| def.render()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_load() {
        let defs = all();
        assert_eq!(defs.len(), 3, "üç sistem tanımı gömülü olmalı");
        for def in defs {
            assert!(!def.id.is_empty());
            assert!(!def.name.is_empty());
            assert!(!def.summary.is_empty());
        }
    }

    #[test]
    fn render_includes_core_sections() {
        let def = &all()[0];
        let text = def.render();
        assert!(text.contains("SİSTEM TANIMI"));
        assert!(text.contains("DEPLOY"));
        assert!(text.contains("KURALLAR"));
    }

    #[test]
    fn match_by_directory_name() {
        let def = match_system(Path::new("/somewhere/else/micup-rs"));
        assert_eq!(def.map(|d| d.id), Some("micup-rs".to_string()));
    }

    #[test]
    fn no_match_returns_none() {
        assert!(match_system(Path::new("/tmp/unknown-project")).is_none());
    }
}
