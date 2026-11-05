use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Límite por defecto para una respuesta completa de un agente.
/// Una respuesta que lee el repo y arma un plan puede tardar bastante más que un comando corto.
pub const TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// Límite para comandos de inspección (listar modelos, estado, `--help`).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
/// Límite corto para sondear binarios desconocidos del PATH.
pub const QUICK_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub type CancelFlag = Arc<AtomicBool>;

pub fn new_cancel_flag() -> CancelFlag {
    Arc::new(AtomicBool::new(false))
}

pub fn run_command(bin: &Path, args: &[&str], cwd: &Path) -> Result<String, String> {
    run_command_timeout(bin, args, cwd, TURN_TIMEOUT)
}

pub fn run_command_timeout(
    bin: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(cwd);
    configure_command(&mut cmd);

    let child = cmd
        .spawn()
        .map_err(|e| format!("No se pudo ejecutar {}: {e}", bin.display()))?;

    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    let output = match rx.recv_timeout(timeout) {
        Ok(result) => result.map_err(|e| format!("Error al esperar al proceso: {e}"))?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_pid(pid);
            return Err(timeout_message(bin, timeout));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("El proceso terminó de forma inesperada.".into());
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else if stdout.is_empty() {
        Err(if stderr.is_empty() {
            format!("Comando falló ({})", output.status)
        } else {
            stderr
        })
    } else {
        Ok(stdout)
    }
}

/// Ejecuta un comando y entrega cada línea de stdout a `on_line` conforme llega.
/// Devuelve stderr acumulado al terminar. Respeta `cancel` y `timeout`.
pub fn run_command_streaming(
    bin: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
    cancel: &CancelFlag,
    mut on_line: impl FnMut(&str),
) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(cwd);
    configure_command(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("No se pudo ejecutar {}: {e}", bin.display()))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Sin salida estándar del proceso.".to_string())?;
    let stderr = child.stderr.take();

    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut err) = stderr {
            let _ = err.read_to_string(&mut buf);
        }
        buf
    });

    let (line_tx, line_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let started = Instant::now();
    let mut finished_reading = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            kill_child(&mut child);
            return Err("Cancelado.".into());
        }
        if started.elapsed() > timeout {
            kill_child(&mut child);
            return Err(timeout_message(bin, timeout));
        }
        match line_rx.recv_timeout(Duration::from_millis(60)) {
            Ok(line) => on_line(&line),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if finished_reading {
                    break;
                }
                if let Ok(Some(_)) = child.try_wait() {
                    // Drenar lo que quede en el canal antes de salir.
                    while let Ok(line) = line_rx.try_recv() {
                        on_line(&line);
                    }
                    finished_reading = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Error al esperar al proceso: {e}"))?;
    let stderr_text = stderr_handle.join().unwrap_or_default().trim().to_string();

    if status.success() {
        Ok(stderr_text)
    } else if stderr_text.is_empty() {
        Err(format!("Comando falló ({status})"))
    } else {
        Err(stderr_text)
    }
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn timeout_message(bin: &Path, timeout: Duration) -> String {
    format!(
        "{} no respondió en {} s.",
        bin.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("El comando"),
        timeout.as_secs()
    )
}

fn configure_command(cmd: &mut Command) {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "dumb");
    cmd.env("CI", "1");

    if let Ok(home) = std::env::var("HOME") {
        let path = std::env::var("PATH").unwrap_or_default();
        let prefix =
            format!("{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
        cmd.env("PATH", format!("{prefix}:{path}"));
    }
}
