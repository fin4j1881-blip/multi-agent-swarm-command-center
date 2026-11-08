//! Parsing and normalizing the JSON contracts exchanged with the models.
//!
//! Models are asked for strict JSON, but local CLIs and smaller models drift.
//! The parsers therefore accept JSON, fenced JSON and the `### FILE:` markdown
//! protocol, and fail loudly with a useful preview when nothing matches.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTask {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub objective: String,
    #[serde(default, alias = "acceptanceCriteria", alias = "criteria")]
    pub acceptance_criteria: Vec<String>,
    #[serde(default, alias = "contextFiles")]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

impl PlanTask {
    pub fn normalized_file(&self) -> String {
        self.file
            .trim()
            .trim_matches('`')
            .trim()
            .trim_start_matches("./")
            .replace('\\', "/")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub tasks: Vec<PlanTask>,
}

impl Plan {
    pub fn is_complete(&self) -> bool {
        self.status.eq_ignore_ascii_case("complete")
            || self.status.eq_ignore_ascii_case("done")
            || self.status.eq_ignore_ascii_case("tamam")
    }
}

#[derive(Debug, Clone, Default)]
pub struct Proposal {
    pub file: String,
    pub summary: String,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct Verdict {
    pub approved: bool,
    pub score: f32,
    pub summary: String,
    pub issues: Vec<String>,
    pub revision: String,
}

/// Extracts the first well-formed JSON object/array from a model answer.
pub fn extract_json(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }

    // Fenced blocks first: ```json ... ```
    for fence in ["```json", "```JSON", "```"] {
        if let Some(start) = trimmed.find(fence) {
            let after = &trimmed[start + fence.len()..];
            if let Some(end) = after.find("```") {
                if let Ok(value) = serde_json::from_str::<Value>(after[..end].trim()) {
                    return Some(value);
                }
            }
        }
    }

    let mut tried = 0usize;
    for (idx, _) in trimmed.match_indices('{').take(24) {
        tried += 1;
        if let Some(slice) = balanced_slice(trimmed, idx) {
            if let Ok(value) = serde_json::from_str::<Value>(slice) {
                return Some(value);
            }
        }
    }
    if tried == 0 {
        for (idx, _) in trimmed.match_indices('[').take(4) {
            if let Some(slice) = balanced_slice(trimmed, idx) {
                if let Ok(value) = serde_json::from_str::<Value>(slice) {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// Returns the substring starting at `start` that closes the JSON object.
fn balanced_slice(raw: &str, start: usize) -> Option<&str> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in raw[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..start + offset + ch.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

pub fn parse_plan(raw: &str) -> Result<Plan, String> {
    let value = extract_json(raw)
        .ok_or_else(|| format!("Komutan geçerli JSON üretmedi: {}", preview(raw)))?;

    let mut plan: Plan = if value.is_array() {
        Plan {
            status: "continue".to_string(),
            summary: String::new(),
            tasks: serde_json::from_value(value)
                .map_err(|e| format!("Görev listesi şeması hatalı: {e}"))?,
        }
    } else {
        serde_json::from_value(value).map_err(|e| format!("Görev planı şeması hatalı: {e}"))?
    };

    plan.tasks.retain(|task| !task.file.trim().is_empty());

    let mut counter = 0usize;
    for task in &mut plan.tasks {
        counter += 1;
        task.file = task.normalized_file();
        if task.id.trim().is_empty() {
            task.id = format!("T{counter}");
        }
        if task.title.trim().is_empty() {
            task.title = task.file.clone();
        }
        if task.objective.trim().is_empty() {
            task.objective = task.title.clone();
        }
    }

    Ok(plan)
}

pub fn parse_proposal(raw: &str, fallback_file: &str) -> Result<Proposal, String> {
    if let Some(value) = extract_json(raw) {
        let content = value
            .get("content")
            .or_else(|| value.get("code"))
            .or_else(|| value.get("fileContent"))
            .and_then(Value::as_str);
        if let Some(content) = content {
            if !content.trim().is_empty() {
                let file = value
                    .get("file")
                    .or_else(|| value.get("path"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| fallback_file.to_string());
                return Ok(Proposal {
                    file: normalize_file(&file, fallback_file),
                    summary: value
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    content: content.to_string(),
                });
            }
        }
    }

    let file = find_header(raw, &["### FILE:", "## FILE:", "FILE:"])
        .unwrap_or_else(|| fallback_file.to_string());
    let summary =
        find_header(raw, &["### SUMMARY:", "## SUMMARY:", "SUMMARY:"]).unwrap_or_default();
    let (_, content) =
        extract_fence(raw).ok_or_else(|| format!("İşçi kod bloğu üretmedi: {}", preview(raw)))?;

    Ok(Proposal {
        file: normalize_file(&file, fallback_file),
        summary,
        content,
    })
}

pub fn parse_verdict(raw: &str) -> Verdict {
    if let Some(value) = extract_json(raw) {
        let approved = value
            .get("approved")
            .and_then(Value::as_bool)
            .or_else(|| {
                value
                    .get("verdict")
                    .or_else(|| value.get("decision"))
                    .and_then(Value::as_str)
                    .map(|text| {
                        let lower = text.to_lowercase();
                        (lower.contains("approve")
                            || lower.contains("onay")
                            || lower.contains("accept")
                            || lower.contains("pass"))
                            && !lower.contains("reject")
                            && !lower.contains("red")
                    })
            })
            .unwrap_or(false);

        let issues = value
            .get("issues")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| match item {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        return Verdict {
            approved,
            score: value
                .get("score")
                .and_then(Value::as_f64)
                .map(|s| s as f32)
                .unwrap_or(if approved { 8.0 } else { 0.0 }),
            summary: value
                .get("summary")
                .or_else(|| value.get("reasoning"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            issues,
            revision: value
                .get("revision")
                .or_else(|| value.get("feedback"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
    }

    let lower = raw.to_lowercase();
    let rejected = ["reject", "redded", "revizyon", "ret edil"]
        .iter()
        .any(|needle| lower.contains(needle));
    let approved = ["approve", "onay", "lgtm", "kabul"]
        .iter()
        .any(|needle| lower.contains(needle));

    Verdict {
        approved: approved && !rejected,
        score: if approved && !rejected { 7.0 } else { 0.0 },
        summary: preview(raw),
        issues: if rejected {
            vec![preview(raw)]
        } else {
            Vec::new()
        },
        revision: if approved && !rejected {
            String::new()
        } else {
            preview(raw)
        },
    }
}

fn normalize_file(candidate: &str, fallback: &str) -> String {
    let cleaned = candidate
        .trim()
        .trim_matches('`')
        .trim()
        .trim_start_matches("./")
        .replace('\\', "/");
    if cleaned.is_empty() {
        fallback.trim().trim_start_matches("./").to_string()
    } else {
        cleaned
    }
}

fn find_header(raw: &str, prefixes: &[&str]) -> Option<String> {
    for line in raw.lines() {
        let trimmed = line.trim();
        for prefix in prefixes {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let value = rest.trim().trim_matches('`').trim().to_string();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// Returns the longest fenced code block: `(language, content)`.
fn extract_fence(raw: &str) -> Option<(String, String)> {
    let mut best: Option<(String, String)> = None;
    let mut search_from = 0usize;

    while let Some(open_rel) = raw[search_from..].find("```") {
        let open = search_from + open_rel;
        let after_open = &raw[open + 3..];
        let newline = match after_open.find('\n') {
            Some(pos) => pos,
            None => break,
        };
        let language = after_open[..newline].trim().to_string();
        let body_start = open + 3 + newline + 1;
        let rest = &raw[body_start..];
        let close_rel = match rest.find("```") {
            Some(pos) => pos,
            None => break,
        };
        let content = rest[..close_rel].trim_end().to_string();
        if !content.trim().is_empty() {
            let better = best
                .as_ref()
                .map(|(_, current)| content.len() > current.len())
                .unwrap_or(true);
            if better {
                best = Some((language, content));
            }
        }
        search_from = body_start + close_rel + 3;
    }

    best
}

fn preview(raw: &str) -> String {
    let trimmed = raw.trim();
    let cut: String = trimmed.chars().take(400).collect();
    if trimmed.chars().count() > 400 {
        format!("{cut}…")
    } else {
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_from_fenced_json_with_prose() {
        let raw = r#"Elbette, plan şu:
```json
{"status":"continue","summary":"başlangıç","tasks":[
  {"id":"T1","title":"Ana dosya","file":"./src/main.rs","objective":"Başlat",
   "acceptanceCriteria":["derlenir"],"contextFiles":["Cargo.toml"]}
]}
```
Uygulayabiliriz."#;
        let plan = parse_plan(raw).expect("plan parse");
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].file, "src/main.rs");
        assert_eq!(plan.tasks[0].acceptance_criteria, vec!["derlenir"]);
        assert!(!plan.is_complete());
    }

    #[test]
    fn plan_bare_array() {
        let raw = r#"[{"title":"A","file":"a.rs","objective":"x"}]"#;
        let plan = parse_plan(raw).expect("array plan");
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].id, "T1");
    }

    #[test]
    fn proposal_markdown_protocol() {
        let raw = "### FILE: src/lib.rs\n### SUMMARY: eklendi\n```rust\nfn main() {\n    println!(\"}\");\n}\n```\n";
        let proposal = parse_proposal(raw, "fallback.rs").expect("proposal");
        assert_eq!(proposal.file, "src/lib.rs");
        assert_eq!(proposal.summary, "eklendi");
        assert!(proposal.content.contains("println!"));
    }

    #[test]
    fn proposal_json_with_braces_in_string() {
        let raw = r#"{"file":"ui/app.js","summary":"ok","content":"const t = \"{x}\";\nif (a) { b(); }\n"}"#;
        let proposal = parse_proposal(raw, "fallback.js").expect("json proposal");
        assert_eq!(proposal.file, "ui/app.js");
        assert!(proposal.content.contains("{x}"));
    }

    #[test]
    fn proposal_without_code_fails() {
        assert!(parse_proposal("Sadece açıklama var.", "a.rs").is_err());
    }

    #[test]
    fn verdict_variants() {
        let approved = parse_verdict(r#"{"approved":true,"score":9,"summary":"iyi","issues":[]}"#);
        assert!(approved.approved);
        assert_eq!(approved.score, 9.0);

        let rejected = parse_verdict(
            r#"{"approved":false,"score":3,"summary":"eksik","issues":["import yok"],"revision":"import ekle"}"#,
        );
        assert!(!rejected.approved);
        assert_eq!(rejected.issues, vec!["import yok"]);
        assert_eq!(rejected.revision, "import ekle");

        let wordy = parse_verdict("Karar: APPROVE, sorun yok.");
        assert!(wordy.approved);
    }

    #[test]
    fn extract_json_survives_braces_in_strings() {
        let raw = r#"öncesi {"a":"} { içinde","b":{"c":1}} sonrası"#;
        let value = extract_json(raw).expect("json");
        assert_eq!(value["b"]["c"], 1);
    }
}
