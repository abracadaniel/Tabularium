//! SQL code editor widget: syntax highlighting, line numbers, code completion,
//! inspections, find/replace, live templates and line commenting.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::text::{CCursor, CCursorRange, LayoutJob};
use egui::{Color32, FontId, Galley, Key, Modifiers, Stroke, TextFormat};
use tabularium_core::Dialect;
use tabularium_sql::catalog::SchemaCatalog;
use tabularium_sql::complete::{Completion, CompletionKind};
use tabularium_sql::inspect::{Diagnostic, Severity};

use crate::theme::Palette;

/// Environment the editor needs each frame.
pub struct EditorEnv<'a> {
    pub catalog: Option<&'a SchemaCatalog>,
    pub font_size: f32,
    pub auto_completion: bool,
    pub inspections: bool,
    pub line_numbers: bool,
}

#[derive(Default)]
pub struct EditorResponse {
    pub changed: bool,
    pub has_focus: bool,
}

struct CompletionState {
    completion: Completion,
    selected: usize,
}

#[derive(Default)]
struct FindBar {
    open: bool,
    query: String,
    replacement: String,
    match_case: bool,
    focus: bool,
}

pub struct CodeEditor {
    pub text: String,
    pub dialect: Dialect,
    pub read_only: bool,
    id: egui::Id,
    /// Caret position (byte offset) as of the last frame.
    cursor: usize,
    /// Selected byte range (ordered) as of the last frame, if non-empty.
    selection: Option<(usize, usize)>,
    pending_select: Option<(usize, usize)>,
    completion: Option<CompletionState>,
    diagnostics: Vec<Diagnostic>,
    diag_hash: u64,
    diag_due: Option<Instant>,
    /// Range flagged by the last execution error.
    pub error_range: Option<(usize, usize)>,
    find: FindBar,
    pub request_focus: bool,
    highlight_current_statement: bool,
    layout_cache: RefCell<Option<(u64, Arc<Galley>)>>,
    pending_command: Option<EditorCommand>,
}

impl CodeEditor {
    pub fn new(id_source: impl Hash, text: String, dialect: Dialect) -> Self {
        Self {
            cursor: text.len(),
            text,
            dialect,
            read_only: false,
            id: egui::Id::new(("code_editor", id_source)),
            selection: None,
            pending_select: None,
            completion: None,
            diagnostics: vec![],
            diag_hash: 0,
            diag_due: Some(Instant::now()),
            error_range: None,
            find: FindBar::default(),
            request_focus: false,
            highlight_current_statement: true,
            layout_cache: RefCell::new(None),
            pending_command: None,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self.highlight_current_statement = false;
        self
    }

    /// Caret byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor.min(self.text.len())
    }

    /// Selected text, if any.
    pub fn selected_text(&self) -> Option<&str> {
        let (a, b) = self.selection?;
        self.text.get(a..b).filter(|s| !s.trim().is_empty())
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        self.selection
    }

    /// Select a byte range (applied on the next frame) and focus the editor.
    pub fn select_range(&mut self, start: usize, end: usize) {
        let start = floor_char_boundary(&self.text, start);
        let end = floor_char_boundary(&self.text, end);
        self.pending_select = Some((start, end));
        self.request_focus = true;
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.cursor.min(self.text.len());
        self.selection = None;
        self.diag_due = Some(Instant::now());
        self.error_range = None;
    }

    /// Replace a byte range and put the caret after the inserted text.
    pub fn replace_range(&mut self, start: usize, end: usize, with: &str) {
        let start = floor_char_boundary(&self.text, start.min(self.text.len()));
        let end = floor_char_boundary(&self.text, end.min(self.text.len())).max(start);
        self.text.replace_range(start..end, with);
        let caret = start + with.len();
        self.select_range(caret, caret);
        self.diag_due = Some(Instant::now() + Duration::from_millis(300));
    }

    pub fn open_find(&mut self) {
        self.find.open = true;
        self.find.focus = true;
        if let Some(sel) = self.selected_text() {
            if !sel.contains('\n') {
                self.find.query = sel.to_string();
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, env: &EditorEnv) -> EditorResponse {
        let palette = Palette::current(ui.ctx());
        let font_id = FontId::monospace(env.font_size);
        let mut resp = EditorResponse::default();

        if self.find.open {
            self.find_bar_ui(ui);
        }

        let has_focus = ui.memory(|m| m.has_focus(self.id));
        // ---- keyboard handling before the TextEdit sees the events ----------------------
        let mut accept_completion = false;
        let mut trigger_completion = false;
        if has_focus && !self.read_only {
            ui.input_mut(|i| {
                if self.completion.is_some() {
                    if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                        if let Some(c) = &mut self.completion {
                            c.selected = (c.selected + 1).min(c.completion.items.len().saturating_sub(1));
                        }
                    }
                    if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        if let Some(c) = &mut self.completion {
                            c.selected = c.selected.saturating_sub(1);
                        }
                    }
                    if i.consume_key(Modifiers::NONE, Key::PageDown) {
                        if let Some(c) = &mut self.completion {
                            c.selected = (c.selected + 10).min(c.completion.items.len().saturating_sub(1));
                        }
                    }
                    if i.consume_key(Modifiers::NONE, Key::PageUp) {
                        if let Some(c) = &mut self.completion {
                            c.selected = c.selected.saturating_sub(10);
                        }
                    }
                    if i.consume_key(Modifiers::NONE, Key::Escape) {
                        self.completion = None;
                    }
                    if i.modifiers.is_none()
                        && (i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab))
                    {
                        accept_completion = true;
                    }
                }
                if i.consume_key(Modifiers::COMMAND, Key::Space) || i.consume_key(Modifiers::CTRL, Key::Space) {
                    trigger_completion = true;
                    i.events.retain(|e| !matches!(e, egui::Event::Text(t) if t == " "));
                }
                if i.consume_key(Modifiers::COMMAND, Key::Slash) {
                    self.pending_command = Some(EditorCommand::ToggleComment);
                }
                if i.consume_key(Modifiers::COMMAND, Key::D) {
                    self.pending_command = Some(EditorCommand::DuplicateLine);
                }
                if i.consume_key(Modifiers::COMMAND, Key::F) {
                    self.pending_command = Some(EditorCommand::Find);
                }
                if i.consume_key(Modifiers::NONE, Key::F3) {
                    self.pending_command = Some(EditorCommand::FindNext);
                }
                if self.completion.is_none() && i.modifiers.is_none() && i.key_pressed(Key::Tab)
                    && self.template_at_cursor().is_some()
                {
                    i.consume_key(Modifiers::NONE, Key::Tab);
                    self.pending_command = Some(EditorCommand::ExpandTemplate);
                }
            });
        }
        if let Some(cmd) = self.pending_command.take() {
            self.run_command(cmd);
        }
        if accept_completion {
            self.accept_completion();
        }

        // ---- apply a programmatic selection ---------------------------------------------
        if let Some((a, b)) = self.pending_select.take() {
            let mut state = egui::TextEdit::load_state(ui.ctx(), self.id).unwrap_or_default();
            let ca = byte_to_char(&self.text, a);
            let cb = byte_to_char(&self.text, b);
            state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(ca), CCursor::new(cb))));
            state.store(ui.ctx(), self.id);
        }

        // ---- layout with highlighting ---------------------------------------------------
        let dialect = self.dialect;
        let diagnostics = &self.diagnostics;
        let error_range = self.error_range;
        let find_query = if self.find.open && !self.find.query.is_empty() {
            Some((self.find.query.clone(), self.find.match_case))
        } else {
            None
        };
        let current_stmt = if self.highlight_current_statement && has_focus {
            tabularium_sql::split::statement_at(&self.text, self.cursor(), dialect).map(|s| (s.start, s.end))
        } else {
            None
        };
        let cache = &self.layout_cache;
        let font_for_layout = font_id.clone();
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| -> Arc<Galley> {
            let text = buf.as_str();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            font_for_layout.size.to_bits().hash(&mut h);
            ui.visuals().dark_mode.hash(&mut h);
            for d in diagnostics {
                (d.start, d.end, d.severity as u8).hash(&mut h);
            }
            error_range.hash(&mut h);
            find_query.hash(&mut h);
            current_stmt.hash(&mut h);
            let key = h.finish();
            if let Some((k, g)) = cache.borrow().as_ref() {
                if *k == key {
                    return g.clone();
                }
            }
            let job = highlight_job(
                text,
                dialect,
                &font_for_layout,
                &palette,
                diagnostics,
                error_range,
                find_query.as_ref().map(|(q, c)| (q.as_str(), *c)),
                current_stmt,
            );
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            *cache.borrow_mut() = Some((key, galley.clone()));
            galley
        };

        let line_count = self.text.split('\n').count().max(1);
        let row_height = ui.fonts_mut(|f| f.row_height(&font_id));
        let output = egui::ScrollArea::both()
            .id_salt(self.id.with("scroll"))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    if env.line_numbers {
                        let digits = line_count.to_string().len().max(2);
                        let gutter_width = (digits as f32 + 1.0) * env.font_size * 0.62;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(gutter_width, row_height * line_count as f32 + 4.0),
                            egui::Sense::hover(),
                        );
                        let painter = ui.painter_at(rect);
                        let clip = ui.clip_rect();
                        let first = (((clip.top() - rect.top()) / row_height).floor().max(0.0)) as usize;
                        let last = (((clip.bottom() - rect.top()) / row_height).ceil() as usize + 1).min(line_count);
                        let diag_lines: Vec<(usize, Severity)> = self
                            .diagnostics
                            .iter()
                            .map(|d| (line_of(&self.text, d.start), d.severity))
                            .collect();
                        for line in first..last {
                            let y = rect.top() + 2.0 + line as f32 * row_height;
                            let mut color = palette.gutter;
                            if let Some((_, sev)) = diag_lines.iter().filter(|(l, _)| *l == line).max_by_key(|(_, s)| *s) {
                                color = match sev {
                                    Severity::Error => palette.error,
                                    Severity::Warning => palette.warning,
                                    Severity::Info => palette.gutter,
                                };
                            }
                            painter.text(
                                egui::pos2(rect.right() - env.font_size * 0.6, y),
                                egui::Align2::RIGHT_TOP,
                                (line + 1).to_string(),
                                font_id.clone(),
                                color,
                            );
                        }
                    }
                    if self.read_only {
                        // A read-only buffer: let the user select/copy but not edit.
                        let mut ro = self.text.as_str();
                        egui::TextEdit::multiline(&mut ro)
                            .id(self.id)
                            .font(font_id.clone())
                            .code_editor()
                            .desired_width(f32::INFINITY)
                            .frame(false)
                            .layouter(&mut layouter)
                            .show(ui)
                    } else {
                        egui::TextEdit::multiline(&mut self.text)
                            .id(self.id)
                            .font(font_id.clone())
                            .code_editor()
                            .lock_focus(true)
                            .desired_width(f32::INFINITY)
                            .desired_rows(8)
                            .frame(false)
                            .layouter(&mut layouter)
                            .show(ui)
                    }
                })
                .inner
            })
            .inner;

        if self.request_focus {
            output.response.request_focus();
            self.request_focus = false;
        }
        resp.changed = output.response.changed();
        resp.has_focus = output.response.has_focus();

        if let Some(range) = output.cursor_range {
            let a = char_to_byte(&self.text, range.primary.index);
            let b = char_to_byte(&self.text, range.secondary.index);
            self.cursor = a;
            self.selection = if a == b { None } else { Some((a.min(b), a.max(b))) };
        }

        if resp.changed {
            self.error_range = None;
            self.diag_due = Some(Instant::now() + Duration::from_millis(350));
            *self.layout_cache.borrow_mut() = None;
            if env.auto_completion && !self.read_only {
                let before = self.text[..self.cursor()].chars().next_back();
                let typed_ident = before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
                if typed_ident {
                    trigger_completion = true;
                } else {
                    self.completion = None;
                }
            }
        } else if self.completion.is_some() && output.response.clicked() {
            self.completion = None;
        }

        if trigger_completion && !self.read_only {
            if let Some(catalog) = env.catalog {
                self.refresh_completion(catalog);
            } else {
                self.refresh_completion(&SchemaCatalog::default());
            }
        }
        if !resp.has_focus && !has_focus {
            self.completion = None;
        }

        // ---- inspections (debounced) ------------------------------------------------------
        if env.inspections {
            if let Some(due) = self.diag_due {
                if Instant::now() >= due {
                    self.diag_due = None;
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    self.text.hash(&mut h);
                    let hash = h.finish();
                    if hash != self.diag_hash || env.catalog.is_some() {
                        self.diag_hash = hash;
                        self.diagnostics = tabularium_sql::inspect::inspect(&self.text, self.dialect, env.catalog);
                        *self.layout_cache.borrow_mut() = None;
                    }
                } else {
                    ui.ctx().request_repaint_after(due - Instant::now());
                }
            }
        } else if !self.diagnostics.is_empty() {
            self.diagnostics.clear();
        }

        // ---- hover tooltips for diagnostics ----------------------------------------------
        if let Some(hover) = output.response.hover_pos() {
            let local = hover - output.galley_pos;
            let cc = output.galley.cursor_from_pos(local);
            let byte = char_to_byte(&self.text, cc.index);
            let msgs: Vec<&Diagnostic> =
                self.diagnostics.iter().filter(|d| d.start <= byte && byte < d.end.max(d.start + 1)).collect();
            if !msgs.is_empty() {
                output.response.clone().on_hover_ui_at_pointer(|ui| {
                    for d in msgs {
                        let color = match d.severity {
                            Severity::Error => palette.error,
                            Severity::Warning => palette.warning,
                            Severity::Info => palette.info,
                        };
                        ui.colored_label(color, &d.message);
                    }
                });
            }
        }

        // ---- completion popup -------------------------------------------------------------
        if let Some(state) = &mut self.completion {
            if state.completion.items.is_empty() {
                self.completion = None;
            } else {
                let caret = byte_to_char(&self.text, state.completion.replace_start.min(self.text.len()));
                let rect = output.galley.pos_from_cursor(CCursor::new(caret));
                let pos = output.galley_pos + rect.left_bottom().to_vec2() + egui::vec2(0.0, 2.0);
                let mut clicked = None;
                egui::Area::new(self.id.with("completion"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_min_width(260.0);
                            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                for (i, item) in state.completion.items.iter().enumerate() {
                                    let selected = i == state.selected;
                                    let r = ui.horizontal(|ui| {
                                        ui.colored_label(kind_color(item.kind, &palette), kind_icon(item.kind));
                                        let label = ui.selectable_label(
                                            selected,
                                            egui::RichText::new(&item.label).monospace(),
                                        );
                                        if let Some(d) = &item.detail {
                                            ui.weak(d);
                                        }
                                        label
                                    });
                                    if selected {
                                        r.inner.scroll_to_me(None);
                                    }
                                    if r.inner.clicked() {
                                        clicked = Some(i);
                                    }
                                }
                            });
                        });
                    });
                if let Some(i) = clicked {
                    state.selected = i;
                    self.accept_completion();
                }
            }
        }

        resp
    }

    fn refresh_completion(&mut self, catalog: &SchemaCatalog) {
        let cursor = self.cursor();
        let completion = tabularium_sql::complete::complete(&self.text, cursor, self.dialect, catalog);
        if completion.items.is_empty() {
            self.completion = None;
        } else {
            self.completion = Some(CompletionState { completion, selected: 0 });
        }
    }

    fn accept_completion(&mut self) {
        let Some(state) = self.completion.take() else { return };
        let Some(item) = state.completion.items.get(state.selected) else { return };
        let start = state.completion.replace_start.min(self.text.len());
        let end = state.completion.replace_end.max(self.cursor()).min(self.text.len()).max(start);
        if item.kind == CompletionKind::Snippet {
            self.insert_template(start, end, &item.insert_text);
        } else {
            let mut insert = item.insert_text.clone();
            if item.kind == CompletionKind::Function && !self.text[end..].starts_with('(') {
                insert.push_str("()");
                self.replace_range(start, end, &insert);
                let caret = start + insert.len() - 1;
                self.select_range(caret, caret);
                return;
            }
            self.replace_range(start, end, &insert);
        }
    }

    fn insert_template(&mut self, start: usize, end: usize, body: &str) {
        // Indent continuation lines like the line the template is expanded on.
        let line_start = self.text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let indent: String = self.text[line_start..start].chars().take_while(|c| c.is_whitespace()).collect();
        let body = body.replace('\n', &format!("\n{indent}"));
        let caret_marker = "$CURSOR$";
        let caret_off = body.find(caret_marker);
        let clean = body.replace(caret_marker, "");
        self.replace_range(start, end, &clean);
        if let Some(off) = caret_off {
            self.select_range(start + off, start + off);
        }
    }

    fn template_at_cursor(&self) -> Option<(usize, tabularium_sql::templates::Template)> {
        let cursor = self.cursor();
        let before = &self.text[..cursor];
        let word_start = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map(|(i, _)| i)?;
        let word = &before[word_start..];
        if word.is_empty() {
            return None;
        }
        let after = self.text[cursor..].chars().next();
        if after.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        tabularium_sql::templates::templates(self.dialect)
            .into_iter()
            .find(|t| t.abbrev.eq_ignore_ascii_case(word))
            .map(|t| (word_start, t))
    }

    fn run_command(&mut self, cmd: EditorCommand) {
        match cmd {
            EditorCommand::ToggleComment => self.toggle_comment(),
            EditorCommand::DuplicateLine => {
                let (a, b) = self.selection.unwrap_or((self.cursor(), self.cursor()));
                let ls = self.text[..a].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let le = self.text[b..].find('\n').map(|i| b + i).unwrap_or(self.text.len());
                let chunk = self.text[ls..le].to_string();
                self.text.insert_str(le, &format!("\n{chunk}"));
                let caret = self.cursor() + chunk.len() + 1;
                self.select_range(caret, caret);
                *self.layout_cache.borrow_mut() = None;
            }
            EditorCommand::Find => self.open_find(),
            EditorCommand::FindNext => self.find_next(false),
            EditorCommand::ExpandTemplate => {
                if let Some((start, t)) = self.template_at_cursor() {
                    let end = self.cursor();
                    self.insert_template(start, end, t.body);
                }
            }
        }
    }

    /// Toggle `-- ` on every line touched by the selection (or the caret line).
    pub fn toggle_comment(&mut self) {
        let (a, b) = self.selection.unwrap_or((self.cursor(), self.cursor()));
        let ls = self.text[..a].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let le = self.text[b..].find('\n').map(|i| b + i).unwrap_or(self.text.len());
        let block = &self.text[ls..le];
        let lines: Vec<&str> = block.split('\n').collect();
        let all_commented =
            lines.iter().filter(|l| !l.trim().is_empty()).all(|l| l.trim_start().starts_with("--"));
        let new_lines: Vec<String> = lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    l.to_string()
                } else if all_commented {
                    let idx = l.find("--").unwrap_or(0);
                    let rest = &l[idx + 2..];
                    let rest = rest.strip_prefix(' ').unwrap_or(rest);
                    format!("{}{}", &l[..idx], rest)
                } else {
                    format!("-- {l}")
                }
            })
            .collect();
        let new_block = new_lines.join("\n");
        self.text.replace_range(ls..le, &new_block);
        self.select_range(ls, ls + new_block.len());
        *self.layout_cache.borrow_mut() = None;
        self.diag_due = Some(Instant::now());
    }

    fn find_matches(&self) -> Vec<(usize, usize)> {
        find_all(&self.text, &self.find.query, self.find.match_case)
    }

    fn find_next(&mut self, backwards: bool) {
        let matches = self.find_matches();
        if matches.is_empty() {
            return;
        }
        let cur = self.selection.map(|(a, _)| a).unwrap_or(self.cursor());
        let m = if backwards {
            matches.iter().rev().find(|(a, _)| *a < cur).or(matches.last())
        } else {
            matches.iter().find(|(a, _)| *a > cur).or(matches.first())
        };
        if let Some(&(a, b)) = m {
            self.select_range(a, b);
        }
    }

    fn find_bar_ui(&mut self, ui: &mut egui::Ui) {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label("Find:");
            let r = ui.add(egui::TextEdit::singleline(&mut self.find.query).desired_width(180.0));
            if self.find.focus {
                r.request_focus();
                self.find.focus = false;
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.find_next(ui.input(|i| i.modifiers.shift));
                self.find.focus = true;
            }
            if ui.small_button("▲").on_hover_text("Previous (Shift+Enter)").clicked() {
                self.find_next(true);
            }
            if ui.small_button("▼").on_hover_text("Next (Enter / F3)").clicked() {
                self.find_next(false);
            }
            ui.checkbox(&mut self.find.match_case, "Aa").on_hover_text("Match case");
            let n = if self.find.query.is_empty() { 0 } else { self.find_matches().len() };
            ui.weak(format!("{n} matches"));
            ui.separator();
            ui.label("Replace:");
            ui.add(egui::TextEdit::singleline(&mut self.find.replacement).desired_width(140.0));
            if ui.button("Replace").clicked() {
                if let Some((a, b)) = self.selection {
                    let matches = self.find_matches();
                    if matches.contains(&(a, b)) {
                        let rep = self.find.replacement.clone();
                        self.replace_range(a, b, &rep);
                    }
                }
                self.find_next(false);
            }
            if ui.button("Replace all").clicked() {
                let rep = self.find.replacement.clone();
                for (a, b) in self.find_matches().into_iter().rev() {
                    self.text.replace_range(a..b, &rep);
                }
                *self.layout_cache.borrow_mut() = None;
            }
            if ui.small_button("✕").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if close {
            self.find.open = false;
            self.request_focus = true;
            *self.layout_cache.borrow_mut() = None;
        }
    }
}

enum EditorCommand {
    ToggleComment,
    DuplicateLine,
    Find,
    FindNext,
    ExpandTemplate,
}

#[allow(clippy::too_many_arguments)]
fn highlight_job(
    text: &str,
    dialect: Dialect,
    font: &FontId,
    palette: &Palette,
    diagnostics: &[Diagnostic],
    error_range: Option<(usize, usize)>,
    find: Option<(&str, bool)>,
    current_stmt: Option<(usize, usize)>,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let tokens = tabularium_sql::lexer::tokenize(text, dialect);
    let matches = find.map(|(q, case)| find_all(text, q, case)).unwrap_or_default();

    // Collect boundaries where formatting may change so we can split tokens.
    let mut cuts: Vec<usize> = Vec::new();
    for d in diagnostics {
        cuts.push(d.start);
        cuts.push(d.end);
    }
    if let Some((a, b)) = error_range {
        cuts.push(a);
        cuts.push(b);
    }
    for &(a, b) in &matches {
        cuts.push(a);
        cuts.push(b);
    }
    if let Some((a, b)) = current_stmt {
        cuts.push(a);
        cuts.push(b);
    }
    cuts.sort_unstable();
    cuts.dedup();

    for tok in tokens {
        let color = palette.token(tok.kind);
        let mut pos = tok.start;
        let start_idx = cuts.partition_point(|&c| c <= tok.start);
        let mut segment_ends: Vec<usize> =
            cuts[start_idx..].iter().copied().take_while(|&c| c < tok.end).collect();
        segment_ends.push(tok.end);
        for end in segment_ends {
            if end <= pos || !text.is_char_boundary(end) {
                continue;
            }
            let mut fmt = TextFormat { font_id: font.clone(), color, ..Default::default() };
            if tok.kind == tabularium_sql::lexer::TokenKind::Comment {
                fmt.italics = true;
            }
            if let Some((a, b)) = current_stmt {
                if pos >= a && end <= b {
                    fmt.background = palette.current_statement;
                }
            }
            if matches.iter().any(|&(a, b)| pos >= a && end <= b) {
                fmt.background = palette.search_match;
            }
            let mut sev: Option<Severity> = None;
            for d in diagnostics {
                let (ds, de) = (d.start, d.end.max(d.start + 1));
                if pos < de && end > ds {
                    sev = Some(sev.map_or(d.severity, |s: Severity| s.max(d.severity)));
                }
            }
            if let Some((a, b)) = error_range {
                if pos < b && end > a {
                    sev = Some(Severity::Error);
                }
            }
            if let Some(s) = sev {
                let c = match s {
                    Severity::Error => palette.error,
                    Severity::Warning => palette.warning,
                    Severity::Info => Color32::TRANSPARENT,
                };
                if c != Color32::TRANSPARENT && !text[pos..end].trim().is_empty() {
                    fmt.underline = Stroke::new(1.5_f32, c);
                }
            }
            job.append(&text[pos..end], 0.0, fmt);
            pos = end;
        }
    }
    job
}

fn find_all(text: &str, query: &str, match_case: bool) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return vec![];
    }
    if match_case {
        text.match_indices(query).map(|(i, m)| (i, i + m.len())).collect()
    } else {
        let lower_text = text.to_lowercase();
        let lower_q = query.to_lowercase();
        // Lowercasing can change byte lengths for some scripts; only trust it when it doesn't.
        if lower_text.len() != text.len() {
            return text.match_indices(query).map(|(i, m)| (i, i + m.len())).collect();
        }
        lower_text.match_indices(&lower_q).map(|(i, m)| (i, i + m.len())).collect()
    }
}

fn kind_icon(kind: CompletionKind) -> &'static str {
    match kind {
        CompletionKind::Keyword => "k",
        CompletionKind::Schema => "S",
        CompletionKind::Table => "T",
        CompletionKind::View => "V",
        CompletionKind::Column => "c",
        CompletionKind::Function => "ƒ",
        CompletionKind::Alias => "a",
        CompletionKind::Snippet => "⚡",
        CompletionKind::Type => "t",
    }
}

fn kind_color(kind: CompletionKind, p: &Palette) -> Color32 {
    match kind {
        CompletionKind::Keyword | CompletionKind::Type => p.keyword,
        CompletionKind::Schema => p.quoted,
        CompletionKind::Table | CompletionKind::View => p.number,
        CompletionKind::Column | CompletionKind::Alias => p.string,
        CompletionKind::Function => p.function,
        CompletionKind::Snippet => p.parameter,
    }
}

/// 0-based line number of a byte offset.
pub fn line_of(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    text.as_bytes()[..byte].iter().filter(|&&b| b == b'\n').count()
}

pub fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(text.len())
}

pub fn byte_to_char(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte.min(text.len()));
    text[..byte].chars().count()
}

pub fn floor_char_boundary(text: &str, mut i: usize) -> usize {
    if i >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_byte_roundtrip() {
        let s = "aé€b";
        for c in 0..=4 {
            assert_eq!(byte_to_char(s, char_to_byte(s, c)), c);
        }
        assert_eq!(floor_char_boundary(s, 2), 1);
    }

    #[test]
    fn find_is_case_insensitive() {
        assert_eq!(find_all("Select select", "SELECT", false), vec![(0, 6), (7, 13)]);
        assert_eq!(find_all("Select select", "select", true), vec![(7, 13)]);
    }
}
