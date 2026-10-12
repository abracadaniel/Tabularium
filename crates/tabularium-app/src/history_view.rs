//! Query history: every executed statement with time, duration, rows and errors.

use crate::ctx::{AppAction, Ctx, TabView};
use crate::theme::Palette;

pub struct HistoryTab {
    query: String,
    connection: Option<String>,
    only_errors: bool,
    selected: Option<usize>,
}

impl HistoryTab {
    pub fn new(_id: u64) -> Self {
        Self { query: String::new(), connection: None, only_errors: false, selected: None }
    }
}

impl TabView for HistoryTab {
    fn title(&self) -> String {
        "Query history".into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        let palette = Palette::current(ui.ctx());
        let mut connections: Vec<String> = ctx.history.entries().iter().map(|e| e.connection.clone()).collect();
        connections.sort();
        connections.dedup();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("🔍 Search SQL").desired_width(300.0));
            egui::ComboBox::from_id_salt("hist_conn")
                .selected_text(self.connection.clone().unwrap_or_else(|| "All data sources".into()))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.connection, None, "All data sources");
                    for c in &connections {
                        ui.selectable_value(&mut self.connection, Some(c.clone()), c);
                    }
                });
            ui.checkbox(&mut self.only_errors, "Errors only");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear history").clicked() {
                    ctx.act(AppAction::Confirm {
                        title: "Clear history".into(),
                        message: "Delete the whole query history?".into(),
                        then: Box::new(AppAction::ClearHistory),
                    });
                }
            });
        });
        ui.separator();
        let entries: Vec<_> = ctx
            .history
            .search(&self.query, self.connection.as_deref())
            .into_iter()
            .filter(|e| !self.only_errors || e.error.is_some())
            .take(2000)
            .cloned()
            .collect();
        let selected_sql = self.selected.and_then(|i| entries.get(i)).map(|e| (e.sql.clone(), e.connection.clone(), e.error.clone()));
        egui::TopBottomPanel::bottom("hist_preview").resizable(true).default_height(180.0).show_inside(ui, |ui| {
            match &selected_sql {
                Some((sql, conn, err)) => {
                    ui.horizontal(|ui| {
                        let ds = ctx.sources.list.iter().find(|d| &d.config.name == conn).map(|d| d.config.id);
                        if ui.add_enabled(ds.is_some(), egui::Button::new("Open in console")).clicked() {
                            if let Some(ds) = ds {
                                ctx.act(AppAction::OpenConsole { ds, sql: Some(sql.clone()), run: false });
                            }
                        }
                        if ui.button("Copy").clicked() {
                            ctx.act(AppAction::CopyToClipboard(sql.clone()));
                        }
                    });
                    if let Some(e) = err {
                        ui.colored_label(palette.error, e);
                    }
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let mut t = sql.as_str();
                        ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
                    });
                }
                None => {
                    ui.weak("Select an entry to preview it; double-click to open it in a console.");
                }
            }
        });
        let row_h = 20.0;
        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .column(egui_extras::Column::initial(150.0))
            .column(egui_extras::Column::initial(140.0))
            .column(egui_extras::Column::initial(70.0))
            .column(egui_extras::Column::initial(70.0))
            .column(egui_extras::Column::remainder())
            .header(22.0, |mut h| {
                for t in ["Time", "Data source", "Duration", "Rows", "SQL"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, entries.len(), |mut row| {
                    let i = row.index();
                    let e = &entries[i];
                    row.set_selected(self.selected == Some(i));
                    row.col(|ui| {
                        ui.label(e.timestamp.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string());
                    });
                    row.col(|ui| {
                        ui.label(&e.connection);
                    });
                    row.col(|ui| {
                        ui.label(format!("{} ms", e.duration_ms));
                    });
                    row.col(|ui| {
                        ui.label(e.rows.map(|r| r.to_string()).unwrap_or_default());
                    });
                    row.col(|ui| {
                        let one: String = e.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                        let text = egui::RichText::new(one).monospace();
                        if e.error.is_some() {
                            ui.label(text.color(palette.error));
                        } else {
                            ui.label(text);
                        }
                    });
                    let r = row.response();
                    if r.clicked() {
                        self.selected = Some(i);
                    }
                    if r.double_clicked() {
                        if let Some(ds) = ctx.sources.list.iter().find(|d| d.config.name == e.connection) {
                            ctx.act(AppAction::OpenConsole { ds: ds.config.id, sql: Some(e.sql.clone()), run: false });
                        }
                    }
                });
            });
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
