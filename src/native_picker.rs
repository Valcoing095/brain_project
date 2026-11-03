use std::path::PathBuf;
use std::process::Command;

fn run_osascript(script: &str) -> Result<String, String> {
    let output = Command::new("osascript")
        .args(["-e", script])
        .output()
        .map_err(|e| format!("No se pudo abrir el selector nativo: {e}"))?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let lower = error.to_ascii_lowercase();
        if error.contains("-128") || lower.contains("cancel") || lower.contains("canceled") {
            return Err("Cancelado.".into());
        }
        return Err(if error.is_empty() {
            "No se pudo completar la selección.".into()
        } else {
            error
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
pub fn pick_folder(prompt: &str) -> Result<PathBuf, String> {
    let script = format!(
        "POSIX path of (choose folder with prompt \"{}\")",
        escape(prompt)
    );
    let output = run_osascript(&script)?;
    let path = output.trim();
    if path.is_empty() {
        Err("No se eligió ninguna carpeta.".into())
    } else {
        Ok(PathBuf::from(path))
    }
}

#[cfg(target_os = "macos")]
pub fn pick_files(prompt: &str) -> Result<Vec<PathBuf>, String> {
    let script = format!(
        "set theFiles to choose file with prompt \"{}\" with multiple selections allowed\n\
         set out to \"\"\n\
         repeat with f in theFiles\n\
         set out to out & POSIX path of f & linefeed\n\
         end repeat\n\
         return out",
        escape(prompt)
    );
    let output = run_osascript(&script)?;
    let files: Vec<PathBuf> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();
    if files.is_empty() {
        Err("No se eligió ningún archivo.".into())
    } else {
        Ok(files)
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pick_folder(_prompt: &str) -> Result<PathBuf, String> {
    Err("El selector nativo de carpetas sólo está disponible en macOS.".into())
}

#[cfg(not(target_os = "macos"))]
pub fn pick_files(_prompt: &str) -> Result<Vec<PathBuf>, String> {
    Err("El selector nativo de archivos sólo está disponible en macOS.".into())
}
