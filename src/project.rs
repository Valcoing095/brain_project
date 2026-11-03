use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const REGISTRY_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub brain_dir: PathBuf,
    #[serde(default)]
    pub scanned_at_unix: u64,
    #[serde(default)]
    pub last_scan_files: usize,
    #[serde(default)]
    pub last_used_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectRegistry {
    pub version: u32,
    pub projects: Vec<ProjectEntry>,
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            projects: Vec::new(),
        }
    }
}

impl ProjectRegistry {
    pub fn load() -> Self {
        let Ok(raw) = std::fs::read_to_string(registry_path()) else {
            return Self::default();
        };
        let Ok(registry) = serde_json::from_str::<Self>(&raw) else {
            return Self::default();
        };
        if registry.version != REGISTRY_VERSION {
            return Self::default();
        }
        registry
    }

    pub fn save(&self) -> Result<(), String> {
        let path = registry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("No se pudo crear la carpeta de configuración: {e}"))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("No se pudo serializar la lista de proyectos: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| format!("No se pudo guardar la lista de proyectos: {e}"))
    }

    pub fn upsert(&mut self, entry: ProjectEntry) {
        if let Some(existing) = self.projects.iter_mut().find(|p| p.id == entry.id) {
            *existing = entry;
        } else {
            self.projects.push(entry);
        }
        self.projects
            .sort_by(|a, b| b.last_used_unix.cmp(&a.last_used_unix));
    }

    pub fn most_recent(&self) -> Option<ProjectEntry> {
        self.projects.iter().max_by_key(|p| p.last_used_unix).cloned()
    }
}

pub fn registry_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hola_mundo")
        .join("projects.json")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn sanitize(name: &str) -> String {
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
        trimmed
    }
}

pub fn default_brain_dir(root: &Path, name: &str) -> PathBuf {
    root.join(format!("brain_{}", sanitize(name)))
}

pub fn project_name(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "proyecto".into())
}
