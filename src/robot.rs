//! El mismo robot, en ocho estados, para la isla del notch.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    Idle,
    Scanning,
    Thinking,
    Working,
    Syncing,
    Alert,
    Error,
    Done,
}

impl Mood {
    pub fn label(self) -> &'static str {
        match self {
            Mood::Idle => "Yeison en espera",
            Mood::Scanning => "Escaneando",
            Mood::Thinking => "Pensando",
            Mood::Working => "Trabajando",
            Mood::Syncing => "Sincronizando",
            Mood::Alert => "Índice por actualizar",
            Mood::Error => "Hubo un error",
            Mood::Done => "Listo",
        }
    }
}

pub struct Signals {
    pub replying: bool,
    pub streaming: bool,
    pub scanning: bool,
    pub syncing: bool,
    pub error: bool,
    pub stale: bool,
    pub just_done: bool,
}

pub fn resolve(signals: Signals) -> Mood {
    if signals.replying {
        return if signals.streaming {
            Mood::Working
        } else {
            Mood::Thinking
        };
    }
    if signals.scanning {
        return Mood::Scanning;
    }
    if signals.syncing {
        return Mood::Syncing;
    }
    if signals.error {
        return Mood::Error;
    }
    if signals.stale {
        return Mood::Alert;
    }
    if signals.just_done {
        return Mood::Done;
    }
    Mood::Idle
}

pub fn paint(painter: &egui::Painter, rect: Rect, mood: Mood, time: f32) {
    let eyes = eye_color(mood);
    let bob = match mood {
        Mood::Working => (time * 7.0).sin() * 0.7,
        Mood::Syncing => (time * 3.0).sin() * 0.35,
        _ => 0.0,
    };
    let shake = if mood == Mood::Error {
        (time * 22.0).sin() * 0.55
    } else {
        0.0
    };
    let center = rect.center() + egui::vec2(shake, bob);
    let head = Rect::from_center_size(center + egui::vec2(0.0, 1.0), egui::vec2(22.0, 16.5));

    painter.circle_filled(center, 16.0, Color32::from_rgba_premultiplied(eyes.r(), eyes.g(), eyes.b(), 28));

    let shell = Color32::from_rgb(46, 50, 62);
    let edge = Color32::from_rgb(118, 126, 150);
    painter.rect_filled(head, 5.0, shell);
    painter.rect_stroke(head, 5.0, Stroke::new(1.1_f32, edge), egui::StrokeKind::Inside);

    let antenna_top = head.center_top() + egui::vec2(0.0, -7.0);
    painter.line_segment(
        [head.center_top() + egui::vec2(0.0, -1.0), antenna_top],
        Stroke::new(1.3_f32, edge),
    );
    painter.circle_filled(antenna_top, 2.1, eyes);

    for side in [-1.0_f32, 1.0] {
        let ear = Rect::from_center_size(
            head.center() + egui::vec2(side * 12.2, 0.4),
            egui::vec2(3.2, 6.0),
        );
        painter.rect_filled(ear, 1.5, shell);
    }

    let look = match mood {
        Mood::Thinking => (time * 2.4).sin() * 1.7,
        Mood::Scanning => (time * 3.2).sin() * 0.8,
        _ => 0.0,
    };
    let blink = matches!(mood, Mood::Idle | Mood::Done) && (time * 0.33).fract() > 0.94;
    let eye_y = head.center().y - 0.4;
    paint_eye(painter, head.center() + egui::vec2(-4.6 + look, eye_y - head.center().y), eyes, blink, mood, time);
    paint_eye(painter, head.center() + egui::vec2(4.6 + look, eye_y - head.center().y), eyes, blink, mood, time);

    if matches!(mood, Mood::Thinking) {
        let brow = Stroke::new(1.2_f32, Color32::from_rgb(186, 194, 214));
        painter.line_segment(
            [head.center() + egui::vec2(-7.2, -5.6), head.center() + egui::vec2(-2.2, -4.4)],
            brow,
        );
        painter.line_segment(
            [head.center() + egui::vec2(7.2, -5.6), head.center() + egui::vec2(2.2, -4.4)],
            brow,
        );
    }

    paint_mouth(painter, head.center() + egui::vec2(0.0, 4.6), mood, eyes);
    paint_extras(painter, head, antenna_top, mood, eyes, time);

    let body = Rect::from_center_size(center + egui::vec2(0.0, 12.2), egui::vec2(14.0, 5.0));
    painter.rect_filled(body, 2.5, shell);
}

fn eye_color(mood: Mood) -> Color32 {
    match mood {
        Mood::Alert => Color32::from_rgb(245, 176, 48),
        Mood::Error => Color32::from_rgb(226, 72, 86),
        Mood::Done => Color32::from_rgb(74, 205, 140),
        Mood::Working => Color32::from_rgb(120, 168, 255),
        _ => Color32::from_rgb(96, 140, 255),
    }
}

fn paint_eye(painter: &egui::Painter, center: Pos2, color: Color32, blink: bool, mood: Mood, time: f32) {
    if blink {
        painter.line_segment(
            [center + egui::vec2(-2.6, 0.0), center + egui::vec2(2.6, 0.0)],
            Stroke::new(1.4_f32, color),
        );
        return;
    }
    painter.circle_filled(center, 3.15, Color32::from_rgb(16, 18, 26));
    painter.circle_filled(center, 2.35, color);
    painter.circle_filled(center + egui::vec2(-0.7, -0.7), 0.85, Color32::from_white_alpha(210));
    if mood == Mood::Scanning {
        let sweep = time * 5.5;
        let dir = egui::vec2(sweep.cos(), sweep.sin());
        painter.circle_stroke(center, 3.5, Stroke::new(1.0_f32, Color32::from_white_alpha(90)));
        painter.line_segment([center, center + dir * 3.3], Stroke::new(1.3_f32, Color32::WHITE));
    }
}

fn paint_mouth(painter: &egui::Painter, center: Pos2, mood: Mood, color: Color32) {
    let stroke = Stroke::new(1.3_f32, color);
    match mood {
        Mood::Done => arc(painter, center + egui::vec2(0.0, -1.0), 3.4, 0.25, std::f32::consts::PI - 0.25, stroke),
        Mood::Error => arc(painter, center + egui::vec2(0.0, 2.4), 3.2, std::f32::consts::PI + 0.3, std::f32::consts::TAU - 0.3, stroke),
        Mood::Thinking | Mood::Scanning => {
            painter.line_segment([center + egui::vec2(-2.2, 0.0), center + egui::vec2(2.2, 0.0)], stroke);
        }
        Mood::Alert => {
            painter.circle_stroke(center, 1.5, stroke);
        }
        _ => {
            painter.line_segment([center + egui::vec2(-2.4, 0.4), center + egui::vec2(2.4, 0.4)], stroke);
        }
    }
}

fn paint_extras(painter: &egui::Painter, head: Rect, antenna: Pos2, mood: Mood, color: Color32, time: f32) {
    match mood {
        Mood::Working => {
            let phase = (time * 3.4).floor() as i32;
            for index in 0..3 {
                let x = head.left() + 5.0 + index as f32 * 6.0;
                let on = phase.rem_euclid(3) == index;
                painter.circle_filled(
                    egui::pos2(x, head.bottom() + 3.4),
                    1.45,
                    if on { color } else { Color32::from_rgb(58, 64, 78) },
                );
            }
        }
        Mood::Syncing => {
            let angle = time * 4.2;
            let orbit = head.width() * 0.78;
            let p = head.center() + egui::vec2(angle.cos(), angle.sin()) * orbit;
            painter.circle_stroke(head.center(), orbit, Stroke::new(1.0_f32, Color32::from_white_alpha(50)));
            painter.circle_filled(p, 2.0, color);
            let tip = p + egui::vec2((-angle).sin(), angle.cos()) * 2.4;
            painter.line_segment([p, tip], Stroke::new(1.3_f32, Color32::WHITE));
        }
        Mood::Alert => {
            let tip = antenna + egui::vec2(7.5, -1.0);
            let left = tip + egui::vec2(-4.2, 7.2);
            let right = tip + egui::vec2(4.2, 7.2);
            let stroke = Stroke::new(1.3_f32, color);
            painter.line_segment([tip, left], stroke);
            painter.line_segment([left, right], stroke);
            painter.line_segment([right, tip], stroke);
            painter.line_segment([tip + egui::vec2(0.0, 2.4), tip + egui::vec2(0.0, 4.4)], stroke);
        }
        Mood::Done => {
            painter.circle_filled(head.left_center() + egui::vec2(3.0, 2.0), 1.2, Color32::from_rgba_premultiplied(255, 150, 170, 160));
            painter.circle_filled(head.right_center() + egui::vec2(-3.0, 2.0), 1.2, Color32::from_rgba_premultiplied(255, 150, 170, 160));
        }
        _ => {}
    }
}

fn arc(painter: &egui::Painter, center: Pos2, radius: f32, start: f32, end: f32, stroke: Stroke) {
    let steps = 7;
    let mut previous = center + egui::vec2(start.cos(), start.sin()) * radius;
    for step in 1..=steps {
        let t = start + (end - start) * (step as f32 / steps as f32);
        let point = center + egui::vec2(t.cos(), t.sin()) * radius;
        painter.line_segment([previous, point], stroke);
        previous = point;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(replying: bool, streaming: bool, scanning: bool, syncing: bool, error: bool, stale: bool, just_done: bool) -> Signals {
        Signals { replying, streaming, scanning, syncing, error, stale, just_done }
    }

    #[test]
    fn la_respuesta_en_curso_manda_sobre_el_resto() {
        assert_eq!(resolve(signals(true, false, true, true, true, true, true)), Mood::Thinking);
        assert_eq!(resolve(signals(true, true, true, true, true, true, true)), Mood::Working);
    }

    #[test]
    fn el_resto_sigue_el_orden_del_brain() {
        assert_eq!(resolve(signals(false, false, true, true, true, true, true)), Mood::Scanning);
        assert_eq!(resolve(signals(false, false, false, true, true, true, true)), Mood::Syncing);
        assert_eq!(resolve(signals(false, false, false, false, true, true, true)), Mood::Error);
        assert_eq!(resolve(signals(false, false, false, false, false, true, true)), Mood::Alert);
        assert_eq!(resolve(signals(false, false, false, false, false, false, true)), Mood::Done);
        assert_eq!(resolve(signals(false, false, false, false, false, false, false)), Mood::Idle);
    }
}
