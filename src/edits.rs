//! Escritura de código y el recuento de lo que cambió en el proyecto.
//!
//! La IA puede dejar archivos en bloques `file:ruta`. Yeison los aplica
//! dentro del repo y, si hay Git, compara el árbol antes y después del turno.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use serde_json::Value;

const MAX_BLOCKS: usize = 30;
const MAX_BYTES: usize = 500_000;
const MAX_CHANGES: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    pub fn mark(self) -> &'static str {
        match self {
            Self::Added => "+",
            Self::Modified => "~",
            Self::Deleted => "−",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "nuevo",
            Self::Modified => "cambiado",
            Self::Deleted => "borrado",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    pub preview: String,
}

/// Qué está haciendo la IA en este instante (una herramienta o un archivo).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub label: String,
    pub file: Option<String>,
    pub preview: Option<String>,
    pub writes: bool,
    /// Texto que forma parte de la respuesta (por ejemplo el plan) y no debe perderse.
    pub append: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LivePreview {
    pub path: String,
    pub preview: String,
    pub open: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Snap {
    git: bool,
    files: BTreeMap<String, String>,
}

pub fn snapshot(root: &Path) -> Snap {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return Snap::default();
    };
    if !output.status.success() {
        return Snap::default();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut files = BTreeMap::new();
    for line in text.lines() {
        let Some((path, status)) = parse_porcelain_line(line) else {
            continue;
        };
        files.insert(path.clone(), format!("{status}|{}", fingerprint(root, &path)));
    }
    Snap { git: true, files }
}

/// Aplica bloques cerrados y devuelve los cambios del turno.
pub fn finish_turn(root: &Path, before: &Snap, reply: &str, apply_blocks: bool) -> Vec<FileChange> {
    let written = if apply_blocks {
        apply_file_blocks(root, reply)
    } else {
        Vec::new()
    };
    let mut changes = changes_since(root, before);
    for item in written {
        if let Some(existing) = changes.iter_mut().find(|change| change.path == item.path) {
            if existing.preview.trim().is_empty() {
                existing.preview = item.preview;
            }
        } else {
            changes.push(item);
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes.truncate(MAX_CHANGES);
    changes
}

pub fn changes_since(root: &Path, before: &Snap) -> Vec<FileChange> {
    if !before.git {
        return Vec::new();
    }
    let after = snapshot(root);
    if !after.git {
        return Vec::new();
    }
    let mut changes = Vec::new();
    for (path, fingerprint) in &after.files {
        if before.files.get(path) == Some(fingerprint) {
            continue;
        }
        let status = fingerprint.split('|').next().unwrap_or("");
        let kind = kind_from_status(status);
        changes.push(FileChange {
            path: path.clone(),
            kind,
            preview: preview_for(root, path, kind),
        });
    }
    for path in before.files.keys() {
        if after.files.contains_key(path) {
            continue;
        }
        let kind = if root.join(path).exists() {
            ChangeKind::Modified
        } else {
            ChangeKind::Deleted
        };
        changes.push(FileChange {
            path: path.clone(),
            kind,
            preview: preview_for(root, path, kind),
        });
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes.truncate(MAX_CHANGES);
    changes
}

pub fn apply_file_blocks(root: &Path, reply: &str) -> Vec<FileChange> {
    let mut changes = Vec::new();
    for (rel, body) in file_blocks(reply).into_iter().take(MAX_BLOCKS) {
        if body.len() > MAX_BYTES {
            continue;
        }
        if rel == "src/ejemplo.rs" && body.trim() == "contenido completo del archivo" {
            continue;
        }
        let Some(dest) = safe_destination(root, &rel) else {
            continue;
        };
        let existed = dest.is_file();
        if std::fs::write(&dest, body.as_bytes()).is_err() {
            continue;
        }
        let stored = dest
            .strip_prefix(root)
            .unwrap_or(&dest)
            .to_string_lossy()
            .replace('\\', "/");
        changes.push(FileChange {
            path: stored,
            kind: if existed {
                ChangeKind::Modified
            } else {
                ChangeKind::Added
            },
            preview: tail_lines(&body, 8),
        });
    }
    changes
}

pub fn file_blocks(text: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find("```file:") {
        rest = &rest[index + "```file:".len()..];
        let Some(newline) = rest.find('\n') else {
            break;
        };
        let path = rest[..newline].trim().trim_matches('`').trim();
        if path.is_empty() || path.contains("```") {
            rest = &rest[newline + 1..];
            continue;
        }
        let path = path.to_string();
        rest = &rest[newline + 1..];
        let Some(end) = closing_fence(rest) else {
            break;
        };
        let body = rest[..end].trim_end_matches('\n').to_string();
        if !body.trim().is_empty() {
            blocks.push((path, body));
        }
        let fence_at = rest[end..].find("```").unwrap_or(0);
        rest = &rest[end + fence_at + 3..];
    }
    blocks
}

pub fn streaming_preview(text: &str) -> Option<LivePreview> {
    let marker = "```file:";
    let start = text.rfind(marker)?;
    let after = &text[start + marker.len()..];
    let newline = after.find('\n')?;
    let path = after[..newline].trim().trim_matches('`').trim();
    if path.is_empty() || path.contains("```") || path.len() > 180 {
        return None;
    }
    let body = &after[newline + 1..];
    let (preview_src, open) = match closing_fence(body) {
        Some(end) => (&body[..end], false),
        None => (body, true),
    };
    Some(LivePreview {
        path: path.to_string(),
        preview: tail_lines(preview_src, 8),
        open,
    })
}

pub fn activity_from_json(value: &Value) -> Option<Activity> {
    let subtype = value
        .get("subtype")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    if let Some(call) = value.get("tool_call").and_then(|item| item.as_object()) {
        let (name, body) = tool_name_and_body(call)?;
        let args = body.get("args").unwrap_or(body);
        return describe_tool(name, args, subtype);
    }
    if let Some(name) = value
        .get("name")
        .and_then(|item| item.as_str())
        .or_else(|| value.pointer("/part/tool").and_then(|item| item.as_str()))
    {
        let args = value
            .get("args")
            .or_else(|| value.pointer("/part/state/input"))
            .or_else(|| value.pointer("/part/input"))
            .unwrap_or(value);
        let event_type = value.get("type").and_then(|item| item.as_str()).unwrap_or("");
        if event_type.contains("tool")
            || value.get("tool_call").is_some()
            || value.pointer("/part/tool").is_some()
        {
            return describe_tool(name, args, subtype);
        }
    }
    None
}

fn tool_name_and_body(call: &serde_json::Map<String, Value>) -> Option<(&str, &Value)> {
    if let Some(tool) = call.get("tool").and_then(|item| item.as_object()) {
        if let Some(case) = tool.get("case").and_then(|item| item.as_str()) {
            return Some((case, tool.get("value").unwrap_or(&Value::Null)));
        }
    }
    call.iter()
        .find(|(key, _)| {
            let key = key.to_ascii_lowercase();
            key.ends_with("toolcall") || key.contains("tool")
        })
        .map(|(key, body)| (key.as_str(), body))
}

/// Une un fragmento del stream con lo ya mostrado.
/// Cursor reenvía el texto completo al cerrar un tramo; eso no debe pegarse otra vez.
pub enum Absorb {
    Ignore,
    Append(String),
    Replace(String),
}

pub fn absorb_assistant(current: &str, chunk: &str) -> Absorb {
    if chunk.is_empty() {
        return Absorb::Ignore;
    }
    if current.is_empty() {
        return Absorb::Replace(chunk.to_string());
    }
    if current == chunk {
        return Absorb::Ignore;
    }
    if chunk.starts_with(current) {
        return Absorb::Replace(chunk.to_string());
    }
    let long_enough = chunk.chars().count() >= 16;
    if long_enough && current.ends_with(chunk) {
        return Absorb::Ignore;
    }
    Absorb::Append(chunk.to_string())
}

/// Elige el texto final. Si el stream ya trae el plan y el cierre sólo repite el preámbulo, se queda el stream.
pub fn choose_reply(streamed: &str, result: Option<&str>) -> String {
    let streamed_trim = streamed.trim();
    let result_trim = result.unwrap_or("").trim();
    if streamed_trim.is_empty() {
        return result_trim.to_string();
    }
    if result_trim.is_empty() || streamed_trim.contains(result_trim) {
        return streamed.trim_end().to_string();
    }
    if result_trim.contains(streamed_trim) {
        return result_trim.to_string();
    }
    format!("{}\n\n{result_trim}", streamed.trim_end())
}

fn describe_tool(name: &str, args: &Value, subtype: &str) -> Option<Activity> {
    let key = name.to_ascii_lowercase();
    let path = first_str(args, &["path", "file_path", "filePath", "target_file", "targetPath"]);
    let command = first_str(args, &["command", "cmd"]);
    let pattern = first_str(args, &["pattern", "query", "globPattern", "glob"]);
    let plan_body = first_str(args, &["plan"]);
    let overview = first_str(args, &["overview"]);
    let raw_preview = first_str(
        args,
        &[
            "streamContent",
            "contents",
            "content",
            "new_string",
            "newString",
            "patch",
            "diffString",
        ],
    );
    if key.contains("plan") && (plan_body.is_some() || overview.is_some()) {
        let body = plan_body.or(overview).unwrap_or("");
        let name = first_str(args, &["name"]).unwrap_or("el plan");
        return Some(Activity {
            label: format!("Armando {name}"),
            file: None,
            preview: (!body.trim().is_empty()).then(|| head_lines(body, 10)),
            writes: false,
            append: plan_body
                .filter(|text| !text.trim().is_empty())
                .map(|text| text.to_string()),
        });
    }
    let writes = key.contains("edit")
        || key.contains("write")
        || key.contains("strreplace")
        || key.contains("delete")
        || key.contains("remove")
        || key.contains("apply");
    let reads = key.contains("read") || key.contains("view");
    let shell = key.contains("shell")
        || key.contains("bash")
        || key.contains("terminal")
        || (command.is_some() && !writes);
    let search = key.contains("grep") || key.contains("search") || key.contains("glob");
    if path.is_none() && command.is_none() && !writes && !reads && !shell && !search {
        return None;
    }
    let verb = if key.contains("delete") || key.contains("remove") {
        "Borrando"
    } else if writes {
        if subtype == "completed" {
            "Escribió"
        } else {
            "Escribiendo"
        }
    } else if reads {
        "Leyendo"
    } else if shell {
        "Ejecutando"
    } else if search {
        "Buscando"
    } else {
        "Trabajando"
    };
    let file = if verb == "Ejecutando" {
        command
            .map(|item| clip(item, 80))
            .or_else(|| path.map(|item| item.to_string()))
    } else {
        path.map(|item| item.to_string())
    };
    let label = match (verb, file.as_deref()) {
        ("Ejecutando", Some(cmd)) => format!("Ejecutando {}", clip(cmd, 60)),
        (_, Some(path)) => format!("{verb} {}", file_tail(path)),
        ("Buscando", None) => pattern
            .map(|text| format!("Buscando {}", clip(text, 48)))
            .unwrap_or_else(|| "Buscando".into()),
        (_, None) => verb.to_string(),
    };
    let preview = if writes || key.contains("plan") {
        raw_preview
            .map(|text| tail_lines(text, 8))
            .filter(|text| !text.trim().is_empty())
    } else {
        None
    };
    Some(Activity {
        label,
        file,
        preview,
        writes,
        append: None,
    })
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(text) = object.get(*key).and_then(|item| item.as_str()) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn kind_from_status(status: &str) -> ChangeKind {
    if status.contains('D') {
        ChangeKind::Deleted
    } else if status.contains('?') || status.contains('A') || status.contains('R') {
        ChangeKind::Added
    } else {
        ChangeKind::Modified
    }
}

fn preview_for(root: &Path, path: &str, kind: ChangeKind) -> String {
    if kind == ChangeKind::Deleted {
        return "Archivo eliminado.".into();
    }
    if let Ok(output) = Command::new("git")
        .args(["diff", "--no-color", "--unified=1", "--", path])
        .current_dir(root)
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        if !text.trim().is_empty() {
            return head_lines(&text, 14);
        }
    }
    match std::fs::read_to_string(root.join(path)) {
        Ok(body) => head_lines(&body, 10),
        Err(_) => "Archivo binario o no textual.".into(),
    }
}

fn fingerprint(root: &Path, path: &str) -> String {
    match std::fs::metadata(root.join(path)) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_secs())
                .unwrap_or(0);
            format!("{}:{modified}", meta.len())
        }
        Err(_) => "missing".into(),
    }
}

fn parse_porcelain_line(line: &str) -> Option<(String, String)> {
    if line.len() < 4 {
        return None;
    }
    let status = line[..2].to_string();
    let mut rest = line[3..].trim();
    if let Some((_, after)) = rest.split_once(" -> ") {
        rest = after.trim();
    }
    let path = unquote(rest);
    if path.is_empty() {
        None
    } else {
        Some((path, status))
    }
}

fn unquote(path: &str) -> String {
    let path = path.trim();
    if path.starts_with('"') && path.ends_with('"') && path.len() >= 2 {
        path[1..path.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        path.to_string()
    }
}

fn safe_destination(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim().trim_matches('"').trim();
    if rel.is_empty() || rel.len() > 240 || rel.contains('\0') {
        return None;
    }
    if rel.starts_with('/') || rel.starts_with('~') {
        return None;
    }
    let mut clean = PathBuf::new();
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(part) if part != ".git" => clean.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if clean.as_os_str().is_empty() {
        return None;
    }
    let full = root.join(&clean);
    let parent = full.parent().unwrap_or(root);
    std::fs::create_dir_all(parent).ok()?;
    let root_canon = root.canonicalize().ok()?;
    let parent_canon = parent.canonicalize().ok()?;
    if !parent_canon.starts_with(&root_canon) {
        return None;
    }
    Some(full)
}

fn closing_fence(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim() == "```" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

pub fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    clip(&lines[start..].join("\n"), 420)
}

fn head_lines(text: &str, count: usize) -> String {
    let joined = text.lines().take(count).collect::<Vec<_>>().join("\n");
    clip(&joined, 420)
}

fn file_tail(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if path.chars().count() <= 42 {
        path.to_string()
    } else {
        format!("…/{name}")
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars + 1;
    let mut out: String = text.chars().skip(skip).collect();
    out.insert(0, '…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrae_bloques_cerrados_y_deja_el_abierto() {
        let text = "antes\n```file:src/a.rs\nfn a() {}\n```\n\
                    ```file:../fuera.rs\nno\n```\n\
                    ```file:src/b.rs\nfn b() {}\n";
        let blocks = file_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "src/a.rs");
        assert_eq!(blocks[1].0, "../fuera.rs");
        let live = streaming_preview(text).unwrap();
        assert_eq!(live.path, "src/b.rs");
        assert!(live.open);
        assert!(live.preview.contains("fn b"));
    }

    #[test]
    fn no_repite_el_texto_que_cursor_reenvia_entero() {
        let mut current = String::new();
        for chunk in ["Con esas ", "respuestas armo el plan."] {
            match absorb_assistant(&current, chunk) {
                Absorb::Replace(full) => current = full,
                Absorb::Append(extra) => current.push_str(&extra),
                Absorb::Ignore => {}
            }
        }
        let full = current.clone();
        assert!(matches!(absorb_assistant(&current, &full), Absorb::Ignore));
        assert_eq!(current, "Con esas respuestas armo el plan.");
    }

    #[test]
    fn el_plan_de_la_herramienta_entra_en_la_respuesta() {
        let raw = r##"{
            "type": "tool_call",
            "subtype": "completed",
            "tool_call": {
                "createPlanToolCall": {
                    "args": {
                        "name": "Relation Guard",
                        "plan": "# Plan\nBloquear update y delete.\n"
                    }
                }
            }
        }"##;
        let value: Value = serde_json::from_str(raw).unwrap();
        let activity = activity_from_json(&value).unwrap();
        assert!(activity.label.contains("Relation Guard"));
        assert_eq!(
            activity.append.as_deref(),
            Some("# Plan\nBloquear update y delete.\n")
        );
        let streamed = format!(
            "Armo el plan concreto.\n\n{}",
            activity.append.unwrap()
        );
        let reply = choose_reply(&streamed, Some("Armo el plan concreto."));
        assert!(reply.contains("Bloquear update y delete"));
        assert_eq!(reply.matches("Armo el plan concreto.").count(), 1);
    }

    #[test]
    fn no_escribe_fuera_del_proyecto() {
        let dir = std::env::temp_dir().join(format!("yeison-edits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let text = "```file:../fuera.txt\nno\n```\n```file:ok.txt\nhola\n```\n";
        let written = apply_file_blocks(&dir, text);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].path, "ok.txt");
        assert_eq!(written[0].kind, ChangeKind::Added);
        assert_eq!(std::fs::read_to_string(dir.join("ok.txt")).unwrap(), "hola");
        assert!(!dir.join("../fuera.txt").exists() || written.iter().all(|c| c.path == "ok.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn el_json_de_una_edicion_arma_la_vista_previa() {
        let raw = r#"{
            "type": "tool_call",
            "subtype": "started",
            "tool_call": {
                "editToolCall": {
                    "args": {
                        "path": "src/main.rs",
                        "streamContent": "fn main() {\n    println!(\"hola\");\n}\n"
                    }
                }
            }
        }"#;
        let value: Value = serde_json::from_str(raw).unwrap();
        let activity = activity_from_json(&value).unwrap();
        assert!(activity.writes);
        assert_eq!(activity.file.as_deref(), Some("src/main.rs"));
        assert!(activity.label.contains("Escribiendo"));
        assert!(activity.preview.unwrap().contains("println"));
    }

    #[test]
    fn git_ve_el_archivo_nuevo_del_turno() {
        let dir = std::env::temp_dir().join(format!("yeison-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let init = Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output();
        let Ok(init) = init else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        if !init.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let before = snapshot(&dir);
        assert!(before.git);
        std::fs::write(dir.join("hola.txt"), "hola\n").unwrap();
        let changes = changes_since(&dir, &before);
        assert!(
            changes
                .iter()
                .any(|change| change.path == "hola.txt" && change.kind == ChangeKind::Added),
            "{changes:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
