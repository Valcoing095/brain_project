use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod ai;
mod brain;
mod cursor_agent;
mod macos_overlay;
mod native_picker;
mod project;

use ai::{
    init_workspace, load_catalog_cache, new_cancel_flag, save_catalog_cache,
    scan_installed_providers_background, send_message, set_workspace, AgentSelection, AiCatalog,
    CancelFlag, ProviderEntry, ReadyState, ScanEvent, SessionState, StreamEvent,
};
use brain::{BrainReport, SyncMode, SyncOutcome};
use cursor_agent::AgentMode;
use display_info::DisplayInfo;
use eframe::egui;
use project::{ProjectEntry, ProjectRegistry};
use readmouse::Mouse;

const DOCK_WIDTH_CM: f32 = 8.5;
const DOCK_HEIGHT_CM: f32 = 11.0;
/// Espacio entre el borde superior del monitor y el panel del dock.
const DOCK_MARGIN_TOP_CM: f32 = 1.0;
/// Altura de la franja superior que abre el dock (solo cuando está oculto).
const HOT_ZONE_HEIGHT_CM: f32 = 1.0;
/// Ancho de la franja: centrada, un poco más ancha que el panel para facilitar el apuntado.
const HOT_ZONE_EXTRA_WIDTH_CM: f32 = 2.0;
/// Tras abrir el panel, ignorar cierres por clic (macOS a veces no entrega el primer clic a egui).
const OPEN_CLICK_GRACE: Duration = Duration::from_millis(450);

// ---------- Paleta ----------
mod palette {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgba_premultiplied(26, 27, 33, 242);
    pub const BORDER: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 28);
    pub const CARD: Color32 = Color32::from_rgb(42, 43, 52);
    pub const CARD_SELECTED: Color32 = Color32::from_rgb(58, 66, 110);
    pub const ACCENT: Color32 = Color32::from_rgb(96, 118, 255);
    pub const ACCENT_DARK: Color32 = Color32::from_rgb(76, 94, 215);
    pub const DANGER: Color32 = Color32::from_rgb(214, 72, 86);
    pub const TEXT: Color32 = Color32::from_rgb(238, 238, 243);
    pub const TEXT_DIM: Color32 = Color32::from_rgb(164, 166, 180);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(118, 120, 136);
    pub const OK: Color32 = Color32::from_rgb(74, 205, 140);
    pub const WARN: Color32 = Color32::from_rgb(245, 170, 48);
    pub const NEUTRAL: Color32 = Color32::from_rgb(140, 142, 156);
}

fn cm_to_px(cm: f32) -> f32 {
    const DPI: f32 = 96.0;
    (cm / 2.54) * DPI
}

fn cm_to_physical(cm: f32, scale: f32) -> f32 {
    cm_to_px(cm) * scale
}

fn ago_label(unix: u64) -> String {
    let now = project::now_unix();
    let diff = now.saturating_sub(unix);
    if diff < 60 {
        "hace un momento".into()
    } else if diff < 3_600 {
        format!("hace {} min", diff / 60)
    } else if diff < 86_400 {
        format!("hace {} h", diff / 3_600)
    } else {
        format!("hace {} días", diff / 86_400)
    }
}

#[derive(Clone, Copy, Debug)]
struct RectPhysical {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl RectPhysical {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

fn display_bounds(display: &DisplayInfo) -> RectPhysical {
    RectPhysical {
        x: display.x as f32,
        y: display.y as f32,
        w: display.width as f32,
        h: display.height as f32,
    }
}

fn dock_rect_for_display(display: &DisplayInfo) -> RectPhysical {
    let scale = display.scale_factor;
    let w = cm_to_physical(DOCK_WIDTH_CM, scale);
    let h = cm_to_physical(DOCK_HEIGHT_CM, scale);
    let margin = cm_to_physical(DOCK_MARGIN_TOP_CM, scale);
    let x = display.x as f32 + (display.width as f32 - w) * 0.5;
    let y = display.y as f32 + margin;
    RectPhysical { x, y, w, h }
}

/// Franja en el borde superior del monitor, centrada con el dock.
fn hot_zone_rect_for_display(display: &DisplayInfo) -> RectPhysical {
    let scale = display.scale_factor;
    let zone_h = cm_to_physical(HOT_ZONE_HEIGHT_CM, scale);
    let zone_w = cm_to_physical(DOCK_WIDTH_CM + HOT_ZONE_EXTRA_WIDTH_CM, scale);
    let x = display.x as f32 + (display.width as f32 - zone_w) * 0.5;
    let y = display.y as f32;
    RectPhysical {
        x,
        y,
        w: zone_w,
        h: zone_h,
    }
}

fn display_index_at_mouse(displays: &[DisplayInfo], mx: f32, my: f32) -> Option<usize> {
    displays
        .iter()
        .enumerate()
        .find(|(_, display)| display_bounds(display).contains(mx, my))
        .map(|(idx, _)| idx)
}

fn mouse_in_hot_zone(display: &DisplayInfo, mx: f32, my: f32) -> bool {
    hot_zone_rect_for_display(display).contains(mx, my)
}

// ---------- Estado de la app ----------

#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageRole {
    User,
    Assistant,
    Error,
}

#[derive(Clone)]
struct ChatMessage {
    role: MessageRole,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DockScreen {
    PickProject,
    PickProvider,
    PickModel,
    Chat,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickKind {
    RootFolder,
    BrainFolder,
    Files,
}

struct NewProjectDraft {
    name: String,
    root: PathBuf,
    brain_dir: String,
}

enum ProjectAction {
    Select(ProjectEntry),
    Create,
    CancelCreate,
    PickRoot,
    PickBrain,
    PickFiles,
    StartScan,
    Rebuild,
    StopScan,
    Continue,
    Change,
}

enum UiEvent {
    ScanProgress(String),
    ScanProvider(ProviderEntry),
    ScanFinished,
    ReplyThinking { seq: u64 },
    ReplyDelta { seq: u64, text: String },
    ReplyReplace { seq: u64, text: String },
    ReplyDone { seq: u64, text: String, session: SessionState },
    ReplyFailed { seq: u64, error: String },
    BrainProgress(String),
    BrainSynced {
        brain_dir: PathBuf,
        result: Result<SyncOutcome, String>,
        from_scan: bool,
    },
    PickerDone {
        kind: PickKind,
        result: Result<Vec<PathBuf>, String>,
    },
}

struct PendingReply {
    seq: u64,
    text: String,
    thinking: bool,
    started: Instant,
    cancel: CancelFlag,
}

struct YeisonDock {
    screen: DockScreen,
    visible: bool,
    display_idx: Option<usize>,
    left_button_was_pressed: bool,
    press_began_on_dock: bool,
    outside_click_enabled_after: Option<Instant>,
    overlay_configured: bool,

    // Proyecto / brain
    projects: ProjectRegistry,
    active_project: Option<ProjectEntry>,
    new_project: Option<NewProjectDraft>,
    pending_picker: Option<PickKind>,
    picker_open: bool,
    reopen_after_picker: bool,
    notice: Option<String>,
    brain_scanning: bool,
    brain_progress: String,
    brain_cancel: Option<CancelFlag>,
    brain_scan_target: Option<PathBuf>,
    brain_status: Option<brain::BrainStatus>,
    session_sync_pending: bool,

    // Catálogo / escaneo
    catalog: AiCatalog,
    catalog_scanning: bool,
    scan_progress: String,
    scan_cancel: Option<CancelFlag>,
    scan_started_at: Option<Instant>,

    // Selección
    selected_provider: Option<String>,
    selected_model_id: String,
    picker_mode: AgentMode,
    model_filter: String,
    agent_selection: Option<AgentSelection>,

    // Chat
    messages: Vec<ChatMessage>,
    draft: String,
    focus_input: bool,
    session: SessionState,
    pending: Option<PendingReply>,
    reply_gen: u64,

    ui_tx: Sender<UiEvent>,
    ui_rx: Receiver<UiEvent>,
}

impl YeisonDock {
    fn new() -> Self {
        let (ui_tx, ui_rx) = mpsc::channel();
        let catalog = load_catalog_cache().unwrap_or_else(AiCatalog::empty);
        let projects = ProjectRegistry::load();
        let active_project = projects.most_recent();
        let mut app = Self {
            screen: DockScreen::PickProject,
            visible: false,
            display_idx: None,
            left_button_was_pressed: false,
            press_began_on_dock: false,
            outside_click_enabled_after: None,
            overlay_configured: false,
            projects,
            active_project: None,
            new_project: None,
            pending_picker: None,
            picker_open: false,
            reopen_after_picker: false,
            notice: None,
            brain_scanning: false,
            brain_progress: String::new(),
            brain_cancel: None,
            brain_scan_target: None,
            brain_status: None,
            session_sync_pending: false,
            catalog,
            catalog_scanning: false,
            scan_progress: String::new(),
            scan_cancel: None,
            scan_started_at: None,
            selected_provider: None,
            selected_model_id: String::new(),
            picker_mode: AgentMode::Ask,
            model_filter: String::new(),
            agent_selection: None,
            messages: Vec::new(),
            draft: String::new(),
            focus_input: false,
            session: SessionState::Empty,
            pending: None,
            reply_gen: 0,
            ui_tx,
            ui_rx,
        };
        if let Some(project) = active_project {
            app.activate_project(project, false);
        }
        app
    }

    // ----- Proyecto / brain -----

    fn activate_project(&mut self, entry: ProjectEntry, touch: bool) {
        let changed = self
            .active_project
            .as_ref()
            .map(|p| p.id != entry.id)
            .unwrap_or(true);
        let mut entry = entry;
        if touch {
            entry.last_used_unix = project::now_unix();
        }
        set_workspace(entry.root.clone());
        self.brain_status = None;
        self.session_sync_pending = true;
        self.brain_progress = "Comprobando rama y último índice…".into();
        self.active_project = Some(entry.clone());
        self.projects.upsert(entry);
        if let Err(error) = self.projects.save() {
            self.notice = Some(error);
        }
        if changed {
            self.new_chat();
        }
        self.screen = DockScreen::PickProject;
    }

    fn confirm_new_project(&mut self) {
        let Some(draft) = self.new_project.take() else {
            return;
        };
        let brain_dir = PathBuf::from(draft.brain_dir.trim());
        if brain_dir.as_os_str().is_empty() {
            self.notice = Some("Indicá la carpeta del brain.".into());
            self.new_project = Some(draft);
            return;
        }
        if let Err(error) = std::fs::create_dir_all(&brain_dir) {
            self.notice = Some(format!("No se pudo crear el brain: {error}"));
            self.new_project = Some(draft);
            return;
        }
        let entry = ProjectEntry {
            id: draft.root.to_string_lossy().to_string(),
            name: draft.name,
            root: draft.root,
            brain_dir,
            scanned_at_unix: 0,
            last_scan_files: 0,
            last_used_unix: project::now_unix(),
        };
        self.activate_project(entry, true);
    }

    fn start_brain_sync(&mut self, ctx: &egui::Context, force_full: bool) {
        if self.brain_scanning {
            return;
        }
        let Some(active) = self.active_project.clone() else {
            self.notice = Some("Elegí un proyecto primero.".into());
            return;
        };
        if let Err(error) = std::fs::create_dir_all(&active.brain_dir) {
            self.notice = Some(format!("No se pudo crear el brain: {error}"));
            return;
        }

        let cancel = new_cancel_flag();
        self.brain_cancel = Some(cancel.clone());
        self.brain_scan_target = Some(active.brain_dir.clone());
        self.session_sync_pending = false;
        self.brain_scanning = true;
        self.brain_progress = "Comprobando rama y último índice…".into();
        self.notice = None;

        let tx = self.ui_tx.clone();
        let ctx = ctx.clone();
        let brain_dir = active.brain_dir.clone();
        std::thread::spawn(move || {
            let tx_progress = tx.clone();
            let ctx_progress = ctx.clone();
            let result = brain::sync_session(
                &active.root,
                &active.brain_dir,
                force_full,
                &cancel,
                |message| {
                    let _ = tx_progress.send(UiEvent::BrainProgress(message));
                    ctx_progress.request_repaint();
                },
            );
            let _ = tx.send(UiEvent::BrainSynced {
                brain_dir,
                result,
                from_scan: true,
            });
            ctx.request_repaint();
        });
    }

    fn kick_session_sync(&mut self, ctx: &egui::Context) {
        if self.session_sync_pending
            && !self.brain_scanning
            && !self.picker_open
            && self.active_project.is_some()
        {
            self.start_brain_sync(ctx, false);
        }
    }

    fn stop_brain_scan(&mut self) {
        if let Some(cancel) = &self.brain_cancel {
            cancel.store(true, Ordering::Relaxed);
            self.notice = Some("Deteniendo escaneo…".into());
        }
    }

    fn apply_brain_status(&mut self, status: brain::BrainStatus) {
        let files = status.files;
        let scanned_at = status.scanned_at_unix;
        self.brain_status = Some(status);
        let Some(active) = self.active_project.clone() else {
            return;
        };
        if active.scanned_at_unix == scanned_at && active.last_scan_files == files {
            return;
        }
        let mut updated = active;
        updated.scanned_at_unix = scanned_at;
        updated.last_scan_files = files;
        self.active_project = Some(updated.clone());
        self.projects.upsert(updated);
        if let Err(error) = self.projects.save() {
            self.notice = Some(error);
        }
    }

    fn ingest_files(&mut self, paths: Vec<PathBuf>) {
        let Some(active) = self.active_project.clone() else {
            self.notice = Some("Elegí un proyecto primero.".into());
            return;
        };
        match brain::ingest_files(&active.brain_dir, &paths) {
            Ok(count) => {
                self.notice = Some(format!("{count} archivo(s) agregados al brain."));
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn spawn_picker(&mut self, kind: PickKind, ctx: &egui::Context) {
        self.picker_open = true;
        let tx = self.ui_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = match kind {
                PickKind::RootFolder => {
                    native_picker::pick_folder("Elegí la raíz del proyecto").map(|path| vec![path])
                }
                PickKind::BrainFolder => {
                    native_picker::pick_folder("Elegí dónde guardar la carpeta brain")
                        .map(|path| vec![path])
                }
                PickKind::Files => {
                    native_picker::pick_files("Elegí archivos para alimentar el brain")
                }
            };
            let _ = tx.send(UiEvent::PickerDone { kind, result });
            ctx.request_repaint();
        });
    }

    fn handle_picker_result(
        &mut self,
        kind: PickKind,
        result: Result<Vec<PathBuf>, String>,
    ) {
        match (kind, result) {
            (_, Err(error)) => {
                if error != "Cancelado." {
                    self.notice = Some(error);
                }
            }
            (PickKind::RootFolder, Ok(mut paths)) => {
                if let Some(root) = paths.pop() {
                    let name = project::project_name(&root);
                    let brain_dir = project::default_brain_dir(&root, &name);
                    self.new_project = Some(NewProjectDraft {
                        name,
                        root,
                        brain_dir: brain_dir.to_string_lossy().to_string(),
                    });
                    self.notice = None;
                }
            }
            (PickKind::BrainFolder, Ok(mut paths)) => {
                if let Some(path) = paths.pop() {
                    if let Some(draft) = &mut self.new_project {
                        draft.brain_dir = path.to_string_lossy().to_string();
                    }
                }
            }
            (PickKind::Files, Ok(paths)) => self.ingest_files(paths),
        }
    }

    fn apply_project_action(&mut self, action: ProjectAction, ctx: &egui::Context) {
        match action {
            ProjectAction::Select(entry) => self.activate_project(entry, true),
            ProjectAction::Create => self.confirm_new_project(),
            ProjectAction::CancelCreate => {
                self.new_project = None;
                self.notice = None;
            }
            ProjectAction::PickRoot => self.pending_picker = Some(PickKind::RootFolder),
            ProjectAction::PickBrain => self.pending_picker = Some(PickKind::BrainFolder),
            ProjectAction::PickFiles => self.pending_picker = Some(PickKind::Files),
            ProjectAction::StartScan => self.start_brain_sync(ctx, false),
            ProjectAction::Rebuild => self.start_brain_sync(ctx, true),
            ProjectAction::StopScan => self.stop_brain_scan(),
            ProjectAction::Continue => self.screen = DockScreen::PickProvider,
            ProjectAction::Change => {
                self.active_project = None;
                self.brain_status = None;
                self.session_sync_pending = false;
                self.new_project = None;
                self.notice = None;
            }
        }
    }

    // ----- Escaneo -----

    fn start_scan(&mut self) {
        if self.catalog_scanning {
            return;
        }
        let cancel = new_cancel_flag();
        self.scan_cancel = Some(cancel.clone());
        self.catalog_scanning = true;
        self.scan_started_at = Some(Instant::now());
        self.scan_progress = "Preparando búsqueda…".into();

        let ui_tx = self.ui_tx.clone();
        let (scan_tx, scan_rx) = mpsc::channel::<ScanEvent>();
        std::thread::spawn(move || scan_installed_providers_background(&scan_tx, cancel));
        std::thread::spawn(move || {
            while let Ok(event) = scan_rx.recv() {
                match event {
                    ScanEvent::Progress(text) => {
                        let _ = ui_tx.send(UiEvent::ScanProgress(text));
                    }
                    ScanEvent::Provider(provider) => {
                        let _ = ui_tx.send(UiEvent::ScanProvider(provider));
                    }
                    ScanEvent::Finished { cancelled } => {
                        if cancelled {
                            let _ = ui_tx.send(UiEvent::ScanProgress(
                                "Búsqueda detenida. Puedes usar lo encontrado.".into(),
                            ));
                        }
                        let _ = ui_tx.send(UiEvent::ScanFinished);
                        break;
                    }
                }
            }
        });
    }

    fn stop_scan_keep_found(&mut self) {
        if let Some(cancel) = &self.scan_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        self.finish_scan();
    }

    fn finish_scan(&mut self) {
        self.catalog_scanning = false;
        self.scan_cancel = None;
        self.scan_progress.clear();
        self.catalog.finalize_scan();
        self.persist_catalog();
    }

    fn persist_catalog(&self) {
        if let Err(error) = save_catalog_cache(&self.catalog) {
            eprintln!("No se pudo guardar caché de IAs: {error}");
        }
    }

    // ----- Chat -----

    fn provider_entry(&self, id: &str) -> Option<&ProviderEntry> {
        self.catalog.provider(id)
    }

    fn start_chat(&mut self) {
        if self.active_project.is_none() {
            self.screen = DockScreen::PickProject;
            return;
        }
        let Some(provider_id) = self.selected_provider.clone() else {
            return;
        };
        let Some(provider) = self.provider_entry(&provider_id).cloned() else {
            return;
        };
        if self.selected_model_id.is_empty() {
            return;
        }

        let cursor_modes = provider.kind.supports_cursor_modes();
        let same_agent = self
            .agent_selection
            .as_ref()
            .is_some_and(|s| s.provider.id == provider.id && s.model_id == self.selected_model_id);

        self.agent_selection = Some(AgentSelection {
            provider,
            model_id: self.selected_model_id.clone(),
            cursor_mode: cursor_modes.then_some(self.picker_mode),
        });
        if !same_agent {
            self.new_chat();
        }
        self.screen = DockScreen::Chat;
        self.focus_input = true;
    }

    fn new_chat(&mut self) {
        self.cancel_pending();
        self.messages.clear();
        self.session = SessionState::Empty;
        self.draft.clear();
        self.focus_input = true;
    }

    fn cancel_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Relaxed);
            if !pending.text.trim().is_empty() {
                self.messages.push(ChatMessage {
                    role: MessageRole::Assistant,
                    text: format!("{}\n\n(respuesta cancelada)", pending.text.trim_end()),
                });
            } else {
                self.messages.push(ChatMessage {
                    role: MessageRole::Error,
                    text: "Respuesta cancelada.".into(),
                });
            }
        }
    }

    fn send_draft(&mut self, ctx: &egui::Context) {
        let text = self.draft.trim().to_string();
        if text.is_empty() || self.pending.is_some() {
            return;
        }
        let Some(selection) = self.agent_selection.clone() else {
            return;
        };

        let project = self.active_project.clone();

        self.messages.push(ChatMessage {
            role: MessageRole::User,
            text: text.clone(),
        });
        self.draft.clear();
        self.focus_input = true;

        self.reply_gen += 1;
        let seq = self.reply_gen;
        let cancel = new_cancel_flag();
        self.pending = Some(PendingReply {
            seq,
            text: String::new(),
            thinking: false,
            started: Instant::now(),
            cancel: cancel.clone(),
        });

        let mut session = self.session.clone();
        let tx = self.ui_tx.clone();
        let ctx_clone = ctx.clone();
        std::thread::spawn(move || {
            let tx_events = tx.clone();
            let ctx_events = ctx_clone.clone();
            let (effective, cancelled) = if let Some(active) = project {
                let brain_dir = active.brain_dir.clone();
                let (prompt, outcome) = brain::compose_prompt_live(
                    &active.name,
                    &active.root,
                    &active.brain_dir,
                    &text,
                    &cancel,
                    |message| {
                        let _ = tx_events.send(UiEvent::BrainProgress(message));
                        ctx_events.request_repaint();
                    },
                );
                let cancelled = cancel.load(Ordering::Relaxed)
                    || matches!(&outcome, Err(error) if error == "Escaneo cancelado.");
                let _ = tx.send(UiEvent::BrainSynced {
                    brain_dir,
                    result: outcome,
                    from_scan: false,
                });
                (prompt, cancelled)
            } else {
                (text.clone(), cancel.load(Ordering::Relaxed))
            };
            if cancelled {
                let _ = tx.send(UiEvent::ReplyFailed {
                    seq,
                    error: "Cancelado.".into(),
                });
                ctx_clone.request_repaint();
                return;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                send_message(&selection, &mut session, &effective, &cancel, |event| {
                    let ui_event = match event {
                        StreamEvent::Thinking => UiEvent::ReplyThinking { seq },
                        StreamEvent::Delta(t) => UiEvent::ReplyDelta { seq, text: t },
                        StreamEvent::Replace(t) => UiEvent::ReplyReplace { seq, text: t },
                    };
                    let _ = tx_events.send(ui_event);
                    ctx_events.request_repaint();
                })
            }));
            match result {
                Ok(Ok(reply)) => {
                    let _ = tx.send(UiEvent::ReplyDone {
                        seq,
                        text: reply,
                        session,
                    });
                }
                Ok(Err(error)) => {
                    let _ = tx.send(UiEvent::ReplyFailed { seq, error });
                }
                Err(_) => {
                    let _ = tx.send(UiEvent::ReplyFailed {
                        seq,
                        error: "Fallo interno al ejecutar el agente.".into(),
                    });
                }
            }
            ctx_clone.request_repaint();
        });
        ctx.request_repaint();
    }

    fn poll_ui_events(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        while let Ok(event) = self.ui_rx.try_recv() {
            events.push(event);
        }

        for event in events {
            match event {
                UiEvent::ScanProgress(text) => {
                    if self.catalog_scanning {
                        self.scan_progress = text;
                    }
                }
                UiEvent::ScanProvider(provider) => {
                    self.catalog.upsert_provider(provider);
                    if !self.catalog_scanning {
                        // Llegó después de "Detener": igual lo guardamos.
                        self.persist_catalog();
                    }
                }
                UiEvent::ScanFinished => {
                    if self.catalog_scanning {
                        self.finish_scan();
                    }
                }
                UiEvent::ReplyThinking { seq } => {
                    if let Some(p) = self.pending.as_mut().filter(|p| p.seq == seq) {
                        p.thinking = true;
                    }
                }
                UiEvent::ReplyDelta { seq, text } => {
                    if let Some(p) = self.pending.as_mut().filter(|p| p.seq == seq) {
                        p.thinking = false;
                        p.text.push_str(&text);
                    }
                }
                UiEvent::ReplyReplace { seq, text } => {
                    if let Some(p) = self.pending.as_mut().filter(|p| p.seq == seq) {
                        p.thinking = false;
                        p.text = text;
                    }
                }
                UiEvent::ReplyDone { seq, text, session } => {
                    if self.pending.as_ref().is_some_and(|p| p.seq == seq) {
                        self.pending = None;
                        self.session = session;
                        self.messages.push(ChatMessage {
                            role: MessageRole::Assistant,
                            text,
                        });
                    }
                }
                UiEvent::ReplyFailed { seq, error } => {
                    if self.pending.as_ref().is_some_and(|p| p.seq == seq) {
                        let partial = self.pending.take().map(|p| p.text).unwrap_or_default();
                        if error == "Cancelado." {
                            continue;
                        }
                        if !partial.trim().is_empty() {
                            self.messages.push(ChatMessage {
                                role: MessageRole::Assistant,
                                text: partial,
                            });
                        }
                        self.messages.push(ChatMessage {
                            role: MessageRole::Error,
                            text: error,
                        });
                    }
                }
                UiEvent::BrainProgress(text) => {
                    self.brain_progress = text;
                }
                UiEvent::BrainSynced {
                    brain_dir,
                    result,
                    from_scan,
                } => {
                    if from_scan {
                        self.brain_scanning = false;
                        self.brain_cancel = None;
                        self.brain_scan_target = None;
                    }
                    self.brain_progress.clear();
                    let matches_active = self
                        .active_project
                        .as_ref()
                        .is_some_and(|active| active.brain_dir == brain_dir);
                    if !matches_active {
                        continue;
                    }
                    match result {
                        Ok(SyncOutcome::Unchanged(status)) => {
                            self.apply_brain_status(status.clone());
                            if from_scan {
                                let git = if status.git.available {
                                    format!(
                                        "rama {} · {}",
                                        status.git.branch,
                                        status.git.dirty_label()
                                    )
                                } else {
                                    "sin Git".into()
                                };
                                self.notice = Some(format!("Índice al día. {git}."));
                            }
                        }
                        Ok(SyncOutcome::Updated(status, report)) => {
                            self.apply_brain_status(status);
                            self.notice = Some(format_sync_report(&report));
                        }
                        Err(error) => {
                            if error != "Escaneo cancelado." {
                                self.notice = Some(error);
                            }
                        }
                    }
                }
                UiEvent::PickerDone { kind, result } => {
                    self.picker_open = false;
                    self.handle_picker_result(kind, result);
                    self.reopen_after_picker = true;
                }
            }
        }

        if self.pending.is_some() || self.catalog_scanning || self.brain_scanning || self.picker_open {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    // ----- Visibilidad / overlay -----

    fn mouse_physical() -> (f32, f32) {
        let (x, y) = Mouse::location();
        (x as f32, y as f32)
    }

    fn apply_geometry(&self, ctx: &egui::Context, display: &DisplayInfo) {
        let scale = display.scale_factor;
        let dock = dock_rect_for_display(display);
        let logical_size = egui::vec2(dock.w / scale, dock.h / scale);
        let logical_pos = egui::pos2(dock.x / scale, dock.y / scale);

        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(logical_size));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(logical_pos));
    }

    fn pointer_on_dock_ui(ctx: &egui::Context) -> bool {
        ctx.input(|i| {
            i.pointer.primary_down() || i.pointer.any_click() || i.pointer.interact_pos().is_some()
        })
    }

    fn sync_visibility(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if self.picker_open {
            ctx.request_repaint_after(Duration::from_millis(120));
            return;
        }
        let displays = DisplayInfo::all().unwrap_or_default();
        if displays.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(33));
            return;
        }

        let (mx, my) = Self::mouse_physical();
        let monitor = display_index_at_mouse(&displays, mx, my);
        let in_hot_zone = monitor.is_some_and(|idx| mouse_in_hot_zone(&displays[idx], mx, my));

        let left_down = Mouse::Left.is_pressed();
        let left_release = !left_down && self.left_button_was_pressed;

        if left_down && Self::pointer_on_dock_ui(ctx) {
            self.press_began_on_dock = true;
        }

        self.left_button_was_pressed = left_down;

        if !self.visible {
            if in_hot_zone {
                let idx = monitor.expect("hot zone implies monitor");
                self.visible = true;
                self.display_idx = Some(idx);
                self.press_began_on_dock = false;
                self.outside_click_enabled_after = Some(Instant::now() + OPEN_CLICK_GRACE);
                self.focus_input = self.screen == DockScreen::Chat;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                self.apply_geometry(ctx, &displays[idx]);
                macos_overlay::sync_overlay_presence(frame, true);
            }
        } else if let Some(idx) = self.display_idx {
            self.apply_geometry(ctx, &displays[idx]);
            macos_overlay::sync_overlay_presence(frame, true);

            // Cerrar solo al soltar el clic fuera del panel (egui), no al presionar.
            let outside_clicks_enabled = self
                .outside_click_enabled_after
                .is_none_or(|t| Instant::now() >= t);
            let ui_claimed_click = ctx.input(|i| i.pointer.any_click());

            if left_release
                && outside_clicks_enabled
                && !self.press_began_on_dock
                && !ui_claimed_click
            {
                self.visible = false;
                self.display_idx = None;
                self.outside_click_enabled_after = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                macos_overlay::sync_overlay_presence(frame, false);
            }

            if left_release {
                self.press_began_on_dock = false;
            }
        }

        ctx.request_repaint_after(Duration::from_millis(33));
    }

    // ----- Widgets reutilizables -----

    fn paint_panel_background(ui: &mut egui::Ui) {
        let outer = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(outer, 16.0, palette::BG);
        painter.rect_stroke(
            outer,
            16.0,
            egui::Stroke::new(1.0_f32, palette::BORDER),
            egui::StrokeKind::Inside,
        );
    }

    fn primary_button(ui: &mut egui::Ui, label: &str, enabled: bool, full_width: bool) -> bool {
        let mut button = egui::Button::new(
            egui::RichText::new(label)
                .size(13.0)
                .strong()
                .color(egui::Color32::WHITE),
        )
        .fill(if enabled {
            palette::ACCENT
        } else {
            palette::ACCENT_DARK
        })
        .corner_radius(10.0);
        if full_width {
            button = button.min_size(egui::vec2(ui.available_width(), 32.0));
        } else {
            button = button.min_size(egui::vec2(0.0, 30.0));
        }
        ui.add_enabled(enabled, button).clicked()
    }

    fn ghost_button(ui: &mut egui::Ui, label: &str) -> bool {
        ui.add(
            egui::Button::new(egui::RichText::new(label).size(11.5).color(palette::TEXT_DIM))
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::new(1.0_f32, palette::BORDER))
                .corner_radius(8.0),
        )
        .clicked()
    }

    fn danger_button(ui: &mut egui::Ui, label: &str) -> bool {
        ui.add(
            egui::Button::new(
                egui::RichText::new(label)
                    .size(12.0)
                    .color(egui::Color32::WHITE),
            )
            .fill(palette::DANGER)
            .corner_radius(10.0)
            .min_size(egui::vec2(0.0, 30.0)),
        )
        .clicked()
    }

    fn status_dot(ui: &mut egui::Ui, ready: ReadyState) {
        let color = match ready {
            ReadyState::Ready => palette::OK,
            ReadyState::NeedsAuth => palette::WARN,
            ReadyState::Unknown => palette::NEUTRAL,
        };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.5, color);
    }

    fn ready_label(ready: ReadyState) -> &'static str {
        match ready {
            ReadyState::Ready => "Listo",
            ReadyState::NeedsAuth => "Requiere acceso",
            ReadyState::Unknown => "Sin verificar",
        }
    }

    /// Tarjeta clicable; devuelve la respuesta de clic.
    fn card(
        ui: &mut egui::Ui,
        selected: bool,
        add_contents: impl FnOnce(&mut egui::Ui),
    ) -> egui::Response {
        let fill = if selected {
            palette::CARD_SELECTED
        } else {
            palette::CARD
        };
        let response = egui::Frame::new()
            .fill(fill)
            .corner_radius(12.0)
            .inner_margin(egui::Margin::symmetric(12, 9))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add_contents(ui);
            })
            .response
            .interact(egui::Sense::click());
        if response.hovered() {
            ui.painter().rect_stroke(
                response.rect,
                12.0,
                egui::Stroke::new(1.0_f32, palette::ACCENT),
                egui::StrokeKind::Inside,
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response
    }

    fn header(ui: &mut egui::Ui, title: &str, subtitle: Option<&str>) {
        ui.label(
            egui::RichText::new(title)
                .strong()
                .size(17.0)
                .color(palette::TEXT),
        );
        if let Some(sub) = subtitle {
            ui.label(egui::RichText::new(sub).size(11.0).color(palette::TEXT_DIM));
        }
    }

    fn scanned_ago(&self) -> Option<String> {
        if self.catalog.scanned_at_unix == 0 {
            return None;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let diff = now.saturating_sub(self.catalog.scanned_at_unix);
        Some(if diff < 60 {
            "hace un momento".into()
        } else if diff < 3600 {
            format!("hace {} min", diff / 60)
        } else if diff < 86_400 {
            format!("hace {} h", diff / 3600)
        } else {
            format!("hace {} días", diff / 86_400)
        })
    }

    fn active_index_label(&self) -> (String, egui::Color32) {
        let Some(status) = &self.brain_status else {
            return (
                "Comprobando rama y último índice…".into(),
                palette::WARN,
            );
        };
        let git = if status.git.available {
            format!(
                "{} · {} · {}",
                status.git.branch,
                status.git.head_short,
                status.git.dirty_label()
            )
        } else {
            "Sin repositorio Git".into()
        };
        let when = if status.scanned_at_unix == 0 {
            "sin índice".into()
        } else {
            format!(
                "último índice {} · {} archivos · {} símbolos · {} módulos",
                ago_label(status.scanned_at_unix),
                status.files,
                status.symbols,
                status.modules
            )
        };
        let freshness = if status.stale {
            status.reason.as_str()
        } else {
            "al día"
        };
        let color = if status.stale {
            palette::WARN
        } else {
            palette::OK
        };
        let learning = if status.consultations == 0 {
            String::new()
        } else {
            format!(
                " · {} consultas · {} fichas asentadas",
                status.consultations, status.settled
            )
        };
        (format!("{git}\n{when} · {freshness}{learning}"), color)
    }

    // ----- Pantalla: proyecto -----

    fn show_pick_project(&mut self, ui: &mut egui::Ui) {
        Self::paint_panel_background(ui);
        let inner = ui.max_rect().shrink(14.0);
        let action = ui
            .allocate_new_ui(egui::UiBuilder::new().max_rect(inner), |ui| {
                ui.vertical(|ui| {
                    let mut action: Option<ProjectAction> = None;
                    Self::header(ui, "Yeison", Some("Proyecto de trabajo"));
                    ui.add_space(8.0);

                    if let Some(draft) = &mut self.new_project {
                        ui.label(
                            egui::RichText::new("Nuevo proyecto")
                                .strong()
                                .size(12.5)
                                .color(palette::TEXT_DIM),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&draft.name)
                                .strong()
                                .size(15.0)
                                .color(palette::TEXT),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("Raíz: {}", draft.root.display()))
                                    .size(9.5)
                                    .color(palette::TEXT_MUTED),
                            )
                            .wrap(),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Carpeta del brain")
                                .size(11.0)
                                .color(palette::TEXT_DIM),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut draft.brain_dir)
                                .desired_width(f32::INFINITY)
                                .margin(egui::vec2(8.0, 6.0)),
                        );
                        ui.label(
                            egui::RichText::new("Ahí se guardan los .md del escaneo.")
                                .size(9.0)
                                .color(palette::TEXT_MUTED),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if Self::ghost_button(ui, "Elegir otra…") {
                                action = Some(ProjectAction::PickBrain);
                            }
                            if Self::ghost_button(ui, "Cancelar") {
                                action = Some(ProjectAction::CancelCreate);
                            }
                        });
                        ui.add_space(6.0);
                        if Self::primary_button(ui, "Crear y escanear", true, true) {
                            action = Some(ProjectAction::Create);
                        }
                    } else if let Some(project) = self.active_project.clone() {
                        Self::card(ui, false, |ui| {
                            ui.label(
                                egui::RichText::new(&project.name)
                                    .strong()
                                    .size(15.0)
                                    .color(palette::TEXT),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "Raíz: {}",
                                        project.root.display()
                                    ))
                                    .size(9.5)
                                    .color(palette::TEXT_DIM),
                                )
                                .wrap(),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "Brain: {}",
                                        project.brain_dir.display()
                                    ))
                                    .size(9.0)
                                    .color(palette::TEXT_MUTED),
                                )
                                .wrap(),
                            );
                            let (status, color) = self.active_index_label();
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(status).size(10.0).color(color),
                                )
                                .wrap(),
                            );
                        });
                        ui.add_space(8.0);

                        if self.brain_scanning {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(13.0).color(palette::ACCENT));
                                ui.label(
                                    egui::RichText::new(&self.brain_progress)
                                        .size(10.0)
                                        .color(palette::TEXT_DIM),
                                );
                            });
                            ui.add_space(5.0);
                            if Self::ghost_button(ui, "Detener escaneo") {
                                action = Some(ProjectAction::StopScan);
                            }
                        } else {
                            let index_label = if project.scanned_at_unix == 0 {
                                "Crear índice"
                            } else {
                                "Actualizar índice"
                            };
                            ui.horizontal(|ui| {
                                if Self::primary_button(ui, index_label, true, false) {
                                    action = Some(ProjectAction::StartScan);
                                }
                                if Self::ghost_button(ui, "Añadir archivos") {
                                    action = Some(ProjectAction::PickFiles);
                                }
                            });
                            ui.add_space(4.0);
                            if Self::ghost_button(ui, "Reconstruir índice") {
                                action = Some(ProjectAction::Rebuild);
                            }
                            ui.add_space(6.0);
                            if Self::primary_button(ui, "Elegir IA y chatear", true, true) {
                                action = Some(ProjectAction::Continue);
                            }
                            ui.add_space(4.0);
                            if Self::ghost_button(ui, "Cambiar de proyecto") {
                                action = Some(ProjectAction::Change);
                            }
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("Elegí un proyecto")
                                .strong()
                                .size(12.5)
                                .color(palette::TEXT_DIM),
                        );
                        ui.label(
                            egui::RichText::new(
                                "Al abrirlo se comprueba la rama y el último índice. \
                                 Si hay cambios, se actualiza sólo eso. No consume tokens.",
                            )
                            .size(9.5)
                            .color(palette::TEXT_MUTED),
                        );
                        ui.add_space(6.0);

                        if self.projects.projects.is_empty() {
                            ui.add_space(14.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new("Sin proyectos")
                                        .strong()
                                        .size(13.0)
                                        .color(palette::TEXT),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "Agregá la carpeta raíz de un proyecto existente.",
                                    )
                                    .size(10.5)
                                    .color(palette::TEXT_DIM),
                                );
                            });
                            ui.add_space(12.0);
                        } else {
                            let list = self.projects.projects.clone();
                            let height = (ui.available_height() - 48.0).max(80.0);
                            egui::ScrollArea::vertical()
                                .max_height(height)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    for project in list {
                                        let scanned = if project.scanned_at_unix == 0 {
                                            "sin escanear".to_string()
                                        } else {
                                            format!(
                                                "escaneado {}",
                                                ago_label(project.scanned_at_unix)
                                            )
                                        };
                                        let response = Self::card(ui, false, |ui| {
                                            ui.label(
                                                egui::RichText::new(&project.name)
                                                    .strong()
                                                    .size(13.0)
                                                    .color(palette::TEXT),
                                            );
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(
                                                        project.root.display().to_string(),
                                                    )
                                                    .size(9.0)
                                                    .color(palette::TEXT_MUTED),
                                                )
                                                .truncate(),
                                            );
                                            ui.label(
                                                egui::RichText::new(scanned)
                                                    .size(9.0)
                                                    .color(palette::TEXT_MUTED),
                                            );
                                        });
                                        if response.clicked() {
                                            action = Some(ProjectAction::Select(project.clone()));
                                        }
                                        ui.add_space(5.0);
                                    }
                                });
                        }

                        if Self::primary_button(ui, "Agregar proyecto", true, true) {
                            action = Some(ProjectAction::PickRoot);
                        }
                    }

                    if let Some(notice) = &self.notice {
                        ui.add_space(5.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(notice)
                                    .size(9.0)
                                    .color(palette::TEXT_MUTED),
                            )
                            .wrap(),
                        );
                    }

                    action
                })
                .inner
            })
            .inner;

        if let Some(action) = action {
            self.apply_project_action(action, ui.ctx());
        }
    }

    // ----- Pantalla: proveedores -----

    fn show_pick_provider(&mut self, ui: &mut egui::Ui) {
        if self.active_project.is_none() {
            self.screen = DockScreen::PickProject;
            return;
        }
        Self::paint_panel_background(ui);
        let inner = ui.max_rect().shrink(14.0);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.vertical(|ui| {
                if Self::ghost_button(ui, "‹ Proyecto") {
                    self.screen = DockScreen::PickProject;
                }
                Self::header(ui, "Yeison", Some("Elige con qué asistente quieres hablar"));
                ui.add_space(10.0);

                let providers: Vec<ProviderEntry> =
                    self.catalog.installed_providers().cloned().collect();
                let has_providers = !providers.is_empty();

                // Barra de escaneo
                if self.catalog_scanning {
                    egui::Frame::new()
                        .fill(palette::CARD)
                        .corner_radius(12.0)
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(14.0).color(palette::ACCENT));
                                ui.add_space(4.0);
                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new("Buscando asistentes…")
                                            .strong()
                                            .size(12.0)
                                            .color(palette::TEXT),
                                    );
                                    let elapsed = self
                                        .scan_started_at
                                        .map(|t| t.elapsed().as_secs())
                                        .unwrap_or(0);
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} · {} s",
                                            self.scan_progress, elapsed
                                        ))
                                        .size(10.5)
                                        .color(palette::TEXT_MUTED),
                                    );
                                });
                            });
                            ui.add_space(6.0);
                            let label = if has_providers {
                                "Detener y usar lo encontrado"
                            } else {
                                "Detener búsqueda"
                            };
                            if Self::ghost_button(ui, label) {
                                self.stop_scan_keep_found();
                            }
                        });
                    ui.add_space(8.0);
                } else if !has_providers {
                    // Estado inicial: invitación a escanear
                    ui.add_space(18.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("✦").size(34.0).color(palette::ACCENT));
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("Aún no hay asistentes")
                                .strong()
                                .size(14.0)
                                .color(palette::TEXT),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Busca las herramientas de IA instaladas en tu Mac \
                                 (Cursor, OpenCode, Claude, Gemini…). Podrás usar \
                                 cada una en cuanto aparezca.",
                            )
                            .size(11.5)
                            .color(palette::TEXT_DIM),
                        );
                        ui.add_space(14.0);
                        if Self::primary_button(ui, "Buscar asistentes", true, true) {
                            self.start_scan();
                        }
                    });
                    return;
                }

                if has_providers {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Asistentes")
                                .size(12.0)
                                .strong()
                                .color(palette::TEXT_DIM),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !self.catalog_scanning && Self::ghost_button(ui, "↻ Buscar de nuevo")
                            {
                                self.start_scan();
                            }
                        });
                    });
                    ui.add_space(4.0);
                }

                let footer_h = 22.0;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height((ui.available_height() - footer_h).max(60.0))
                    .show(ui, |ui| {
                        for provider in &providers {
                            let selected =
                                self.selected_provider.as_deref() == Some(provider.id.as_str());
                            let response = Self::card(ui, selected, |ui| {
                                ui.horizontal(|ui| {
                                    Self::status_dot(ui, provider.ready);
                                    ui.add_space(2.0);
                                    ui.label(
                                        egui::RichText::new(&provider.display_name)
                                            .strong()
                                            .size(13.5)
                                            .color(palette::TEXT),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new("›")
                                                    .size(16.0)
                                                    .color(palette::TEXT_MUTED),
                                            );
                                        },
                                    );
                                });
                                let models = provider.models.len();
                                let detail = match provider.ready {
                                    ReadyState::Ready => format!(
                                        "{} · {} modelo{}",
                                        Self::ready_label(provider.ready),
                                        models,
                                        if models == 1 { "" } else { "s" }
                                    ),
                                    _ => format!(
                                        "{} · {}",
                                        Self::ready_label(provider.ready),
                                        provider.status_note
                                    ),
                                };
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(detail)
                                            .size(10.5)
                                            .color(palette::TEXT_DIM),
                                    )
                                    .wrap(),
                                );
                            });
                            if response.clicked() {
                                self.selected_provider = Some(provider.id.clone());
                                self.selected_model_id = provider
                                    .default_model()
                                    .map(|m| m.id.clone())
                                    .unwrap_or_default();
                                self.model_filter.clear();
                                self.screen = DockScreen::PickModel;
                            }
                            ui.add_space(6.0);
                        }
                    });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    if let Some(ago) = self.scanned_ago() {
                        ui.label(
                            egui::RichText::new(format!("Última búsqueda {ago}"))
                                .size(10.0)
                                .color(palette::TEXT_MUTED),
                        );
                    }
                });
            });
        });
    }

    // ----- Pantalla: modelo -----

    fn show_pick_model(&mut self, ui: &mut egui::Ui) {
        Self::paint_panel_background(ui);
        let inner = ui.max_rect().shrink(14.0);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(inner), |ui| {
            let Some(provider_id) = self.selected_provider.clone() else {
                self.screen = DockScreen::PickProvider;
                return;
            };
            let Some(provider) = self.provider_entry(&provider_id).cloned() else {
                self.screen = DockScreen::PickProvider;
                return;
            };

            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    if Self::ghost_button(ui, "‹ Asistentes") {
                        self.screen = DockScreen::PickProvider;
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    Self::status_dot(ui, provider.ready);
                    ui.label(
                        egui::RichText::new(&provider.display_name)
                            .strong()
                            .size(17.0)
                            .color(palette::TEXT),
                    );
                });
                ui.label(
                    egui::RichText::new(format!(
                        "{} · {}",
                        Self::ready_label(provider.ready),
                        provider.status_note
                    ))
                    .size(10.5)
                    .color(if provider.ready == ReadyState::NeedsAuth {
                        palette::WARN
                    } else {
                        palette::TEXT_DIM
                    }),
                );
                ui.add_space(10.0);

                // Modo (solo Cursor)
                if provider.kind.supports_cursor_modes() {
                    ui.label(
                        egui::RichText::new("Modo")
                            .size(12.0)
                            .strong()
                            .color(palette::TEXT_DIM),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let total_w = ui.available_width();
                        let gap = 6.0;
                        let w = (total_w - gap * 2.0) / 3.0;
                        for mode in AgentMode::ALL {
                            let selected = self.picker_mode == mode;
                            let button = egui::Button::new(
                                egui::RichText::new(mode.title()).size(12.0).color(
                                    if selected {
                                        egui::Color32::WHITE
                                    } else {
                                        palette::TEXT_DIM
                                    },
                                ),
                            )
                            .fill(if selected {
                                palette::ACCENT
                            } else {
                                palette::CARD
                            })
                            .corner_radius(9.0)
                            .min_size(egui::vec2(w, 28.0));
                            if ui.add(button).clicked() {
                                self.picker_mode = mode;
                            }
                        }
                    });
                    ui.label(
                        egui::RichText::new(self.picker_mode.description())
                            .size(10.5)
                            .color(palette::TEXT_MUTED),
                    );
                    ui.add_space(10.0);
                }

                // Modelo
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Modelo")
                            .size(12.0)
                            .strong()
                            .color(palette::TEXT_DIM),
                    );
                    if provider.models.len() > 6 {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.model_filter)
                                    .hint_text("Filtrar…")
                                    .desired_width(120.0)
                                    .margin(egui::vec2(8.0, 4.0)),
                            );
                        });
                    }
                });
                ui.add_space(4.0);

                let filter = self.model_filter.trim().to_lowercase();
                let models: Vec<_> = provider
                    .models
                    .iter()
                    .filter(|m| {
                        filter.is_empty()
                            || m.id.to_lowercase().contains(&filter)
                            || m.label.to_lowercase().contains(&filter)
                    })
                    .cloned()
                    .collect();

                let button_h = 44.0;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height((ui.available_height() - button_h).max(60.0))
                    .show(ui, |ui| {
                        if models.is_empty() {
                            ui.label(
                                egui::RichText::new("Ningún modelo coincide.")
                                    .size(11.0)
                                    .color(palette::TEXT_MUTED),
                            );
                        }
                        for model in &models {
                            let selected = self.selected_model_id == model.id;
                            let response = egui::Frame::new()
                                .fill(if selected {
                                    palette::CARD_SELECTED
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .corner_radius(9.0)
                                .inner_margin(egui::Margin::symmetric(10, 6))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(if selected { "●" } else { "○" })
                                                .size(10.0)
                                                .color(if selected {
                                                    palette::ACCENT
                                                } else {
                                                    palette::TEXT_MUTED
                                                }),
                                        );
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&model.label)
                                                    .size(12.5)
                                                    .color(palette::TEXT),
                                            )
                                            .truncate(),
                                        );
                                    });
                                })
                                .response
                                .interact(egui::Sense::click());
                            if response.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if response.clicked() {
                                self.selected_model_id = model.id.clone();
                            }
                        }
                    });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    let can_start = !self.selected_model_id.is_empty();
                    if Self::primary_button(ui, "Empezar a chatear", can_start, true) {
                        self.start_chat();
                    }
                });
            });
        });
    }

    // ----- Pantalla: chat -----

    fn chat_title(&self) -> (String, String) {
        match &self.agent_selection {
            Some(s) => {
                let model_label = s
                    .provider
                    .models
                    .iter()
                    .find(|m| m.id == s.model_id)
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| s.model_id.clone());
                let sub = match s.cursor_mode {
                    Some(mode) => format!("{} · {}", mode.title(), model_label),
                    None => model_label,
                };
                (s.provider.display_name.clone(), sub)
            }
            None => ("Chat".into(), String::new()),
        }
    }

    fn bubble(ui: &mut egui::Ui, role: MessageRole, text: &str, max_w: f32) {
        let (fill, color, align) = match role {
            MessageRole::User => (palette::ACCENT, egui::Color32::WHITE, egui::Align::Max),
            MessageRole::Assistant => (palette::CARD, palette::TEXT, egui::Align::Min),
            MessageRole::Error => (
                egui::Color32::from_rgb(70, 38, 44),
                egui::Color32::from_rgb(255, 180, 188),
                egui::Align::Min,
            ),
        };
        ui.with_layout(egui::Layout::top_down(align), |ui| {
            ui.set_max_width(max_w);
            egui::Frame::new()
                .fill(fill)
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(11, 8))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).size(13.0).color(color)).wrap(),
                    );
                });
        });
    }

    fn show_chat(&mut self, ui: &mut egui::Ui) {
        let project_name = self.active_project.as_ref().map(|p| p.name.clone());
        Self::paint_panel_background(ui);
        let inner = ui.max_rect().shrink(14.0);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.vertical(|ui| {
                // Cabecera
                let (title, subtitle) = self.chat_title();
                ui.horizontal(|ui| {
                    if Self::ghost_button(ui, "‹") {
                        self.screen = DockScreen::PickModel;
                    }
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(title)
                                .strong()
                                .size(14.0)
                                .color(palette::TEXT),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(subtitle)
                                    .size(10.0)
                                    .color(palette::TEXT_MUTED),
                            )
                            .truncate(),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if Self::ghost_button(ui, "Nuevo chat") {
                            self.new_chat();
                        }
                    });
                });
                if let Some(name) = &project_name {
                    let index = self
                        .brain_status
                        .as_ref()
                        .map(|status| {
                            let git = if status.git.available {
                                status.git.branch.as_str()
                            } else {
                                "sin Git"
                            };
                            let freshness = if status.stale {
                                "índice por actualizar"
                            } else {
                                "índice al día"
                            };
                            format!("{name} · {git} · {freshness}")
                        })
                        .unwrap_or_else(|| format!("Proyecto: {name}"));
                    ui.add(
                        egui::Label::new(egui::RichText::new(index).size(9.0).color(palette::TEXT_MUTED))
                            .truncate(),
                    );
                }
                ui.add_space(8.0);
                let input_h = 46.0;
                let messages_h = (ui.available_height() - input_h).max(60.0);
                let max_bubble_w = ui.available_width() * 0.88;
                egui::ScrollArea::vertical()
                    .max_height(messages_h)
                    .min_scrolled_height(messages_h)
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if self.messages.is_empty() && self.pending.is_none() {
                            ui.add_space(messages_h * 0.3);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new("¿En qué te ayudo?")
                                        .size(15.0)
                                        .strong()
                                        .color(palette::TEXT_DIM),
                                );
                                ui.label(
                                    egui::RichText::new("Escribe abajo y pulsa Enter.")
                                        .size(11.0)
                                        .color(palette::TEXT_MUTED),
                                );
                            });
                        }
                        for message in &self.messages {
                            Self::bubble(ui, message.role, &message.text, max_bubble_w);
                            ui.add_space(6.0);
                        }
                        if let Some(pending) = &self.pending {
                            let secs = pending.started.elapsed().as_secs();
                            if pending.text.trim().is_empty() {
                                ui.horizontal(|ui| {
                                    ui.add(egui::Spinner::new().size(12.0).color(palette::ACCENT));
                                    let status = if !self.brain_progress.is_empty() {
                                        self.brain_progress.clone()
                                    } else if pending.thinking {
                                        "Razonando…".into()
                                    } else {
                                        "Conectando…".into()
                                    };
                                    ui.label(
                                        egui::RichText::new(format!("{status} {secs} s"))
                                            .size(11.0)
                                            .color(palette::TEXT_MUTED),
                                    );
                                });
                            } else {
                                Self::bubble(
                                    ui,
                                    MessageRole::Assistant,
                                    &format!("{}▍", pending.text),
                                    max_bubble_w,
                                );
                            }
                            ui.add_space(6.0);
                        }
                    });

                // Entrada
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.horizontal(|ui| {
                        let busy = self.pending.is_some();
                        let button_w = 76.0;
                        let text_edit = egui::TextEdit::singleline(&mut self.draft)
                            .hint_text(if busy {
                                "Esperando respuesta…"
                            } else {
                                "Escribe un mensaje…"
                            })
                            .margin(egui::vec2(10.0, 8.0))
                            .desired_width(ui.available_width() - button_w - 8.0);
                        let response = ui.add_enabled(!busy, text_edit);
                        if self.focus_input && !busy {
                            response.request_focus();
                            self.focus_input = false;
                        }
                        let enter = response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));

                        if busy {
                            if Self::danger_button(ui, "Cancelar") {
                                self.cancel_pending();
                            }
                        } else {
                            let can_send = !self.draft.trim().is_empty();
                            if Self::primary_button(ui, "Enviar", can_send, false) || (enter && can_send)
                            {
                                self.send_draft(ui.ctx());
                            }
                        }
                    });
                });
            });
        });
    }
}

impl eframe::App for YeisonDock {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if !self.overlay_configured {
            macos_overlay::configure_for_fullscreen_overlay(frame);
            self.overlay_configured = true;
        }

        self.poll_ui_events(ctx);
        self.kick_session_sync(ctx);

        // El selector nativo debe abrirse por encima del overlay: lo ocultamos.
        if let Some(kind) = self.pending_picker.take() {
            self.visible = false;
            self.picker_open = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            macos_overlay::sync_overlay_presence(frame, false);
            self.spawn_picker(kind, ctx);
        }

        // Archivos arrastrados sobre el dock alimentan el brain activo.
        if self.active_project.is_some() {
            let dropped: Vec<PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|file| file.path.clone())
                    .collect()
            });
            if !dropped.is_empty() {
                self.ingest_files(dropped);
            }
        }

        // Volver a mostrar el dock cuando termina el selector nativo.
        if self.reopen_after_picker {
            self.reopen_after_picker = false;
            let displays = DisplayInfo::all().unwrap_or_default();
            let (mx, my) = Self::mouse_physical();
            let display = display_index_at_mouse(&displays, mx, my).or(self.display_idx);
            if !displays.is_empty() {
                self.display_idx = Some(display.unwrap_or(0).min(displays.len() - 1));
            }
            self.visible = true;
            self.press_began_on_dock = false;
            self.outside_click_enabled_after = Some(Instant::now() + OPEN_CLICK_GRACE);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }

        if !self.visible {
            self.kick_session_sync(ctx);
            self.sync_visibility(ctx, frame);
            return;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| match self.screen {
                DockScreen::PickProject => self.show_pick_project(ui),
                DockScreen::PickProvider => self.show_pick_provider(ui),
                DockScreen::PickModel => self.show_pick_model(ui),
                DockScreen::Chat => self.show_chat(ui),
            });

        // Después de procesar la UI para que egui registre clics dentro del chat.
        self.kick_session_sync(ctx);
        self.sync_visibility(ctx, frame);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn format_sync_report(report: &BrainReport) -> String {
    let git = if report.git.available {
        format!(
            "rama {} · {} · {}",
            report.git.branch,
            report.git.head_short,
            report.git.dirty_label()
        )
    } else {
        "sin Git".into()
    };
    match report.mode {
        SyncMode::Full => format!(
            "Índice completo: {} archivos, {} símbolos, {} módulos, {:.1} MB. {git}.",
            report.files,
            report.symbols,
            report.modules,
            report.total_bytes as f64 / 1_048_576.0
        ),
        SyncMode::Incremental => format!(
            "Índice incremental: {} nuevos, {} actualizados, {} eliminados. Quedan {} archivos. {git}.",
            report.added, report.updated, report.removed, report.files
        ),
        SyncMode::GitOnly => {
            format!("El código indexado sigue vigente. Git quedó sincronizado: {git}.")
        }
        SyncMode::NoChanges => format!("Índice al día. {git}."),
    }
}

fn main() -> eframe::Result<()> {
    let workspace = std::env::var("CURSOR_WORKSPACE")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    init_workspace(workspace);

    let width = cm_to_px(DOCK_WIDTH_CM);
    let height = cm_to_px(DOCK_HEIGHT_CM);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Yeison")
            .with_inner_size([width, height])
            .with_min_inner_size([width, height])
            .with_max_inner_size([width, height])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_visible(false),
        ..Default::default()
    };

    eframe::run_native(
        "Yeison",
        options,
        Box::new(|cc| {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::TRANSPARENT;
            visuals.window_fill = egui::Color32::TRANSPARENT;
            visuals.extreme_bg_color = egui::Color32::from_rgb(34, 35, 44);
            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(44, 45, 55);
            visuals.selection.bg_fill = egui::Color32::from_rgb(96, 118, 255);
            cc.egui_ctx.set_visuals(visuals);
            Ok(Box::new(YeisonDock::new()))
        }),
    )
}
