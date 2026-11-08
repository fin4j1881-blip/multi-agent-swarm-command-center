//! Parsing and normalizing the JSON contracts exchanged with the models.
//!
//! Models are asked for strict JSON, but local CLIs and smaller models drift.
//! The parsers therefore accept JSON, fenced JSON and the `### FILE:` markdown
//! protocol, and fail loudly with a useful preview when nothing matches.

use crate::util::snippet;
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
    // The architect prompt documents snake_case keys, so accept both spellings.
    #[serde(
        default,
        alias = "acceptance_criteria",
        alias = "criteria",
        deserialize_with = "string_list"
    )]
    pub acceptance_criteria: Vec<String>,
    #[serde(default, alias = "context_files", deserialize_with = "string_list")]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

/// Accepts `["a","b"]`, a single `"a"` or `null`; models are sloppy about this.
fn string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) if !text.trim().is_empty() => vec![text],
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::String(text) if !text.trim().is_empty() => Some(text),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    })
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
        .ok_or_else(|| format!("Komutan geçerli JSON üretmedi: {}", snippet(raw, 400)))?;

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
    // The documented protocol is markdown. When its header is present it wins,
    // otherwise a JSON file being *written* (which may itself contain a
    // `content` key) could be mistaken for a JSON envelope.
    let has_header = find_header(raw, &["### FILE:", "## FILE:", "FILE:"]).is_some();
    if has_header {
        if let Ok(proposal) = parse_markdown_proposal(raw, fallback_file) {
            return Ok(proposal);
        }
    }

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
                    .unwrap_or(fallback_file);
                return Ok(Proposal {
                    file: normalize_file(file, fallback_file),
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

    parse_markdown_proposal(raw, fallback_file)
}

fn parse_markdown_proposal(raw: &str, fallback_file: &str) -> Result<Proposal, String> {
    let file = find_header(raw, &["### FILE:", "## FILE:", "FILE:"])
        .unwrap_or_else(|| fallback_file.to_string());
    let summary =
        find_header(raw, &["### SUMMARY:", "## SUMMARY:", "SUMMARY:"]).unwrap_or_default();
    let content = extract_fence(raw)
        .ok_or_else(|| format!("İşçi kod bloğu üretmedi: {}", snippet(raw, 400)))?;

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
            .and_then(|v| match v {
                Value::Bool(b) => Some(*b),
                Value::String(text) => Some(text.trim().eq_ignore_ascii_case("true")),
                _ => None,
            })
            .or_else(|| {
                value
                    .get("verdict")
                    .or_else(|| value.get("decision"))
                    .and_then(Value::as_str)
                    .map(|text| text_says_approved(&text.to_lowercase(), true))
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
                .map(|s| s.clamp(0.0, 10.0) as f32)
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

    // Free text: anything ambiguous or negated counts as a rejection.
    let approved = text_says_approved(&raw.to_lowercase(), false);
    Verdict {
        approved,
        score: if approved { 7.0 } else { 0.0 },
        summary: snippet(raw, 400),
        issues: if approved {
            Vec::new()
        } else {
            vec![snippet(raw, 400)]
        },
        revision: if approved {
            String::new()
        } else {
            snippet(raw, 400)
        },
    }
}

/// `lower` must already be lower-cased. `short_field` is true for a dedicated
/// `verdict`/`decision` value ("accepted", "pass"); in free prose only explicit
/// approvals count, because words like "kabul kriterleri" show up in rejections.
fn text_says_approved(lower: &str, short_field: bool) -> bool {
    const NEGATIVE: &[&str] = &[
        "reject",
        "redded",
        "reddet",
        "revizyon",
        "ret edil",
        "not approv",
        "disapprov",
        "onaylanmad",
        "onaylamıyor",
        "onaylamiyor",
        "not accept",
        "kabul edilemez",
        "fail",
    ];
    const STRONG: &[&str] = &["approve", "lgtm", "onaylandı", "onaylıyorum", "onayladı"];
    const WEAK: &[&str] = &["onay", "accept", "kabul", "pass"];

    !NEGATIVE.iter().any(|needle| lower.contains(needle))
        && (STRONG.iter().any(|needle| lower.contains(needle))
            || (short_field && WEAK.iter().any(|needle| lower.contains(needle))))
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

/// Returns the body of the longest fenced code block.
///
/// Fences are matched by length, so a ```` outer fence may safely contain
/// ``` blocks (typical when the generated file is itself markdown). A closing
/// fence must sit on its own line.
fn extract_fence(raw: &str) -> Option<String> {
    let mut best: Option<String> = None;
    // (fence length, collected body lines)
    let mut open: Option<(usize, Vec<&str>)> = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();

        match open.as_mut() {
            None => {
                if ticks >= 3 {
                    open = Some((ticks, Vec::new()));
                }
            }
            Some((fence_len, body)) => {
                if ticks >= *fence_len && trimmed.len() == ticks {
                    let content = body.join("\n").trim_end().to_string();
                    let better = !content.trim().is_empty()
                        && best.as_ref().map_or(true, |b| content.len() > b.len());
                    if better {
                        best = Some(content);
                    }
                    open = None;
                } else {
                    body.push(line);
                }
            }
        }
    }

    best
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

    #[test]
    fn plan_accepts_snake_case_keys_from_the_documented_schema() {
        let raw = r#"{"status":"continue","tasks":[{"id":"T1","title":"A","file":"a.rs",
            "objective":"x","acceptance_criteria":["derlenir"],"context_files":["Cargo.toml"]}]}"#;
        let plan = parse_plan(raw).unwrap();
        assert_eq!(plan.tasks[0].acceptance_criteria, vec!["derlenir"]);
        assert_eq!(plan.tasks[0].context_files, vec!["Cargo.toml"]);
    }

    #[test]
    fn plan_tolerates_string_instead_of_list() {
        let raw =
            r#"{"tasks":[{"file":"a.rs","acceptanceCriteria":"derlenir","contextFiles":null}]}"#;
        let plan = parse_plan(raw).unwrap();
        assert_eq!(plan.tasks[0].acceptance_criteria, vec!["derlenir"]);
        assert!(plan.tasks[0].context_files.is_empty());
    }

    #[test]
    fn longer_outer_fence_may_contain_inner_fences() {
        let raw =
            "### FILE: README.md\n````markdown\n# Başlık\n```bash\ncargo test\n```\nson\n````\n";
        let proposal = parse_proposal(raw, "x.md").unwrap();
        assert!(proposal.content.contains("```bash"));
        assert!(proposal.content.ends_with("son"));
    }

    #[test]
    fn unclosed_fence_is_a_failure_not_a_truncated_file() {
        let raw = "### FILE: a.rs\n```rust\nfn main() {\n";
        assert!(parse_proposal(raw, "a.rs").is_err());
    }

    #[test]
    fn json_file_with_content_key_is_not_mistaken_for_an_envelope() {
        let raw =
            "### FILE: data.json\n```json\n{\"content\": \"merhaba dünya\", \"id\": 1}\n```\n";
        let proposal = parse_proposal(raw, "data.json").unwrap();
        assert!(proposal.content.contains("\"id\": 1"));
    }

    #[test]
    fn negated_or_ambiguous_free_text_is_a_rejection() {
        assert!(!parse_verdict("Bu öneriyi onaylamıyorum.").approved);
        assert!(!parse_verdict("Not approved: missing import").approved);
        assert!(!parse_verdict("Kabul kriterleri karşılanmıyor.").approved);
        assert!(!parse_verdict("Belirsiz bir yanıt").approved);
        assert!(parse_verdict("LGTM").approved);
    }

    #[test]
    fn verdict_field_variants() {
        assert!(parse_verdict(r#"{"verdict":"accepted"}"#).approved);
        assert!(!parse_verdict(r#"{"verdict":"required changes"}"#).approved);
        assert!(parse_verdict(r#"{"approved":"true","score":42}"#).approved);
        assert_eq!(parse_verdict(r#"{"approved":true,"score":42}"#).score, 10.0);
    }
}
