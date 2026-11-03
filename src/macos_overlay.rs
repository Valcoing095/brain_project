#[cfg(target_os = "macos")]
fn with_ns_window(frame: &eframe::Frame, apply: impl FnOnce(&objc2_app_kit::NSWindow)) {
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };

    let ns_view = appkit.ns_view.as_ptr().cast::<NSView>();
    if ns_view.is_null() {
        return;
    }
    let ns_view = unsafe { &*ns_view };
    let Some(window) = ns_view.window() else {
        return;
    };

    apply(&window);
}

/// Nivel por encima de apps en pantalla completa (`kCGScreenSaverWindowLevel`).
#[cfg(target_os = "macos")]
const SCREEN_SAVER_LEVEL: isize = 1000;

#[cfg(target_os = "macos")]
pub fn configure_for_fullscreen_overlay(frame: &eframe::Frame) {
    use objc2_app_kit::NSWindowCollectionBehavior;

    with_ns_window(frame, |window| {
        window.setLevel(SCREEN_SAVER_LEVEL);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setHidesOnDeactivate(false);
        window.setCanHide(false);
        window.setIgnoresMouseEvents(true);
    });
}

#[cfg(target_os = "macos")]
pub fn sync_overlay_presence(frame: &eframe::Frame, visible: bool) {
    with_ns_window(frame, |window| {
        if visible {
            window.setIgnoresMouseEvents(false);
            window.setAcceptsMouseMovedEvents(true);
            window.makeKeyAndOrderFront(None);
        } else {
            window.setIgnoresMouseEvents(true);
        }
    });
}

/// La isla del robot vive en otra ventana. La subimos al mismo nivel que el dock
/// para que se vea dentro del notch y sobre las apps en pantalla completa.
#[cfg(target_os = "macos")]
pub fn raise_island_window() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSWindowCollectionBehavior};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();
    for index in 0..windows.count() {
        let window = windows.objectAtIndex(index);
        if window.title().to_string() != "Yeison Isla" {
            continue;
        }
        window.setLevel(SCREEN_SAVER_LEVEL);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setHidesOnDeactivate(false);
        window.setIgnoresMouseEvents(false);
        window.orderFront(None);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn configure_for_fullscreen_overlay(_frame: &eframe::Frame) {}

#[cfg(not(target_os = "macos"))]
pub fn sync_overlay_presence(_frame: &eframe::Frame, _visible: bool) {}

#[cfg(not(target_os = "macos"))]
pub fn raise_island_window() {}
