//! Full-text search in data: look for a string in every (text) column of every table.

use std::sync::mpsc;

use tabularium_core::{driver::ExecOptions, Dialect, ObjectKind, ObjectRef, StatementResult, TableInfo};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, TabView};
use crate::grid::ResultGrid;
use crate::theme::Palette;

enum SearchEvent {
    Progress { done: usize, total: usize },
    Hit { object: ObjectRef, filter: String, result: StatementResult },
    Error { object: ObjectRef, message: String },
    Finished,
}

struct Hit {
    object: ObjectRef,
    filter: String,
    grid: ResultGrid,
    rows: usize,
    truncated: bool,
}

pub struct FindInDbTab {
    id: u64,
    ds: Uuid,
    schema: Option<String>,
    query: String,
    all_columns: bool,
    case_sensitive: bool,
    limit_per_table: usize,
    include_views: bool,
    running: Option<mpsc::Receiver<SearchEvent>>,
    progress: (usize, usize),
    hits: Vec<Hit>,
    errors: Vec<String>,
    selected: Option<usize>,
}

impl FindInDbTab {
    pub fn new(id: u64, ds: Uuid, schema: Option<String>) -> Self {
        Self {
            id,
            ds,
            schema,
            query: String::new(),
            all_columns: false,
            case_sensitive: false,
            limit_per_table: 100,
            include_views: false,
            running: None,
            progress: (0, 0),
            hits: vec![],
            errors: vec![],
            selected: None,
        }
    }

    fn start(&mut self, ctx: &mut Ctx) {
        let Some(src) = ctx.sources.get(self.ds) else { return };
        let conn = src.conn.clone();
        let dialect = src.config.dialect();
        let schema = self.schema.clone();
        let needle = self.query.clone();
        let (all_columns, case_sensitive, limit, include_views) =
            (self.all_columns, self.case_sensitive, self.limit_per_table, self.include_views);
        let (tx, rx) = mpsc::channel();
        let egui_ctx = ctx.egui.clone();
        self.hits.clear();
        self.errors.clear();
        self.selected = None;
        ctx.rt.spawn(async move {
            let send = |ev: SearchEvent| {
                let _ = tx.send(ev);
                egui_ctx.request_repaint();
            };
            let c = match conn.get().await {
                Ok(c) => c,
                Err(e) => {
                    send(SearchEvent::Error { object: ObjectRef::table(None, "-"), message: e.to_string() });
                    send(SearchEvent::Finished);
                    return;
                }
            };
            let schema = match schema {
                Some(s) => Some(s),
                None => c.current_schema().await.ok().flatten(),
            };
            let kinds: &[ObjectKind] = if include_views { &[ObjectKind::Table, ObjectKind::View] } else { &[ObjectKind::Table] };
            let objects = c.list_objects(schema.as_deref(), kinds).await.unwrap_or_default();
            let total = objects.len();
            for (i, o) in objects.into_iter().enumerate() {
                send(SearchEvent::Progress { done: i, total });
                let info = match c.describe_table(&o.object).await {
                    Ok(t) => t,
                    Err(e) => {
                        send(SearchEvent::Error { object: o.object.clone(), message: e.to_string() });
                        continue;
                    }
                };
                let Some(filter) = build_filter(&info, &needle, dialect, all_columns, case_sensitive) else { continue };
                let sql = format!("SELECT * FROM {} WHERE {filter}", o.object.qualified(dialect));
                match c.execute(&sql, &ExecOptions::with_limit(limit)).await {
                    Ok(r) if !r.rows.is_empty() => send(SearchEvent::Hit { object: o.object.clone(), filter, result: r }),
                    Ok(_) => {}
                    Err(e) => send(SearchEvent::Error { object: o.object.clone(), message: e.to_string() }),
                }
            }
            send(SearchEvent::Progress { done: total, total });
            send(SearchEvent::Finished);
        });
        self.running = Some(rx);
    }

    fn poll(&mut self, ctx: &Ctx) {
        let Some(rx) = &self.running else { return };
        let mut finished = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SearchEvent::Progress { done, total } => self.progress = (done, total),
                SearchEvent::Hit { object, filter, result } => {
                    let rows = result.rows.len();
                    let mut grid = ResultGrid::new(("find", self.id, self.hits.len()), result.columns, result.rows);
                    grid.null_text = ctx.settings.null_text.clone();
                    grid.font_size = ctx.settings.grid_font_size;
                    grid.table = Some(object.clone());
                    self.hits.push(Hit { object, filter, grid, rows, truncated: result.truncated });
                    if self.selected.is_none() {
                        self.selected = Some(0);
                    }
                }
                SearchEvent::Error { object, message } => self.errors.push(format!("{}: {message}", object.display_name())),
                SearchEvent::Finished => finished = true,
            }
        }
        if finished {
            self.running = None;
        }
    }
}

/// WHERE clause matching `needle` in the searchable columns of a table.
fn build_filter(info: &TableInfo, needle: &str, dialect: Dialect, all_columns: bool, case_sensitive: bool) -> Option<String> {
    let pattern = format!("%{}%", needle.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let lit = dialect.quote_string(&pattern);
    let mut parts = vec![];
    for c in &info.columns {
        let t = c.data_type.to_ascii_lowercase();
        let is_text = ["char", "text", "clob", "string", "json", "xml", "uuid", "enum", "name", "citext"].iter().any(|k| t.contains(k));
        let is_binary = ["blob", "bytea", "binary", "image"].iter().any(|k| t.contains(k));
        if is_binary || (!all_columns && !is_text) {
            continue;
        }
        let col = dialect.quote_ident_if_needed(&c.name);
        let expr = match dialect {
            Dialect::Postgres => {
                let op = if case_sensitive { "LIKE" } else { "ILIKE" };
                format!("{col}::text {op} {lit}")
            }
            Dialect::MySql => {
                if case_sensitive {
                    format!("CAST({col} AS CHAR) LIKE BINARY {lit}")
                } else {
                    format!("CAST({col} AS CHAR) LIKE {lit}")
                }
            }
            Dialect::MsSql => format!("CAST({col} AS NVARCHAR(MAX)) LIKE {lit} ESCAPE '\\'"),
            _ => {
                if case_sensitive {
                    format!("CAST({col} AS TEXT) GLOB {}", dialect.quote_string(&format!("*{needle}*")))
                } else {
                    format!("CAST({col} AS TEXT) LIKE {lit} ESCAPE '\\'")
                }
            }
        };
        parts.push(expr);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" OR "))
    }
}

impl TabView for FindInDbTab {
    fn title(&self) -> String {
        if self.query.is_empty() { "Search in data".into() } else { format!("Search: {}", self.query) }
    }

    fn data_source(&self) -> Option<Uuid> {
        Some(self.ds)
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        self.poll(ctx);
        let palette = Palette::current(ui.ctx());
        let running = self.running.is_some();
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("Text to find in table data").desired_width(280.0));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(!running && !self.query.is_empty(), egui::Button::new("Search")).clicked() || enter) && !self.query.is_empty() {
                self.start(ctx);
            }
            ui.label("in");
            ui.weak(self.schema.clone().unwrap_or_else(|| "default schema".into()));
            ui.checkbox(&mut self.case_sensitive, "Case sensitive");
            ui.checkbox(&mut self.all_columns, "All column types");
            ui.checkbox(&mut self.include_views, "Views");
            ui.label("Max rows/table:");
            ui.add(egui::DragValue::new(&mut self.limit_per_table).range(1..=100_000));
        });
        if running {
            let (d, t) = self.progress;
            ui.add(egui::ProgressBar::new(if t == 0 { 0.0 } else { d as f32 / t as f32 }).text(format!("{d} / {t} tables")));
        }
        for e in self.errors.iter().take(5) {
            ui.colored_label(palette.warning, e);
        }
        ui.separator();
        egui::SidePanel::left(egui::Id::new(("find_hits", self.id))).resizable(true).default_width(240.0).show_inside(ui, |ui| {
            if self.hits.is_empty() && !running {
                ui.weak("No results");
            }
            for (i, h) in self.hits.iter().enumerate() {
                let label = format!("{} ({}{})", h.object.display_name(), h.rows, if h.truncated { "+" } else { "" });
                if ui.selectable_label(self.selected == Some(i), label).clicked() {
                    self.selected = Some(i);
                }
            }
        });
        if let Some(h) = self.selected.and_then(|i| self.hits.get_mut(i)) {
            ui.horizontal(|ui| {
                ui.strong(h.object.display_name());
                if ui.button("Open in data editor").clicked() {
                    ctx.act(AppAction::OpenTableData { ds: self.ds, object: h.object.clone(), filter: Some(h.filter.clone()) });
                }
            });
            h.grid.show(ui);
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabularium_core::ColumnInfo;

    #[test]
    fn filter_text_columns_only() {
        let mut t = TableInfo::new(ObjectRef::table(None, "t"));
        t.columns = vec![
            ColumnInfo { name: "id".into(), data_type: "integer".into(), ..Default::default() },
            ColumnInfo { name: "name".into(), data_type: "varchar(20)".into(), ..Default::default() },
            ColumnInfo { name: "pic".into(), data_type: "bytea".into(), ..Default::default() },
        ];
        let f = build_filter(&t, "a_b", Dialect::Postgres, false, false).unwrap();
        assert_eq!(f, "name::text ILIKE '%a\\_b%'");
        let f = build_filter(&t, "x", Dialect::Postgres, true, true).unwrap();
        assert!(f.contains("id::text LIKE") && f.contains("name::text LIKE") && !f.contains("pic"));
    }
}
