//! Local IDE / CLI bridge.
//!
//! Two transports are supported:
//!  * **HTTP** – a localhost bridge (e.g. `opencode serve`, a VS Code extension
//!    HTTP shim, a CDP sidecar) that accepts the generic JSON contract.
//!  * **CLI** – a local agent CLI (`opencode run`, `claude -p`, `cursor-agent -p`)
//!    executed with `tokio::process::Command`, prompt via argv or stdin.
//!
//! Generic HTTP contract (request):
//! `{ "model": "...", "system": "...", "prompt": "...", "stream": false }`
//! Accepted response shapes: `{content}`, `{text}`, `{output}`, `{response}`,
//! `{message:{content}}`, `{choices:[{message:{content}}]}`, `{parts:[{text}]}`,
//! `{content:[{text}]}`.

use crate::config::{IdeKind, ProviderConfig};
use crate::provider::ModelProvider;
use crate::util::snippet;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

enum Transport {
    Http {
        endpoint: String,
        bearer: Option<String>,
    },
    Cli {
        program: String,
        args: Vec<String>,
        prompt_via_stdin: bool,
        /// Built-in CLIs take `--model <id>`; a user-defined CLI has its own
        /// conventions and its arguments must never be rewritten.
        inject_model: bool,
    },
}

/// Largest single argv entry we pass. Linux caps one argument at 128 KiB
/// (MAX_ARG_STRLEN) and Windows caps the whole command line at ~32 K chars.
const MAX_ARGV_BYTES: usize = if cfg!(windows) { 30_000 } else { 120_000 };

/// Resolves the binary for a built-in CLI bridge.
///
/// GUI applications often start with a PATH that lacks the shell's additions,
/// so besides the bare program name we probe well-known install locations.
/// `OPENCODE_BIN`, `CLAUDE_BIN` and `CURSOR_AGENT_BIN` override the search.
pub fn resolve_program(kind: IdeKind) -> String {
    let program = match kind.default_cli() {
        Some((program, _)) => program,
        None => return String::new(),
    };

    let env_key = match kind {
        IdeKind::Opencode => "OPENCODE_BIN",
        IdeKind::ClaudeCode => "CLAUDE_BIN",
        IdeKind::CursorAgent => "CURSOR_AGENT_BIN",
        _ => return program.to_string(),
    };

    if let Ok(value) = std::env::var(env_key) {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }

    let candidates: &[&str] = match kind {
        IdeKind::Opencode => &[
            "/usr/lib/opencode-desktop/opencode-cli",
            "/usr/local/bin/opencode",
            "/usr/local/bin/opencode-cli",
            "~/.opencode/bin/opencode",
            "~/.local/bin/opencode",
            "~/.local/bin/opencode-cli",
            "/opt/opencode/bin/opencode",
            "/snap/bin/opencode",
        ],
        IdeKind::ClaudeCode => &[
            "~/.claude/local/claude",
            "~/.local/bin/claude",
            "/usr/local/bin/claude",
            "/usr/bin/claude",
        ],
        IdeKind::CursorAgent => &[
            "~/.local/bin/cursor-agent",
            "~/.local/bin/cursor",
            "/usr/local/bin/cursor-agent",
            "/usr/local/bin/cursor",
            "/usr/bin/cursor",
            "/opt/Cursor/cursor",
        ],
        _ => &[],
    };

    // 1. Whatever the user's PATH resolves (the most specific, least surprising).
    if let Some(found) = find_in_path(program) {
        return found;
    }

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();

    // 2. Claude desktop bundles its own CLI: ~/.config/Claude/claude-code/<ver>/<hash>/claude.
    if kind == IdeKind::ClaudeCode && !home.is_empty() {
        if let Some(bin) =
            newest_bundled_claude(&Path::new(&home).join(".config/Claude/claude-code"))
        {
            return bin;
        }
    }

    // 3. Well-known install locations.
    for candidate in candidates {
        let path = match candidate.strip_prefix("~/") {
            Some(rest) if !home.is_empty() => format!("{home}/{rest}"),
            _ => (*candidate).to_string(),
        };
        if Path::new(&path).is_file() {
            return path;
        }
    }

    program.to_string()
}

/// Looks `program` up in `PATH` without spawning anything.
fn find_in_path(program: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    std::env::split_paths(&paths)
        .flat_map(|dir| {
            suffixes
                .iter()
                .map(move |ext| dir.join(format!("{program}{ext}")))
        })
        .find(|candidate| candidate.is_file())
        .map(|p| p.to_string_lossy().to_string())
}

/// Picks the `claude` binary of the highest bundled version directory.
fn newest_bundled_claude(base: &Path) -> Option<String> {
    let version_key = |name: &str| -> Vec<u64> {
        name.split(|c: char| !c.is_ascii_digit())
            .filter_map(|part| part.parse().ok())
            .collect()
    };

    let mut versions: Vec<_> = std::fs::read_dir(base).ok()?.flatten().collect();
    versions
        .sort_by_key(|entry| std::cmp::Reverse(version_key(&entry.file_name().to_string_lossy())));

    versions.iter().find_map(|version| {
        std::fs::read_dir(version.path())
            .ok()?
            .flatten()
            .map(|hash| hash.path().join("claude"))
            .find(|bin| bin.is_file())
            .map(|bin| bin.to_string_lossy().to_string())
    })
}

pub struct IdeBridgeProvider {
    client: reqwest::Client,
    transport: Transport,
    model: String,
    cwd: std::path::PathBuf,
    timeout: Duration,
    label: String,
}

impl IdeBridgeProvider {
    pub fn new(cfg: &ProviderConfig, project_dir: &Path) -> Result<Self, String> {
        let endpoint = cfg
            .ide_endpoint
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let transport = if let Some(endpoint) = endpoint {
            if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
                return Err(
                    "Yerel soket köprüsü bu sürümde desteklenmiyor. http://127.0.0.1:PORT endpoint'i \
                     veya CLI modu kullan."
                        .into(),
                );
            }
            Transport::Http {
                endpoint: endpoint.trim_end_matches('/').to_string(),
                // Only a key typed for this bridge; never an env key meant for a cloud API.
                bearer: cfg.explicit_api_key(),
            }
        } else {
            match cfg.ide_kind {
                IdeKind::CustomHttp => {
                    return Err(
                        "HTTP köprüsü için endpoint gerekli (http://127.0.0.1:PORT).".into(),
                    )
                }
                IdeKind::CustomCli => {
                    let program = cfg
                        .ide_command
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or("Custom CLI için komut adı gerekli.")?
                        .to_string();
                    let mut args = cfg.ide_args.clone();
                    // `{prompt}` placeholder is replaced at call time.
                    if !args.iter().any(|a| a.contains("{prompt}")) {
                        args.push("{prompt}".to_string());
                    }
                    Transport::Cli {
                        program,
                        args,
                        prompt_via_stdin: false,
                        inject_model: false,
                    }
                }
                kind => {
                    let (_, base_args) = kind
                        .default_cli()
                        .ok_or_else(|| format!("{} için CLI tanımı yok.", kind.as_str()))?;
                    let mut args: Vec<String> = base_args.iter().map(|s| s.to_string()).collect();
                    if !cfg.model.trim().is_empty() {
                        args.push("--model".into());
                        args.push(cfg.model.trim().to_string());
                    }
                    args.extend(cfg.ide_args.iter().cloned());
                    Transport::Cli {
                        program: resolve_program(kind),
                        args,
                        prompt_via_stdin: kind.prompt_via_stdin(),
                        inject_model: true,
                    }
                }
            }
        };

        let label = match &transport {
            Transport::Http { endpoint, .. } => format!(
                "IDE · HTTP bridge · {}{}",
                endpoint,
                if cfg.model.trim().is_empty() {
                    String::new()
                } else {
                    format!(" · {}", cfg.model.trim())
                }
            ),
            Transport::Cli { program, .. } => format!(
                "IDE · {} · {}{}",
                cfg.ide_kind.as_str(),
                program,
                if cfg.model.trim().is_empty() {
                    String::new()
                } else {
                    format!(" · {}", cfg.model.trim())
                }
            ),
        };

        let client = reqwest::Client::builder()
            .user_agent("swarm-command-center/0.1")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(cfg.timeout_secs.max(30)))
            .build()
            .map_err(|e| format!("HTTP istemcisi oluşturulamadı: {e}"))?;

        Ok(Self {
            client,
            transport,
            model: cfg.model.trim().to_string(),
            cwd: project_dir.to_path_buf(),
            timeout: Duration::from_secs(cfg.timeout_secs.max(30)),
            label,
        })
    }

    /// Clones this provider with an alternate model name for fallback execution.
    pub fn with_model(&self, new_model: &str) -> Self {
        let new_model = new_model.trim().to_string();
        let (new_transport, new_label) = match &self.transport {
            Transport::Http { endpoint, bearer } => {
                let lbl = format!(
                    "IDE · HTTP bridge · {}{}",
                    endpoint,
                    if new_model.is_empty() {
                        String::new()
                    } else {
                        format!(" · {new_model}")
                    }
                );
                (
                    Transport::Http {
                        endpoint: endpoint.clone(),
                        bearer: bearer.clone(),
                    },
                    lbl,
                )
            }
            Transport::Cli {
                program,
                args,
                prompt_via_stdin,
                inject_model,
            } => {
                let mut updated_args = Vec::with_capacity(args.len() + 2);
                if *inject_model {
                    let mut skip_next = false;
                    for arg in args {
                        if skip_next {
                            skip_next = false;
                        } else if arg == "--model" {
                            skip_next = true;
                        } else if !arg.starts_with("--model=") {
                            updated_args.push(arg.clone());
                        }
                    }
                    if !new_model.is_empty() {
                        updated_args.push("--model".into());
                        updated_args.push(new_model.clone());
                    }
                } else {
                    updated_args.clone_from(args);
                }
                let lbl = format!(
                    "IDE · CLI · {}{}",
                    program,
                    if new_model.is_empty() {
                        String::new()
                    } else {
                        format!(" · {new_model}")
                    }
                );
                (
                    Transport::Cli {
                        program: program.clone(),
                        args: updated_args,
                        prompt_via_stdin: *prompt_via_stdin,
                        inject_model: *inject_model,
                    },
                    lbl,
                )
            }
        };

        Self {
            client: self.client.clone(),
            transport: new_transport,
            model: new_model,
            cwd: self.cwd.clone(),
            timeout: self.timeout,
            label: new_label,
        }
    }

    async fn via_http(
        &self,
        endpoint: &str,
        bearer: Option<&str>,
        prompt: &str,
        system_prompt: &str,
    ) -> Result<String, String> {
        let body = json!({
            "model": self.model,
            "system": system_prompt,
            "prompt": prompt,
            "stream": false
        });

        let mut req = self.client.post(endpoint).json(&body);
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await.map_err(|e| {
            format!("IDE köprüsüne ulaşılamadı ({endpoint}): {e}. Köprü süreci çalışıyor mu?")
        })?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "IDE köprüsü HTTP {} döndürdü: {}",
                status.as_u16(),
                snippet(&text, 600)
            ));
        }

        let value: Value = serde_json::from_str(&text)
            .map_err(|e| format!("IDE köprüsü JSON döndürmedi ({e}): {}", snippet(&text, 400)))?;

        extract_text(&value).ok_or_else(|| {
            format!(
                "IDE köprüsü yanıtında metin bulunamadı. Beklenen alanlardan biri: content, text, \
                 output, response, message.content, choices[0].message.content, parts[].text. Gelen: {}",
                snippet(&text, 400)
            )
        })
    }

    async fn via_cli(
        &self,
        program: &str,
        args: &[String],
        via_stdin: bool,
        prompt: &str,
        system_prompt: &str,
    ) -> Result<String, String> {
        let full_prompt = if system_prompt.trim().is_empty() {
            prompt.to_string()
        } else {
            format!("[SİSTEM TALİMATI]\n{system_prompt}\n\n[GÖREV]\n{prompt}")
        };

        let mut final_args: Vec<String> = Vec::with_capacity(args.len() + 1);
        let mut prompt_included = false;
        for arg in args {
            if arg.contains("{prompt}") {
                final_args.push(arg.replace("{prompt}", &full_prompt));
                prompt_included = true;
            } else {
                final_args.push(arg.clone());
            }
        }
        if !via_stdin && !prompt_included {
            final_args.push(full_prompt.clone());
        }

        if let Some(too_big) = final_args.iter().find(|a| a.len() > MAX_ARGV_BYTES) {
            return Err(format!(
                "Prompt ({} KB) komut satırı sınırını aşıyor. Bu CLI promptu argüman olarak \
                 aldığı için dosya bağlamı küçültülmeli (Gelişmiş → Dosya bağlamı) ya da \
                 prompt'u stdin'den okuyan Claude Code / HTTP köprüsü kullanılmalı.",
                too_big.len() / 1024
            ));
        }

        let mut cmd = Command::new(program);
        cmd.args(&final_args)
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if via_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "'{program}' başlatılamadı: {e}. Program kurulu mu ve PATH'te mi? \
                 (IDE köprüsü ayarlarından komutu değiştirebilirsin.)"
            )
        })?;

        // Feed stdin from a task: a child that never reads it must not block
        // us past the timeout. A broken pipe just means the child exited early,
        // and its own error output is more useful than ours.
        if via_stdin {
            if let Some(mut stdin) = child.stdin.take() {
                let payload = full_prompt.clone();
                tokio::spawn(async move {
                    let _ = stdin.write_all(payload.as_bytes()).await;
                    let _ = stdin.shutdown().await;
                });
            }
        }

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                format!(
                    "'{program}' {} saniye içinde yanıt vermedi; süreç öldürüldü.",
                    self.timeout.as_secs()
                )
            })?
            .map_err(|e| format!("CLI çıktısı okunamadı: {e}"))?;

        let stdout = strip_ansi(&String::from_utf8_lossy(&output.stdout));
        let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));

        if !output.status.success() {
            let detail = if stderr.trim().is_empty() {
                snippet(&stdout, 900)
            } else {
                snippet(&stderr, 900)
            };
            return Err(format!(
                "'{program}' hata kodu {:?} ile çıktı: {detail}",
                output.status.code()
            ));
        }

        let answer = stdout.trim();
        if answer.is_empty() {
            if !stderr.trim().is_empty() {
                return Err(format!(
                    "'{program}' boş yanıt döndürdü. stderr: {}",
                    snippet(&stderr, 900)
                ));
            }
            return Err(format!("'{program}' boş yanıt döndürdü."));
        }

        Ok(answer.to_string())
    }
}

#[async_trait]
impl ModelProvider for IdeBridgeProvider {
    fn label(&self) -> String {
        self.label.clone()
    }

    async fn complete(&self, prompt: &str, system_prompt: &str) -> Result<String, String> {
        match &self.transport {
            Transport::Http { endpoint, bearer } => {
                self.via_http(endpoint, bearer.as_deref(), prompt, system_prompt)
                    .await
            }
            Transport::Cli {
                program,
                args,
                prompt_via_stdin,
                ..
            } => {
                self.via_cli(program, args, *prompt_via_stdin, prompt, system_prompt)
                    .await
            }
        }
    }
}

fn extract_text(value: &Value) -> Option<String> {
    for key in ["content", "text", "output", "response", "result", "answer"] {
        if let Some(s) = value[key].as_str() {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    for path in [
        &value["message"]["content"],
        &value["data"]["content"],
        &value["choices"][0]["message"]["content"],
        &value["choices"][0]["text"],
    ] {
        if let Some(s) = path.as_str() {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    for array_path in [
        &value["parts"],
        &value["content"],
        &value["message"]["content"],
    ] {
        if let Some(parts) = array_path.as_array() {
            let joined = parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("");
            if !joined.trim().is_empty() {
                return Some(joined);
            }
        }
    }
    None
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IdeKind, ProviderSource};

    fn serve_once(body: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn http_bridge_roundtrip() {
        let base = serve_once(r#"{"content":"IDE-YANIT"}"#);
        let cfg = ProviderConfig {
            source: ProviderSource::Ide,
            ide_kind: IdeKind::CustomHttp,
            ide_endpoint: Some(base),
            ..Default::default()
        };
        let provider = IdeBridgeProvider::new(&cfg, std::path::Path::new(".")).expect("provider");
        assert_eq!(provider.complete("hi", "sys").await.unwrap(), "IDE-YANIT");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_cli_placeholder() {
        let cfg = ProviderConfig {
            source: ProviderSource::Ide,
            ide_kind: IdeKind::CustomCli,
            ide_command: Some("printf".into()),
            ide_args: vec!["%s".into(), "{prompt}".into()],
            ..Default::default()
        };
        let provider = IdeBridgeProvider::new(&cfg, std::path::Path::new(".")).expect("provider");
        let output = provider.complete("merhaba", "sistem").await.unwrap();
        assert!(output.contains("merhaba"));
        assert!(output.contains("sistem"));
    }

    #[test]
    fn rejects_unix_socket_paths() {
        let cfg = ProviderConfig {
            source: ProviderSource::Ide,
            ide_endpoint: Some("/tmp/ide.sock".into()),
            ..Default::default()
        };
        assert!(IdeBridgeProvider::new(&cfg, std::path::Path::new(".")).is_err());
    }

    #[test]
    fn resolves_claude_binary_on_system() {
        let bin = resolve_program(IdeKind::ClaudeCode);
        println!("Resolved Claude: {}", bin);
        assert!(!bin.is_empty());
    }

    #[test]
    fn custom_cli_arguments_are_never_rewritten_for_fallback_models() {
        let cfg = ProviderConfig {
            source: ProviderSource::Ide,
            ide_kind: IdeKind::CustomCli,
            ide_command: Some("aider".into()),
            ide_args: vec!["--model".into(), "mine".into(), "{prompt}".into()],
            ..Default::default()
        };
        let base = IdeBridgeProvider::new(&cfg, std::path::Path::new(".")).unwrap();
        let alt = base.with_model("other");
        match alt.transport {
            Transport::Cli { ref args, .. } => {
                assert_eq!(args, &["--model", "mine", "{prompt}"]);
            }
            _ => panic!("expected CLI transport"),
        }
    }

    #[test]
    fn builtin_cli_swaps_the_model_flag() {
        let cfg = ProviderConfig {
            source: ProviderSource::Ide,
            ide_kind: IdeKind::Opencode,
            model: "a/one".into(),
            ..Default::default()
        };
        let alt = IdeBridgeProvider::new(&cfg, std::path::Path::new("."))
            .unwrap()
            .with_model("b/two");
        match alt.transport {
            Transport::Cli { ref args, .. } => {
                assert_eq!(args.iter().filter(|a| *a == "--model").count(), 1);
                assert_eq!(args.last().map(String::as_str), Some("b/two"));
            }
            _ => panic!("expected CLI transport"),
        }
    }

    #[test]
    fn bridge_never_receives_env_api_keys() {
        std::env::set_var("OPENROUTER_API_KEY", "sk-should-not-leak");
        let cfg = ProviderConfig {
            source: ProviderSource::Ide,
            ide_kind: IdeKind::CustomHttp,
            ide_endpoint: Some("http://127.0.0.1:9".into()),
            ..Default::default()
        };
        let provider = IdeBridgeProvider::new(&cfg, std::path::Path::new(".")).unwrap();
        match provider.transport {
            Transport::Http { bearer, .. } => assert!(bearer.is_none()),
            _ => panic!("expected HTTP transport"),
        }
        std::env::remove_var("OPENROUTER_API_KEY");
    }
}
