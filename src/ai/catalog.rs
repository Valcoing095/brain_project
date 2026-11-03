use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::process::{
    run_command_timeout, CancelFlag, PROBE_TIMEOUT, QUICK_PROBE_TIMEOUT,
};

pub const CATALOG_VERSION: u32 = 3;

#[derive(Clone, Debug)]
pub enum ScanEvent {
    /// Texto corto para mostrar qué se está revisando ahora.
    Progress(String),
    Provider(ProviderEntry),
    Finished { cancelled: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    CursorAgent,
    Opencode,
    ClaudeCode,
    GeminiCli,
    Aider,
    Ollama,
    LlmCli,
    CodexCli,
    GenericPrint,
}

impl ProviderKind {
    pub fn supports_cursor_modes(self) -> bool {
        matches!(self, Self::CursorAgent)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyState {
    /// Autenticado y con modelos: se puede usar ya.
    Ready,
    /// Instalado pero falta login / API key.
    NeedsAuth,
    /// Instalado pero no pudimos verificarlo (puede funcionar).
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub id: String,
    pub kind: ProviderKind,
    pub display_name: String,
    pub binary_path: String,
    pub installed: bool,
    pub ready: ReadyState,
    pub status_note: String,
    pub models: Vec<ModelEntry>,
}

impl ProviderEntry {
    pub fn default_model(&self) -> Option<&ModelEntry> {
        let preferred: &[&str] = match self.kind {
            ProviderKind::CursorAgent => &["composer-2.5", "auto"],
            ProviderKind::ClaudeCode => &["sonnet"],
            ProviderKind::Opencode => &["opencode/glm-5.3-flash-free", "opencode/big-pickle"],
            _ => &[],
        };
        preferred
            .iter()
            .find_map(|id| self.models.iter().find(|m| m.id == *id))
            .or_else(|| self.models.first())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiCatalog {
    pub version: u32,
    pub scanned_at_unix: u64,
    pub providers: Vec<ProviderEntry>,
}

impl AiCatalog {
    pub fn empty() -> Self {
        Self {
            version: CATALOG_VERSION,
            scanned_at_unix: 0,
            providers: Vec::new(),
        }
    }

    pub fn installed_providers(&self) -> impl Iterator<Item = &ProviderEntry> {
        self.providers.iter().filter(|p| p.installed)
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderEntry> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn upsert_provider(&mut self, entry: ProviderEntry) {
        if let Some(existing) = self.providers.iter_mut().find(|p| p.id == entry.id) {
            *existing = entry;
        } else {
            self.providers.push(entry);
        }
        self.sort();
    }

    pub fn finalize_scan(&mut self) {
        self.scanned_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.sort();
    }

    fn sort(&mut self) {
        // Listos primero, luego por nombre.
        self.providers.sort_by(|a, b| {
            let rank = |p: &ProviderEntry| match p.ready {
                ReadyState::Ready => 0,
                ReadyState::Unknown => 1,
                ReadyState::NeedsAuth => 2,
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.display_name.cmp(&b.display_name))
        });
    }
}

struct CliDefinition {
    id: &'static str,
    display_name: &'static str,
    binaries: &'static [&'static str],
    kind: ProviderKind,
}

/// Herramientas conocidas con lógica propia de modelos/sesión.
const CLI_REGISTRY: &[CliDefinition] = &[
    CliDefinition {
        id: "cursor",
        display_name: "Cursor",
        binaries: &["cursor-agent", "cursor"],
        kind: ProviderKind::CursorAgent,
    },
    CliDefinition {
        id: "opencode",
        display_name: "OpenCode",
        binaries: &["opencode"],
        kind: ProviderKind::Opencode,
    },
    CliDefinition {
        id: "claude",
        display_name: "Claude Code",
        binaries: &["claude"],
        kind: ProviderKind::ClaudeCode,
    },
    CliDefinition {
        id: "gemini",
        display_name: "Gemini CLI",
        binaries: &["gemini"],
        kind: ProviderKind::GeminiCli,
    },
    CliDefinition {
        id: "codex",
        display_name: "Codex CLI",
        binaries: &["codex"],
        kind: ProviderKind::CodexCli,
    },
    CliDefinition {
        id: "aider",
        display_name: "Aider",
        binaries: &["aider"],
        kind: ProviderKind::Aider,
    },
    CliDefinition {
        id: "ollama",
        display_name: "Ollama",
        binaries: &["ollama"],
        kind: ProviderKind::Ollama,
    },
    CliDefinition {
        id: "llm",
        display_name: "llm",
        binaries: &["llm"],
        kind: ProviderKind::LlmCli,
    },
];

/// Otros CLIs de IA conocidos por nombre exacto (modo genérico).
const KNOWN_GENERIC: &[(&str, &str)] = &[
    ("amp", "Amp"),
    ("crush", "Crush"),
    ("copilot", "GitHub Copilot CLI"),
    ("github-copilot-cli", "GitHub Copilot CLI"),
    ("sgpt", "ShellGPT"),
    ("gptme", "gptme"),
    ("mods", "Mods"),
    ("plandex", "Plandex"),
    ("goose", "Goose"),
    ("mentat", "Mentat"),
    ("fabric", "Fabric"),
    ("cody", "Sourcegraph Cody"),
    ("gpt", "gpt"),
    ("chatgpt", "ChatGPT CLI"),
    ("kiro", "Kiro"),
    ("q", "Amazon Q"),
    ("gentle-ai", "Gentle AI"),
];

/// Prefijos de nombre que justifican sondear un binario desconocido del PATH.
const AI_NAME_PREFIXES: &[&str] = &[
    "gpt", "llm", "claude", "gemini", "codex", "copilot", "ollama", "aider", "opencode",
    "chatgpt", "openai", "anthropic", "mistral", "groq", "deepseek",
];

/// Carpetas del sistema donde no tiene sentido buscar CLIs de IA.
const SKIP_DIRS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/System",
    "/Library/Apple",
    "/usr/libexec",
];

pub fn catalog_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hola_mundo")
        .join("ai_catalog.json")
}

pub fn load_catalog_cache() -> Option<AiCatalog> {
    let path = catalog_cache_path();
    let raw = std::fs::read_to_string(path).ok()?;
    let catalog: AiCatalog = serde_json::from_str(&raw).ok()?;
    if catalog.version != CATALOG_VERSION {
        return None;
    }
    Some(catalog)
}

pub fn save_catalog_cache(catalog: &AiCatalog) -> Result<(), String> {
    let path = catalog_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("No se pudo crear carpeta de caché: {e}"))?;
    }
    let json = serde_json::to_string_pretty(catalog)
        .map_err(|e| format!("No se pudo serializar catálogo: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("No se pudo guardar catálogo: {e}"))
}

fn cancelled(cancel: &CancelFlag) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// Escaneo completo en segundo plano. Emite proveedores conforme se confirman.
/// Si `cancel` se activa, termina lo antes posible y emite `Finished { cancelled: true }`.
pub fn scan_installed_providers_background(tx: &Sender<ScanEvent>, cancel: CancelFlag) {
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_bins: HashSet<String> = HashSet::new();

    // 1) Proveedores conocidos, en paralelo: los rápidos aparecen sin esperar a los lentos.
    let _ = tx.send(ScanEvent::Progress("Revisando asistentes conocidos…".into()));
    let (ptx, prx) = std::sync::mpsc::channel::<Option<ProviderEntry>>();
    let mut pending = 0usize;
    for def in CLI_REGISTRY {
        if cancelled(&cancel) {
            break;
        }
        let Some(bin) = find_executable(def.binaries) else {
            continue;
        };
        pending += 1;
        let ptx = ptx.clone();
        let cancel = cancel.clone();
        let (id, name, kind) = (def.id, def.display_name, def.kind);
        std::thread::spawn(move || {
            let entry = if cancelled(&cancel) {
                None
            } else {
                scan_by_kind(id, name, kind, bin)
            };
            let _ = ptx.send(entry);
        });
    }
    drop(ptx);

    let mut received = 0usize;
    while received < pending {
        if cancelled(&cancel) {
            break;
        }
        match prx.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(entry)) => {
                received += 1;
                let _ = tx.send(ScanEvent::Progress(format!(
                    "Encontrado {}",
                    entry.display_name
                )));
                emit_provider(tx, &mut seen_ids, &mut seen_bins, entry);
            }
            Ok(None) => received += 1,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if cancelled(&cancel) {
        let _ = tx.send(ScanEvent::Finished { cancelled: true });
        return;
    }

    // 2) Otros CLIs de IA conocidos por nombre exacto.
    let _ = tx.send(ScanEvent::Progress("Buscando otros CLIs de IA…".into()));
    for (name, label) in KNOWN_GENERIC {
        if cancelled(&cancel) {
            break;
        }
        if seen_ids.contains(*name) {
            continue;
        }
        let Some(bin) = find_executable(&[name]) else {
            continue;
        };
        if seen_bins.contains(bin.to_string_lossy().as_ref()) {
            continue;
        }
        if let Some(entry) = scan_by_kind(name, label, ProviderKind::GenericPrint, bin) {
            emit_provider(tx, &mut seen_ids, &mut seen_bins, entry);
        }
    }

    if cancelled(&cancel) {
        let _ = tx.send(ScanEvent::Finished { cancelled: true });
        return;
    }

    // 3) Rastreo del PATH con nombres que empiezan por un prefijo de IA.
    let _ = tx.send(ScanEvent::Progress("Explorando el PATH…".into()));
    discover_ai_like_on_path(tx, &cancel, &mut seen_ids, &mut seen_bins);

    let _ = tx.send(ScanEvent::Finished {
        cancelled: cancelled(&cancel),
    });
}

fn emit_provider(
    tx: &Sender<ScanEvent>,
    seen_ids: &mut HashSet<String>,
    seen_bins: &mut HashSet<String>,
    entry: ProviderEntry,
) {
    if seen_ids.contains(&entry.id) || seen_bins.contains(&entry.binary_path) {
        return;
    }
    seen_ids.insert(entry.id.clone());
    seen_bins.insert(entry.binary_path.clone());
    let _ = tx.send(ScanEvent::Provider(entry));
}

fn discover_ai_like_on_path(
    tx: &Sender<ScanEvent>,
    cancel: &CancelFlag,
    seen_ids: &mut HashSet<String>,
    seen_bins: &mut HashSet<String>,
) {
    let Some(path_var) = std::env::var_os("PATH") else {
        return;
    };

    for dir in std::env::split_paths(&path_var) {
        if cancelled(cancel) {
            return;
        }
        let dir_str = dir.to_string_lossy();
        if SKIP_DIRS.iter().any(|skip| dir_str.starts_with(skip)) {
            continue;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };

        for entry in read.flatten() {
            if cancelled(cancel) {
                return;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(base) = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if !basename_looks_like_ai(&base) {
                continue;
            }
            let id = sanitize_id(&base);
            if seen_ids.contains(&id) || seen_bins.contains(path.to_string_lossy().as_ref()) {
                continue;
            }
            let _ = tx.send(ScanEvent::Progress(format!("Probando {base}…")));
            if !help_smells_like_ai_cli(&path) {
                continue;
            }
            if let Some(entry) = scan_by_kind(&id, &base, ProviderKind::GenericPrint, path) {
                emit_provider(tx, seen_ids, seen_bins, entry);
            }
        }
    }
}

fn find_executable(candidates: &[&str]) -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let extra_dirs = [
        format!("{home}/.local/bin"),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        format!("{home}/.cargo/bin"),
        format!("{home}/bin"),
    ];
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = extra_dirs
        .iter()
        .map(PathBuf::from)
        .chain(std::env::split_paths(&path_var))
        .collect();

    for name in candidates {
        for dir in &dirs {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn probe(bin: &Path, args: &[&str]) -> Result<String, String> {
    let cwd = std::env::temp_dir();
    run_command_timeout(bin, args, &cwd, PROBE_TIMEOUT)
}

fn quick_probe(bin: &Path, args: &[&str]) -> Result<String, String> {
    let cwd = std::env::temp_dir();
    run_command_timeout(bin, args, &cwd, QUICK_PROBE_TIMEOUT)
}

fn scan_by_kind(
    id: &str,
    display_name: &str,
    kind: ProviderKind,
    bin: PathBuf,
) -> Option<ProviderEntry> {
    let (ready, status_note, models) = match kind {
        ProviderKind::CursorAgent => scan_cursor_details(&bin),
        ProviderKind::Opencode => scan_opencode_details(&bin),
        ProviderKind::ClaudeCode => scan_claude_details(&bin),
        ProviderKind::GeminiCli => scan_gemini_details(&bin),
        ProviderKind::Aider => scan_aider_details(&bin),
        ProviderKind::Ollama => scan_ollama_details(&bin),
        ProviderKind::LlmCli => scan_llm_details(&bin),
        ProviderKind::CodexCli => scan_generic_details(&bin, "Codex"),
        ProviderKind::GenericPrint => scan_generic_details(&bin, display_name),
    };

    Some(ProviderEntry {
        id: id.to_string(),
        kind,
        display_name: display_name.to_string(),
        binary_path: bin.to_string_lossy().into(),
        installed: true,
        ready,
        status_note,
        models,
    })
}

fn scan_cursor_details(bin: &Path) -> (ReadyState, String, Vec<ModelEntry>) {
    // `status` y `models` en paralelo: juntos tardan ~9 s en serie.
    let bin_a = bin.to_path_buf();
    let status_handle = std::thread::spawn(move || {
        cursor_cmd(&bin_a, &["status"]).unwrap_or_default()
    });
    let models = match cursor_cmd(bin, &["models"]) {
        Ok(raw) => parse_cursor_models(&raw),
        Err(_) => Vec::new(),
    };
    let status = status_handle.join().unwrap_or_default();
    let logged_in = status.contains("Logged in") || status.contains('✓');

    let models = if models.is_empty() {
        cursor_fallback_models()
    } else {
        models
    };

    if logged_in {
        (ReadyState::Ready, "Sesión iniciada".into(), models)
    } else {
        (
            ReadyState::NeedsAuth,
            "Ejecuta `cursor agent login` en la terminal".into(),
            models,
        )
    }
}

/// Ejecuta un subcomando de Cursor Agent tanto si el binario es `cursor` como `cursor-agent`.
fn cursor_cmd(bin: &Path, sub: &[&str]) -> Result<String, String> {
    let is_wrapper = bin
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s == "cursor")
        .unwrap_or(false);
    let mut args: Vec<&str> = Vec::new();
    if is_wrapper {
        args.push("agent");
    }
    args.extend_from_slice(sub);
    probe(bin, &args)
}

fn parse_cursor_models(raw: &str) -> Vec<ModelEntry> {
    let mut models = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Available") {
            continue;
        }
        let Some((id, label)) = line.split_once(" - ") else {
            continue;
        };
        models.push(ModelEntry {
            id: id.trim().to_string(),
            label: label.trim().to_string(),
        });
    }
    models
}

fn cursor_fallback_models() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            id: "composer-2.5".into(),
            label: "Composer 2.5".into(),
        },
        ModelEntry {
            id: "auto".into(),
            label: "Auto".into(),
        },
    ]
}

fn scan_opencode_details(bin: &Path) -> (ReadyState, String, Vec<ModelEntry>) {
    match probe(bin, &["models"]) {
        Ok(raw) => {
            let models = parse_opencode_models(&raw);
            if models.is_empty() {
                (
                    ReadyState::NeedsAuth,
                    "Sin modelos: configura un proveedor con `opencode auth`".into(),
                    models,
                )
            } else {
                (ReadyState::Ready, "Listo".into(), models)
            }
        }
        Err(error) => (ReadyState::Unknown, error, Vec::new()),
    }
}

fn parse_opencode_models(raw: &str) -> Vec<ModelEntry> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && line.contains('/'))
        .map(|id| {
            let (provider, name) = id.split_once('/').unwrap_or(("", id));
            ModelEntry {
                id: id.to_string(),
                label: format!("{} · {}", name.replace('-', " "), provider),
            }
        })
        .collect()
}

fn scan_claude_details(bin: &Path) -> (ReadyState, String, Vec<ModelEntry>) {
    let models = vec![
        ModelEntry {
            id: "sonnet".into(),
            label: "Claude Sonnet".into(),
        },
        ModelEntry {
            id: "opus".into(),
            label: "Claude Opus".into(),
        },
        ModelEntry {
            id: "haiku".into(),
            label: "Claude Haiku".into(),
        },
    ];

    if quick_probe(bin, &["--version"]).is_err() {
        return (ReadyState::Unknown, "No se pudo verificar".into(), models);
    }

    let has_key = std::env::var("ANTHROPIC_API_KEY").is_ok();
    let home = std::env::var("HOME").unwrap_or_default();
    let has_creds = Path::new(&format!("{home}/.claude/.credentials.json")).exists();
    let has_oauth = std::fs::read_to_string(format!("{home}/.claude.json"))
        .map(|s| s.contains("oauthAccount") || s.contains("primaryApiKey"))
        .unwrap_or(false);

    if has_key || has_creds || has_oauth {
        (ReadyState::Ready, "Sesión iniciada".into(), models)
    } else {
        (
            ReadyState::NeedsAuth,
            "Ejecuta `claude` y usa /login".into(),
            models,
        )
    }
}

fn scan_gemini_details(bin: &Path) -> (ReadyState, String, Vec<ModelEntry>) {
    let models = vec![
        ModelEntry {
            id: "default".into(),
            label: "Modelo por defecto".into(),
        },
        ModelEntry {
            id: "gemini-2.5-pro".into(),
            label: "Gemini 2.5 Pro".into(),
        },
        ModelEntry {
            id: "gemini-2.5-flash".into(),
            label: "Gemini 2.5 Flash".into(),
        },
    ];

    if quick_probe(bin, &["--version"]).is_err() {
        return (ReadyState::Unknown, "No se pudo verificar".into(), models);
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let has_env = std::env::var("GEMINI_API_KEY").is_ok()
        || std::env::var("GOOGLE_API_KEY").is_ok()
        || std::env::var("GOOGLE_GENAI_USE_VERTEXAI").is_ok();
    let has_oauth = Path::new(&format!("{home}/.gemini/oauth_creds.json")).exists();
    let has_settings_auth = std::fs::read_to_string(format!("{home}/.gemini/settings.json"))
        .map(|s| s.contains("selectedAuthType") || s.contains("selectedType"))
        .unwrap_or(false);

    if has_env || has_oauth || has_settings_auth {
        (ReadyState::Ready, "Listo".into(), models)
    } else {
        (
            ReadyState::NeedsAuth,
            "Define GEMINI_API_KEY o inicia sesión con `gemini`".into(),
            models,
        )
    }
}

fn scan_aider_details(bin: &Path) -> (ReadyState, String, Vec<ModelEntry>) {
    let _ = quick_probe(bin, &["--version"]);
    (
        ReadyState::Unknown,
        "Usa el modelo configurado en tu entorno".into(),
        vec![ModelEntry {
            id: "default".into(),
            label: "Configuración local".into(),
        }],
    )
}

fn scan_ollama_details(bin: &Path) -> (ReadyState, String, Vec<ModelEntry>) {
    match probe(bin, &["list"]) {
        Ok(raw) => {
            let models = parse_ollama_models(&raw);
            if models.is_empty() {
                (
                    ReadyState::NeedsAuth,
                    "Sin modelos descargados (`ollama pull <modelo>`)".into(),
                    models,
                )
            } else {
                (ReadyState::Ready, "Modelos locales".into(), models)
            }
        }
        Err(error) => (ReadyState::Unknown, error, Vec::new()),
    }
}

fn parse_ollama_models(raw: &str) -> Vec<ModelEntry> {
    raw.lines()
        .skip(1)
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            if name.is_empty() || name == "NAME" {
                return None;
            }
            Some(ModelEntry {
                id: name.to_string(),
                label: name.to_string(),
            })
        })
        .collect()
}

fn scan_llm_details(bin: &Path) -> (ReadyState, String, Vec<ModelEntry>) {
    match probe(bin, &["models", "list"]) {
        Ok(raw) => {
            let models = raw
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|line| {
                    let id = line
                        .split(':')
                        .nth(1)
                        .map(str::trim)
                        .and_then(|rest| rest.split_whitespace().next())
                        .unwrap_or(line);
                    ModelEntry {
                        id: id.to_string(),
                        label: line.to_string(),
                    }
                })
                .collect::<Vec<_>>();
            if models.is_empty() {
                (ReadyState::Unknown, "Sin modelos listados".into(), default_model())
            } else {
                (ReadyState::Ready, "Listo".into(), models)
            }
        }
        Err(_) => (ReadyState::Unknown, "CLI llm".into(), default_model()),
    }
}

fn scan_generic_details(bin: &Path, name: &str) -> (ReadyState, String, Vec<ModelEntry>) {
    let help = quick_probe(bin, &["--help"]).unwrap_or_default().to_lowercase();
    let looks_ai = help.contains("model") || help.contains("prompt") || help.contains("agent");
    (
        if looks_ai {
            ReadyState::Unknown
        } else {
            ReadyState::NeedsAuth
        },
        format!("{name} · modo genérico"),
        default_model(),
    )
}

fn default_model() -> Vec<ModelEntry> {
    vec![ModelEntry {
        id: "default".into(),
        label: "Predeterminado".into(),
    }]
}

fn basename_looks_like_ai(name: &str) -> bool {
    let lower = name.to_lowercase();
    if lower.ends_with(".js") || lower.ends_with(".py") || lower.contains("askpass") {
        return false;
    }
    AI_NAME_PREFIXES.iter().any(|p| lower.starts_with(p))
}

fn help_smells_like_ai_cli(bin: &Path) -> bool {
    let help = quick_probe(bin, &["--help"])
        .or_else(|_| quick_probe(bin, &["-h"]))
        .unwrap_or_default()
        .to_lowercase();
    (help.contains("model") || help.contains("prompt"))
        && (help.contains("agent") || help.contains("chat") || help.contains("print"))
}

fn sanitize_id(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
