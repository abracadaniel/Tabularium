//! Result grid: virtualized table view with selection, copy/paste, in-place editing
//! (pending changes are tracked and turned into DML by the owner), sorting, search,
//! value viewer, transposed view and selection aggregates.

use std::collections::{BTreeSet, HashMap};

use egui::{Align2, FontId, Key, Sense, Stroke};
use egui_extras::{Column, TableBuilder};
use tabularium_core::{ColumnMeta, Dialect, ObjectRef, TableInfo, Value};
use tabularium_tools::changes::{row_key, PendingChange};
use tabularium_tools::export::{export_to_string, ExportFormat, ExportOptions};

use crate::theme::Palette;

/// Events the owner of the grid must handle.
#[derive(Debug, Clone)]
pub enum GridEvent {
    /// Header clicked in server-sort mode: `None` clears sorting.
    SortRequested(Option<(usize, bool)>),
    /// "Filter by value": a WHERE fragment for the table editor.
    FilterBy(String),
    ExportRequested,
    /// Selection changed (for status/aggregates in the owner).
    SelectionChanged,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cell {
    row: usize, // display index
    col: usize,
}

struct EditState {
    row: usize, // data index
    col: usize,
    text: String,
    focus: bool,
    is_null: bool,
}

pub struct ResultGrid {
    id: egui::Id,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    /// Display order -> data index.
    order: Vec<usize>,
    sort: Option<(usize, bool)>,
    /// Sorting is done by the owner (table editor) instead of in memory.
    pub server_sort: bool,
    anchor: Option<Cell>,
    focus: Option<Cell>,
    has_focus: bool,
    pub editable: bool,
    edits: HashMap<(usize, usize), Value>,
    deleted: BTreeSet<usize>,
    inserted: BTreeSet<usize>,
    editing: Option<EditState>,
    pub transposed: bool,
    pub show_value_viewer: bool,
    viewer_text: String,
    viewer_cell: Option<(usize, usize)>,
    search: String,
    search_open: bool,
    /// Row number shown for the first row (page offset).
    pub row_offset: usize,
    pub dialect: Dialect,
    /// Table the rows come from (for "Copy as SQL INSERT").
    pub table: Option<ObjectRef>,
    pub null_text: String,
    pub max_cell_chars: usize,
    pub font_size: f32,
    widths: Vec<f32>,
    scroll_to: Option<usize>,
}

impl ResultGrid {
    pub fn new(id_source: impl std::hash::Hash, columns: Vec<ColumnMeta>, rows: Vec<Vec<Value>>) -> Self {
        let mut g = Self {
            id: egui::Id::new(("result_grid", id_source)),
            columns: vec![],
            rows: vec![],
            order: vec![],
            sort: None,
            server_sort: false,
            anchor: None,
            focus: None,
            has_focus: false,
            editable: false,
            edits: HashMap::new(),
            deleted: BTreeSet::new(),
            inserted: BTreeSet::new(),
            editing: None,
            transposed: false,
            show_value_viewer: false,
            viewer_text: String::new(),
            viewer_cell: None,
            search: String::new(),
            search_open: false,
            row_offset: 0,
            dialect: Dialect::Generic,
            table: None,
            null_text: "<null>".into(),
            max_cell_chars: 200,
            font_size: 12.5,
            widths: vec![],
            scroll_to: None,
        };
        g.set_data(columns, rows);
        g
    }

    /// Replace the data; keeps the sort indicator (server-sorted data arrives pre-sorted).
    pub fn set_data(&mut self, columns: Vec<ColumnMeta>, rows: Vec<Vec<Value>>) {
        let same_columns = self.columns.len() == columns.len()
            && self.columns.iter().zip(&columns).all(|(a, b)| a.name == b.name);
        self.columns = columns;
        self.rows = rows;
        self.edits.clear();
        self.deleted.clear();
        self.inserted.clear();
        self.editing = None;
        if !same_columns {
            self.widths.clear();
            self.sort = None;
            self.anchor = None;
            self.focus = None;
        }
        if self.anchor.is_some_and(|c| c.row >= self.rows.len()) {
            self.anchor = None;
            self.focus = None;
        }
        self.rebuild_order();
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn set_sort_indicator(&mut self, sort: Option<(usize, bool)>) {
        self.sort = sort;
    }

    fn rebuild_order(&mut self) {
        let mut order: Vec<usize> = (0..self.rows.len()).collect();
        if !self.search.is_empty() {
            let needle = self.search.to_lowercase();
            order.retain(|&i| {
                self.rows[i].iter().any(|v| v.display_text().is_some_and(|t| t.to_lowercase().contains(&needle)))
            });
        }
        if let (Some((col, asc)), false) = (self.sort, self.server_sort) {
            order.sort_by(|&a, &b| {
                let va = self.value(a, col);
                let vb = self.value(b, col);
                let o = compare_values(va, vb);
                if asc { o } else { o.reverse() }
            });
        }
        self.order = order;
    }

    /// Current value of a cell (pending edit if any).
    pub fn value(&self, data_row: usize, col: usize) -> &Value {
        self.edits
            .get(&(data_row, col))
            .unwrap_or_else(|| self.rows.get(data_row).and_then(|r| r.get(col)).unwrap_or(&Value::Null))
    }

    // ----- pending changes -------------------------------------------------------------------

    pub fn has_pending_changes(&self) -> bool {
        !self.edits.is_empty() || !self.deleted.is_empty() || !self.inserted.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        let edited_rows: BTreeSet<usize> =
            self.edits.keys().map(|(r, _)| *r).filter(|r| !self.inserted.contains(r)).collect();
        edited_rows.len() + self.deleted.len() + self.inserted.len()
    }

    pub fn revert(&mut self) {
        let inserted: Vec<usize> = self.inserted.iter().rev().copied().collect();
        for i in inserted {
            if i < self.rows.len() {
                self.rows.remove(i);
            }
        }
        self.edits.clear();
        self.deleted.clear();
        self.inserted.clear();
        self.editing = None;
        self.rebuild_order();
    }

    /// Build DML-ready changes. `table` provides the row identity (PK / unique key).
    pub fn pending_changes(&self, table: &TableInfo) -> Vec<PendingChange> {
        let mut out = Vec::new();
        for &r in &self.deleted {
            if self.inserted.contains(&r) {
                continue;
            }
            out.push(PendingChange::Delete { key: row_key(table, &self.columns, &self.rows[r]) });
        }
        let mut by_row: std::collections::BTreeMap<usize, Vec<(String, Value)>> = Default::default();
        for ((r, c), v) in &self.edits {
            if self.deleted.contains(r) || self.inserted.contains(r) {
                continue;
            }
            if self.rows[*r].get(*c) == Some(v) {
                continue;
            }
            by_row.entry(*r).or_default().push((self.columns[*c].name.clone(), v.clone()));
        }
        for (r, mut set) in by_row {
            set.sort_by_key(|(name, _)| self.columns.iter().position(|c| &c.name == name));
            out.push(PendingChange::Update { key: row_key(table, &self.columns, &self.rows[r]), set });
        }
        for &r in &self.inserted {
            if self.deleted.contains(&r) {
                continue;
            }
            let values: Vec<(String, Value)> = (0..self.columns.len())
                .filter_map(|c| self.edits.get(&(r, c)).map(|v| (self.columns[c].name.clone(), v.clone())))
                .collect();
            out.push(PendingChange::Insert { values });
        }
        out
    }

    /// After a successful submit: fold edits into the data, drop deleted rows.
    pub fn commit_pending(&mut self) {
        for ((r, c), v) in std::mem::take(&mut self.edits) {
            if let Some(cell) = self.rows.get_mut(r).and_then(|row| row.get_mut(c)) {
                *cell = v;
            }
        }
        for r in std::mem::take(&mut self.deleted).into_iter().rev() {
            if r < self.rows.len() {
                self.rows.remove(r);
            }
        }
        self.inserted.clear();
        self.rebuild_order();
    }

    pub fn add_row(&mut self) {
        let idx = self.rows.len();
        self.rows.push(vec![Value::Null; self.columns.len()]);
        self.inserted.insert(idx);
        self.rebuild_order();
        if let Some(display) = self.order.iter().position(|&i| i == idx) {
            let cell = Cell { row: display, col: 0 };
            self.anchor = Some(cell);
            self.focus = Some(cell);
            self.scroll_to = Some(display);
            self.start_edit(idx, 0);
        }
    }

    pub fn clone_selected_row(&mut self) {
        let Some(r) = self.focus.and_then(|c| self.order.get(c.row).copied()) else { return };
        let values: Vec<Value> = (0..self.columns.len()).map(|c| self.value(r, c).clone()).collect();
        let idx = self.rows.len();
        self.rows.push(vec![Value::Null; self.columns.len()]);
        self.inserted.insert(idx);
        for (c, v) in values.into_iter().enumerate() {
            self.edits.insert((idx, c), v);
        }
        self.rebuild_order();
    }

    pub fn delete_selected_rows(&mut self) {
        for r in self.selected_data_rows() {
            if self.inserted.contains(&r) {
                self.deleted.insert(r);
            } else if !self.deleted.insert(r) {
                // Toggle: deleting an already-deleted row restores it.
                self.deleted.remove(&r);
            }
        }
    }

    pub fn set_selected_null(&mut self) {
        for (r, c) in self.selected_data_cells() {
            self.edits.insert((r, c), Value::Null);
        }
    }

    fn start_edit(&mut self, data_row: usize, col: usize) {
        if !self.editable || self.deleted.contains(&data_row) {
            return;
        }
        let v = self.value(data_row, col);
        self.editing = Some(EditState {
            row: data_row,
            col,
            text: match v {
                Value::Json(j) => serde_json::to_string(j).unwrap_or_default(),
                other => other.edit_text(),
            },
            focus: true,
            is_null: v.is_null(),
        });
    }

    fn finish_edit(&mut self, commit: bool) {
        let Some(e) = self.editing.take() else { return };
        if !commit {
            return;
        }
        let ty = &self.columns[e.col].type_name;
        let original = self.rows.get(e.row).and_then(|r| r.get(e.col)).cloned().unwrap_or_default();
        let new_value = if e.is_null && e.text.is_empty() {
            Value::Null
        } else {
            parse_cell_input(&e.text, ty, &original)
        };
        if new_value == original && !self.inserted.contains(&e.row) {
            self.edits.remove(&(e.row, e.col));
        } else {
            self.edits.insert((e.row, e.col), new_value);
        }
    }

    // ----- selection ---------------------------------------------------------------------------

    fn selection_rect(&self) -> Option<(usize, usize, usize, usize)> {
        let a = self.anchor?;
        let f = self.focus.unwrap_or(a);
        Some((a.row.min(f.row), a.row.max(f.row), a.col.min(f.col), a.col.max(f.col)))
    }

    fn is_selected(&self, display_row: usize, col: usize) -> bool {
        self.selection_rect()
            .is_some_and(|(r0, r1, c0, c1)| display_row >= r0 && display_row <= r1 && col >= c0 && col <= c1)
    }

    pub fn selected_data_rows(&self) -> Vec<usize> {
        match self.selection_rect() {
            Some((r0, r1, _, _)) => (r0..=r1).filter_map(|r| self.order.get(r).copied()).collect(),
            None => vec![],
        }
    }

    fn selected_data_cells(&self) -> Vec<(usize, usize)> {
        let Some((r0, r1, c0, c1)) = self.selection_rect() else { return vec![] };
        let mut out = vec![];
        for r in r0..=r1 {
            if let Some(&dr) = self.order.get(r) {
                for c in c0..=c1.min(self.columns.len().saturating_sub(1)) {
                    out.push((dr, c));
                }
            }
        }
        out
    }

    pub fn select_all(&mut self) {
        if self.order.is_empty() || self.columns.is_empty() {
            return;
        }
        self.anchor = Some(Cell { row: 0, col: 0 });
        self.focus = Some(Cell { row: self.order.len() - 1, col: self.columns.len() - 1 });
    }

    /// Selected cells as (columns, rows) for copy/export.
    pub fn selection_as_table(&self) -> (Vec<ColumnMeta>, Vec<Vec<Value>>) {
        let Some((r0, r1, c0, c1)) = self.selection_rect() else {
            return (self.columns.clone(), self.display_rows());
        };
        let c1 = c1.min(self.columns.len().saturating_sub(1));
        let cols = self.columns[c0..=c1].to_vec();
        let rows = (r0..=r1)
            .filter_map(|r| self.order.get(r))
            .map(|&dr| (c0..=c1).map(|c| self.value(dr, c).clone()).collect())
            .collect();
        (cols, rows)
    }

    /// All rows in display order with pending edits applied (deleted rows excluded).
    pub fn display_rows(&self) -> Vec<Vec<Value>> {
        self.order
            .iter()
            .filter(|r| !self.deleted.contains(r))
            .map(|&r| (0..self.columns.len()).map(|c| self.value(r, c).clone()).collect())
            .collect()
    }

    fn copy_selection(&self, ctx: &egui::Context, with_header: bool) {
        let (cols, rows) = self.selection_as_table();
        let mut out = String::new();
        if with_header {
            out.push_str(&cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join("\t"));
            out.push('\n');
        }
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let line: Vec<String> = row.iter().map(|v| v.display_text().unwrap_or_default()).collect();
            out.push_str(&line.join("\t"));
        }
        ctx.copy_text(out);
    }

    fn copy_as(&self, ctx: &egui::Context, format: ExportFormat) {
        let (cols, rows) = self.selection_as_table();
        let mut opts = ExportOptions::new(format);
        opts.dialect = self.dialect;
        opts.table = self.table.clone();
        match export_to_string(&cols, &rows, &opts) {
            Ok(s) => ctx.copy_text(s),
            Err(e) => log::error!("copy as {format:?}: {e}"),
        }
    }

    fn paste(&mut self, text: &str) {
        if !self.editable {
            return;
        }
        let Some(start) = self.anchor else { return };
        let lines: Vec<&str> = text.trim_end_matches(['\n', '\r']).split('\n').collect();
        for (dy, line) in lines.iter().enumerate() {
            let display = start.row + dy;
            let data_row = match self.order.get(display) {
                Some(&r) => r,
                None => {
                    self.add_row_silent();
                    self.rows.len() - 1
                }
            };
            for (dx, field) in line.trim_end_matches('\r').split('\t').enumerate() {
                let col = start.col + dx;
                if col >= self.columns.len() {
                    break;
                }
                let original = self.rows[data_row].get(col).cloned().unwrap_or_default();
                let v = parse_cell_input(field, &self.columns[col].type_name, &original);
                self.edits.insert((data_row, col), v);
            }
        }
    }

    fn add_row_silent(&mut self) {
        let idx = self.rows.len();
        self.rows.push(vec![Value::Null; self.columns.len()]);
        self.inserted.insert(idx);
        self.order.push(idx);
    }

    /// Aggregates over selected numeric cells: (count, sum, avg, min, max).
    pub fn selection_aggregates(&self) -> Option<String> {
        let cells = self.selected_data_cells();
        if cells.len() < 2 {
            return None;
        }
        let nums: Vec<f64> = cells.iter().filter_map(|&(r, c)| self.value(r, c).as_f64()).collect();
        let count = cells.len();
        if nums.is_empty() {
            return Some(format!("Count: {count}"));
        }
        let sum: f64 = nums.iter().sum();
        let min = nums.iter().copied().fold(f64::INFINITY, f64::min);
        let max = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        Some(format!(
            "Count: {count}  Sum: {}  Avg: {}  Min: {}  Max: {}",
            fmt_num(sum),
            fmt_num(sum / nums.len() as f64),
            fmt_num(min),
            fmt_num(max)
        ))
    }

    // ----- UI ------------------------------------------------------------------------------------

    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<GridEvent> {
        let mut events = Vec::new();
        if self.search_open {
            ui.horizontal(|ui| {
                ui.label("🔍");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.search).hint_text("Search in results").desired_width(220.0),
                );
                if r.changed() {
                    self.rebuild_order();
                }
                ui.weak(format!("{} of {} rows", self.order.len(), self.rows.len()));
                if ui.small_button("✕").clicked() {
                    self.search.clear();
                    self.search_open = false;
                    self.rebuild_order();
                }
            });
        }

        if self.show_value_viewer {
            egui::SidePanel::right(self.id.with("viewer"))
                .resizable(true)
                .default_width(320.0)
                .show_inside(ui, |ui| self.value_viewer_ui(ui));
        }

        if self.columns.is_empty() {
            ui.centered_and_justified(|ui| ui.weak("No columns"));
            return events;
        }

        self.handle_keys(ui, &mut events);

        let grid_rect = ui.available_rect_before_wrap();
        if ui.input(|i| i.pointer.any_pressed())
            && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| !grid_rect.contains(p))
        {
            self.has_focus = false;
        }

        if self.transposed {
            self.transposed_ui(ui);
        } else {
            self.table_ui(ui, &mut events);
        }
        events
    }

    fn handle_keys(&mut self, ui: &mut egui::Ui, events: &mut Vec<GridEvent>) {
        if !self.has_focus || self.editing.is_some() {
            return;
        }
        let ctx = ui.ctx().clone();
        let (copy, paste, mods) = ui.input(|i| {
            let copy = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            let paste = i.events.iter().find_map(|e| match e {
                egui::Event::Paste(s) => Some(s.clone()),
                _ => None,
            });
            (copy, paste, i.modifiers)
        });
        if copy {
            self.copy_selection(&ctx, false);
        }
        if let Some(p) = paste {
            self.paste(&p);
        }
        let n_rows = self.order.len();
        let n_cols = self.columns.len();
        let mut moved = None;
        ui.input_mut(|i| {
            if i.consume_key(egui::Modifiers::COMMAND, Key::A) {
                moved = Some(None);
            }
            if i.consume_key(egui::Modifiers::COMMAND, Key::F) {
                self.search_open = true;
            }
            let dirs = [
                (Key::ArrowDown, 1i64, 0i64),
                (Key::ArrowUp, -1, 0),
                (Key::ArrowRight, 0, 1),
                (Key::ArrowLeft, 0, -1),
                (Key::PageDown, 20, 0),
                (Key::PageUp, -20, 0),
            ];
            for (key, dy, dx) in dirs {
                if i.key_pressed(key) {
                    let cur = self.focus.unwrap_or(Cell { row: 0, col: 0 });
                    let row = (cur.row as i64 + dy).clamp(0, n_rows.saturating_sub(1) as i64) as usize;
                    let col = (cur.col as i64 + dx).clamp(0, n_cols.saturating_sub(1) as i64) as usize;
                    moved = Some(Some((Cell { row, col }, mods.shift)));
                }
            }
            if i.key_pressed(Key::Home) && mods.command {
                moved = Some(Some((Cell { row: 0, col: self.focus.map_or(0, |c| c.col) }, mods.shift)));
            }
            if i.key_pressed(Key::End) && mods.command {
                moved = Some(Some((
                    Cell { row: n_rows.saturating_sub(1), col: self.focus.map_or(0, |c| c.col) },
                    mods.shift,
                )));
            }
        });
        match moved {
            Some(None) => {
                self.select_all();
                events.push(GridEvent::SelectionChanged);
            }
            Some(Some((cell, extend))) => {
                if !extend || self.anchor.is_none() {
                    self.anchor = Some(cell);
                }
                self.focus = Some(cell);
                self.scroll_to = Some(cell.row);
                events.push(GridEvent::SelectionChanged);
            }
            None => {}
        }
        if self.editable {
            let (enter, f2, del, ins) = ui.input(|i| {
                (
                    i.key_pressed(Key::Enter),
                    i.key_pressed(Key::F2),
                    i.key_pressed(Key::Delete) && i.modifiers.command,
                    i.key_pressed(Key::Insert) || (i.modifiers.alt && i.key_pressed(Key::Insert)),
                )
            });
            if enter || f2 {
                if let Some(c) = self.focus {
                    if let Some(&dr) = self.order.get(c.row) {
                        self.start_edit(dr, c.col);
                    }
                }
            }
            if del {
                self.delete_selected_rows();
            }
            if ins {
                self.add_row();
            }
        }
    }

    fn table_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<GridEvent>) {
        let palette = Palette::current(ui.ctx());
        let font = FontId::monospace(self.font_size);
        let row_height = ui.fonts_mut(|f| f.row_height(&font)) + 6.0;
        if self.widths.len() != self.columns.len() {
            self.widths = self.initial_widths(ui, &font);
        }
        let n = self.order.len();
        let row_num_width = ((n + self.row_offset).max(1).to_string().len() as f32 * self.font_size * 0.62 + 16.0).max(36.0);
        let mut header_clicked: Option<usize> = None;
        let mut cell_clicked: Option<(Cell, bool, bool)> = None; // cell, shift, double
        let mut context_cell: Option<Cell> = None;
        let mut edit_done: Option<(bool, Option<(i64, i64)>)> = None;
        let scroll_to = self.scroll_to.take();

        egui::ScrollArea::horizontal().id_salt(self.id.with("hscroll")).auto_shrink([false, false]).show(ui, |ui| {
            let mut tb = TableBuilder::new(ui)
                .id_salt(self.id.with("table"))
                .striped(true)
                .resizable(true)
                .auto_shrink([false, false])
                .min_scrolled_height(0.0)
                .max_scroll_height(f32::INFINITY)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(row_num_width));
            for w in &self.widths {
                tb = tb.column(Column::initial(*w).at_least(30.0).clip(true).resizable(true));
            }
            if let Some(r) = scroll_to {
                tb = tb.scroll_to_row(r, None);
            }
            tb.header(row_height + 2.0, |mut header| {
                header.col(|ui| {
                    ui.weak("#");
                });
                for (ci, col) in self.columns.iter().enumerate() {
                    header.col(|ui| {
                        let arrow = match self.sort {
                            Some((c, true)) if c == ci => " ▲",
                            Some((c, false)) if c == ci => " ▼",
                            _ => "",
                        };
                        let r = ui
                            .add(
                                egui::Label::new(egui::RichText::new(format!("{}{arrow}", col.name)).strong())
                                    .sense(Sense::click())
                                    .truncate(),
                            )
                            .on_hover_text(format!(
                                "{}\n{}{}",
                                col.name,
                                col.type_name,
                                col.table.as_ref().map(|t| format!("\nfrom {t}")).unwrap_or_default()
                            ));
                        if r.clicked() {
                            header_clicked = Some(ci);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, n, |mut row| {
                    let display = row.index();
                    let data_row = self.order[display];
                    let deleted = self.deleted.contains(&data_row);
                    let inserted = self.inserted.contains(&data_row);
                    row.col(|ui| {
                        let rect = ui.max_rect();
                        if deleted {
                            ui.painter().rect_filled(rect, 0.0, palette.deleted_row);
                        } else if inserted {
                            ui.painter().rect_filled(rect, 0.0, palette.inserted_row);
                        }
                        let label = if inserted { "+".to_string() } else { (data_row + 1 + self.row_offset).to_string() };
                        let r = ui.allocate_rect(rect, Sense::click());
                        ui.painter().text(
                            rect.right_center() - egui::vec2(6.0, 0.0),
                            Align2::RIGHT_CENTER,
                            label,
                            font.clone(),
                            palette.gutter,
                        );
                        if r.clicked() {
                            // Select the whole row.
                            let shift = ui.input(|i| i.modifiers.shift);
                            let last = self.columns.len().saturating_sub(1);
                            if !shift || self.anchor.is_none() {
                                self.anchor = Some(Cell { row: display, col: 0 });
                            }
                            self.focus = Some(Cell { row: display, col: last });
                            self.has_focus = true;
                        }
                    });
                    for ci in 0..self.columns.len() {
                        row.col(|ui| {
                            let rect = ui.max_rect();
                            let editing_here =
                                self.editing.as_ref().is_some_and(|e| e.row == data_row && e.col == ci);
                            if editing_here {
                                let e = self.editing.as_mut().unwrap();
                                let resp = ui.add_sized(
                                    rect.size(),
                                    egui::TextEdit::singleline(&mut e.text).font(font.clone()).margin(egui::vec2(2.0, 0.0)),
                                );
                                if e.focus {
                                    resp.request_focus();
                                    e.focus = false;
                                }
                                if resp.changed() {
                                    e.is_null = false;
                                }
                                let (enter, esc, tab, shift) = ui.input(|i| {
                                    (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape), i.key_pressed(Key::Tab), i.modifiers.shift)
                                });
                                if esc {
                                    edit_done = Some((false, None));
                                } else if tab {
                                    edit_done = Some((true, Some((0, if shift { -1 } else { 1 }))));
                                } else if enter {
                                    edit_done = Some((true, Some((1, 0))));
                                } else if resp.lost_focus() {
                                    edit_done = Some((true, None));
                                }
                                return;
                            }
                            let selected = self.is_selected(display, ci);
                            let edited = self.edits.contains_key(&(data_row, ci));
                            let painter = ui.painter_at(rect);
                            if deleted {
                                painter.rect_filled(rect, 0.0, palette.deleted_row);
                            } else if inserted {
                                painter.rect_filled(rect, 0.0, palette.inserted_row);
                            }
                            if edited && !inserted {
                                painter.rect_filled(rect, 0.0, palette.modified_cell);
                            }
                            if selected {
                                painter.rect_filled(rect, 0.0, palette.selection);
                            }
                            if self.focus == Some(Cell { row: display, col: ci }) && self.has_focus {
                                painter.rect_stroke(rect.shrink(0.5), 0.0, Stroke::new(1.0_f32, palette.number), egui::StrokeKind::Inside);
                            }
                            let v = self.value(data_row, ci);
                            let (text, color, italic) = match v.display_text() {
                                None => (self.null_text.clone(), palette.null, true),
                                Some(t) => {
                                    let mut t = t.replace(['\n', '\r'], "↵");
                                    if t.chars().count() > self.max_cell_chars {
                                        t = t.chars().take(self.max_cell_chars).collect::<String>() + "…";
                                    }
                                    let c = match v {
                                        Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_) => palette.number,
                                        Value::Bytes(_) => palette.quoted,
                                        _ => ui.visuals().text_color(),
                                    };
                                    (t, c, false)
                                }
                            };
                            let right = v.is_numeric() || (v.is_null() && self.columns[ci].is_numeric_type());
                            let mut job = egui::text::LayoutJob::single_section(
                                text,
                                egui::TextFormat { font_id: font.clone(), color, italics: italic, ..Default::default() },
                            );
                            job.wrap.max_width = rect.width() - 8.0;
                            job.wrap.max_rows = 1;
                            job.wrap.break_anywhere = true;
                            let galley = ui.fonts_mut(|f| f.layout_job(job));
                            let y = rect.center().y - galley.size().y / 2.0;
                            let x = if right { rect.right() - 4.0 - galley.size().x } else { rect.left() + 4.0 };
                            painter.galley(egui::pos2(x, y), galley, color);
                            if deleted {
                                painter.line_segment(
                                    [rect.left_center(), rect.right_center()],
                                    Stroke::new(1.0_f32, palette.error.gamma_multiply(0.7)),
                                );
                            }
                            let resp = ui.allocate_rect(rect, Sense::click());
                            if resp.clicked() || resp.secondary_clicked() && !selected {
                                let shift = ui.input(|i| i.modifiers.shift);
                                cell_clicked = Some((Cell { row: display, col: ci }, shift, false));
                            }
                            if resp.double_clicked() {
                                cell_clicked = Some((Cell { row: display, col: ci }, false, true));
                            }
                            if resp.secondary_clicked() {
                                context_cell = Some(Cell { row: display, col: ci });
                            }
                            resp.context_menu(|ui| {
                                self.context_menu_ui(ui, events, data_row, ci);
                            });
                        });
                    }
                });
            });
        });

        if let Some(col) = header_clicked {
            let next = match self.sort {
                Some((c, true)) if c == col => Some((col, false)),
                Some((c, false)) if c == col => None,
                _ => Some((col, true)),
            };
            self.sort = next;
            if self.server_sort {
                events.push(GridEvent::SortRequested(next));
            } else {
                self.rebuild_order();
            }
        }
        if let Some((cell, shift, double)) = cell_clicked {
            self.has_focus = true;
            if self.editing.is_some() {
                self.finish_edit(true);
            }
            if shift && self.anchor.is_some() {
                self.focus = Some(cell);
            } else {
                self.anchor = Some(cell);
                self.focus = Some(cell);
            }
            events.push(GridEvent::SelectionChanged);
            if double {
                if let Some(&dr) = self.order.get(cell.row) {
                    if self.editable {
                        self.start_edit(dr, cell.col);
                    } else {
                        self.show_value_viewer = true;
                    }
                }
            }
        }
        if let Some(cell) = context_cell {
            self.has_focus = true;
            if !self.is_selected(cell.row, cell.col) {
                self.anchor = Some(cell);
                self.focus = Some(cell);
            }
        }
        if let Some((commit, mv)) = edit_done {
            let (row, col) = self.editing.as_ref().map(|e| (e.row, e.col)).unwrap_or((0, 0));
            self.finish_edit(commit);
            if let Some((dy, dx)) = mv {
                if let Some(display) = self.order.iter().position(|&r| r == row) {
                    let nr = (display as i64 + dy).clamp(0, self.order.len() as i64 - 1) as usize;
                    let nc = (col as i64 + dx).clamp(0, self.columns.len() as i64 - 1) as usize;
                    let cell = Cell { row: nr, col: nc };
                    self.anchor = Some(cell);
                    self.focus = Some(cell);
                    if dx != 0 {
                        if let Some(&dr) = self.order.get(nr) {
                            self.start_edit(dr, nc);
                        }
                    }
                }
            }
        }
    }

    fn context_menu_ui(&mut self, ui: &mut egui::Ui, events: &mut Vec<GridEvent>, data_row: usize, col: usize) {
        let ctx = ui.ctx().clone();
        if ui.button("Copy").clicked() {
            self.copy_selection(&ctx, false);
            ui.close();
        }
        if ui.button("Copy with header").clicked() {
            self.copy_selection(&ctx, true);
            ui.close();
        }
        ui.menu_button("Copy as", |ui| {
            for f in [
                ExportFormat::Csv,
                ExportFormat::Tsv,
                ExportFormat::Json,
                ExportFormat::SqlInsert,
                ExportFormat::SqlInsertMultiRow,
                ExportFormat::SqlUpdate,
                ExportFormat::Markdown,
                ExportFormat::Html,
                ExportFormat::Xml,
                ExportFormat::TextTable,
            ] {
                if ui.button(f.label()).clicked() {
                    self.copy_as(&ctx, f);
                    ui.close();
                }
            }
        });
        if ui.button("Copy column name").clicked() {
            ctx.copy_text(self.columns[col].name.clone());
            ui.close();
        }
        ui.separator();
        if ui.button("Open value viewer").clicked() {
            self.show_value_viewer = true;
            ui.close();
        }
        if ui.button(if self.transposed { "Normal view" } else { "Transpose" }).clicked() {
            self.transposed = !self.transposed;
            ui.close();
        }
        if ui.button("Search in results…").clicked() {
            self.search_open = true;
            ui.close();
        }
        if self.server_sort {
            let v = self.value(data_row, col).clone();
            let name = self.dialect.quote_ident_if_needed(&self.columns[col].name);
            let clause = if v.is_null() {
                format!("{name} IS NULL")
            } else {
                format!("{name} = {}", v.to_sql_literal(self.dialect))
            };
            if ui.button(format!("Filter by {}", truncate(&clause, 40))).clicked() {
                events.push(GridEvent::FilterBy(clause));
                ui.close();
            }
        }
        if self.editable {
            ui.separator();
            if ui.button("Edit cell (F2)").clicked() {
                self.start_edit(data_row, col);
                ui.close();
            }
            if ui.button("Set NULL").clicked() {
                self.set_selected_null();
                ui.close();
            }
            if ui.button("Add row (Insert)").clicked() {
                self.add_row();
                ui.close();
            }
            if ui.button("Clone row").clicked() {
                self.clone_selected_row();
                ui.close();
            }
            if ui.button("Delete row(s) (Ctrl+Delete)").clicked() {
                self.delete_selected_rows();
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Export data…").clicked() {
            events.push(GridEvent::ExportRequested);
            ui.close();
        }
    }

    fn transposed_ui(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::current(ui.ctx());
        let font = FontId::monospace(self.font_size);
        let max_records = self.order.len().min(200);
        egui::ScrollArea::both().id_salt(self.id.with("transposed")).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(self.id.with("tgrid")).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                ui.weak("column");
                for d in 0..max_records {
                    ui.weak(format!("#{}", self.order[d] + 1 + self.row_offset));
                }
                ui.end_row();
                for (ci, col) in self.columns.iter().enumerate() {
                    ui.label(egui::RichText::new(&col.name).strong()).on_hover_text(&col.type_name);
                    for d in 0..max_records {
                        let v = self.value(self.order[d], ci);
                        match v.display_text() {
                            None => ui.label(egui::RichText::new(&self.null_text).font(font.clone()).italics().color(palette.null)),
                            Some(t) => ui.label(egui::RichText::new(truncate(&t, self.max_cell_chars)).font(font.clone())),
                        };
                    }
                    ui.end_row();
                }
            });
            if self.order.len() > max_records {
                ui.weak(format!("Showing first {max_records} of {} rows", self.order.len()));
            }
        });
    }

    fn value_viewer_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Value");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").clicked() {
                    self.show_value_viewer = false;
                }
            });
        });
        let Some(cell) = self.focus else {
            ui.weak("Select a cell");
            return;
        };
        let Some(&dr) = self.order.get(cell.row) else { return };
        let col = cell.col.min(self.columns.len().saturating_sub(1));
        let v = self.value(dr, col).clone();
        if self.viewer_cell != Some((dr, col)) {
            self.viewer_cell = Some((dr, col));
            self.viewer_text = match &v {
                Value::Json(j) => serde_json::to_string_pretty(j).unwrap_or_default(),
                Value::Text(t) if t.trim_start().starts_with(['{', '[']) => serde_json::from_str::<serde_json::Value>(t)
                    .ok()
                    .and_then(|j| serde_json::to_string_pretty(&j).ok())
                    .unwrap_or_else(|| t.clone()),
                Value::Bytes(b) => hex_dump(b),
                other => other.edit_text(),
            };
        }
        ui.weak(format!("{} · {} · {}", self.columns[col].name, self.columns[col].type_name, v.kind_name()));
        if v.is_null() {
            ui.weak("NULL");
        }
        let editable = self.editable && !matches!(v, Value::Bytes(_));
        egui::ScrollArea::vertical().id_salt(self.id.with("viewer_scroll")).show(ui, |ui| {
            let mut text = self.viewer_text.as_str();
            if editable {
                ui.add(egui::TextEdit::multiline(&mut self.viewer_text).code_editor().desired_width(f32::INFINITY));
            } else {
                ui.add(egui::TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY));
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(self.viewer_text.clone());
            }
            if editable && ui.button("Apply").clicked() {
                let original = self.rows[dr].get(col).cloned().unwrap_or_default();
                let nv = parse_cell_input(&self.viewer_text, &self.columns[col].type_name, &original);
                self.edits.insert((dr, col), nv);
            }
        });
    }

    fn initial_widths(&self, ui: &egui::Ui, font: &FontId) -> Vec<f32> {
        let char_w = ui.fonts_mut(|f| f.glyph_width(font, '0')).max(6.0);
        self.columns
            .iter()
            .enumerate()
            .map(|(ci, c)| {
                let mut max_chars = c.name.chars().count() + 3;
                for row in self.rows.iter().take(100) {
                    if let Some(v) = row.get(ci) {
                        let len = v.display_text().map(|t| t.chars().count()).unwrap_or(self.null_text.len());
                        max_chars = max_chars.max(len.min(60));
                    }
                }
                (max_chars as f32 * char_w + 14.0).clamp(50.0, 420.0)
            })
            .collect()
    }

    pub fn toggle_search(&mut self) {
        self.search_open = !self.search_open;
    }
}

/// Parse text typed into a cell, guided by column type and the original value's variant.
fn parse_cell_input(text: &str, type_name: &str, original: &Value) -> Value {
    let parsed = Value::parse_for_type(text, type_name);
    // When the type name didn't help, mimic the original variant.
    if matches!(parsed, Value::Text(_)) {
        match original {
            Value::Int(_) => text.trim().parse().map(Value::Int).unwrap_or(parsed),
            Value::Float(_) => text.trim().parse().map(Value::Float).unwrap_or(parsed),
            Value::Bool(_) => match text.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "t" => Value::Bool(true),
                "false" | "0" | "f" => Value::Bool(false),
                _ => parsed,
            },
            Value::Json(_) => serde_json::from_str(text).map(Value::Json).unwrap_or(parsed),
            _ => parsed,
        }
    } else {
        parsed
    }
}

/// Total order over values for sorting: NULLs first, numbers numerically, else by text.
pub fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) if a.is_numeric() && b.is_numeric() => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => match (a, b) {
                (Value::Date(x), Value::Date(y)) => x.cmp(y),
                (Value::DateTime(x), Value::DateTime(y)) => x.cmp(y),
                (Value::DateTimeTz(x), Value::DateTimeTz(y)) => x.cmp(y),
                (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
                _ => a.to_string().cmp(&b.to_string()),
            },
        },
    }
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x:.4}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

fn hex_dump(b: &[u8]) -> String {
    let mut out = String::new();
    for (i, chunk) in b.chunks(16).take(4096).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|x| format!("{x:02x}")).collect();
        let ascii: String = chunk.iter().map(|&x| if x.is_ascii_graphic() || x == b' ' { x as char } else { '.' }).collect();
        out.push_str(&format!("{:08x}  {:<48} {}\n", i * 16, hex.join(" "), ascii));
    }
    if b.len() > 16 * 4096 {
        out.push_str(&format!("… ({} bytes total)\n", b.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabularium_core::{ColumnInfo, KeyInfo};

    fn grid() -> ResultGrid {
        let cols = vec![ColumnMeta::new("id", "int4"), ColumnMeta::new("name", "text")];
        let rows = vec![
            vec![Value::Int(2), Value::from("b")],
            vec![Value::Int(1), Value::from("a")],
            vec![Value::Int(3), Value::Null],
        ];
        ResultGrid::new("t", cols, rows)
    }

    fn table() -> TableInfo {
        let mut t = TableInfo::new(ObjectRef::table(Some("public"), "t"));
        t.columns = vec![
            ColumnInfo { name: "id".into(), data_type: "integer".into(), ..Default::default() },
            ColumnInfo { name: "name".into(), data_type: "text".into(), nullable: true, ..Default::default() },
        ];
        t.primary_key = Some(KeyInfo { name: None, columns: vec!["id".into()] });
        t
    }

    #[test]
    fn sorting_in_memory() {
        let mut g = grid();
        g.sort = Some((0, true));
        g.rebuild_order();
        assert_eq!(g.order, vec![1, 0, 2]);
        g.sort = Some((1, false));
        g.rebuild_order();
        assert_eq!(g.order, vec![0, 1, 2]); // b, a, NULL(last when descending)
    }

    #[test]
    fn pending_changes_roundtrip() {
        let mut g = grid();
        g.editable = true;
        g.edits.insert((0, 1), Value::from("bb"));
        g.deleted.insert(1);
        g.add_row_silent();
        g.edits.insert((3, 0), Value::Int(9));
        let changes = g.pending_changes(&table());
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0], PendingChange::Delete { key } if key[0].1 == Value::Int(1)));
        assert!(matches!(&changes[1], PendingChange::Update { key, set } if key[0].1 == Value::Int(2) && set[0].1 == Value::from("bb")));
        assert!(matches!(&changes[2], PendingChange::Insert { values } if values == &vec![("id".to_string(), Value::Int(9))]));
        g.commit_pending();
        assert_eq!(g.rows.len(), 3);
        assert_eq!(g.rows[0][1], Value::from("bb"));
        assert!(!g.has_pending_changes());
    }

    #[test]
    fn search_filters_rows() {
        let mut g = grid();
        g.search = "A".into();
        g.rebuild_order();
        assert_eq!(g.order, vec![1]);
    }

    #[test]
    fn aggregates() {
        let mut g = grid();
        g.anchor = Some(Cell { row: 0, col: 0 });
        g.focus = Some(Cell { row: 2, col: 0 });
        assert_eq!(g.selection_aggregates().unwrap(), "Count: 3  Sum: 6  Avg: 2  Min: 1  Max: 3");
    }
}
