use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

static BRAIN_LOCK: Mutex<()> = Mutex::new(());

use serde::{Deserialize, Serialize};

use crate::ai::CancelFlag;
use crate::project;

pub const BRAIN_INDEX: &str = "BRAIN_INDEX.md";
pub const BRAIN_STATE: &str = ".brain_state.json";

const BRAIN_VERSION: u32 = 2;
const MAX_FILES: usize = 25_000;
const MAX_DEPTH: usize = 12;
const MAX_READ_BYTES: u64 = 160 * 1024;
const MAX_ANALYZE_BYTES: u64 = 192 * 1024;
const MAX_FILE_INDEX: usize = 2_000;
const MAX_CONTEXT_BYTES: usize = 24 * 1024;
const MAX_EXTRA_CONTENT: usize = 128 * 1024;
const MAX_SYMBOLS_PER_FILE: usize = 120;
const MAX_IMPORTS_PER_FILE: usize = 80;
const MAX_GIT_ENTRIES: usize = 2_000;

/// Progreso fino del Brain mientras arma contexto (archivo + bytes del pack).
#[derive(Clone, Debug)]
pub enum ProgressTick {
    Status(String),
    File { path: String, detail: String },
    Packed { bytes: usize },
}

/// Estimación grosera de tokens (~4 caracteres por token).
pub fn estimate_tokens(text: &str) -> u32 {
    let chars = text.chars().count();
    ((chars + 3) / 4) as u32
}

pub fn estimate_tokens_bytes(bytes: usize) -> u32 {
    ((bytes + 3) / 4) as u32
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "bin",
    "obj",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".idea",
    ".vscode",
    ".gradle",
    ".terraform",
    "Pods",
    "DerivedData",
    "coverage",
    ".cache",
    ".turbo",
    ".parcel-cache",
    ".DS_Store",
    "brain",
];

const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "tsconfig.json",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "Pipfile",
    "go.mod",
    "go.sum",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "Gemfile",
    "composer.json",
    "pubspec.yaml",
    "CMakeLists.txt",
    "Dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
];

const ENTRYPOINT_NAMES: &[&str] = &[
    "main.rs",
    "lib.rs",
    "main.py",
    "app.py",
    "manage.py",
    "main.go",
    "main.c",
    "main.cpp",
    "main.ts",
    "main.js",
    "index.ts",
    "index.js",
    "index.tsx",
    "index.jsx",
    "app.ts",
    "app.tsx",
    "server.ts",
    "server.js",
    "Program.cs",
    "main.dart",
];

const README_NAMES: &[&str] = &[
    "README.md",
    "README.MD",
    "readme.md",
    "README.txt",
    "README.rst",
    "README",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    Full,
    Incremental,
    /// El código indexado sigue vigente; sólo cambió la rama, el HEAD o el working tree.
    GitOnly,
    NoChanges,
}

impl SyncMode {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            SyncMode::Full => "completo",
            SyncMode::Incremental => "incremental",
            SyncMode::GitOnly => "git",
            SyncMode::NoChanges => "sin cambios",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitEntry {
    pub status: String,
    pub path: String,
    pub untracked: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitState {
    pub available: bool,
    pub branch: String,
    pub head: String,
    pub head_short: String,
    pub dirty: bool,
    pub entries: Vec<GitEntry>,
    pub truncated: bool,
    pub captured_at_unix: u64,
    pub error: Option<String>,
}

impl GitState {
    pub fn untracked_paths(&self) -> BTreeSet<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.untracked)
            .map(|entry| entry.path.as_str())
            .collect()
    }

    pub fn dirty_label(&self) -> String {
        if !self.available {
            return "sin Git".into();
        }
        if self.dirty {
            if self.truncated {
                format!("{} cambios (truncado)", self.entries.len())
            } else {
                format!("{} cambios sin commitear", self.entries.len())
            }
        } else {
            "working tree limpio".into()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub kind: String,
    pub name: String,
    pub line: usize,
    /// Línea de la declaración, copiada del archivo.
    #[serde(default)]
    pub signature: String,
    /// Comentario de documentación inmediatamente anterior, copiado del archivo.
    #[serde(default)]
    pub doc: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub size: u64,
    pub mtime_unix: u64,
    pub text: bool,
    pub lang: String,
    pub untracked: bool,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<String>,
    /// Comentario de módulo copiado del archivo (`//!`, `///` inicial o equivalente).
    #[serde(default)]
    pub doc: String,
    /// Ya se extrajeron firmas y comentarios. Los índices viejos se completan al consultarlos.
    #[serde(default)]
    pub enriched: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct BrainState {
    version: u32,
    root: String,
    scanned_at_unix: u64,
    dirs: usize,
    total_bytes: u64,
    truncated: bool,
    git: GitState,
    files: BTreeMap<String, FileRecord>,
    /// Qué archivos respondieron cada término. Sirve para afinar la siguiente consulta.
    #[serde(default)]
    recall: Recall,
    /// Fichas de módulo ancladas al tamaño y la fecha de sus archivos.
    #[serde(default)]
    settled: BTreeMap<String, SettledCard>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Recall {
    consultations: u64,
    terms: BTreeMap<String, BTreeMap<String, u32>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SettledCard {
    fingerprint: String,
    lines: Vec<String>,
    hits: u32,
}

#[derive(Clone, Debug)]
pub struct BrainReport {
    pub mode: SyncMode,
    pub files: usize,
    pub dirs: usize,
    pub total_bytes: u64,
    pub symbols: usize,
    pub modules: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub stack: Vec<String>,
    pub scanned_at_unix: u64,
    pub git: GitState,
}

#[derive(Clone, Debug)]
pub struct BrainStatus {
    pub has_state: bool,
    pub scanned_at_unix: u64,
    pub files: usize,
    pub dirs: usize,
    pub symbols: usize,
    pub modules: usize,
    pub stack: Vec<String>,
    pub git: GitState,
    pub stale: bool,
    pub reason: String,
    pub consultations: u64,
    pub settled: usize,
}

#[derive(Clone, Debug)]
pub enum SyncOutcome {
    Unchanged(BrainStatus),
    Updated(BrainStatus, BrainReport),
}

#[derive(Clone, Debug)]
struct FileMeta {
    rel: String,
    size: u64,
    mtime_unix: u64,
    text: bool,
}

#[derive(Clone, Debug)]
struct ModuleRecord {
    path: String,
    files: usize,
    languages: Vec<String>,
    symbols: Vec<String>,
    depends_on: Vec<String>,
}

/// Comprueba rama, HEAD, working tree y la fecha del último índice, sin reindexar.
#[allow(dead_code)]
pub fn inspect(root: &Path, brain_dir: &Path) -> BrainStatus {
    inspect_unlocked(root, brain_dir)
}

fn inspect_unlocked(root: &Path, brain_dir: &Path) -> BrainStatus {
    let live = filter_git(root, brain_dir, inspect_git(root));
    let state = load_state(brain_dir);
    let mut status = BrainStatus {
        has_state: state.is_some(),
        scanned_at_unix: 0,
        files: 0,
        dirs: 0,
        symbols: 0,
        modules: 0,
        stack: Vec::new(),
        git: live.clone(),
        stale: true,
        reason: "sin índice inicial".into(),
        consultations: 0,
        settled: 0,
    };
    let Some(state) = state else {
        return status;
    };
    status.scanned_at_unix = state.scanned_at_unix;
    status.files = state.files.len();
    status.dirs = state.dirs;
    status.symbols = sum_symbols(&state.files);
    status.modules = unique_modules(&state.files);
    status.stack = detect_stack(root, &state.files);
    status.consultations = state.recall.consultations;
    status.settled = fresh_settled_count(&state);
    if state.version != BRAIN_VERSION {
        status.reason = "índice de una versión anterior; hace falta reconstruirlo".into();
        return status;
    }
    if state.root != path_string(root) {
        status.reason = "el índice pertenece a otra raíz".into();
        return status;
    }
    if !live.available {
        status.reason = "sin Git: se verifica por mtime al sincronizar".into();
        return status;
    }
    if !state.git.available {
        status.reason = "el índice se creó sin Git; se reconstruye".into();
        return status;
    }
    if live.truncated {
        status.reason = "demasiados cambios en Git; se reconstruye el índice".into();
        return status;
    }
    if live.head != state.git.head {
        status.reason = format!(
            "HEAD cambió ({} → {})",
            short_head(&state.git.head),
            short_head(&live.head)
        );
        return status;
    }
    if live.branch != state.git.branch {
        status.reason = format!(
            "la rama cambió (`{}` → `{}`)",
            state.git.branch, live.branch
        );
        return status;
    }
    let live_set: BTreeSet<(&str, &str)> = live
        .entries
        .iter()
        .map(|entry| (entry.status.as_str(), entry.path.as_str()))
        .collect();
    let scan_set: BTreeSet<(&str, &str)> = state
        .git
        .entries
        .iter()
        .map(|entry| (entry.status.as_str(), entry.path.as_str()))
        .collect();
    if live_set != scan_set {
        status.reason = "el working tree cambió desde el último índice".into();
        return status;
    }
    for entry in &live.entries {
        let Some(record) = state.files.get(&entry.path) else {
            status.reason = format!("hay un archivo nuevo sin indexar: `{}`", entry.path);
            return status;
        };
        match fs::metadata(root.join(&entry.path)) {
            Ok(meta) => {
                if meta.len() != record.size || mtime_unix(&meta) != record.mtime_unix {
                    status.reason = format!("cambió `{}` después del índice", entry.path);
                    return status;
                }
            }
            Err(_) => {
                status.reason = format!("ya no existe `{}`", entry.path);
                return status;
            }
        }
    }
    status.stale = false;
    status.reason = "índice al día".into();
    status
}

/// Sincronización incremental. Reutiliza el índice existente y sólo relee lo que cambió.
#[allow(dead_code)]
pub fn sync_project<F: FnMut(String)>(
    root: &Path,
    brain_dir: &Path,
    force_full: bool,
    cancel: &CancelFlag,
    progress: F,
) -> Result<BrainReport, String> {
    let _guard = BRAIN_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    sync_project_locked(root, brain_dir, force_full, cancel, progress)
}

/// Al abrir una sesión: mira la rama, el HEAD y la fecha del último índice.
/// Si no hay cambios posteriores, no reconstruye nada.
pub fn sync_session<F: FnMut(String)>(
    root: &Path,
    brain_dir: &Path,
    force_full: bool,
    cancel: &CancelFlag,
    progress: F,
) -> Result<SyncOutcome, String> {
    let _guard = BRAIN_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    sync_session_locked(root, brain_dir, force_full, cancel, progress)
}

fn sync_session_locked<F: FnMut(String)>(
    root: &Path,
    brain_dir: &Path,
    force_full: bool,
    cancel: &CancelFlag,
    mut progress: F,
) -> Result<SyncOutcome, String> {
    if !force_full {
        let status = inspect_unlocked(root, brain_dir);
        if status.has_state && !status.stale {
            return Ok(SyncOutcome::Unchanged(status));
        }
        progress(format!(
            "Índice desactualizado ({}). Actualizando sólo lo afectado…",
            status.reason
        ));
    } else {
        progress("Reconstruyendo el índice completo…".into());
    }
    let report = sync_project_locked(root, brain_dir, force_full, cancel, &mut progress)?;
    let mut status = inspect_unlocked(root, brain_dir);
    status.stale = false;
    if report.mode == SyncMode::NoChanges {
        status.reason = "índice al día".into();
        Ok(SyncOutcome::Unchanged(status))
    } else {
        status.reason = match report.mode {
            SyncMode::GitOnly => "git sincronizado; el código indexado sigue vigente".into(),
            SyncMode::Incremental => "índice actualizado de forma incremental".into(),
            SyncMode::Full => "índice reconstruido".into(),
            SyncMode::NoChanges => "índice al día".into(),
        };
        status.scanned_at_unix = report.scanned_at_unix;
        status.files = report.files;
        status.dirs = report.dirs;
        status.symbols = report.symbols;
        status.modules = report.modules;
        status.stack = report.stack.clone();
        status.git = report.git.clone();
        Ok(SyncOutcome::Updated(status, report))
    }
}

fn sync_project_locked<F: FnMut(String)>(
    root: &Path,
    brain_dir: &Path,
    force_full: bool,
    cancel: &CancelFlag,
    mut progress: F,
) -> Result<BrainReport, String> {
    if !root.is_dir() {
        return Err(format!(
            "La raíz no existe o no es una carpeta: {}",
            root.display()
        ));
    }
    ensure_brain_dir(root, brain_dir)?;
    let live_git = filter_git(root, brain_dir, inspect_git(root));
    let root_key = path_string(root);

    let mut state = match load_state(brain_dir) {
        Some(state)
            if !force_full
                && state.version == BRAIN_VERSION
                && state.root == root_key =>
        {
            state
        }
        _ => {
            progress("Sin índice utilizable: escaneo completo…".into());
            BrainState {
                version: BRAIN_VERSION,
                root: root_key,
                ..BrainState::default()
            }
        }
    };

    let needs_full_reconcile = !state.files.is_empty()
        && live_git.available
        && (!state.git.available
            || (!live_git.head.is_empty() && state.git.head.is_empty()));
    let full = force_full
        || (state.files.is_empty() && state.scanned_at_unix == 0)
        || needs_full_reconcile;
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut removed = 0usize;
    let mut index_touched = false;
    let mut changed_rel: BTreeSet<String> = BTreeSet::new();
    let mut mode = SyncMode::NoChanges;

    if full {
        mode = SyncMode::Full;
        progress("Escaneando el proyecto completo…".into());
        let (metas, truncated) = collect_files(root, brain_dir, cancel)?;
        let untracked = live_git.untracked_paths();
        let mut files = BTreeMap::new();
        for (index, meta) in metas.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err("Escaneo cancelado.".into());
            }
            if index % 250 == 0 {
                progress(format!("Indexando {} de {} archivos…", index + 1, metas.len()));
            }
            let record = analyze_file(
                root,
                meta,
                untracked.contains(meta.rel.as_str()),
                cancel,
            )?;
            files.insert(meta.rel.clone(), record);
        }
        added = files.len();
        state.files = files;
        state.truncated = truncated;
        state.scanned_at_unix = now_unix();
    } else if live_git.available {
        progress(format!(
            "Rama `{}` · HEAD `{}`. Comparando con el índice del {}…",
            live_git.branch,
            short_head(&live_git.head),
            format_timestamp(state.scanned_at_unix)
        ));
        if live_git.truncated {
            progress("Demasiados cambios en Git: reconstruyendo el índice…".into());
            return sync_project_locked(root, brain_dir, true, cancel, progress);
        }
        let mut plan: BTreeMap<String, (bool, bool)> = BTreeMap::new();
        if !state.git.head.is_empty() && state.git.head != live_git.head {
            match git_diff_name_status(root, &state.git.head, &live_git.head) {
                Ok(changes) => {
                    for (path, change) in changes {
                        if is_ignored_rel(&path, root, brain_dir) {
                            continue;
                        }
                        plan.insert(path, (change == PathChange::Remove, true));
                    }
                }
                Err(error) => {
                    progress(format!(
                        "No se pudo comparar commits ({error}); índice completo…"
                    ));
                    return sync_project_locked(root, brain_dir, true, cancel, progress);
                }
            }
        }
        let old_dirty: BTreeSet<String> = state
            .git
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        let new_dirty: BTreeSet<String> = live_git
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        for path in &new_dirty {
            if is_ignored_rel(path, root, brain_dir) {
                continue;
            }
            let exists = root.join(path).is_file();
            let slot = plan.entry(path.clone()).or_insert((false, false));
            slot.0 = !exists;
        }
        for path in old_dirty.difference(&new_dirty) {
            if is_ignored_rel(path, root, brain_dir) {
                continue;
            }
            let exists = root.join(path).is_file();
            let slot = plan.entry(path.clone()).or_insert((false, false));
            slot.0 = !exists;
        }
        let current_untracked = live_git.untracked_paths();
        if !plan.is_empty() {
            progress(format!(
                "Revisando {} ruta(s) afectadas, sin reconstruir el resto…",
                plan.len()
            ));
        }
        for (path, (remove, in_diff)) in plan {
            if cancel.load(Ordering::Relaxed) {
                return Err("Escaneo cancelado.".into());
            }
            if is_ignored_rel(&path, root, brain_dir) {
                if state.files.remove(&path).is_some() {
                    removed += 1;
                    changed_rel.insert(path);
                }
                continue;
            }
            let untracked = current_untracked.contains(path.as_str());
            let was_dirty = old_dirty.contains(&path);
            let still_dirty = new_dirty.contains(&path);
            if remove || !root.join(&path).is_file() {
                if state.files.remove(&path).is_some() {
                    removed += 1;
                    changed_rel.insert(path);
                }
                continue;
            }
            let metadata = fs::metadata(root.join(&path))
                .map_err(|err| format!("No se pudo leer {}: {err}", path))?;
            let same_meta = state.files.get(&path).is_some_and(|record| {
                record.size == metadata.len() && record.mtime_unix == mtime_unix(&metadata)
            });
            if !should_reread(same_meta, was_dirty, still_dirty, in_diff) {
                if let Some(record) = state.files.get_mut(&path) {
                    if record.untracked != untracked {
                        record.untracked = untracked;
                        index_touched = true;
                    }
                }
                continue;
            }
            let before = state.files.get(&path).cloned();
            let applied = apply_path(root, &path, untracked, cancel, &mut state.files)?;
            if !applied {
                if before.is_some() {
                    removed += 1;
                    changed_rel.insert(path);
                }
                continue;
            }
            match (before, state.files.get(&path)) {
                (Some(old), Some(new)) if same_analysis(&old, new) => index_touched = true,
                (Some(_), Some(_)) => {
                    updated += 1;
                    changed_rel.insert(path);
                }
                (None, Some(_)) => {
                    added += 1;
                    changed_rel.insert(path);
                }
                _ => {}
            }
        }
        if added + updated + removed > 0 {
            mode = SyncMode::Incremental;
            state.scanned_at_unix = now_unix();
            state.truncated = false;
        }
    } else {
        progress("Sin Git: comparando fechas de modificación con el último índice…".into());
        let (metas, truncated) = collect_files(root, brain_dir, cancel)?;
        let mut seen = BTreeSet::new();
        for meta in &metas {
            if cancel.load(Ordering::Relaxed) {
                return Err("Escaneo cancelado.".into());
            }
            seen.insert(meta.rel.clone());
            let changed = match state.files.get(&meta.rel) {
                Some(record) if record.size == meta.size && record.mtime_unix == meta.mtime_unix => {
                    false
                }
                _ => true,
            };
            if changed {
                let before = state.files.get(&meta.rel).cloned();
                let record = analyze_file(root, meta, false, cancel)?;
                match &before {
                    Some(old) if same_analysis(old, &record) => index_touched = true,
                    Some(_) => {
                        updated += 1;
                        changed_rel.insert(meta.rel.clone());
                    }
                    None => {
                        added += 1;
                        changed_rel.insert(meta.rel.clone());
                    }
                }
                state.files.insert(meta.rel.clone(), record);
            }
        }
        let gone: Vec<String> = state
            .files
            .keys()
            .filter(|rel| !seen.contains(*rel))
            .cloned()
            .collect();
        for rel in gone {
            state.files.remove(&rel);
            changed_rel.insert(rel);
            removed += 1;
        }
        state.truncated = truncated;
        if added + updated + removed > 0 {
            mode = SyncMode::Incremental;
            state.scanned_at_unix = now_unix();
        }
    }

    state.dirs = count_dirs(&state.files);
    state.total_bytes = state.files.values().map(|record| record.size).sum();
    state.version = BRAIN_VERSION;

    let old_entries: BTreeSet<(&str, &str)> = state
        .git
        .entries
        .iter()
        .map(|entry| (entry.status.as_str(), entry.path.as_str()))
        .collect();
    let new_entries: BTreeSet<(&str, &str)> = live_git
        .entries
        .iter()
        .map(|entry| (entry.status.as_str(), entry.path.as_str()))
        .collect();
    let git_changed = state.git.available != live_git.available
        || state.git.head != live_git.head
        || state.git.branch != live_git.branch
        || state.git.dirty != live_git.dirty
        || old_entries != new_entries;
    if added + updated + removed == 0 && git_changed && mode != SyncMode::Full {
        mode = SyncMode::GitOnly;
    }
    if mode != SyncMode::NoChanges || git_changed {
        state.git = live_git.clone();
    }

    let stack = detect_stack(root, &state.files);
    let modules = build_modules(&state.files);
    let module_count = unique_modules(&state.files);
    let symbols = sum_symbols(&state.files);

    match mode {
        SyncMode::Full | SyncMode::Incremental => {
            progress("Actualizando overview, mapa y módulos afectados…".into());
            render_docs(
                root,
                brain_dir,
                &state,
                &stack,
                &modules,
                &changed_rel,
                mode == SyncMode::Full,
            )?;
        }
        SyncMode::GitOnly => {
            progress("El código indexado no cambió. Actualizando el estado de Git en el overview…".into());
            refresh_git_docs(root, brain_dir, &state, &stack, &modules)?;
        }
        SyncMode::NoChanges => {
            ensure_docs_exist(root, brain_dir, &state, &stack, &modules)?;
        }
    }

    if mode != SyncMode::NoChanges || git_changed || index_touched {
        state.git = live_git.clone();
        save_state(brain_dir, &state)?;
    }

    Ok(BrainReport {
        mode,
        files: state.files.len(),
        dirs: state.dirs,
        total_bytes: state.total_bytes,
        symbols,
        modules: module_count,
        added,
        updated,
        removed,
        stack,
        scanned_at_unix: state.scanned_at_unix,
        git: live_git,
    })
}

/// Sincroniza sólo si detecta cambios. Devuelve `None` cuando el índice ya estaba al día.
#[allow(dead_code)]
pub fn sync_if_stale<F: FnMut(String)>(
    root: &Path,
    brain_dir: &Path,
    cancel: &CancelFlag,
    progress: F,
) -> Result<Option<BrainReport>, String> {
    match sync_session(root, brain_dir, false, cancel, progress)? {
        SyncOutcome::Unchanged(_) => Ok(None),
        SyncOutcome::Updated(_, report) => Ok(Some(report)),
    }
}

/// Prepara el prompt final: sincroniza si hace falta y arma un context pack progresivo.
pub fn compose_prompt_live<F: FnMut(ProgressTick)>(
    name: &str,
    root: &Path,
    brain_dir: &Path,
    question: &str,
    cancel: &CancelFlag,
    mut progress: F,
) -> (String, Result<SyncOutcome, String>) {
    let _guard = BRAIN_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    progress(ProgressTick::Status(
        "Brain: comprobando rama e índice…".into(),
    ));
    let mut outcome = sync_session_locked(root, brain_dir, false, cancel, |message| {
        progress(ProgressTick::Status(message));
    });
    let mut state = load_state(brain_dir);
    if !cancel.load(Ordering::Relaxed) {
        if let Some(state) = state.as_mut() {
            progress(ProgressTick::Status(
                "Brain: asentando conocimiento de la consulta…".into(),
            ));
            settle_question(root, state, question, &mut progress);
            let _ = save_state(brain_dir, state);
            if let Ok(kind) = outcome.as_mut() {
                let status = match kind {
                    SyncOutcome::Unchanged(status) | SyncOutcome::Updated(status, _) => status,
                };
                status.consultations = state.recall.consultations;
                status.settled = fresh_settled_count(state);
            }
        }
    }
    let live_git = match outcome.as_ref() {
        Ok(SyncOutcome::Unchanged(status)) => status.git.clone(),
        Ok(SyncOutcome::Updated(status, _)) => status.git.clone(),
        Err(_) => filter_git(root, brain_dir, inspect_git(root)),
    };
    progress(ProgressTick::Status(
        "Brain: armando el context pack…".into(),
    ));
    let prompt = compose_prompt_from_state(
        name,
        root,
        brain_dir,
        state.as_ref(),
        &live_git,
        question,
        &mut progress,
    );
    progress(ProgressTick::Packed { bytes: prompt.len() });
    (prompt, outcome)
}

fn settle_question<F: FnMut(ProgressTick)>(
    root: &Path,
    state: &mut BrainState,
    question: &str,
    progress: &mut F,
) {
    let keywords = keywords(question);
    let mut ranked: Vec<(i32, String)> = state
        .files
        .iter()
        .filter_map(|(rel, record)| {
            let score = file_score(state, rel, record, &keywords);
            (score > 0).then_some((score, rel.clone()))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.truncate(6);
    let chosen: Vec<String> = ranked.into_iter().map(|(_, rel)| rel).collect();
    for rel in &chosen {
        progress(ProgressTick::File {
            path: rel.clone(),
            detail: "Enriqueciendo desde el Brain".into(),
        });
        enrich_file(root, state, rel);
    }
    prune_stale_cards(state);
    let modules: BTreeSet<String> = chosen.iter().map(|rel| module_for(rel)).collect();
    for module in &modules {
        progress(ProgressTick::File {
            path: format!("modules/{module}"),
            detail: "Asentando ficha".into(),
        });
        write_card(state, module);
    }
    remember(state, &keywords, &chosen);
    state.recall.consultations = state.recall.consultations.saturating_add(1);
}

fn enrich_file(root: &Path, state: &mut BrainState, rel: &str) {
    let Some(record) = state.files.get(rel).cloned() else {
        return;
    };
    if record.enriched || !record.text || !is_code_like(rel) {
        if let Some(slot) = state.files.get_mut(rel) {
            slot.enriched = true;
        }
        return;
    }
    let Some(content) = read_text(&root.join(rel), MAX_ANALYZE_BYTES) else {
        return;
    };
    let (symbols, imports, doc) = analyze_code(rel, &content);
    if let Some(slot) = state.files.get_mut(rel) {
        slot.symbols = symbols;
        slot.imports = imports;
        slot.doc = doc;
        slot.enriched = true;
    }
}

fn write_card(state: &mut BrainState, module: &str) {
    let fingerprint = module_fingerprint(&state.files, module);
    let lines = card_lines(&state.files, module);
    if lines.is_empty() || fingerprint.is_empty() {
        return;
    }
    if let Some(card) = state.settled.get_mut(module) {
        if card.fingerprint == fingerprint && card.lines == lines {
            card.hits = card.hits.saturating_add(1);
        } else {
            card.fingerprint = fingerprint;
            card.lines = lines;
            card.hits = 1;
        }
        return;
    }
    if state.settled.len() >= 80 {
        let victim = state
            .settled
            .iter()
            .min_by(|a, b| a.1.hits.cmp(&b.1.hits).then_with(|| a.0.cmp(b.0)))
            .map(|(path, _)| path.clone());
        if let Some(victim) = victim {
            state.settled.remove(&victim);
        }
    }
    state.settled.insert(
        module.to_string(),
        SettledCard {
            fingerprint,
            lines,
            hits: 1,
        },
    );
}

fn card_lines(files: &BTreeMap<String, FileRecord>, module: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for (rel, record) in files {
        if module_for(rel) != module || !is_code_like(rel) {
            continue;
        }
        if !record.doc.is_empty() && lines.len() < 12 {
            lines.push(format!("`{rel}` — {}", record.doc));
        }
        for symbol in record.symbols.iter().take(8) {
            if lines.len() >= 12 {
                break;
            }
            let signature = if symbol.signature.is_empty() {
                symbol.name.clone()
            } else {
                symbol.signature.clone()
            };
            if symbol.doc.is_empty() {
                lines.push(format!("`{rel}` {} `{signature}`", symbol.kind));
            } else {
                lines.push(format!(
                    "`{rel}` {} `{signature}` — {}",
                    symbol.kind, symbol.doc
                ));
            }
        }
    }
    lines
}

fn module_fingerprint(files: &BTreeMap<String, FileRecord>, module: &str) -> String {
    let mut parts = Vec::new();
    for (rel, record) in files {
        if module_for(rel) == module && is_code_like(rel) {
            parts.push(format!("{rel}:{}:{}", record.size, record.mtime_unix));
        }
    }
    parts.sort();
    parts.join("|")
}

fn prune_stale_cards(state: &mut BrainState) {
    let stale: Vec<String> = state
        .settled
        .iter()
        .filter(|(module, card)| card.fingerprint != module_fingerprint(&state.files, module))
        .map(|(module, _)| module.clone())
        .collect();
    for module in stale {
        state.settled.remove(&module);
    }
}

fn fresh_settled_count(state: &BrainState) -> usize {
    state
        .settled
        .iter()
        .filter(|(module, card)| card.fingerprint == module_fingerprint(&state.files, module))
        .count()
}

fn remember(state: &mut BrainState, keywords: &[String], files: &[String]) {
    for term in keywords.iter().take(12) {
        let paths = state.recall.terms.entry(term.clone()).or_default();
        for rel in files.iter().take(4) {
            let hits = paths.entry(rel.clone()).or_insert(0);
            *hits = hits.saturating_add(1);
        }
        if paths.len() > 6 {
            let mut ranked: Vec<(String, u32)> = paths.iter().map(|(path, hits)| (path.clone(), *hits)).collect();
            ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
            while paths.len() > 6 {
                let Some((path, _)) = ranked.first().cloned() else {
                    break;
                };
                paths.remove(&path);
                ranked.remove(0);
            }
        }
    }
    if state.recall.terms.len() > 400 {
        let mut ranked: Vec<(String, u32)> = state
            .recall
            .terms
            .iter()
            .map(|(term, paths)| (term.clone(), paths.values().copied().sum()))
            .collect();
        ranked.sort_by_key(|(_, hits)| *hits);
        while state.recall.terms.len() > 400 {
            let Some((term, _)) = ranked.first().cloned() else {
                break;
            };
            state.recall.terms.remove(&term);
            ranked.remove(0);
        }
    }
}

fn file_score(state: &BrainState, rel: &str, record: &FileRecord, keywords: &[String]) -> i32 {
    let mut score = 0i32;
    let mut recall = 0i32;
    let lower_rel = rel.to_ascii_lowercase();
    let file_doc = record.doc.to_ascii_lowercase();
    for keyword in keywords {
        if lower_rel.contains(keyword) {
            score += 12;
        }
        if file_doc.contains(keyword) {
            score += 5;
        }
        if record.symbols.iter().any(|symbol| {
            symbol.name.to_ascii_lowercase().contains(keyword)
                || symbol.signature.to_ascii_lowercase().contains(keyword)
                || symbol.doc.to_ascii_lowercase().contains(keyword)
        }) {
            score += 7;
        }
        if record
            .imports
            .iter()
            .any(|import| import.to_ascii_lowercase().contains(keyword))
        {
            score += 2;
        }
        if let Some(hits) = state.recall.terms.get(keyword).and_then(|paths| paths.get(rel)) {
            recall += *hits as i32;
        }
    }
    score + recall.min(15)
}

fn render_settled(state: &BrainState, question: &str) -> String {
    let keywords = keywords(question);
    let mut ranked: Vec<(i32, &str, &SettledCard)> = Vec::new();
    for (module, card) in &state.settled {
        if card.fingerprint != module_fingerprint(&state.files, module) {
            continue;
        }
        let mut score = 0i32;
        let module_l = module.to_ascii_lowercase();
        let blob = card.lines.join("\n").to_ascii_lowercase();
        for keyword in &keywords {
            if module_l.contains(keyword) {
                score += 8;
            }
            if blob.contains(keyword) {
                score += 6;
            }
            if state.recall.terms.get(keyword).is_some_and(|paths| {
                paths.keys().any(|path| module_for(path) == *module)
            }) {
                score += 4;
            }
        }
        if score == 0 {
            continue;
        }
        score += card.hits.min(10) as i32;
        ranked.push((score, module.as_str(), card));
    }
    if ranked.is_empty() {
        return String::new();
    }
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let mut out = String::from(
        "CONOCIMIENTO ASENTADO (firmas y comentarios copiados del código; sólo vale mientras el ancla del archivo siga igual):\n",
    );
    for (_, module, card) in ranked.into_iter().take(3) {
        out.push_str(&format!(
            "- `{module}` · visto {} {}\n",
            card.hits,
            if card.hits == 1 { "vez" } else { "veces" }
        ));
        for line in card.lines.iter().take(6) {
            out.push_str(&format!("  - {line}\n"));
        }
    }
    out.push('\n');
    out
}

fn compose_prompt_from_state<F: FnMut(ProgressTick)>(
    name: &str,
    root: &Path,
    brain_dir: &Path,
    state: Option<&BrainState>,
    live_git: &GitState,
    question: &str,
    progress: &mut F,
) -> String {
    let mut pack = String::new();
    pack.push_str(&format!("PROYECTO: «{name}»\n"));
    pack.push_str(&format!("RAÍZ: `{}`\n", root.display()));
    pack.push_str(&format!("BRAIN: `{}`\n\n", brain_dir.display()));

    pack.push_str("ESTADO GIT (verificado ahora):\n");
    pack.push_str(&format_git_block(live_git));
    pack.push('\n');

    match state {
        Some(state) => {
            let symbols = sum_symbols(&state.files);
            let modules = unique_modules(&state.files);
            pack.push_str("ÍNDICE DEL REPOSITORIO:\n");
            pack.push_str(&format!(
                "- Archivos indexados: {} · símbolos: {} · módulos: {} · tamaño: {}\n",
                state.files.len(),
                symbols,
                modules,
                human_bytes(state.total_bytes)
            ));
            pack.push_str(&format!(
                "- Último índice: {}\n",
                format_timestamp(state.scanned_at_unix)
            ));
            if state.scanned_at_unix != state.git.captured_at_unix && !state.git.head.is_empty() {
                pack.push_str(&format!(
                    "- Git al indexar: rama `{}` · HEAD `{}` · {}\n",
                    state.git.branch,
                    short_head(&state.git.head),
                    state.git.dirty_label()
                ));
            }
            if state.truncated {
                pack.push_str("- Aviso: el índice se truncó por tamaño; no contiene todo el repo.\n");
            }
            pack.push('\n');

            let stack = detect_stack(root, &state.files);
            pack.push_str("NIVEL 1 — OVERVIEW (documentación general):\n");
            pack.push_str("- Stack: ");
            if stack.is_empty() {
                pack.push_str("no detectado\n");
            } else {
                pack.push_str(&stack.join(", "));
                pack.push('\n');
            }
            let tops = top_dirs(root);
            if !tops.is_empty() {
                pack.push_str(&format!("- Carpetas de primer nivel: {}\n", tops.join(", ")));
            }
            let entrypoints = entrypoint_paths(&state.files);
            if !entrypoints.is_empty() {
                pack.push_str(&format!("- Entrypoints: {}\n", entrypoints.join(", ")));
            }
            let module_summary = compact_module_summary(&state.files, 12);
            if !module_summary.is_empty() {
                pack.push_str("- Módulos principales: ");
                pack.push_str(&module_summary.join("; "));
                pack.push('\n');
            }
            pack.push_str("- `00_overview.md` es documentación general. No sustituye al código.\n");
            pack.push('\n');
            progress(ProgressTick::File {
                path: "conocimiento_asentado".into(),
                detail: "Cargando fichas del Brain".into(),
            });
            pack.push_str(&render_settled(state, question));

            let sources = load_current_sources(root, state, question, progress);
            if !sources.is_empty() {
                pack.push_str(&sources);
                pack.push('\n');
            }
            progress(ProgressTick::File {
                path: "índice".into(),
                detail: "Recuperando archivos relevantes".into(),
            });
            pack.push_str(&retrieve_relevant(state, question));
            pack.push('\n');

            if let Some(extra) = read_extra_context(brain_dir) {
                progress(ProgressTick::File {
                    path: "06_extra_context.md".into(),
                    detail: "Contexto extra aportado".into(),
                });
                pack.push_str("CONTEXTO EXTRA APORTADO POR EL USUARIO:\n");
                pack.push_str(&extra);
                pack.push('\n');
            }
        }
        None => {
            pack.push_str("ÍNDICE DEL REPOSITORIO: todavía no existe o no se pudo leer.\n");
            pack.push_str("Pedí que se ejecute el primer índice antes de describir el proyecto.\n\n");
        }
    }

    let pack = truncate_bytes(&pack, MAX_CONTEXT_BYTES);
    format!(
        "Estás asistiendo sobre el proyecto «{name}».\n\n\
         CONTEXTO DEL PROYECTO (context engineering + progressive disclosure):\n\
         <<<\n{pack}\n>>>\n\n\
         REGLAS OBLIGATORIAS:\n\
         1. El código en disco es la fuente de verdad. El Brain es un índice incremental sincronizado con Git.\n\
         2. Nunca asumas que una descripción del Brain representa el comportamiento actual del código.\n\
         3. El overview es documentación general. El bloque «CONOCIMIENTO ASENTADO» copia firmas y comentarios del código; si no coincide con «CÓDIGO ACTUAL», manda el código.\n\
         4. Antes de modificar un módulo, usá el bloque «CÓDIGO ACTUAL» si está. Si no alcanza, leé esos archivos y sus dependencias en `{root}`.\n\
         5. Si el working tree tiene cambios posteriores al índice, priorizá esos archivos.\n\
         6. Respondé SOLO con información verificable. Si un dato no está en el contexto ni en el código, decilo y pedí la fuente.\n\
         7. Citá rutas relativas a la raíz cuando sea útil.\n\
         8. La raíz es `{root}` y el brain está en `{brain_dir}`.\n\n\
         PREGUNTA DEL USUARIO:\n{question}",
        root = root.display(),
        brain_dir = brain_dir.display()
    )
}

fn retrieve_relevant(state: &BrainState, question: &str) -> String {
    let keywords = keywords(question);
    let mut scored: Vec<(i32, String)> = Vec::new();

    for (rel, record) in &state.files {
        let score = file_score(state, rel, record, &keywords);
        if score > 0 {
            scored.push((score, rel.clone()));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut selected: Vec<String> = Vec::new();
    let mut per_module: BTreeMap<String, usize> = BTreeMap::new();
    for (_, rel) in scored {
        let module = module_for(&rel);
        let count = per_module.entry(module).or_insert(0);
        if *count >= 2 {
            continue;
        }
        *count += 1;
        selected.push(rel);
        if selected.len() >= 8 {
            break;
        }
    }

    let mut out = String::from(
        "NIVEL 2 — ÍNDICE RELEVANTE (rutas y símbolos, sin volcar el código):\n",
    );
    if selected.is_empty() {
        let entrypoints = entrypoint_paths(&state.files);
        if entrypoints.is_empty() {
            out.push_str("- No hay coincidencias. Indicá una ruta o un módulo.\n");
        } else {
            out.push_str("- No hay coincidencias. Entrypoints del índice: ");
            out.push_str(&entrypoints.join(", "));
            out.push('\n');
        }
        return out;
    }
    for rel in &selected {
        let Some(record) = state.files.get(rel) else {
            continue;
        };
        out.push_str(&format!("- `{rel}`"));
        if !record.symbols.is_empty() {
            let names: Vec<String> = record
                .symbols
                .iter()
                .take(8)
                .map(|symbol| format!("{} {}:{ }", symbol.kind, symbol.name, symbol.line))
                .collect();
            out.push_str(&format!(" — {}", names.join(", ")));
        }
        out.push('\n');
    }
    out.push_str("El detalle completo de cada módulo está en `modules/` dentro del brain.\n");
    out
}

fn question_needs_source(question: &str) -> bool {
    let lower = question.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "modific", "cambi", "arregl", "implement", "agreg", "añad", "anad", "elimin", "borr",
        "refactor", "corrig", "fix", "update", "edit", "remove", "crea", "escrib", "patch",
        "bug", "error", "fall", "inspeccion", "revis", "actualiz", "renombr", "mueve", "mover",
        "reemplaz", "optimiz", "haz que", "rompe", "roto", "broken", "deberia", "debería",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

fn load_current_sources<F: FnMut(ProgressTick)>(
    root: &Path,
    state: &BrainState,
    question: &str,
    progress: &mut F,
) -> String {
    if !question_needs_source(question) {
        return String::new();
    }
    let keywords = keywords(question);
    let mut scores: BTreeMap<String, i32> = BTreeMap::new();
    for (rel, record) in &state.files {
        let module = module_for(rel);
        let mut score = 0i32;
        let rel_l = rel.to_ascii_lowercase();
        let module_l = module.to_ascii_lowercase();
        for keyword in &keywords {
            if module_l.contains(keyword) {
                score += 6;
            }
            if rel_l.contains(keyword) {
                score += 10;
            }
            if record.symbols.iter().any(|symbol| {
                let name = symbol.name.to_ascii_lowercase();
                let signature = symbol.signature.to_ascii_lowercase();
                let doc = symbol.doc.to_ascii_lowercase();
                name.contains(keyword) || signature.contains(keyword) || doc.contains(keyword)
            }) {
                score += 7;
            }
            if state.recall.terms.get(keyword).is_some_and(|paths| {
                paths.keys().any(|path| module_for(path) == module)
            }) {
                score += 4;
            }
        }
        if score > 0 {
            *scores.entry(module).or_insert(0) += score;
        }
    }
    let mut ranked: Vec<(i32, String)> = scores.into_iter().map(|(path, score)| (score, path)).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.truncate(2);
    if ranked.is_empty() {
        return "CÓDIGO ACTUAL: la pregunta pide modificar o inspeccionar un módulo, pero no identifiqué cuál. Indicá la ruta. El overview no sustituye al código.\n".into();
    }

    let modules = build_modules(&state.files);
    let mut out = String::from(
        "NIVEL 3 — CÓDIGO ACTUAL (leído del disco ahora; es la fuente de verdad):\n",
    );
    let mut budget = 14 * 1024usize;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (_, module_path) in &ranked {
        let Some(module) = modules.iter().find(|module| &module.path == module_path) else {
            continue;
        };
        out.push_str(&format!("\n## Módulo `{}`\n", module.path));
        if !module.depends_on.is_empty() {
            out.push_str(&format!(
                "Dependencias directas: {}\n",
                module.depends_on.join(", ")
            ));
        }
        let mut files = files_in_module(&state.files, &module.path);
        files.sort_by(|a, b| file_rank(b, &keywords).cmp(&file_rank(a, &keywords)).then_with(|| a.cmp(b)));
        for rel in files.into_iter().take(4) {
            if !seen.insert(rel.clone()) {
                continue;
            }
            progress(ProgressTick::File {
                path: rel.clone(),
                detail: "Leyendo código actual".into(),
            });
            if !append_live_file(&mut out, root, &rel, &mut budget, 5 * 1024) {
                out.push_str("\n_(se cortó la lectura para no exceder el contexto)_\n");
                return out;
            }
        }
        for dependency in module.depends_on.iter().take(3) {
            let mut dependency_files = files_in_module(&state.files, dependency);
            dependency_files.sort_by(|a, b| {
                entry_rank(a)
                    .cmp(&entry_rank(b))
                    .then_with(|| a.cmp(b))
            });
            let Some(rel) = dependency_files.into_iter().next() else {
                continue;
            };
            if !seen.insert(rel.clone()) {
                continue;
            }
            progress(ProgressTick::File {
                path: rel.clone(),
                detail: format!("Dependencia `{dependency}`"),
            });
            out.push_str(&format!("\n### Dependencia `{dependency}`\n"));
            if !append_live_file(&mut out, root, &rel, &mut budget, 2 * 1024) {
                out.push_str("\n_(se cortó la lectura para no exceder el contexto)_\n");
                return out;
            }
        }
    }
    out
}

fn files_in_module(files: &BTreeMap<String, FileRecord>, module: &str) -> Vec<String> {
    files
        .keys()
        .filter(|rel| module_for(rel) == module && is_code_like(rel))
        .cloned()
        .collect()
}

fn file_rank(rel: &str, keywords: &[String]) -> i32 {
    let lower = rel.to_ascii_lowercase();
    keywords
        .iter()
        .filter(|keyword| lower.contains(keyword.as_str()))
        .count() as i32
}

fn entry_rank(rel: &str) -> u8 {
    let name = Path::new(rel)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(rel);
    if matches!(name, "mod.rs" | "lib.rs" | "index.ts" | "index.js" | "main.rs") {
        0
    } else {
        1
    }
}

fn append_live_file(out: &mut String, root: &Path, rel: &str, budget: &mut usize, per_file: usize) -> bool {
    if *budget < 400 {
        return false;
    }
    let Some(content) = read_text(&root.join(rel), per_file as u64) else {
        out.push_str(&format!("- `{rel}` no se pudo leer ahora.\n"));
        return true;
    };
    let name = Path::new(rel)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(rel);
    let content = if is_config_name(name) {
        redact(&content)
    } else {
        content
    };
    let excerpt = truncate_bytes(content.trim_end(), (*budget).min(per_file));
    let block = format!("\n#### `{rel}`\n\n```{}\n{}\n```\n", fence_lang(rel), excerpt.trim_end());
    if block.len() > *budget && *budget < 800 {
        return false;
    }
    let block = truncate_bytes(&block, *budget);
    *budget = budget.saturating_sub(block.len());
    out.push_str(&block);
    true
}

fn format_git_block(git: &GitState) -> String {
    if !git.available {
        return "- Sin Git disponible o el directorio no es un repositorio.\n".into();
    }
    let mut s = format!(
        "- Rama: `{}` · HEAD: `{}` · {}\n",
        git.branch,
        short_head(&git.head),
        git.dirty_label()
    );
    if let Some(error) = &git.error {
        s.push_str(&format!("- Aviso de Git: {error}\n"));
    }
    if git.dirty {
        let shown: Vec<String> = git
            .entries
            .iter()
            .take(12)
            .map(|entry| format!("{} {}", entry.status, entry.path))
            .collect();
        s.push_str(&format!("- Cambios actuales: {}\n", shown.join(", ")));
        if git.entries.len() > shown.len() {
            s.push_str("- (hay más cambios; el índice se sincroniza antes de responder)\n");
        }
    }
    s
}

fn apply_path(
    root: &Path,
    rel: &str,
    untracked: bool,
    cancel: &CancelFlag,
    files: &mut BTreeMap<String, FileRecord>,
) -> Result<bool, String> {
    let path = root.join(rel);
    if !path.is_file() {
        files.remove(rel);
        return Ok(false);
    }
    let metadata = fs::metadata(&path)
        .map_err(|e| format!("No se pudo leer {}: {e}", path.display()))?;
    let meta = FileMeta {
        rel: rel.to_string(),
        size: metadata.len(),
        mtime_unix: mtime_unix(&metadata),
        text: is_text_like(rel),
    };
    let record = analyze_file(root, &meta, untracked, cancel)?;
    files.insert(rel.to_string(), record);
    Ok(true)
}

fn analyze_file(
    root: &Path,
    meta: &FileMeta,
    untracked: bool,
    cancel: &CancelFlag,
) -> Result<FileRecord, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Escaneo cancelado.".into());
    }
    let lang = lang_for(&meta.rel);
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut doc = String::new();
    if meta.text && is_code_like(&meta.rel) {
        if let Some(content) = read_text(&root.join(&meta.rel), MAX_ANALYZE_BYTES) {
            let (found_symbols, found_imports, found_doc) = analyze_code(&meta.rel, &content);
            symbols = found_symbols;
            imports = found_imports;
            doc = found_doc;
        }
    }
    Ok(FileRecord {
        size: meta.size,
        mtime_unix: meta.mtime_unix,
        text: meta.text,
        lang: lang.to_string(),
        untracked,
        symbols,
        imports,
        doc,
        enriched: true,
    })
}

fn analyze_code(rel: &str, content: &str) -> (Vec<Symbol>, Vec<String>, String) {
    let lang = lang_for(rel);
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut class_indent: Option<usize> = None;
    let mut impl_depth: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_import_block = false;
    let mut in_block_doc = false;
    let mut pending_doc = String::new();
    let mut file_doc = String::new();
    let mut seen_code = false;

    for (index, raw) in content.lines().enumerate() {
        if symbols.len() >= MAX_SYMBOLS_PER_FILE && imports.len() >= MAX_IMPORTS_PER_FILE {
            break;
        }
        let line = raw.trim();
        if line.is_empty() {
            pending_doc.clear();
            continue;
        }
        if let Some(text) = doc_comment_text(lang, line, &mut in_block_doc) {
            if !text.is_empty() && pending_doc.len() < 360 {
                pending_doc.push_str(&text);
                pending_doc.push(' ');
                if !seen_code && file_doc.len() < 360 {
                    file_doc.push_str(&text);
                    file_doc.push(' ');
                }
            }
            continue;
        }
        if line.starts_with("//") || line.starts_with('#') || line.starts_with("/*") {
            pending_doc.clear();
            continue;
        }
        seen_code = true;
        let symbols_before = symbols.len();
        let indentation = raw.len() - raw.trim_start().len();
        let line_no = index + 1;

        match lang {
            "rust" => {
                if line.starts_with("use ") {
                    push_import(&mut imports, line.trim_end_matches(';'));
                } else if line.starts_with("mod ") {
                    if let Some(name) = ident_after(line, "mod ") {
                        push_symbol(&mut symbols, "mod", &name, line_no, line, &mut pending_doc);
                    }
                } else if line.starts_with("impl") {
                    impl_depth = Some(depth);
                } else if line.starts_with("fn ") || line.starts_with("pub fn ") || line.starts_with("async fn ") {
                    if let Some(name) = function_name(line) {
                        let kind = if impl_depth.is_some_and(|d| depth > d) {
                            "method"
                        } else {
                            "fn"
                        };
                        push_symbol(&mut symbols, kind, &name, line_no, line, &mut pending_doc);
                    }
                } else if let Some((kind, name)) = type_declaration(line) {
                    push_symbol(&mut symbols, kind, &name, line_no, line, &mut pending_doc);
                } else if line.starts_with("macro_rules!") {
                    if let Some(name) = ident_after(line, "macro_rules!") {
                        push_symbol(&mut symbols, "macro", &name, line_no, line, &mut pending_doc);
                    }
                }
            }
            "javascript" | "typescript" | "tsx" | "jsx" | "vue" | "svelte" => {
                if line.starts_with("import ") || line.starts_with("export ") && line.contains(" from ") {
                    if let Some(import) = quoted_import(line) {
                        push_import(&mut imports, &import);
                    }
                }
                if let Some(import) = require_import(line) {
                    push_import(&mut imports, &import);
                }
                let mut rest = line
                    .strip_prefix("export default ")
                    .or_else(|| line.strip_prefix("export "))
                    .unwrap_or(line);
                if rest.starts_with("async function ") {
                    rest = rest.trim_start_matches("async ");
                }
                if rest.starts_with("function ") {
                    if let Some(name) = function_name(rest) {
                        push_symbol(&mut symbols, "function", &name, line_no, line, &mut pending_doc);
                    }
                } else if let Some(name) = ident_after(rest, "class ") {
                    push_symbol(&mut symbols, "class", &name, line_no, line, &mut pending_doc);
                    class_indent = Some(indentation);
                } else if let Some(name) = ident_after(rest, "interface ") {
                    push_symbol(&mut symbols, "interface", &name, line_no, line, &mut pending_doc);
                } else if let Some(name) = ident_after(rest, "enum ") {
                    push_symbol(&mut symbols, "enum", &name, line_no, line, &mut pending_doc);
                } else if rest.starts_with("type ") {
                    if let Some(name) = ident_after(rest, "type ") {
                        push_symbol(&mut symbols, "type", &name, line_no, line, &mut pending_doc);
                    }
                } else if rest.starts_with("const ") || rest.starts_with("let ") {
                    if let Some(name) = assignment_name(rest) {
                        if rest.contains("=>") || rest.contains("= function") || rest.contains("= async") {
                            push_symbol(&mut symbols, "function", &name, line_no, line, &mut pending_doc);
                        }
                    }
                } else if class_indent.is_some_and(|base| indentation > base) {
                    if let Some(name) = method_name(rest) {
                        push_symbol(&mut symbols, "method", &name, line_no, line, &mut pending_doc);
                    }
                }
            }
            "python" => {
                if line.starts_with("import ") {
                    push_import(&mut imports, line.trim_start_matches("import "));
                } else if line.starts_with("from ") && line.contains(" import ") {
                    if let Some((module, _)) = line.split_once(" import ") {
                        push_import(&mut imports, module.trim_start_matches("from "));
                    }
                } else if let Some(name) = ident_after(line, "class ") {
                    push_symbol(&mut symbols, "class", &name, line_no, line, &mut pending_doc);
                    class_indent = Some(indentation);
                } else if line.starts_with("def ") || line.starts_with("async def ") {
                    let rest = line.trim_start_matches("async ");
                    if let Some(name) = function_name(rest) {
                        let kind = if class_indent.is_some_and(|base| indentation > base) {
                            "method"
                        } else {
                            "function"
                        };
                        push_symbol(&mut symbols, kind, &name, line_no, line, &mut pending_doc);
                    }
                }
            }
            "go" => {
                if line.starts_with("import (") {
                    in_import_block = true;
                } else if in_import_block {
                    if line == ")" {
                        in_import_block = false;
                    } else if let Some(import) = quoted_import(line) {
                        push_import(&mut imports, &import);
                    }
                } else if line.starts_with("import ") {
                    if let Some(import) = quoted_import(line) {
                        push_import(&mut imports, &import);
                    }
                } else if line.starts_with("func ") {
                    let name = function_name(line).or_else(|| {
                        line.split('(')
                            .nth(1)
                            .and_then(|rest| rest.split(')').nth(1))
                            .and_then(|rest| rest.split('(').next())
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .map(str::to_string)
                    });
                    if let Some(name) = name {
                        let kind = if line.starts_with("func (") {
                            "method"
                        } else {
                            "function"
                        };
                        push_symbol(&mut symbols, kind, &name, line_no, line, &mut pending_doc);
                    }
                } else if line.starts_with("type ") {
                    if let Some(name) = ident_after(line, "type ") {
                        push_symbol(&mut symbols, "type", &name, line_no, line, &mut pending_doc);
                    }
                }
            }
            "java" | "kotlin" | "csharp" => {
                if line.starts_with("package ") {
                    push_import(&mut imports, line.trim_start_matches("package "));
                } else if line.starts_with("import ") {
                    push_import(&mut imports, line.trim_start_matches("import "));
                } else if let Some((kind, name)) = type_declaration(line) {
                    push_symbol(&mut symbols, kind, &name, line_no, line, &mut pending_doc);
                } else if class_indent.is_some_and(|base| indentation > base) {
                    if let Some(name) = method_name(line) {
                        push_symbol(&mut symbols, "method", &name, line_no, line, &mut pending_doc);
                    }
                }
                if type_declaration(line).is_some() {
                    class_indent = Some(indentation);
                }
            }
            "ruby" | "php" | "swift" | "dart" | "c" | "cpp" => {
                if let Some(import) = quoted_import(line) {
                    if line.starts_with("import") || line.starts_with("#include") {
                        push_import(&mut imports, &import);
                    }
                }
                if let Some((kind, name)) = type_declaration(line) {
                    push_symbol(&mut symbols, kind, &name, line_no, line, &mut pending_doc);
                    class_indent = Some(indentation);
                } else if class_indent.is_some_and(|base| indentation > base) {
                    if let Some(name) = method_name(line) {
                        push_symbol(&mut symbols, "method", &name, line_no, line, &mut pending_doc);
                    }
                }
            }
            _ => {}
        }
        if symbols.len() == symbols_before {
            pending_doc.clear();
        }

        depth += raw.matches('{').count();
        if depth < raw.matches('}').count() {
            depth = 0;
        } else {
            depth -= raw.matches('}').count();
        }
        if line == "}" {
            if impl_depth.is_some_and(|d| depth <= d) {
                impl_depth = None;
            }
            if class_indent.is_some_and(|base| indentation <= base) {
                class_indent = None;
            }
        }
    }

    (symbols, imports, squash_text(&file_doc, 280))
}

fn function_name(line: &str) -> Option<String> {
    let open = line.find('(')?;
    let before = &line[..open];
    let name = before.split_whitespace().last()?;
    let name = name.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn method_name(line: &str) -> Option<String> {
    let trimmed = line
        .trim_start_matches("pub ")
        .trim_start_matches("public ")
        .trim_start_matches("private ")
        .trim_start_matches("protected ")
        .trim_start_matches("static ")
        .trim_start_matches("async ");
    if !trimmed.contains('(') || trimmed.starts_with("if ") || trimmed.starts_with("for ") {
        return None;
    }
    let before = trimmed.split('(').next()?;
    let name = before.split_whitespace().last()?;
    let name = name.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
    if name.is_empty() || name == "fn" || name == "function" || name == "def" {
        None
    } else {
        Some(name.to_string())
    }
}

fn assignment_name(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("const ")
        .or_else(|| line.strip_prefix("let "))?;
    let name = rest
        .split(|c: char| c == '=' || c == ':' || c.is_whitespace())
        .next()?
        .trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn type_declaration(line: &str) -> Option<(&'static str, String)> {
    let trimmed = line
        .trim_start_matches("pub ")
        .trim_start_matches("public ")
        .trim_start_matches("private ")
        .trim_start_matches("protected ")
        .trim_start_matches("abstract ")
        .trim_start_matches("sealed ")
        .trim_start_matches("final ")
        .trim_start_matches("data ");
    for (prefix, kind) in [
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("trait ", "trait"),
        ("interface ", "interface"),
        ("class ", "class"),
        ("type ", "type"),
        ("record ", "record"),
    ] {
        if let Some(name) = ident_after(trimmed, prefix) {
            return Some((kind, name));
        }
    }
    if let Some(name) = ident_after(trimmed, "func ") {
        return Some(("function", name));
    }
    None
}

fn ident_after(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?;
    let name = rest
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn quoted_import(line: &str) -> Option<String> {
    for quote in ['"', '\'', '`'] {
        if let Some(start) = line.find(quote) {
            if let Some(end) = line[start + 1..].find(quote) {
                let value = &line[start + 1..start + 1 + end];
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn require_import(line: &str) -> Option<String> {
    let index = line.find("require(")?;
    let rest = &line[index + "require(".len()..];
    let value = rest.trim_start();
    let value = value
        .strip_prefix('"')
        .or_else(|| value.strip_prefix('\''))?;
    let end = value.find(|c| c == '"' || c == '\'')?;
    Some(value[..end].to_string())
}

fn push_symbol(
    symbols: &mut Vec<Symbol>,
    kind: &str,
    name: &str,
    line: usize,
    source: &str,
    doc: &mut String,
) {
    if symbols.len() >= MAX_SYMBOLS_PER_FILE {
        doc.clear();
        return;
    }
    if symbols.iter().any(|symbol| symbol.name == name && symbol.kind == kind) {
        doc.clear();
        return;
    }
    symbols.push(Symbol {
        kind: kind.to_string(),
        name: name.to_string(),
        line,
        signature: clean_signature(source),
        doc: squash_text(doc, 220),
    });
    doc.clear();
}

fn doc_comment_text(lang: &str, line: &str, in_block: &mut bool) -> Option<String> {
    if *in_block {
        if line.contains("*/") {
            *in_block = false;
        }
        let cleaned = line
            .trim()
            .trim_start_matches('*')
            .trim()
            .trim_end_matches("*/")
            .trim();
        return Some(cleaned.to_string());
    }
    match lang {
        "rust" => line
            .strip_prefix("///")
            .or_else(|| line.strip_prefix("//!"))
            .map(|rest| rest.trim().to_string()),
        "python" => line
            .strip_prefix('#')
            .filter(|rest| !rest.starts_with('!'))
            .map(|rest| rest.trim().to_string()),
        "javascript" | "typescript" | "tsx" | "jsx" | "java" | "kotlin" | "go" => {
            if line.starts_with("/**") {
                *in_block = !line.contains("*/");
                let cleaned = line
                    .trim_start_matches('/')
                    .trim_matches('*')
                    .trim()
                    .trim_end_matches("*/")
                    .trim();
                Some(cleaned.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn clean_signature(line: &str) -> String {
    let trimmed = line.trim().trim_end_matches('{').trim().trim_end_matches(':').trim();
    squash_text(&trimmed.to_string(), 180)
}

fn squash_text(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    flat.chars().take(max_chars).collect::<String>() + "…"
}

fn push_import(imports: &mut Vec<String>, value: &str) {
    if imports.len() >= MAX_IMPORTS_PER_FILE {
        return;
    }
    let value = value.trim().trim_matches('{').trim_matches('}').trim();
    if value.is_empty() || imports.iter().any(|existing| existing == value) {
        return;
    }
    imports.push(value.to_string());
}

fn detect_stack(root: &Path, files: &BTreeMap<String, FileRecord>) -> Vec<String> {
    let mut stack: Vec<String> = Vec::new();
    let root_has = |name: &str| root.join(name).exists();
    let any_ext = |ext: &str| {
        files.keys().any(|rel| {
            Path::new(rel)
                .extension()
                .and_then(|s| s.to_str())
                .map(|e| e.eq_ignore_ascii_case(ext))
                .unwrap_or(false)
        })
    };

    if root_has("Cargo.toml") {
        stack.push("Rust (Cargo)".into());
    }
    if root_has("package.json") {
        if root_has("tsconfig.json") {
            stack.push("Node.js + TypeScript".into());
        } else {
            stack.push("Node.js / JavaScript".into());
        }
    }
    if root_has("pyproject.toml")
        || root_has("requirements.txt")
        || root_has("setup.py")
        || root_has("Pipfile")
    {
        stack.push("Python".into());
    }
    if root_has("go.mod") {
        stack.push("Go".into());
    }
    if root_has("pom.xml") {
        stack.push("Java (Maven)".into());
    }
    if root_has("build.gradle") || root_has("build.gradle.kts") {
        stack.push("JVM (Gradle)".into());
    }
    if root_has("Gemfile") {
        stack.push("Ruby".into());
    }
    if root_has("composer.json") {
        stack.push("PHP".into());
    }
    if root_has("pubspec.yaml") {
        stack.push("Dart / Flutter".into());
    }
    if any_ext("csproj") || any_ext("sln") {
        stack.push(".NET".into());
    }
    if root_has("CMakeLists.txt") {
        stack.push("C/C++ (CMake)".into());
    }
    if root_has("Dockerfile")
        || root_has("docker-compose.yml")
        || root_has("docker-compose.yaml")
    {
        stack.push("Docker".into());
    }

    stack.sort();
    stack.dedup();
    stack
}

fn top_dirs(root: &Path) -> Vec<String> {
    let Ok(read) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<String> = read
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !should_skip_dir(n))
        .collect();
    out.sort();
    out.truncate(40);
    out
}

fn entrypoint_paths(files: &BTreeMap<String, FileRecord>) -> Vec<String> {
    let mut out: Vec<String> = files
        .keys()
        .filter(|rel| {
            Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .map(|name| ENTRYPOINT_NAMES.contains(&name))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    out.sort_by_key(|rel| Path::new(rel).components().count());
    out.truncate(8);
    out
}

fn build_modules(files: &BTreeMap<String, FileRecord>) -> Vec<ModuleRecord> {
    let mut map: BTreeMap<String, ModuleRecord> = BTreeMap::new();
    for (rel, record) in files {
        let module = module_for(rel);
        let entry = map.entry(module.clone()).or_insert_with(|| ModuleRecord {
            path: module,
            files: 0,
            languages: Vec::new(),
            symbols: Vec::new(),
            depends_on: Vec::new(),
        });
        entry.files += 1;
        if !entry.languages.contains(&record.lang) {
            entry.languages.push(record.lang.clone());
        }
        for symbol in record.symbols.iter().take(6) {
            if entry.symbols.len() < 12 && !entry.symbols.contains(&symbol.name) {
                entry.symbols.push(symbol.name.clone());
            }
        }
    }

    // Segunda pasada: resolver dependencias sin mantener préstamos mutables sobre el mapa.
    let mut edges: Vec<(String, String)> = Vec::new();
    for (rel, record) in files {
        let from = module_for(rel);
        for import in &record.imports {
            if let Some(target) = resolve_module(rel, import, &map) {
                if target != from {
                    edges.push((from.clone(), target));
                }
            }
        }
    }
    for (from, target) in edges {
        if let Some(entry) = map.get_mut(&from) {
            if !entry.depends_on.contains(&target) {
                entry.depends_on.push(target);
            }
        }
    }

    let mut modules: Vec<ModuleRecord> = map.into_values().collect();
    modules.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.path.cmp(&b.path)));
    modules
}

fn resolve_module(from_rel: &str, import: &str, modules: &BTreeMap<String, ModuleRecord>) -> Option<String> {
    let import = import.trim().trim_matches('"').trim_matches('\'').trim_matches('`');
    if import.is_empty() {
        return None;
    }
    if import.starts_with('.') {
        let base = Path::new(from_rel).parent().unwrap_or(Path::new(""));
        let joined = normalize_path(&base.join(import));
        let candidate = module_for(&joined);
        if modules.contains_key(&candidate) {
            return Some(candidate);
        }
        return None;
    }
    let normalized = import.replace("::", "/").replace('.', "/");
    for module in modules.keys() {
        let tail = module.rsplit('/').next().unwrap_or(module);
        if tail == "(root)" {
            continue;
        }
        if normalized == *module
            || normalized.starts_with(&format!("{module}/"))
            || normalized.starts_with(&format!("{tail}/"))
            || normalized.contains(&format!("/{tail}/"))
            || normalized.ends_with(&format!("/{tail}"))
        {
            return Some(module.clone());
        }
    }
    None
}

fn normalize_path(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            _ => {}
        }
    }
    parts.join("/")
}

fn module_for(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= 1 {
        return "(root)".into();
    }
    let first = parts[0];
    const GROUPING: &[&str] = &["src", "lib", "app", "packages", "crates", "internal", "cmd", "pkg", "modules"];
    if GROUPING.contains(&first) && parts.len() >= 3 {
        format!("{}/{}", first, parts[1])
    } else {
        first.to_string()
    }
}

fn compact_module_summary(files: &BTreeMap<String, FileRecord>, limit: usize) -> Vec<String> {
    let modules = build_modules(files);
    modules
        .into_iter()
        .take(limit)
        .map(|module| {
            let symbols = if module.symbols.is_empty() {
                String::new()
            } else {
                format!(" ({})", module.symbols.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
            };
            format!("`{}` {} archivos{}", module.path, module.files, symbols)
        })
        .collect()
}

fn unique_modules(files: &BTreeMap<String, FileRecord>) -> usize {
    files.keys().map(|rel| module_for(rel)).collect::<BTreeSet<_>>().len()
}

fn sum_symbols(files: &BTreeMap<String, FileRecord>) -> usize {
    files.values().map(|record| record.symbols.len()).sum()
}

fn count_dirs(files: &BTreeMap<String, FileRecord>) -> usize {
    let mut dirs = BTreeSet::new();
    for rel in files.keys() {
        let mut parent = Path::new(rel).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            dirs.insert(path.to_string_lossy().to_string());
            parent = path.parent();
        }
    }
    dirs.len()
}

fn collect_files(
    root: &Path,
    brain_dir: &Path,
    cancel: &CancelFlag,
) -> Result<(Vec<FileMeta>, bool), String> {
    let mut metas = Vec::new();
    let mut truncated = false;
    walk(root, root, brain_dir, cancel, &mut metas, 0, &mut truncated)?;
    Ok((metas, truncated))
}

fn walk(
    root: &Path,
    dir: &Path,
    brain_dir: &Path,
    cancel: &CancelFlag,
    metas: &mut Vec<FileMeta>,
    depth: usize,
    truncated: &mut bool,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) || *truncated {
        return Ok(());
    }
    let read = fs::read_dir(dir)
        .map_err(|e| format!("No se pudo leer {}: {e}", dir.display()))?;
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();

        if file_type.is_dir() {
            if should_skip_dir(&name)
                || path == brain_dir
                || path.starts_with(brain_dir)
                || depth >= MAX_DEPTH
            {
                continue;
            }
            walk(root, &path, brain_dir, cancel, metas, depth + 1, truncated)?;
        } else if file_type.is_file() {
            if is_ignored_rel(&name, root, brain_dir) {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let rel = path_string(&rel);
            if rel.is_empty() {
                continue;
            }
            metas.push(FileMeta {
                rel,
                size: metadata.len(),
                mtime_unix: mtime_unix(&metadata),
                text: is_text_like(&name),
            });
            if metas.len() >= MAX_FILES {
                *truncated = true;
                return Ok(());
            }
        }
    }
    Ok(())
}

fn should_skip_dir(name: &str) -> bool {
    SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(name)) || name.starts_with("brain_")
}

fn is_ignored_rel(rel: &str, root: &Path, brain_dir: &Path) -> bool {
    let normalized = rel.replace('\\', "/");
    if normalized.is_empty() || normalized == ".DS_Store" {
        return true;
    }
    if let Ok(brain_rel) = brain_dir.strip_prefix(root) {
        let brain_rel = path_string(brain_rel);
        if !brain_rel.is_empty()
            && (normalized == brain_rel
                || normalized.starts_with(&format!("{brain_rel}/")))
        {
            return true;
        }
    }
    normalized
        .split('/')
        .any(|component| should_skip_dir(component) || component == ".DS_Store")
}

fn is_text_like(name: &str) -> bool {
    let file_name = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    if file_name.starts_with(".env") {
        return true;
    }
    if matches!(
        file_name,
        "Makefile"
            | "Dockerfile"
            | "Gemfile"
            | "Rakefile"
            | "Procfile"
            | "LICENSE"
            | "CONTRIBUTING"
            | "CHANGELOG"
            | "CODEOWNERS"
            | "CMakeLists.txt"
    ) {
        return true;
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    const TEXT: &[&str] = &[
        "rs", "toml", "json", "jsonc", "yaml", "yml", "md", "txt", "js", "jsx", "ts", "tsx",
        "mjs", "cjs", "py", "go", "java", "kt", "kts", "c", "h", "cpp", "hpp", "cc", "cs", "rb",
        "php", "dart", "swift", "m", "mm", "sh", "bash", "zsh", "fish", "ps1", "sql", "graphql",
        "gql", "html", "htm", "css", "scss", "sass", "less", "vue", "svelte", "astro", "xml",
        "gradle", "properties", "env", "cfg", "ini", "conf", "gitignore", "dockerignore",
        "editorconfig", "nix", "ex", "exs", "erl", "hrl", "clj", "cljs", "lua", "r", "pl", "pm",
        "scala", "sol", "proto", "tf", "tfvars", "hcl", "csv", "tsv",
    ];
    TEXT.contains(&ext.as_str())
}

fn is_code_like(name: &str) -> bool {
    let file_name = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase();
    if file_name.ends_with(".min.js") || file_name.ends_with(".min.css") {
        return false;
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "rs" | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cc"
            | "cs"
            | "rb"
            | "php"
            | "dart"
            | "swift"
            | "vue"
            | "svelte"
    )
}

fn lang_for(name: &str) -> &'static str {
    let file_name = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase();
    if file_name == "dockerfile" {
        return "dockerfile";
    }
    if file_name == "makefile" {
        return "makefile";
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "toml" => "toml",
        "json" => "json",
        "yml" | "yaml" => "yaml",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "dart" => "dart",
        "swift" => "swift",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "vue" => "vue",
        "svelte" => "svelte",
        "xml" => "xml",
        "md" => "markdown",
        _ => "text",
    }
}

fn render_docs(
    root: &Path,
    brain_dir: &Path,
    state: &BrainState,
    stack: &[String],
    modules: &[ModuleRecord],
    changed: &BTreeSet<String>,
    full: bool,
) -> Result<(), String> {
    let now = state.scanned_at_unix;
    write_doc_if_changed(
        brain_dir,
        "00_overview.md",
        &render_overview(state, root, brain_dir, stack, modules, now),
    )?;
    write_doc_if_changed(
        brain_dir,
        "01_structure.md",
        &render_structure(&project::project_name(root), state),
    )?;
    write_doc_if_changed(
        brain_dir,
        "05_files_index.md",
        &render_file_index(state),
    )?;
    write_doc_if_changed(
        brain_dir,
        "08_codebase_map.md",
        &render_codebase_map(state, modules),
    )?;
    write_doc_if_changed(
        brain_dir,
        BRAIN_INDEX,
        &render_index(state, root, brain_dir, stack, modules, now),
    )?;

    if full || changed_any(changed, |rel| is_root_manifest(rel)) {
        write_doc_if_changed(brain_dir, "02_stack_and_dependencies.md", &render_stack(root))?;
    }
    if full || changed_any(changed, is_entrypoint_rel) {
        write_doc_if_changed(brain_dir, "03_entrypoints.md", &render_entrypoints(root, state))?;
    }
    if full || changed_any(changed, |rel| config_rel(rel)) {
        write_doc_if_changed(brain_dir, "04_config_and_env.md", &render_configs(root))?;
    }
    if full || changed_any(changed, |rel| readme_rel(rel)) {
        write_doc_if_changed(brain_dir, "07_readme.md", &render_readme(root))?;
    }
    if full {
        write_doc_if_changed(brain_dir, "06_extra_context.md", &render_extra_context(brain_dir)?)?;
    }
    sync_module_docs(brain_dir, &state.files, modules, changed, full)
}

fn refresh_git_docs(
    root: &Path,
    brain_dir: &Path,
    state: &BrainState,
    stack: &[String],
    modules: &[ModuleRecord],
) -> Result<(), String> {
    let now = state.scanned_at_unix;
    write_doc_if_changed(
        brain_dir,
        "00_overview.md",
        &render_overview(state, root, brain_dir, stack, modules, now),
    )?;
    write_doc_if_changed(
        brain_dir,
        BRAIN_INDEX,
        &render_index(state, root, brain_dir, stack, modules, now),
    )?;
    Ok(())
}

fn sync_module_docs(
    brain_dir: &Path,
    files: &BTreeMap<String, FileRecord>,
    modules: &[ModuleRecord],
    changed: &BTreeSet<String>,
    full: bool,
) -> Result<(), String> {
    let dir = brain_dir.join("modules");
    fs::create_dir_all(&dir)
        .map_err(|err| format!("No se pudo crear la carpeta de módulos: {err}"))?;
    let touched: BTreeSet<String> = changed.iter().map(|rel| module_for(rel)).collect();
    let live: BTreeSet<&str> = modules.iter().map(|module| module.path.as_str()).collect();
    if full {
        if let Ok(read) = fs::read_dir(&dir) {
            for entry in read.flatten() {
                if entry.path().is_file() {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
    for module in modules {
        let dependency_touched = module.depends_on.iter().any(|dep| touched.contains(dep));
        if full || touched.contains(&module.path) || dependency_touched {
            write_doc_if_changed(
                brain_dir,
                &module_doc_name(&module.path),
                &render_module_doc(module, files),
            )?;
        }
    }
    if !full {
        for module_path in &touched {
            if !live.contains(module_path.as_str()) {
                let _ = fs::remove_file(brain_dir.join(module_doc_name(module_path)));
            }
        }
    }
    Ok(())
}

fn module_doc_name(module_path: &str) -> String {
    let safe: String = module_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() {
        "root".to_string()
    } else {
        safe
    };
    format!("modules/{safe}.md")
}

fn render_module_doc(module: &ModuleRecord, files: &BTreeMap<String, FileRecord>) -> String {
    let mut s = format!("# Módulo `{}`\n\n", module.path);
    s.push_str(
        "Detalle del módulo. Es un índice, no el comportamiento vigente: antes de modificarlo hay que leer sus archivos.\n\n",
    );
    s.push_str(&format!("- Archivos: {}\n", module.files));
    s.push_str(&format!("- Lenguajes: {}\n", module.languages.join(", ")));
    if !module.depends_on.is_empty() {
        s.push_str(&format!("- Depende de: {}\n", module.depends_on.join(", ")));
    }
    s.push_str("\n## Archivos\n\n");
    for (rel, record) in files {
        if module_for(rel) != module.path {
            continue;
        }
        s.push_str(&format!("### `{rel}`\n\n"));
        if record.symbols.is_empty() {
            s.push_str("- Sin símbolos detectados.\n\n");
            continue;
        }
        for symbol in &record.symbols {
            s.push_str(&format!(
                "- {} `{}` (línea {})\n",
                symbol.kind, symbol.name, symbol.line
            ));
            if !symbol.signature.is_empty() {
                s.push_str(&format!("  - `{}`\n", symbol.signature));
            }
            if !symbol.doc.is_empty() {
                s.push_str(&format!("  - {}\n", symbol.doc));
            }
        }
        if !record.imports.is_empty() {
            let imports = record
                .imports
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            s.push_str(&format!("\nImports: {imports}\n"));
        }
        s.push('\n');
    }
    s
}

fn ensure_docs_exist(
    root: &Path,
    brain_dir: &Path,
    state: &BrainState,
    stack: &[String],
    modules: &[ModuleRecord],
) -> Result<(), String> {
    let docs = [
        "00_overview.md",
        "01_structure.md",
        "02_stack_and_dependencies.md",
        "03_entrypoints.md",
        "04_config_and_env.md",
        "05_files_index.md",
        "06_extra_context.md",
        "07_readme.md",
        "08_codebase_map.md",
        BRAIN_INDEX,
    ];
    let missing: Vec<&str> = docs
        .iter()
        .copied()
        .filter(|name| !brain_dir.join(name).exists())
        .collect();
    if missing.is_empty() && brain_dir.join("modules").is_dir() {
        return Ok(());
    }
    render_docs(root, brain_dir, state, stack, modules, &BTreeSet::new(), true)
}

fn changed_any<F: Fn(&str) -> bool>(changed: &BTreeSet<String>, predicate: F) -> bool {
    changed.iter().any(|rel| predicate(rel))
}

fn is_root_manifest(rel: &str) -> bool {
    if rel.contains('/') {
        return false;
    }
    MANIFESTS.contains(&rel)
}

fn is_entrypoint_rel(rel: &str) -> bool {
    Path::new(rel)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|name| ENTRYPOINT_NAMES.contains(&name))
        .unwrap_or(false)
}

fn config_rel(rel: &str) -> bool {
    let name = Path::new(rel)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(rel);
    is_config_name(name) || rel.starts_with(".github/workflows/")
}

fn readme_rel(rel: &str) -> bool {
    !rel.contains('/') && README_NAMES.contains(&rel)
}

fn render_overview(
    state: &BrainState,
    root: &Path,
    brain_dir: &Path,
    stack: &[String],
    modules: &[ModuleRecord],
    now: u64,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {}\n\n", project::project_name(root)));
    s.push_str(
        "Documentación general. El comportamiento vigente está en el código: este overview no se usa como sustituto. El índice (`.brain_state.json`) se actualiza de forma incremental y queda sincronizado con Git; una consulta no reconstruye toda la documentación.\n\n",
    );
    s.push_str("## Datos del índice\n\n");
    s.push_str(&format!("- **Raíz:** `{}`\n", root.display()));
    s.push_str(&format!("- **Brain:** `{}`\n", brain_dir.display()));
    s.push_str(&format!("- **Última actualización:** {}\n", format_timestamp(now)));
    s.push_str(&format!("- **Archivos:** {}\n", state.files.len()));
    s.push_str(&format!("- **Carpetas:** {}\n", state.dirs));
    s.push_str(&format!("- **Tamaño:** {}\n", human_bytes(state.total_bytes)));
    s.push_str(&format!(
        "- **Símbolos:** {} · **Módulos:** {}\n",
        sum_symbols(&state.files),
        unique_modules(&state.files)
    ));
    if state.truncated {
        s.push_str("- **Aviso:** índice truncado por cantidad de archivos.\n");
    }

    s.push_str("\n## Estado Git\n\n");
    s.push_str(&format_git_block(&state.git));

    s.push_str("\n## Stack detectado\n\n");
    if stack.is_empty() {
        s.push_str("- No se detectó un stack conocido.\n");
    } else {
        for item in stack {
            s.push_str(&format!("- {item}\n"));
        }
    }

    s.push_str("\n## Carpetas de primer nivel\n\n");
    let tops = top_dirs(root);
    if tops.is_empty() {
        s.push_str("- (sin subcarpetas)\n");
    } else {
        for dir in tops {
            s.push_str(&format!("- `{dir}/`\n"));
        }
    }

    s.push_str("\n## Entrypoints candidatos\n\n");
    let entrypoints = entrypoint_paths(&state.files);
    if entrypoints.is_empty() {
        s.push_str("- No detectados automáticamente.\n");
    } else {
        for file in entrypoints {
            s.push_str(&format!("- `{file}`\n"));
        }
    }

    s.push_str("\n## Módulos principales\n\n");
    for module in modules.iter().take(15) {
        s.push_str(&format!(
            "- `{}` — {} archivos ({})\n",
            module.path,
            module.files,
            module.languages.join(", ")
        ));
        if !module.symbols.is_empty() {
            s.push_str(&format!(
                "  - Símbolos: {}\n",
                module.symbols.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !module.depends_on.is_empty() {
            s.push_str(&format!(
                "  - Depende de: {}\n",
                module.depends_on.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }

    s.push_str("\n## Documentos del brain\n\n");
    for (file, description) in doc_catalog() {
        s.push_str(&format!("- `{file}` — {description}\n"));
    }
    s
}

fn render_structure(root_name: &str, state: &BrainState) -> String {
    let mut lines = BTreeSet::new();
    for rel in state.files.keys() {
        let path = Path::new(rel);
        lines.insert(rel.clone());
        let mut parent = path.parent();
        while let Some(value) = parent {
            if value.as_os_str().is_empty() {
                break;
            }
            let dir = value.to_string_lossy().to_string();
            if !dir.is_empty() {
                lines.insert(format!("{dir}/"));
            }
            parent = value.parent();
        }
    }
    let lines: Vec<String> = lines.into_iter().take(2_000).collect();
    let tree = if lines.is_empty() {
        "(sin archivos)".to_string()
    } else {
        lines.join("\n")
    };
    format!("# Estructura de {root_name}\n\n```text\n{tree}\n```\n")
}

fn render_codebase_map(state: &BrainState, modules: &[ModuleRecord]) -> String {
    let mut s = String::from("# Codebase map\n\n");
    s.push_str("Mapa heurístico de módulos, símbolos y dependencias internas. Sirve como índice, no como fuente de verdad.\n\n");
    s.push_str(&format!("- Archivos: {}\n- Símbolos: {}\n- Módulos: {}\n\n", state.files.len(), sum_symbols(&state.files), modules.len()));

    s.push_str("## Módulos\n\n");
    let mut incoming: BTreeMap<String, usize> = BTreeMap::new();
    for module in modules {
        *incoming.entry(module.path.clone()).or_insert(0) += 0;
        for dependency in &module.depends_on {
            *incoming.entry(dependency.clone()).or_insert(0) += 1;
        }
    }
    for module in modules.iter().take(50) {
        s.push_str(&format!(
            "### `{}`\n\n- Archivos: {} · Lenguajes: {}\n",
            module.path,
            module.files,
            module.languages.join(", ")
        ));
        if !module.symbols.is_empty() {
            s.push_str(&format!("- Símbolos clave: {}\n", module.symbols.join(", ")));
        }
        if !module.depends_on.is_empty() {
            s.push_str(&format!("- Depende de: {}\n", module.depends_on.join(", ")));
        }
        s.push('\n');
    }
    if modules.len() > 50 {
        s.push_str(&format!("… y {} módulos más.\n\n", modules.len() - 50));
    }

    s.push_str("## Hotspots (más referenciados)\n\n");
    let mut hotspots: Vec<(String, usize)> = incoming.into_iter().collect();
    hotspots.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (module, count) in hotspots.into_iter().take(10) {
        s.push_str(&format!("- `{module}` — {count} referencias entrantes\n"));
    }

    s.push_str("\n## Entrypoints\n\n");
    let entrypoints = entrypoint_paths(&state.files);
    if entrypoints.is_empty() {
        s.push_str("- No detectados.\n");
    } else {
        for file in entrypoints {
            s.push_str(&format!("- `{file}`\n"));
        }
    }
    s
}

fn render_stack(root: &Path) -> String {
    let mut s = String::from("# Stack y dependencias\n\n");
    s.push_str("> Se regenera sólo cuando cambian los manifiestos de la raíz.\n\n");
    let mut found = false;
    for name in MANIFESTS {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        let Some(content) = read_text(&path, 8 * 1024) else {
            continue;
        };
        found = true;
        s.push_str(&format!(
            "## `{name}`\n\n```{}\n{}\n```\n\n",
            fence_lang(name),
            truncate_lines(&content, 200).trim_end()
        ));
    }
    if !found {
        s.push_str("No se encontraron manifiestos conocidos en la raíz.\n");
    }
    s
}

fn render_entrypoints(root: &Path, state: &BrainState) -> String {
    let mut s = String::from("# Entrypoints\n\n");
    let entrypoints = entrypoint_paths(&state.files);
    if entrypoints.is_empty() {
        s.push_str("No se detectaron entrypoints automáticamente.\n");
        return s;
    }
    for rel in entrypoints {
        let path = root.join(&rel);
        let Some(content) = read_text(&path, MAX_READ_BYTES) else {
            continue;
        };
        s.push_str(&format!(
            "## `{}`\n\n```{}\n{}\n```\n\n",
            rel,
            fence_lang(&rel),
            truncate_lines(&content, 120).trim_end()
        ));
    }
    s
}

fn render_configs(root: &Path) -> String {
    let mut s = String::from("# Configuración y entorno\n\n");
    s.push_str("> Los valores sensibles (claves, tokens, contraseñas) se muestran redactados.\n\n");

    let mut rels: Vec<PathBuf> = Vec::new();
    if let Ok(read) = fs::read_dir(root) {
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                if is_config_name(name) {
                    rels.push(PathBuf::from(name));
                }
            }
        }
    }
    let workflows = root.join(".github").join("workflows");
    if let Ok(read) = fs::read_dir(&workflows) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    rels.push(rel.to_path_buf());
                }
            }
        }
    }
    rels.sort();
    rels.dedup();

    let mut found = false;
    for rel in rels {
        let path = root.join(&rel);
        let Some(content) = read_text(&path, 8 * 1024) else {
            continue;
        };
        let name = rel.to_string_lossy();
        found = true;
        s.push_str(&format!(
            "## `{}`\n\n```{}\n{}\n```\n\n",
            name,
            fence_lang(&name),
            redact(&content).trim_end()
        ));
    }
    if !found {
        s.push_str("No se encontraron archivos de configuración en la raíz.\n");
    }
    s
}

fn render_file_index(state: &BrainState) -> String {
    let mut s = String::from("# Inventario de archivos\n\n");
    s.push_str(&format!("Total: {} archivos.\n\n", state.files.len()));
    for (rel, file) in state.files.iter().take(MAX_FILE_INDEX) {
        let marker = if file.text { "" } else { " *(binario)*" };
        let symbols = if file.symbols.is_empty() {
            String::new()
        } else {
            format!(" · {} símbolos", file.symbols.len())
        };
        s.push_str(&format!(
            "- `{rel}` — {}{}{}\n",
            human_bytes(file.size),
            marker,
            symbols
        ));
    }
    if state.files.len() > MAX_FILE_INDEX {
        s.push_str(&format!(
            "\n… y {} archivos más.\n",
            state.files.len() - MAX_FILE_INDEX
        ));
    }
    s
}

fn render_extra_context(brain_dir: &Path) -> Result<String, String> {
    let context_dir = brain_dir.join("context");
    let mut s = String::from("# Archivos extra aportados por el usuario\n\n");
    if !context_dir.is_dir() {
        s.push_str("Todavía no se agregaron archivos.\n");
        return Ok(s);
    }
    let read = fs::read_dir(&context_dir)
        .map_err(|e| format!("No se pudo leer la carpeta context: {e}"))?;
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    let mut included = 0usize;
    for entry in entries {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        s.push_str(&format!("## `{name}` ({})\n\n", human_bytes(size)));
        if included >= MAX_EXTRA_CONTENT {
            s.push_str("_(contenido omitido por tamaño del brain)_.\n\n");
            continue;
        }
        if is_text_like(&name) {
            if let Some(content) = read_text(&path, 48 * 1024) {
                let content = truncate_lines(&content, 400);
                included += content.len();
                s.push_str(&format!(
                    "```{}\n{}\n```\n\n",
                    fence_lang(&name),
                    content.trim_end()
                ));
                continue;
            }
        }
        s.push_str("_(archivo binario; sólo se guardó una copia)_.\n\n");
    }
    Ok(s)
}

fn render_readme(root: &Path) -> String {
    for name in README_NAMES {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        if let Some(content) = read_text(&path, 64 * 1024) {
            return format!(
                "# README\n\n```markdown\n{}\n```\n",
                truncate_lines(&content, 200).trim_end()
            );
        }
    }
    "# README\n\nNo se encontró README en la raíz.\n".into()
}

fn render_index(
    state: &BrainState,
    root: &Path,
    brain_dir: &Path,
    stack: &[String],
    modules: &[ModuleRecord],
    now: u64,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Brain — {}\n\n", state.root));
    s.push_str(
        "Índice incremental sincronizado con Git. El overview es documentación general. El detalle de un módulo está en `modules/` y el código del repositorio manda sobre cualquiera de estos textos.\n\n",
    );
    s.push_str(&format!(
        "- **Raíz:** `{}`\n- **Brain:** `{}`\n- **Actualizado:** {}\n- **Archivos:** {} · **Carpetas:** {} · **Tamaño:** {}\n",
        root.display(),
        brain_dir.display(),
        format_timestamp(now),
        state.files.len(),
        state.dirs,
        human_bytes(state.total_bytes)
    ));
    s.push_str(&format!(
        "- **Símbolos:** {} · **Módulos:** {}\n\n",
        sum_symbols(&state.files),
        unique_modules(&state.files)
    ));

    s.push_str("## Git\n\n");
    s.push_str(&format_git_block(&state.git));

    s.push_str("\n## Stack\n\n");
    if stack.is_empty() {
        s.push_str("- No detectado\n");
    } else {
        for item in stack {
            s.push_str(&format!("- {item}\n"));
        }
    }

    s.push_str("\n## Entrypoints\n\n");
    let entrypoints = entrypoint_paths(&state.files);
    if entrypoints.is_empty() {
        s.push_str("- No detectados\n");
    } else {
        for file in entrypoints {
            s.push_str(&format!("- `{file}`\n"));
        }
    }

    s.push_str("\n## Módulos\n\n");
    for module in modules.iter().take(20) {
        s.push_str(&format!(
            "- `{}` — {} archivos\n",
            module.path, module.files
        ));
    }

    s.push_str("\n## Documentos del brain\n\n");
    for (file, description) in doc_catalog() {
        s.push_str(&format!("- `{file}` — {description}\n"));
    }

    s.push_str("\n## Reglas para el asistente\n\n");
    s.push_str("- El código actual es la fuente de verdad; el Brain es un índice incremental sincronizado con Git.\n");
    s.push_str("- Nunca asumas que una descripción del Brain representa el comportamiento actual del código.\n");
    s.push_str("- El overview es documentación general. El detalle se carga por módulo, sólo si hace falta.\n");
    s.push_str("- Antes de modificar un módulo, inspeccioná sus archivos y dependencias en el repositorio.\n");
    s.push_str("- Si falta un dato, pedilo.\n");
    s
}

fn doc_catalog() -> [(&'static str, &'static str); 11] {
    [
        ("00_overview.md", "documentación general: Git, stack y módulos"),
        ("01_structure.md", "carpetas y archivos"),
        ("02_stack_and_dependencies.md", "manifiestos y dependencias"),
        ("03_entrypoints.md", "archivos de entrada con extractos"),
        ("04_config_and_env.md", "configuración, secretos redactados"),
        ("05_files_index.md", "inventario de archivos con símbolos"),
        ("06_extra_context.md", "archivos aportados por el usuario"),
        ("07_readme.md", "extracto del README"),
        ("08_codebase_map.md", "mapa de módulos y dependencias"),
        ("modules/", "detalle por módulo; se reescribe sólo si ese módulo cambió"),
        (BRAIN_INDEX, "índice de nivel 1"),
    ]
}

pub fn ingest_files(brain_dir: &Path, files: &[PathBuf]) -> Result<usize, String> {
    let context_dir = brain_dir.join("context");
    fs::create_dir_all(&context_dir)
        .map_err(|e| format!("No se pudo crear la carpeta context del brain: {e}"))?;

    let mut count = 0usize;
    for source in files {
        let metadata = match fs::metadata(source) {
            Ok(m) => m,
            Err(e) => return Err(format!("No se pudo leer {}: {e}", source.display())),
        };
        if !metadata.is_file() {
            continue;
        }
        let base = source
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archivo");
        let target = unique_target(&context_dir, base);
        fs::copy(source, &target)
            .map_err(|e| format!("No se pudo copiar {}: {e}", source.display()))?;
        count += 1;
    }

    let document = render_extra_context(brain_dir)?;
    write_doc_if_changed(brain_dir, "06_extra_context.md", &document)?;
    Ok(count)
}

/// Copia docs precargados del Brain origen al `context/` del destino.
/// No llama a ninguna IA: sólo mueve markdown local para no quemar tokens.
pub fn transfer_context(
    from_name: &str,
    from_brain: &Path,
    to_brain: &Path,
) -> Result<usize, String> {
    if from_brain == to_brain {
        return Err("Origen y destino son el mismo brain.".into());
    }
    if !from_brain.is_dir() {
        return Err(format!(
            "El brain de origen no existe: {}",
            from_brain.display()
        ));
    }
    fs::create_dir_all(to_brain)
        .map_err(|e| format!("No se pudo crear el brain destino: {e}"))?;

    let context_dir = to_brain.join("context");
    fs::create_dir_all(&context_dir)
        .map_err(|e| format!("No se pudo crear la carpeta context del brain: {e}"))?;

    let prefix = format!("from_{}_", sanitize_transfer_name(from_name));
    let docs = [
        "00_overview.md",
        BRAIN_INDEX,
        "08_codebase_map.md",
        "07_readme.md",
    ];
    let mut count = 0usize;
    for name in docs {
        let source = from_brain.join(name);
        if !source.is_file() {
            continue;
        }
        let base = format!("{prefix}{name}");
        let target = unique_target(&context_dir, &base);
        fs::copy(&source, &target)
            .map_err(|e| format!("No se pudo copiar {}: {e}", source.display()))?;
        count += 1;
    }

    if let Some(summary) = render_settled_transfer(from_name, from_brain) {
        let base = format!("{prefix}settled_cards.md");
        let target = unique_target(&context_dir, &base);
        fs::write(&target, summary)
            .map_err(|e| format!("No se pudo escribir las fichas transferidas: {e}"))?;
        count += 1;
    }

    if count == 0 {
        return Err(format!(
            "El brain de `{from_name}` no tiene documentos para transferir. Escanealo primero."
        ));
    }

    let document = render_extra_context(to_brain)?;
    write_doc_if_changed(to_brain, "06_extra_context.md", &document)?;
    Ok(count)
}

fn sanitize_transfer_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "proyecto".into()
    } else {
        trimmed.chars().take(40).collect()
    }
}

fn render_settled_transfer(from_name: &str, from_brain: &Path) -> Option<String> {
    let state = load_state(from_brain)?;
    if state.settled.is_empty() {
        return None;
    }
    let mut s = format!(
        "# Fichas asentadas transferidas desde `{from_name}`\n\n\
         Copia local del conocimiento ya asentado. No consume tokens. \
         El código del proyecto destino manda sobre estas notas.\n\n"
    );
    let mut cards: Vec<_> = state.settled.iter().collect();
    cards.sort_by(|a, b| b.1.hits.cmp(&a.1.hits).then_with(|| a.0.cmp(b.0)));
    for (module, card) in cards.into_iter().take(24) {
        s.push_str(&format!("## `{module}` (visto {} veces)\n\n", card.hits));
        for line in card.lines.iter().take(12) {
            s.push_str(line);
            s.push('\n');
        }
        s.push('\n');
    }
    Some(truncate_bytes(&s, 12 * 1024))
}

fn read_extra_context(brain_dir: &Path) -> Option<String> {
    let content = fs::read_to_string(brain_dir.join("06_extra_context.md")).ok()?;
    if content.trim().is_empty() || content.contains("Todavía no se agregaron archivos") {
        return None;
    }
    Some(truncate_bytes(&content, 6 * 1024))
}

fn ensure_brain_dir(root: &Path, brain_dir: &Path) -> Result<(), String> {
    if brain_dir == root || root.starts_with(brain_dir) {
        return Err(
            "El brain no puede ser la raíz del proyecto ni una carpeta que la contenga.".into(),
        );
    }
    fs::create_dir_all(brain_dir)
        .map_err(|e| format!("No se pudo crear el brain: {e}"))?;
    if brain_dir != root && brain_dir.starts_with(root) {
        let ignore = brain_dir.join(".gitignore");
        if !ignore.exists() {
            let _ = fs::write(ignore, "*\n");
        }
    }
    Ok(())
}

fn load_state(brain_dir: &Path) -> Option<BrainState> {
    let raw = fs::read_to_string(brain_dir.join(BRAIN_STATE)).ok()?;
    let state = serde_json::from_str::<BrainState>(&raw).ok()?;
    Some(state)
}

fn save_state(brain_dir: &Path, state: &BrainState) -> Result<(), String> {
    let path = brain_dir.join(BRAIN_STATE);
    let tmp = brain_dir.join(".brain_state.json.tmp");
    let json = serde_json::to_string(state)
        .map_err(|e| format!("No se pudo serializar el índice: {e}"))?;
    fs::write(&tmp, json).map_err(|e| format!("No se pudo escribir el índice: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("No se pudo guardar el índice: {e}"))?;
    Ok(())
}

fn inspect_git(root: &Path) -> GitState {
    let now = now_unix();
    let branch = match run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Some(value) => value.trim().to_string(),
        None => {
            return GitState {
                available: false,
                captured_at_unix: now,
                error: Some("git no disponible o no es un repositorio".into()),
                ..GitState::default()
            };
        }
    };
    let head = run_git(root, &["rev-parse", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let status = run_git(
        root,
        &[
            "-c",
            "core.quotepath=false",
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
        ],
    );
    let mut entries = Vec::new();
    let mut truncated = false;
    if let Some(status) = status {
        for line in status.lines() {
            if line.len() < 4 {
                continue;
            }
            if entries.len() >= MAX_GIT_ENTRIES {
                truncated = true;
                break;
            }
            let code = line[..2].to_string();
            let mut path = line[3..].to_string();
            if let Some((_, new_path)) = path.rsplit_once(" -> ") {
                path = new_path.to_string();
            }
            let untracked = code.starts_with("??");
            entries.push(GitEntry {
                status: code,
                path,
                untracked,
            });
        }
    }
    GitState {
        available: true,
        branch,
        head_short: short_head(&head),
        head,
        dirty: !entries.is_empty(),
        entries,
        truncated,
        captured_at_unix: now,
        error: None,
    }
}

fn filter_git(root: &Path, brain_dir: &Path, mut git: GitState) -> GitState {
    git.entries
        .retain(|entry| !is_ignored_rel(&entry.path, root, brain_dir));
    git.dirty = !git.entries.is_empty();
    git
}

fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathChange {
    Upsert,
    Remove,
}

fn should_reread(same_meta: bool, was_dirty: bool, still_dirty: bool, in_diff: bool) -> bool {
    if !same_meta {
        return true;
    }
    if was_dirty && still_dirty {
        return false;
    }
    if in_diff && was_dirty && !still_dirty {
        return false;
    }
    if !was_dirty && !still_dirty && !in_diff {
        return false;
    }
    true
}

fn same_analysis(old: &FileRecord, new: &FileRecord) -> bool {
    old.size == new.size
        && old.text == new.text
        && old.lang == new.lang
        && old.untracked == new.untracked
        && old.symbols == new.symbols
        && old.imports == new.imports
}

fn git_diff_name_status(root: &Path, from: &str, to: &str) -> Result<Vec<(String, PathChange)>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.quotepath=false",
            "diff",
            "--name-status",
            "-M",
            from,
            to,
        ])
        .output()
        .map_err(|err| format!("git diff: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "git diff no pudo comparar los commits".into()
        } else {
            stderr
        });
    }
    Ok(parse_name_status(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_name_status(text: &str) -> Vec<(String, PathChange)> {
    let mut changes = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        let destination = parts.next().map(str::to_string);
        match status.chars().next().unwrap_or('M') {
            'D' => changes.push((path.to_string(), PathChange::Remove)),
            'R' | 'C' => {
                changes.push((path.to_string(), PathChange::Remove));
                if let Some(destination) = destination {
                    changes.push((destination, PathChange::Upsert));
                }
            }
            _ => changes.push((path.to_string(), PathChange::Upsert)),
        }
    }
    changes
}

fn unique_target(dir: &Path, base: &str) -> PathBuf {
    let mut target = dir.join(base);
    if !target.exists() {
        return target;
    }
    let path = Path::new(base);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("archivo");
    let extension = path.extension().and_then(|s| s.to_str());
    for index in 1..1000 {
        let candidate = match extension {
            Some(ext) => format!("{stem}-{index}.{ext}"),
            None => format!("{stem}-{index}"),
        };
        target = dir.join(candidate);
        if !target.exists() {
            return target;
        }
    }
    target
}

fn is_config_name(name: &str) -> bool {
    name.starts_with(".env")
        || matches!(
            name,
            "Dockerfile"
                | "Makefile"
                | "docker-compose.yml"
                | "docker-compose.yaml"
                | "tsconfig.json"
                | ".gitignore"
                | ".dockerignore"
                | ".editorconfig"
                | "rust-toolchain.toml"
                | ".npmrc"
                | ".nvmrc"
                | ".python-version"
                | "vite.config.ts"
                | "vite.config.js"
                | "next.config.js"
                | "next.config.mjs"
                | "next.config.ts"
                | "webpack.config.js"
                | "rollup.config.js"
                | "tailwind.config.js"
                | "tailwind.config.ts"
                | "eslint.config.js"
                | ".eslintrc.json"
                | "babel.config.js"
        )
        || name.ends_with(".config.js")
        || name.ends_with(".config.ts")
        || name.ends_with(".config.mjs")
}

fn redact(text: &str) -> String {
    const SENSITIVE: &[&str] = &[
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PASS",
        "PWD",
        "CREDENTIAL",
        "AUTH",
        "PRIVATE",
    ];
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim();
            let upper = key.to_ascii_uppercase();
            if SENSITIVE.iter().any(|p| upper.contains(p)) {
                out.push_str(&format!("{key}=***REDACTED***\n"));
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn read_text(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    let mut buffer = Vec::new();
    file.take(max_bytes).read_to_end(&mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer).to_string())
}

fn truncate_lines(text: &str, max_lines: usize) -> String {
    text.lines().take(max_lines).collect::<Vec<_>>().join("\n")
}

fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n_(contexto truncado por tamaño)_", &text[..end])
}

fn write_doc_if_changed(brain_dir: &Path, name: &str, content: &str) -> Result<bool, String> {
    let path = brain_dir.join(name);
    if let Ok(existing) = fs::read_to_string(&path) {
        if existing == content {
            return Ok(false);
        }
    }
    fs::write(&path, content).map_err(|e| format!("No se pudo escribir {name} en el brain: {e}"))?;
    Ok(true)
}

fn fence_lang(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.contains("dockerfile") {
        return "dockerfile";
    }
    if lower.ends_with("makefile") {
        return "makefile";
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "toml" => "toml",
        "json" => "json",
        "yml" | "yaml" => "yaml",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "dart" => "dart",
        "swift" => "swift",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "vue" => "vue",
        "svelte" => "svelte",
        "xml" => "xml",
        "md" => "markdown",
        _ => "text",
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn mtime_unix(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn short_head(head: &str) -> String {
    if head.is_empty() {
        "(sin commit)".into()
    } else {
        head.chars().take(8).collect()
    }
}

fn keywords(question: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "have", "what", "where", "when",
        "how", "why", "does", "can", "you", "are", "not", "pero", "para", "como", "donde",
        "cuando", "porque", "sobre", "este", "esta", "estos", "estas", "desde", "hacia", "entre",
        "tiene", "hacen", "hace", "mas", "más", "los", "las", "una", "uno", "del", "que", "con",
        "por", "sus", "era", "ser", "está", "esta", "todo", "toda", "todos", "todas", "algo",
        "cómo", "qué", "cuál", "cuáles",
    ];
    let mut out: Vec<String> = Vec::new();
    for raw in question.split(|c: char| {
        !(c.is_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '.' || c == ':')
    }) {
        let token = raw.trim_matches(|c: char| c == '.' || c == ':' || c == '-' || c == '/');
        if token.len() < 3 {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if STOP.contains(&lower.as_str()) {
            continue;
        }
        if !out.contains(&lower) {
            out.push(lower);
        }
        for part in split_identifier(token) {
            let part = part.to_ascii_lowercase();
            if part.len() >= 3 && !STOP.contains(&part.as_str()) && !out.contains(&part) {
                out.push(part);
            }
        }
        if out.len() >= 16 {
            break;
        }
    }
    out
}

fn split_identifier(token: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in token.chars() {
        if c == '_' || c == '-' || c == '.' || c == '/' || c == ':' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        current.push(c);
        previous_lower = c.is_lowercase();
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_timestamp(unix: u64) -> String {
    let seconds = unix as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (
        year + if month <= 2 { 1 } else { 0 },
        month as u32,
        day as u32,
    )
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut index = 0;
    while size >= 1024.0 && index < UNITS.len() - 1 {
        size /= 1024.0;
        index += 1;
    }
    format!("{size:.1} {}", UNITS[index])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn cancel() -> CancelFlag {
        std::sync::Arc::new(AtomicBool::new(false))
    }

    fn quiet(_: String) {}
    fn quiet_tick(_: ProgressTick) {}

    #[test]
    fn relee_si_el_blob_cambio_y_no_si_el_worktree_sigue_igual() {
        assert!(!should_reread(true, true, true, false));
        assert!(!should_reread(true, true, false, true));
        assert!(should_reread(true, true, false, false));
        assert!(should_reread(true, false, false, true));
        assert!(should_reread(false, true, true, false));
        assert!(!should_reread(true, false, false, false));
    }

    #[test]
    fn guarda_la_firma_y_el_comentario_tal_cual_estan_en_el_archivo() {
        let (symbols, _, doc) = analyze_code(
            "src/app/mod.rs",
            "//! Modulo de saludo.\n\n/// Devuelve el saludo del proyecto.\npub fn hello() -> i32 {\n    1\n}\n",
        );
        assert!(doc.contains("Modulo de saludo"), "{doc}");
        let hello = symbols.iter().find(|symbol| symbol.name == "hello").unwrap();
        assert!(hello.signature.contains("pub fn hello() -> i32"), "{}", hello.signature);
        assert!(hello.doc.contains("Devuelve el saludo del proyecto"), "{}", hello.doc);
    }

    #[test]
    fn la_ficha_se_afina_con_las_consultas_y_cae_si_el_archivo_cambia() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("yeison-learn-{stamp}"));
        let brain = root.join("brain_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/app")).unwrap();
        let app = root.join("src/app/mod.rs");
        fs::write(
            &app,
            "/// Devuelve el saludo del proyecto.\npub fn hello() -> i32 {\n    1\n}\n",
        )
        .unwrap();

        let (first, _) = compose_prompt_live("demo", &root, &brain, "qué hace hello", &cancel(), quiet_tick);
        assert!(first.contains("CONOCIMIENTO ASENTADO"), "{first}");
        assert!(first.contains("Devuelve el saludo del proyecto"), "{first}");
        assert!(first.contains("pub fn hello() -> i32"), "{first}");

        let (second, _) = compose_prompt_live("demo", &root, &brain, "hello de nuevo", &cancel(), quiet_tick);
        assert!(second.contains("visto 2 veces"), "{second}");
        assert_eq!(load_state(&brain).unwrap().recall.consultations, 2);

        fs::write(
            &app,
            "/// Ahora cuenta visitas.\npub fn hello() -> i32 {\n    40\n}\n\npub fn visits() -> i32 {\n    0\n}\n",
        )
        .unwrap();
        let (third, _) = compose_prompt_live("demo", &root, &brain, "qué hace hello", &cancel(), quiet_tick);
        assert!(third.contains("Ahora cuenta visitas"), "{third}");
        assert!(third.contains("fn visits"), "{third}");
        assert!(!third.contains("Devuelve el saludo del proyecto"), "{third}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn el_diff_de_git_distingue_borrados_y_renombres() {
        let changes = parse_name_status("M\tsrc/app/mod.rs\nD\tsrc/old.rs\nR100\tsrc/a.rs\tsrc/b.rs\n");
        assert_eq!(
            changes,
            vec![
                ("src/app/mod.rs".into(), PathChange::Upsert),
                ("src/old.rs".into(), PathChange::Remove),
                ("src/a.rs".into(), PathChange::Remove),
                ("src/b.rs".into(), PathChange::Upsert),
            ]
        );
    }

    #[test]
    fn una_pregunta_no_rearma_el_indice_y_modificar_lee_el_codigo() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("yeison-brain-{stamp}"));
        let brain = root.join("brain_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/app")).unwrap();
        fs::create_dir_all(root.join("src/util")).unwrap();
        let app = root.join("src/app/mod.rs");
        let util = root.join("src/util/mod.rs");
        fs::write(&app, "pub fn hello() -> i32 {\n    1\n}\n").unwrap();
        fs::write(&util, "pub fn helper() -> i32 {\n    2\n}\n").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["add", "src", "Cargo.toml"]);
        git_commit(&root, "inicial");

        let first = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        match first {
            SyncOutcome::Updated(_, report) => assert_eq!(report.mode, SyncMode::Full),
            other => panic!("se esperaba el índice inicial, llegó {other:?}"),
        }
        assert!(fs::read_to_string(brain.join("00_overview.md"))
            .unwrap()
            .contains("Documentación general"));
        assert!(brain.join("modules").join("src_app.md").is_file());
        let overview_time = fs::metadata(brain.join("00_overview.md")).unwrap().modified().unwrap();

        let mut updates = 0;
        let second = sync_session(&root, &brain, false, &cancel(), |_| updates += 1).unwrap();
        assert!(matches!(second, SyncOutcome::Unchanged(_)), "{second:?}");
        assert_eq!(updates, 0);
        assert_eq!(
            fs::metadata(brain.join("00_overview.md")).unwrap().modified().unwrap(),
            overview_time
        );

        fs::write(&app, "pub fn hello() -> i32 {\n    99\n}\n").unwrap();
        let edited = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        match edited {
            SyncOutcome::Updated(_, report) => {
                assert_eq!(report.mode, SyncMode::Incremental);
                assert_eq!(report.updated, 1);
                assert_eq!(report.added, 0);
            }
            other => panic!("el edit debía ser incremental: {other:?}"),
        }
        let again = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        assert!(
            matches!(again, SyncOutcome::Unchanged(_)),
            "un worktree sucio ya indexado no se reconstruye: {again:?}"
        );

        let (ask, _) = compose_prompt_live("demo", &root, &brain, "qué módulos tiene el proyecto", &cancel(), quiet_tick);
        assert!(ask.contains("NIVEL 1"));
        assert!(!ask.contains("NIVEL 3"));
        let (edit_prompt, _) = compose_prompt_live(
            "demo",
            &root,
            &brain,
            "modifica hello para que devuelva otro valor",
            &cancel(),
            quiet_tick,
        );
        assert!(edit_prompt.contains("NIVEL 3"));
        assert!(edit_prompt.contains("fn hello"));
        assert!(edit_prompt.contains("99\n"));

        git(&root, &["checkout", "--", "src/app/mod.rs"]);
        let reverted = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        match reverted {
            SyncOutcome::Updated(_, report) => assert_eq!(report.mode, SyncMode::Incremental),
            other => panic!("revertir el archivo debía actualizar el índice: {other:?}"),
        }
        let (after_revert, _) = compose_prompt_live(
            "demo",
            &root,
            &brain,
            "modifica hello otra vez",
            &cancel(),
            quiet_tick,
        );
        assert!(after_revert.contains("1\n}\n"));
        assert!(!after_revert.contains("99\n"));

        fs::write(&app, "pub fn hello() -> i32 {\n    99\n}\n").unwrap();
        sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        git(&root, &["add", "src/app/mod.rs"]);
        git_commit(&root, "hello 99");
        let committed = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        match committed {
            SyncOutcome::Updated(_, report) => assert_eq!(report.mode, SyncMode::GitOnly),
            other => panic!("commitear el worktree ya indexado no debe reindexar el código: {other:?}"),
        }

        fs::write(root.join("src/app/extra.rs"), "pub fn extra() -> i32 { 3 }\n").unwrap();
        let added = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        match added {
            SyncOutcome::Updated(_, report) => {
                assert_eq!(report.mode, SyncMode::Incremental);
                assert_eq!(report.added, 1);
            }
            other => panic!("un archivo nuevo debía entrar al índice: {other:?}"),
        }
        fs::remove_file(root.join("src/app/extra.rs")).unwrap();
        let removed = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        match removed {
            SyncOutcome::Updated(_, report) => {
                assert_eq!(report.removed, 1);
                assert!(load_state(&brain).unwrap().files.get("src/app/extra.rs").is_none());
            }
            other => panic!("borrar el archivo debía sacarlo del índice: {other:?}"),
        }

        git(&root, &["checkout", "-b", "feature"]);
        fs::write(
            &util,
            "pub fn helper() -> i32 {\n    7\n}\n\npub fn extra_helper() -> i32 {\n    8\n}\n",
        )
        .unwrap();
        git(&root, &["add", "src/util/mod.rs"]);
        git_commit(&root, "helper 7");
        let feature = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        match feature {
            SyncOutcome::Updated(_, report) => {
                assert_eq!(report.mode, SyncMode::Incremental);
                assert_eq!(report.added, 0);
                assert!(report.updated >= 1);
            }
            other => panic!("cambiar de rama debía ser incremental: {other:?}"),
        }
        let (feature_prompt, _) =
            compose_prompt_live("demo", &root, &brain, "modifica helper", &cancel(), quiet_tick);
        assert!(feature_prompt.contains("extra_helper"));
        git(&root, &["checkout", "main"]);
        let back = sync_session(&root, &brain, false, &cancel(), quiet).unwrap();
        match back {
            SyncOutcome::Updated(_, report) => assert_ne!(report.mode, SyncMode::Full),
            other => panic!("volver a main no puede reconstruir todo: {other:?}"),
        }
        let (helper_prompt, _) = compose_prompt_live(
            "demo",
            &root,
            &brain,
            "modifica helper",
            &cancel(),
            quiet_tick,
        );
        assert!(helper_prompt.contains("fn helper"));
        assert!(helper_prompt.contains("2\n}\n"));
        assert!(!helper_prompt.contains("extra_helper"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn transfer_context_copia_docs_al_context_sin_ia() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("yeison-transfer-{stamp}"));
        let from_brain = root.join("from_brain");
        let to_brain = root.join("to_brain");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&from_brain).unwrap();
        fs::create_dir_all(&to_brain).unwrap();
        fs::write(
            from_brain.join("00_overview.md"),
            "# Overview\n\nProyecto origen listo.\n",
        )
        .unwrap();
        fs::write(
            from_brain.join(BRAIN_INDEX),
            "# Índice\n\n- módulo app\n",
        )
        .unwrap();
        fs::write(
            from_brain.join("08_codebase_map.md"),
            "# Mapa\n\napp → util\n",
        )
        .unwrap();
        let state = BrainState {
            version: 1,
            root: "/tmp/from".into(),
            scanned_at_unix: 1,
            dirs: 1,
            total_bytes: 10,
            truncated: false,
            git: GitState::default(),
            files: BTreeMap::new(),
            recall: Recall {
                consultations: 2,
                terms: BTreeMap::new(),
            },
            settled: BTreeMap::from([(
                "src/app".into(),
                SettledCard {
                    fingerprint: "abc".into(),
                    lines: vec!["- `hello()` — saluda".into()],
                    hits: 3,
                },
            )]),
        };
        save_state(&from_brain, &state).unwrap();

        let count = transfer_context("Origen Demo", &from_brain, &to_brain).unwrap();
        assert!(count >= 4, "count={count}");
        let names: Vec<String> = fs::read_dir(to_brain.join("context"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("overview")),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("settled_cards")),
            "{names:?}"
        );
        let extra = fs::read_to_string(to_brain.join("06_extra_context.md")).unwrap();
        assert!(extra.contains("Proyecto origen listo"), "{extra}");
        assert!(extra.contains("hello()"), "{extra}");
        assert!(transfer_context("Origen Demo", &from_brain, &from_brain).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
        if !output.status.success() {
            panic!(
                "git {args:?} falló\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn git_commit(repo: &Path, message: &str) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.email=brain-test@example.com",
                "-c",
                "user.name=Brain Test",
                "commit",
                "-m",
                message,
            ])
            .output()
            .unwrap_or_else(|err| panic!("git commit: {err}"));
        if !output.status.success() {
            panic!(
                "git commit falló\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}


