//! Table data editor: paged, filterable, sortable grid over a table or view with
//! in-place editing that is submitted as generated DML.

use tabularium_core::{driver::ExecOptions, Error, ObjectKind, ObjectRef, StatementResult, TableInfo};
use tabularium_tools::changes::change_statements_with_table;
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, ExportSource, TabView};
use crate::grid::{GridEvent, ResultGrid};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

pub struct TableDataTab {
    id: u64,
    pub ds: Uuid,
    pub object: ObjectRef,
    info: Option<TableInfo>,
    grid: ResultGrid,
    filter: String,
    applied_filter: String,
    order: String,
    applied_order: String,
    offset: u64,
    page_size: u64,
    total: Option<i64>,
    last_sql: String,
    last_summary: String,
    error: Option<String>,
    describe: Option<Pending<Result<TableInfo, Error>>>,
    load: Option<Pending<Result<StatementResult, Error>>>,
    count: Option<Pending<Result<i64, Error>>>,
    submit: Option<Pending<Result<u64, (Error, usize)>>>,
    preview: Option<Vec<String>>,
    initialized: bool,
}

impl TableDataTab {
    pub fn new(id: u64, ds: Uuid, object: ObjectRef, filter: Option<String>) -> Self {
        let mut grid = ResultGrid::new(("table_data", id), vec![], vec![]);
        grid.server_sort = true;
        grid.table = Some(object.clone());
        let f = filter.unwrap_or_default();
        Self {
            id,
            ds,
            object,
            info: None,
            grid,
            filter: f.clone(),
            applied_filter: f,
            order: String::new(),
            applied_order: String::new(),
            offset: 0,
            page_size: 500,
            total: None,
            last_sql: String::new(),
            last_summary: String::new(),
            error: None,
            describe: None,
            load: None,
            count: None,
            submit: None,
            preview: None,
            initialized: false,
        }
    }

    fn dialect(&self, ctx: &Ctx) -> tabularium_core::Dialect {
        ctx.sources.get(self.ds).map(|d| d.config.dialect()).unwrap_or_default()
    }

    fn reload(&mut self, ctx: &mut Ctx) {
        let Some(ds) = ctx.sources.get(self.ds) else { return };
        let dialect = ds.config.dialect();
        let sql = dialect.select_page(
            self.object.schema.as_deref(),
            &self.object.name,
            Some(&self.applied_filter),
            Some(&self.applied_order),
            self.page_size,
            self.offset,
        );
        self.last_sql = sql.clone();
        let conn = ds.conn.clone();
        self.error = None;
        self.load = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await?;
            c.execute(&sql, &ExecOptions::unlimited()).await
        }));
        self.refresh_count(ctx);
    }

    fn refresh_count(&mut self, ctx: &mut Ctx) {
        let Some(ds) = ctx.sources.get(self.ds) else { return };
        let dialect = ds.config.dialect();
        let mut sql = format!("SELECT COUNT(*) FROM {}", self.object.qualified(dialect));
        if !self.applied_filter.trim().is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&self.applied_filter);
        }
        let conn = ds.conn.clone();
        self.total = None;
        self.count = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await?;
            let r = c.execute(&sql, &ExecOptions::with_limit(1)).await?;
            Ok(r.rows.first().and_then(|row| row.first()).and_then(|v| v.as_f64()).map(|f| f as i64).unwrap_or(0))
        }));
    }

    fn start(&mut self, ctx: &mut Ctx) {
        self.initialized = true;
        self.page_size = ctx.settings.page_size.max(1);
        self.grid.null_text = ctx.settings.null_text.clone();
        self.grid.max_cell_chars = ctx.settings.max_cell_chars;
        self.grid.font_size = ctx.settings.grid_font_size;
        self.grid.dialect = self.dialect(ctx);
        let Some(ds) = ctx.sources.get(self.ds) else { return };
        let conn = ds.conn.clone();
        let object = self.object.clone();
        self.describe = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await?;
            c.describe_table(&object).await
        }));
        self.reload(ctx);
    }

    fn submit(&mut self, ctx: &mut Ctx) {
        let Some(info) = &self.info else { return };
        let changes = self.grid.pending_changes(info);
        if changes.is_empty() {
            return;
        }
        let dialect = self.dialect(ctx);
        let statements = change_statements_with_table(info, &changes, dialect);
        let Some(ds) = ctx.sources.get(self.ds) else { return };
        let conn = ds.conn.clone();
        self.submit = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await.map_err(|e| (e, 0))?;
            let mut affected = 0;
            for (i, s) in statements.iter().enumerate() {
                let r = c.execute(s, &ExecOptions::default()).await.map_err(|e| (e, i))?;
                affected += r.rows_affected.unwrap_or(0);
            }
            Ok(affected)
        }));
    }

    fn is_editable(&self) -> bool {
        self.object.kind == ObjectKind::Table && self.info.is_some()
    }

    fn poll(&mut self, ctx: &mut Ctx) {
        if let Some(r) = poll_slot(&mut self.describe) {
            match r {
                Ok(info) => {
                    self.info = Some(info);
                    self.grid.editable = self.is_editable();
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        if let Some(r) = poll_slot(&mut self.load) {
            match r {
                Ok(res) => {
                    self.last_summary = res.summary();
                    self.grid.row_offset = self.offset as usize;
                    self.grid.set_data(res.columns, res.rows);
                    self.grid.editable = self.is_editable();
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        if let Some(r) = poll_slot(&mut self.count) {
            self.total = r.ok();
        }
        if let Some(r) = poll_slot(&mut self.submit) {
            match r {
                Ok(n) => {
                    ctx.info(format!("{}: {n} row(s) affected", self.object.display_name()));
                    self.grid.commit_pending();
                    self.reload(ctx);
                }
                Err((e, i)) => {
                    ctx.error(format!("Submit failed at statement {}: {e}", i + 1));
                    self.error = Some(e.to_string());
                }
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        let loading = self.load.is_some();
        ui.horizontal(|ui| {
            let total_pages = self.total.map(|t| ((t.max(0) as u64).div_ceil(self.page_size)).max(1));
            let page = self.offset / self.page_size + 1;
            if ui.add_enabled(self.offset > 0, egui::Button::new("⏮")).on_hover_text("First page").clicked() {
                self.offset = 0;
                self.reload(ctx);
            }
            if ui.add_enabled(self.offset > 0, egui::Button::new("◀")).on_hover_text("Previous page").clicked() {
                self.offset = self.offset.saturating_sub(self.page_size);
                self.reload(ctx);
            }
            let rows = self.grid.row_count() as u64;
            let label = match self.total {
                Some(t) => format!("{}-{} of {t}", if rows == 0 { 0 } else { self.offset + 1 }, self.offset + rows),
                None => format!("{}-{}", if rows == 0 { 0 } else { self.offset + 1 }, self.offset + rows),
            };
            ui.label(label);
            let has_next = total_pages.map(|tp| page < tp).unwrap_or(rows >= self.page_size);
            if ui.add_enabled(has_next, egui::Button::new("▶")).on_hover_text("Next page").clicked() {
                self.offset += self.page_size;
                self.reload(ctx);
            }
            if ui.add_enabled(has_next && total_pages.is_some(), egui::Button::new("⏭")).on_hover_text("Last page").clicked() {
                if let Some(tp) = total_pages {
                    self.offset = (tp - 1) * self.page_size;
                    self.reload(ctx);
                }
            }
            egui::ComboBox::from_id_salt(("page_size", self.id))
                .width(60.0)
                .selected_text(self.page_size.to_string())
                .show_ui(ui, |ui| {
                    for n in [10u64, 50, 100, 500, 1000, 5000, 10000] {
                        if ui.selectable_value(&mut self.page_size, n, n.to_string()).clicked() {
                            self.offset = 0;
                            self.reload(ctx);
                        }
                    }
                });
            if ui.add_enabled(!loading, egui::Button::new("⟳")).on_hover_text("Reload (F5)").clicked() {
                self.reload(ctx);
            }
            ui.separator();
            if self.grid.editable {
                if ui.button("+").on_hover_text("Add row (Insert)").clicked() {
                    self.grid.add_row();
                }
                if ui.button("-").on_hover_text("Delete selected rows (Ctrl+Delete)").clicked() {
                    self.grid.delete_selected_rows();
                }
                let pending = self.grid.has_pending_changes();
                let submitting = self.submit.is_some();
                if ui
                    .add_enabled(pending && !submitting, egui::Button::new(format!("⬆ Submit ({})", self.grid.pending_count())))
                    .on_hover_text("Submit changes (Ctrl+Enter)")
                    .clicked()
                {
                    self.submit(ctx);
                }
                if ui.add_enabled(pending, egui::Button::new("Preview")).on_hover_text("Show the DML that will be executed").clicked() {
                    if let Some(info) = &self.info {
                        let dialect = self.dialect(ctx);
                        self.preview = Some(change_statements_with_table(info, &self.grid.pending_changes(info), dialect));
                    }
                }
                if ui.add_enabled(pending, egui::Button::new("↶ Revert")).clicked() {
                    self.grid.revert();
                }
                ui.separator();
            } else if self.object.kind != ObjectKind::Table {
                ui.weak("read-only");
                ui.separator();
            }
            if ui.button("Transpose").clicked() {
                self.grid.transposed = !self.grid.transposed;
            }
            if ui.button("Value").on_hover_text("Value viewer").clicked() {
                self.grid.show_value_viewer = !self.grid.show_value_viewer;
            }
            ui.menu_button("Export", |ui| {
                if ui.button("Whole table…").clicked() {
                    ctx.act(AppAction::Export(ExportSource::Table { ds: self.ds, object: self.object.clone() }));
                    ui.close();
                }
                if ui.button("Current page…").clicked() {
                    ctx.act(AppAction::Export(ExportSource::Rows {
                        columns: self.grid.columns.clone(),
                        rows: self.grid.display_rows(),
                        table: Some(self.object.clone()),
                    }));
                    ui.close();
                }
            });
            if self.object.kind == ObjectKind::Table && ui.button("Import…").clicked() {
                ctx.act(AppAction::Import { ds: self.ds, schema: self.object.schema.clone(), table: Some(self.object.clone()) });
            }
            if ui.button("DDL").clicked() {
                ctx.act(AppAction::OpenDdl { ds: self.ds, object: self.object.clone(), signature: None });
            }
            if ui.button("Console").on_hover_text("Open the current query in a console").clicked() {
                ctx.act(AppAction::OpenConsole { ds: self.ds, sql: Some(self.last_sql.clone()), run: false });
            }
            if loading {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            ui.label("WHERE");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("filter, e.g. id > 10 AND name LIKE 'a%'")
                    .desired_width(ui.available_width() * 0.55),
            );
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.applied_filter = self.filter.clone();
                self.offset = 0;
                self.reload(ctx);
            }
            ui.label("ORDER BY");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.order)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("e.g. created_at DESC")
                    .desired_width(ui.available_width() - 30.0),
            );
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.applied_order = self.order.clone();
                self.grid.set_sort_indicator(None);
                self.reload(ctx);
            }
        });
    }
}

impl TabView for TableDataTab {
    fn title(&self) -> String {
        let dirty = if self.grid.has_pending_changes() { " •" } else { "" };
        format!("{}{dirty}", self.object.name)
    }

    fn data_source(&self) -> Option<Uuid> {
        Some(self.ds)
    }

    fn status(&self) -> Option<String> {
        let mut s = format!("{} · {}", self.object.display_name(), self.last_summary);
        if let Some(agg) = self.grid.selection_aggregates() {
            s.push_str(" · ");
            s.push_str(&agg);
        }
        Some(s)
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        if !self.initialized {
            self.start(ctx);
        }
        self.poll(ctx);
        let (reload, submit) = ui.input_mut(|i| {
            (i.consume_key(egui::Modifiers::NONE, egui::Key::F5), i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter))
        });
        if reload {
            self.reload(ctx);
        }
        if submit && self.grid.has_pending_changes() {
            self.submit(ctx);
        }
        self.toolbar(ui, ctx);
        if let Some(e) = &self.error {
            ui.colored_label(Palette::current(ui.ctx()).error, e);
        }
        ui.separator();
        for ev in self.grid.show(ui) {
            match ev {
                GridEvent::SortRequested(sort) => {
                    let dialect = self.dialect(ctx);
                    self.applied_order = match sort {
                        Some((c, asc)) => format!(
                            "{} {}",
                            dialect.quote_ident_if_needed(&self.grid.columns[c].name),
                            if asc { "ASC" } else { "DESC" }
                        ),
                        None => String::new(),
                    };
                    self.order = self.applied_order.clone();
                    self.offset = 0;
                    self.reload(ctx);
                }
                GridEvent::FilterBy(clause) => {
                    self.filter = if self.applied_filter.trim().is_empty() {
                        clause
                    } else {
                        format!("({}) AND {clause}", self.applied_filter)
                    };
                    self.applied_filter = self.filter.clone();
                    self.offset = 0;
                    self.reload(ctx);
                }
                GridEvent::ExportRequested => ctx.act(AppAction::Export(ExportSource::Table { ds: self.ds, object: self.object.clone() })),
                GridEvent::SelectionChanged => {}
            }
        }
        if let Some(stmts) = &self.preview {
            let mut open = true;
            let text = stmts.join(";\n") + ";";
            egui::Window::new("Pending changes").open(&mut open).default_width(600.0).show(ui.ctx(), |ui| {
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    let mut t = text.as_str();
                    ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
                });
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(text.clone());
                }
            });
            if !open {
                self.preview = None;
            }
        }
    }

    fn can_close(&mut self) -> bool {
        true
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
