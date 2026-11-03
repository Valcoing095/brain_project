//! Presenta Markdown como bloques (títulos, listas, código, citas) en egui.

use eframe::egui::{self, Color32, FontFamily, FontId, Stroke, TextFormat};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Clone, Copy)]
pub struct Theme {
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub code_bg: Color32,
    pub quote: Color32,
    pub body: f32,
}

pub fn show(ui: &mut egui::Ui, markdown: &str, theme: &Theme) {
    let blocks = parse(markdown);
    if blocks.is_empty() {
        return;
    }
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        show_blocks(ui, &blocks, theme);
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct Look {
    strong: bool,
    emphasis: bool,
    strike: bool,
    code: bool,
    link: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Piece {
    text: String,
    look: Look,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    checked: Option<bool>,
    blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Block {
    Paragraph(Vec<Piece>),
    Heading(u8, Vec<Piece>),
    Code { lang: String, body: String },
    Rule,
    Quote(Vec<Block>),
    List {
        ordered: bool,
        start: u64,
        items: Vec<Item>,
    },
    Table {
        header: Vec<Vec<Piece>>,
        rows: Vec<Vec<Vec<Piece>>>,
    },
}

#[derive(Default)]
struct Marks {
    strong: u8,
    emphasis: u8,
    strike: u8,
    code: u8,
    link: u8,
}

impl Marks {
    fn look(&self) -> Look {
        Look {
            strong: self.strong > 0,
            emphasis: self.emphasis > 0,
            strike: self.strike > 0,
            code: self.code > 0,
            link: self.link > 0,
        }
    }
}

enum Frame {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List {
        ordered: bool,
        start: u64,
        items: Vec<Item>,
        current: Vec<Block>,
        checked: Option<bool>,
    },
    Table {
        header: Vec<Vec<Piece>>,
        rows: Vec<Vec<Vec<Piece>>>,
        in_head: bool,
        row: Vec<Vec<Piece>>,
        cell: Vec<Piece>,
    },
}

struct Doc {
    stack: Vec<Frame>,
    inline: Vec<Piece>,
    marks: Marks,
    code: Option<(String, String)>,
}

fn parse(markdown: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION;
    let mut doc = Doc {
        stack: vec![Frame::Blocks(Vec::new())],
        inline: Vec::new(),
        marks: Marks::default(),
        code: None,
    };
    for event in Parser::new_ext(markdown, options) {
        doc.on_event(event);
    }
    doc.finish()
}

impl Doc {
    fn on_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(text) => {
                self.marks.code += 1;
                self.push_text(&text);
                self.marks.code -= 1;
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => self.push_text(&text),
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.push_text("\n"),
            Event::Rule => self.push_block(Block::Rule),
            Event::TaskListMarker(checked) => {
                if let Some(Frame::List { checked: slot, .. }) = self.stack.last_mut() {
                    *slot = Some(checked);
                }
            }
            Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::TableCell => {}
            Tag::Heading { .. } => self.inline.clear(),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Indented => String::new(),
                    CodeBlockKind::Fenced(lang) => lang.trim().to_string(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::BlockQuote(_) => self.stack.push(Frame::Quote(Vec::new())),
            Tag::List(start) => self.stack.push(Frame::List {
                ordered: start.is_some(),
                start: start.unwrap_or(1),
                items: Vec::new(),
                current: Vec::new(),
                checked: None,
            }),
            Tag::Item => {
                if let Some(Frame::List { checked, .. }) = self.stack.last_mut() {
                    *checked = None;
                }
            }
            Tag::Table(_) => self.stack.push(Frame::Table {
                header: Vec::new(),
                rows: Vec::new(),
                in_head: false,
                row: Vec::new(),
                cell: Vec::new(),
            }),
            Tag::TableHead => {
                if let Some(Frame::Table { in_head, .. }) = self.stack.last_mut() {
                    *in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
                    row.clear();
                }
            }
            Tag::Emphasis => self.marks.emphasis += 1,
            Tag::Strong => self.marks.strong += 1,
            Tag::Strikethrough => self.marks.strike += 1,
            Tag::Link { .. } => self.marks.link += 1,
            Tag::Image { .. } => {}
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.finish_paragraph(),
            TagEnd::Heading(level) => {
                let pieces = self.take_inline();
                self.push_block(Block::Heading(level_number(level), pieces));
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut body)) = self.code.take() {
                    if body.ends_with('\n') {
                        body.pop();
                    }
                    self.push_block(Block::Code { lang, body });
                }
            }
            TagEnd::BlockQuote(_) => {
                if let Some(Frame::Quote(blocks)) = self.stack.pop() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::Item => self.close_item(),
            TagEnd::List(_) => self.close_list(),
            TagEnd::TableCell => self.close_cell(),
            TagEnd::TableRow => self.close_row(false),
            TagEnd::TableHead => self.close_row(true),
            TagEnd::Table => self.close_table(),
            TagEnd::Emphasis => self.marks.emphasis = self.marks.emphasis.saturating_sub(1),
            TagEnd::Strong => self.marks.strong = self.marks.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.marks.strike = self.marks.strike.saturating_sub(1),
            TagEnd::Link => self.marks.link = self.marks.link.saturating_sub(1),
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some((_, body)) = self.code.as_mut() {
            body.push_str(text);
            return;
        }
        let look = self.marks.look();
        if let Some(last) = self.inline.last_mut() {
            if last.look == look {
                last.text.push_str(text);
                return;
            }
        }
        self.inline.push(Piece {
            text: text.to_string(),
            look,
        });
    }

    fn take_inline(&mut self) -> Vec<Piece> {
        std::mem::take(&mut self.inline)
    }

    fn pieces_blank(pieces: &[Piece]) -> bool {
        pieces.iter().all(|piece| piece.text.trim().is_empty())
    }

    fn finish_paragraph(&mut self) {
        let pieces = self.take_inline();
        if Self::pieces_blank(&pieces) {
            return;
        }
        if let Some(Frame::Table { cell, .. }) = self.stack.last_mut() {
            if !cell.is_empty() {
                cell.push(Piece {
                    text: " ".into(),
                    look: Look::default(),
                });
            }
            cell.extend(pieces);
            return;
        }
        self.push_block(Block::Paragraph(pieces));
    }

    fn close_item(&mut self) {
        self.finish_paragraph();
        let Some(Frame::List {
            items,
            current,
            checked,
            ..
        }) = self.stack.last_mut()
        else {
            return;
        };
        let blocks = std::mem::take(current);
        let mark = checked.take();
        if !blocks.is_empty() {
            items.push(Item {
                checked: mark,
                blocks,
            });
        }
    }

    fn close_list(&mut self) {
        self.close_item();
        let Some(frame) = self.stack.pop() else {
            return;
        };
        if let Frame::List {
            ordered,
            start,
            items,
            ..
        } = frame
        {
            self.push_block(Block::List {
                ordered,
                start,
                items,
            });
        }
    }

    fn close_cell(&mut self) {
        let pieces = self.take_inline();
        let Some(Frame::Table { row, cell, .. }) = self.stack.last_mut() else {
            return;
        };
        let mut content = std::mem::take(cell);
        if !Self::pieces_blank(&pieces) {
            if !content.is_empty() {
                content.push(Piece {
                    text: " ".into(),
                    look: Look::default(),
                });
            }
            content.extend(pieces);
        }
        row.push(content);
    }

    fn close_row(&mut self, head: bool) {
        let Some(Frame::Table {
            header,
            rows,
            in_head,
            row,
            ..
        }) = self.stack.last_mut()
        else {
            return;
        };
        let finished = std::mem::take(row);
        if head || *in_head {
            if !finished.is_empty() {
                *header = finished;
            }
            *in_head = false;
        } else if !finished.is_empty() {
            rows.push(finished);
        }
    }

    fn close_table(&mut self) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        if let Frame::Table { header, rows, .. } = frame {
            self.push_block(Block::Table { header, rows });
        }
    }

    fn push_block(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Frame::Blocks(blocks) | Frame::Quote(blocks)) => blocks.push(block),
            Some(Frame::List { current, .. }) => current.push(block),
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.finish_paragraph();
        if let Some((lang, mut body)) = self.code.take() {
            if body.ends_with('\n') {
                body.pop();
            }
            self.push_block(Block::Code { lang, body });
        }
        while self.stack.len() > 1 {
            match self.stack.last() {
                Some(Frame::List { .. }) => self.close_list(),
                Some(Frame::Quote(_)) => {
                    if let Some(Frame::Quote(blocks)) = self.stack.pop() {
                        self.push_block(Block::Quote(blocks));
                    }
                }
                Some(Frame::Table { .. }) => self.close_table(),
                _ => {
                    self.stack.pop();
                }
            }
        }
        match self.stack.pop() {
            Some(Frame::Blocks(blocks)) => blocks,
            _ => Vec::new(),
        }
    }
}

fn level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn show_blocks(ui: &mut egui::Ui, blocks: &[Block], theme: &Theme) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            let tight = matches!(block, Block::List { .. } | Block::Paragraph(_))
                && matches!(
                    blocks.get(index - 1),
                    Some(Block::Heading(_, _) | Block::Paragraph(_))
                );
            ui.add_space(if tight { 4.0 } else { 8.0 });
        }
        show_block(ui, block, theme);
    }
}

fn show_block(ui: &mut egui::Ui, block: &Block, theme: &Theme) {
    match block {
        Block::Paragraph(pieces) => show_rich(ui, pieces, theme, theme.body, false),
        Block::Heading(level, pieces) => {
            let size = match level {
                1 => theme.body + 6.0,
                2 => theme.body + 4.0,
                3 => theme.body + 2.0,
                _ => theme.body + 1.0,
            };
            show_rich(ui, pieces, theme, size, true);
        }
        Block::Code { lang, body } => show_code(ui, lang, body, theme),
        Block::Rule => show_rule(ui, theme),
        Block::Quote(blocks) => show_quote(ui, blocks, theme),
        Block::List {
            ordered,
            start,
            items,
        } => show_list(ui, *ordered, *start, items, theme),
        Block::Table { header, rows } => show_table(ui, header, rows, theme),
    }
}

fn show_rich(ui: &mut egui::Ui, pieces: &[Piece], theme: &Theme, size: f32, heading: bool) {
    if pieces.is_empty() {
        return;
    }
    let width = ui.available_width().max(8.0);
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = width;
    for piece in pieces {
        let family = if piece.look.code {
            FontFamily::Monospace
        } else if heading || piece.look.strong {
            FontFamily::Name("YeisonBold".into())
        } else {
            FontFamily::Proportional
        };
        let color = if piece.look.link {
            theme.accent
        } else {
            theme.text
        };
        job.append(
            &piece.text,
            0.0,
            TextFormat {
                font_id: FontId::new(
                    if piece.look.code {
                        (size - 1.0).max(11.0)
                    } else {
                        size
                    },
                    family,
                ),
                color,
                italics: piece.look.emphasis,
                background: if piece.look.code {
                    theme.code_bg
                } else {
                    Color32::TRANSPARENT
                },
                strikethrough: if piece.look.strike {
                    Stroke::new(1.0_f32, theme.muted)
                } else {
                    Stroke::NONE
                },
                underline: if piece.look.link {
                    Stroke::new(1.0_f32, theme.accent)
                } else {
                    Stroke::NONE
                },
                ..Default::default()
            },
        );
    }
    ui.add(egui::Label::new(job));
}

fn show_code(ui: &mut egui::Ui, lang: &str, body: &str, theme: &Theme) {
    egui::Frame::new()
        .fill(theme.code_bg)
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !lang.is_empty() {
                ui.label(
                    egui::RichText::new(lang)
                        .size(10.0)
                        .color(theme.muted)
                        .monospace(),
                );
            }
            ui.add(
                egui::Label::new(
                    egui::RichText::new(body)
                        .monospace()
                        .size(12.0)
                        .color(theme.text),
                )
                .wrap(),
            );
        });
}

fn show_rule(ui: &mut egui::Ui, theme: &Theme) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 8.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0_f32, theme.muted),
    );
}

fn show_quote(ui: &mut egui::Ui, blocks: &[Block], theme: &Theme) {
    let inner = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 12,
            right: 0,
            top: 0,
            bottom: 0,
        })
        .show(ui, |ui| {
            show_blocks(ui, blocks, theme);
        });
    let rect = inner.response.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height().max(8.0))),
        2.0,
        theme.quote,
    );
}

fn show_list(ui: &mut egui::Ui, ordered: bool, start: u64, items: &[Item], theme: &Theme) {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            ui.add_space(3.0);
        }
        let number = start + index as u64;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            show_marker(ui, theme, ordered, number, item.checked);
            ui.vertical(|ui| {
                ui.set_min_width(ui.available_width());
                show_blocks(ui, &item.blocks, theme);
            });
        });
    }
}

fn show_marker(ui: &mut egui::Ui, theme: &Theme, ordered: bool, number: u64, checked: Option<bool>) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, theme.body + 6.0), egui::Sense::hover());
    let painter = ui.painter();
    let center = egui::pos2(rect.left() + 7.0, rect.center().y);
    let stroke = Stroke::new(1.4_f32, theme.accent);
    match checked {
        Some(done) => {
            let box_rect = egui::Rect::from_center_size(center, egui::vec2(11.0, 11.0));
            painter.rect_stroke(box_rect, 2.0, stroke, egui::StrokeKind::Inside);
            if done {
                painter.line_segment([center + egui::vec2(-3.0, 0.2), center + egui::vec2(-0.8, 2.4)], stroke);
                painter.line_segment([center + egui::vec2(-0.8, 2.4), center + egui::vec2(3.4, -2.6)], stroke);
            }
        }
        None if ordered => {
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                format!("{number}."),
                FontId::proportional((theme.body - 1.0).max(11.0)),
                theme.muted,
            );
        }
        None => {
            painter.circle_filled(center, 2.5, theme.accent);
        }
    }
}

fn show_table(ui: &mut egui::Ui, header: &[Vec<Piece>], rows: &[Vec<Vec<Piece>>], theme: &Theme) {
    let columns = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(1);
    if !header.is_empty() {
        show_table_row(ui, header, columns, theme, true);
        show_rule(ui, theme);
    }
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            ui.add_space(4.0);
        }
        show_table_row(ui, row, columns, theme, false);
    }
}

fn show_table_row(ui: &mut egui::Ui, cells: &[Vec<Piece>], columns: usize, theme: &Theme, header: bool) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let gaps = 8.0 * columns.saturating_sub(1) as f32;
        let width = ((ui.available_width() - gaps) / columns as f32).max(24.0);
        for index in 0..columns {
            let cell = cells.get(index).map(Vec::as_slice).unwrap_or(&[]);
            ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_width(width);
                    ui.set_max_width(width);
                    if cell.is_empty() {
                        ui.label(egui::RichText::new(" ").size(theme.body));
                    } else if header {
                        let strong: Vec<Piece> = cell
                            .iter()
                            .cloned()
                            .map(|mut piece| {
                                piece.look.strong = true;
                                piece
                            })
                            .collect();
                        show_rich(ui, &strong, theme, theme.body, false);
                    } else {
                        show_rich(ui, cell, theme, theme.body, false);
                    }
                },
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arma_titulo_lista_codigo_y_enfasis() {
        let doc = parse("# Titulo\n\nTexto **fuerte** y `codigo`.\n\n- uno\n- dos\n\n```rust\nfn hola() {}\n```\n");
        assert!(matches!(doc[0], Block::Heading(1, _)));
        match &doc[1] {
            Block::Paragraph(pieces) => {
                assert!(pieces.iter().any(|piece| piece.look.strong && piece.text.contains("fuerte")));
                assert!(pieces.iter().any(|piece| piece.look.code && piece.text.contains("codigo")));
            }
            other => panic!("se esperaba un párrafo, llegó {other:?}"),
        }
        match &doc[2] {
            Block::List { ordered, items, .. } => {
                assert!(!ordered);
                assert_eq!(items.len(), 2);
            }
            other => panic!("se esperaba una lista, llegó {other:?}"),
        }
        match &doc[3] {
            Block::Code { lang, body } => {
                assert_eq!(lang, "rust");
                assert!(body.contains("fn hola"));
            }
            other => panic!("se esperaba código, llegó {other:?}"),
        }
    }

    #[test]
    fn cita_tarea_y_tabla() {
        let doc = parse("> una cita\n\n- [x] hecho\n- [ ] pendiente\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n");
        assert!(matches!(doc[0], Block::Quote(_)));
        match &doc[1] {
            Block::List { items, .. } => {
                assert_eq!(items[0].checked, Some(true));
                assert_eq!(items[1].checked, Some(false));
            }
            other => panic!("se esperaba una lista de tareas, llegó {other:?}"),
        }
        match &doc[2] {
            Block::Table { header, rows } => {
                assert_eq!(header.len(), 2);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].len(), 2);
            }
            other => panic!("se esperaba una tabla, llegó {other:?}"),
        }
    }
}
