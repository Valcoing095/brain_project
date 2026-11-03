use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::catalog::{ProviderEntry, ProviderKind};
use super::process::{run_command, run_command_streaming, CancelFlag, AGENT_TIMEOUT};
use crate::cursor_agent::AgentMode;

static WORKSPACE: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct AgentSelection {
    pub provider: ProviderEntry,
    pub model_id: String,
    pub cursor_mode: Option<AgentMode>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionState {
    #[default]
    Empty,
    /// Sesión con id explícito (Cursor `--resume`, OpenCode `-s`).
    WithId {
        provider_id: String,
        session_id: String,
    },
    /// El CLI continúa su última conversación (`--continue`, `-c`).
    Continued {
        provider_id: String,
    },
}

impl SessionState {
    fn id_for(&self, provider_id: &str) -> Option<&str> {
        match self {
            SessionState::WithId {
                provider_id: pid,
                session_id,
            } if pid == provider_id => Some(session_id),
            _ => None,
        }
    }

    fn continued_for(&self, provider_id: &str) -> bool {
        matches!(self, SessionState::Continued { provider_id: pid } if pid == provider_id)
    }
}

/// Eventos emitidos mientras el agente responde.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    /// El agente está razonando (texto parcial del pensamiento).
    Thinking,
    /// Fragmento incremental de la respuesta.
    Delta(String),
    /// Texto completo acumulado hasta ahora (reemplaza lo anterior).
    Replace(String),
}

pub fn init_workspace(default: PathBuf) {
    let _ = WORKSPACE.set(default);
}

pub fn workspace_dir() -> PathBuf {
    WORKSPACE
        .get()
        .cloned()
        .or_else(|| std::env::var("CURSOR_WORKSPACE").ok().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Envía un mensaje y devuelve la respuesta final. Para proveedores con streaming
/// va llamando `on_event` con fragmentos parciales.
pub fn send_message(
    selection: &AgentSelection,
    session: &mut SessionState,
    message: &str,
    cancel: &CancelFlag,
    mut on_event: impl FnMut(StreamEvent),
) -> Result<String, String> {
    let bin = PathBuf::from(&selection.provider.binary_path);
    let ws = workspace_dir();
    let model = selection.model_id.as_str();
    let pid = selection.provider.id.as_str();

    match selection.provider.kind {
        ProviderKind::CursorAgent => {
            send_cursor(&bin, &ws, selection, session, message, cancel, &mut on_event)
        }
        ProviderKind::Opencode => {
            send_opencode(&bin, &ws, selection, session, message, cancel, &mut on_event)
        }
        ProviderKind::ClaudeCode => {
            let mut args = vec!["-p", "--model", model];
            if session.continued_for(pid) {
                args.push("--continue");
            }
            args.push(message);
            let out = run_command(&bin, &args, &ws)?;
            mark_continued(session, pid);
            Ok(out)
        }
        ProviderKind::GeminiCli => {
            let mut args = vec!["-p"];
            if model != "default" {
                args.push("-m");
                args.push(model);
            }
            args.push(message);
            let out = run_command(&bin, &args, &ws)?;
            mark_continued(session, pid);
            Ok(out)
        }
        ProviderKind::Aider => {
            let mut args = vec!["--message", message, "--yes", "--no-git"];
            if model != "default" {
                args.push("--model");
                args.push(model);
            }
            let out = run_command(&bin, &args, &ws)?;
            mark_continued(session, pid);
            Ok(out)
        }
        ProviderKind::Ollama => {
            let out = run_command(&bin, &["run", model, message], &ws)?;
            mark_continued(session, pid);
            Ok(out)
        }
        ProviderKind::LlmCli => {
            let mut args: Vec<&str> = Vec::new();
            if model != "default" {
                args.extend(["-m", model]);
            }
            if session.continued_for(pid) {
                args.push("-c");
            }
            args.push(message);
            let out = run_command(&bin, &args, &ws)?;
            mark_continued(session, pid);
            Ok(out)
        }
        ProviderKind::CodexCli => {
            let mut args = vec!["exec", "--skip-git-repo-check"];
            if model != "default" {
                args.extend(["-m", model]);
            }
            args.push(message);
            let out = run_command(&bin, &args, &ws)?;
            mark_continued(session, pid);
            Ok(out)
        }
        ProviderKind::GenericPrint => {
            let out = run_command(&bin, &["-p", message], &ws)
                .or_else(|_| run_command(&bin, &["--print", message], &ws))
                .or_else(|_| run_command(&bin, &[message], &ws))?;
            mark_continued(session, pid);
            Ok(out)
        }
    }
}

fn mark_continued(session: &mut SessionState, provider_id: &str) {
    *session = SessionState::Continued {
        provider_id: provider_id.to_string(),
    };
}

/// Ruta al binario `cursor-agent` si `bin` es el shim `cursor` (mucho más lento).
fn cursor_direct_binary(bin: &Path) -> (PathBuf, bool) {
    let is_shim = bin.file_name().and_then(|s| s.to_str()) == Some("cursor");
    if !is_shim {
        return (bin.to_path_buf(), false);
    }
    let sibling = bin.with_file_name("cursor-agent");
    if sibling.is_file() {
        (sibling, false)
    } else {
        (bin.to_path_buf(), true)
    }
}

fn send_cursor(
    bin: &Path,
    ws: &Path,
    selection: &AgentSelection,
    session: &mut SessionState,
    message: &str,
    cancel: &CancelFlag,
    on_event: &mut impl FnMut(StreamEvent),
) -> Result<String, String> {
    let (bin, needs_agent_sub) = cursor_direct_binary(bin);
    let ws_str = ws.to_string_lossy().to_string();
    let pid = selection.provider.id.as_str();

    let mut args: Vec<String> = Vec::new();
    if needs_agent_sub {
        args.push("agent".into());
    }
    args.extend(
        [
            "--print",
            "--trust",
            "--output-format",
            "stream-json",
            "--stream-partial-output",
            "--workspace",
            ws_str.as_str(),
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    if selection.model_id != "default" {
        args.push("--model".into());
        args.push(selection.model_id.clone());
    }
    if let Some(id) = session.id_for(pid) {
        args.push("--resume".into());
        args.push(id.to_string());
    }
    if let Some(flag) = selection.cursor_mode.unwrap_or(AgentMode::Ask).cli_flag() {
        args.push("--mode".into());
        args.push(flag.into());
    }
    args.push(message.to_string());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let mut session_id: Option<String> = None;
    let mut accumulated = String::new();
    let mut final_result: Option<String> = None;
    let mut error_result: Option<String> = None;

    let stderr = run_command_streaming(&bin, &arg_refs, ws, AGENT_TIMEOUT, cancel, |line| {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        if session_id.is_none() {
            if let Some(id) = value.get("session_id").and_then(|v| v.as_str()) {
                session_id = Some(id.to_string());
            }
        }
        match value.get("type").and_then(|v| v.as_str()) {
            Some("thinking") => on_event(StreamEvent::Thinking),
            Some("assistant") => {
                let text = value
                    .pointer("/message/content")
                    .and_then(|c| c.as_array())
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                if text.is_empty() {
                    return;
                }
                if value.get("timestamp_ms").is_some() {
                    accumulated.push_str(&text);
                    on_event(StreamEvent::Delta(text));
                } else {
                    accumulated = text.clone();
                    on_event(StreamEvent::Replace(text));
                }
            }
            Some("result") => {
                let is_error = value
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let text = value
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if is_error {
                    error_result = Some(if text.is_empty() {
                        "El agente devolvió un error.".into()
                    } else {
                        text
                    });
                } else {
                    final_result = Some(text);
                }
            }
            _ => {}
        }
    });

    if let Some(id) = session_id {
        *session = SessionState::WithId {
            provider_id: pid.to_string(),
            session_id: id,
        };
    }

    if let Some(err) = error_result {
        return Err(err);
    }

    match stderr {
        Ok(_) => {}
        Err(e) if accumulated.is_empty() && final_result.is_none() => return Err(e),
        Err(_) => {}
    }

    let reply = final_result
        .filter(|r| !r.trim().is_empty())
        .unwrap_or(accumulated);
    if reply.trim().is_empty() {
        return Err("Cursor no devolvió texto.".into());
    }
    Ok(reply)
}

fn send_opencode(
    bin: &Path,
    ws: &Path,
    selection: &AgentSelection,
    session: &mut SessionState,
    message: &str,
    cancel: &CancelFlag,
    on_event: &mut impl FnMut(StreamEvent),
) -> Result<String, String> {
    let ws_str = ws.to_string_lossy().to_string();
    let pid = selection.provider.id.as_str();

    let mut args: Vec<&str> = vec!["run", "--auto", "--dir", ws_str.as_str(), "--format", "json"];
    if selection.model_id != "default" {
        args.extend(["-m", selection.model_id.as_str()]);
    }
    let existing = session.id_for(pid).map(|s| s.to_string());
    if let Some(id) = existing.as_deref() {
        args.extend(["-s", id]);
    }
    args.push(message);

    let mut session_id: Option<String> = None;
    let mut parts: Vec<String> = Vec::new();
    let mut error_text: Option<String> = None;

    let stderr = run_command_streaming(bin, &args, ws, AGENT_TIMEOUT, cancel, |line| {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        if session_id.is_none() {
            if let Some(id) = value.get("sessionID").and_then(|v| v.as_str()) {
                session_id = Some(id.to_string());
            }
        }
        match value.get("type").and_then(|v| v.as_str()) {
            Some("step_start") => on_event(StreamEvent::Thinking),
            Some("text") => {
                if let Some(text) = value.pointer("/part/text").and_then(|t| t.as_str()) {
                    if !text.trim().is_empty() {
                        parts.push(text.to_string());
                        on_event(StreamEvent::Replace(parts.join("\n\n")));
                    }
                }
            }
            Some("error") => {
                let msg = value
                    .pointer("/error/data/message")
                    .or_else(|| value.pointer("/error/message"))
                    .or_else(|| value.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("OpenCode devolvió un error.");
                error_text = Some(msg.to_string());
            }
            _ => {}
        }
    });

    if let Some(id) = session_id {
        *session = SessionState::WithId {
            provider_id: pid.to_string(),
            session_id: id,
        };
    }

    if let Some(err) = error_text {
        if parts.is_empty() {
            return Err(err);
        }
    }
    if let Err(e) = stderr {
        if parts.is_empty() {
            return Err(e);
        }
    }

    let reply = parts.join("\n\n");
    if reply.trim().is_empty() {
        return Err("OpenCode no devolvió texto.".into());
    }
    Ok(reply)
}
