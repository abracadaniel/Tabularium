//! Export data dialog: result rows, whole tables or queries to CSV/TSV/JSON/SQL/
//! Markdown/HTML/XML/XLSX/text, to a file or the clipboard.

use std::path::PathBuf;

use tabularium_core::{driver::ExecOptions, Dialect, ObjectRef};
use tabularium_tools::export::{ExportFormat, ExportOptions, StreamingExporter};

use crate::ctx::{Ctx, Dialog, ExportSource, NoticeLevel};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Destination {
    File,
    Clipboard,
}

pub struct ExportDialog {
    source: ExportSource,
    format: ExportFormat,
    include_header: bool,
    delimiter: String,
    quote_all: bool,
    null_text: String,
    pretty: bool,
    rows_per_insert: usize,
    table_name: String,
    key_columns: String,
    destination: Destination,
    path: String,
    task: Option<Pending<Result<String, String>>>,
    error: Option<String>,
    initialized: bool,
    done: bool,
}

impl ExportDialog {
    pub fn new(source: ExportSource) -> Self {
        let table_name = match &source {
            ExportSource::Rows { table: Some(t), .. } | ExportSource::Table { object: t, .. } => t.display_name(),
            _ => "my_table".into(),
        };
        Self {
            source,
            format: ExportFormat::Csv,
            include_header: true,
            delimiter: String::new(),
            quote_all: false,
            null_text: String::new(),
            pretty: true,
            rows_per_insert: 100,
            table_name,
            key_columns: String::new(),
            destination: Destination::File,
            path: String::new(),
            task: None,
            error: None,
            initialized: false,
            done: false,
        }
    }

    fn base_name(&self) -> String {
        match &self.source {
            ExportSource::Rows { table: Some(t), .. } | ExportSource::Table { object: t, .. } => t.name.clone(),
            _ => "result".into(),
        }
    }

    fn default_path(&self, ctx: &Ctx) -> String {
        let dir = directories_home().unwrap_or_else(|| ctx.dirs.data.clone());
        dir.join(format!("{}.{}", self.base_name(), self.format.extension())).to_string_lossy().into_owned()
    }

    fn options(&self, dialect: Dialect) -> ExportOptions {
        let mut o = ExportOptions::new(self.format);
        o.include_header = self.include_header;
        o.dialect = dialect;
        o.null_text = self.null_text.clone();
        o.delimiter = match self.delimiter.as_str() {
            "" => None,
            "\\t" | "tab" => Some('\t'),
            s => s.chars().next(),
        };
        o.quote_all = self.quote_all;
        o.pretty = self.pretty;
        o.rows_per_insert = self.rows_per_insert.max(1);
        let tn = self.table_name.trim();
        o.table = if tn.is_empty() {
            None
        } else {
            Some(match tn.split_once('.') {
                Some((s, n)) => ObjectRef::table(Some(s), n),
                None => ObjectRef::table(None, tn),
            })
        };
        o.key_columns = self.key_columns.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        o
    }

    fn dialect(&self, ctx: &Ctx) -> Dialect {
        match &self.source {
            ExportSource::Table { ds, .. } | ExportSource::Query { ds, .. } => {
                ctx.sources.get(*ds).map(|d| d.config.dialect()).unwrap_or_default()
            }
            ExportSource::Rows { .. } => Dialect::Generic,
        }
    }

    fn start(&mut self, ctx: &mut Ctx) {
        self.error = None;
        let dialect = self.dialect(ctx);
        let opts = self.options(dialect);
        let to_clipboard = self.destination == Destination::Clipboard;
        if to_clipboard && self.format.is_binary() {
            self.error = Some("Binary formats can only be written to a file".into());
            return;
        }
        let path = PathBuf::from(self.path.trim());
        if !to_clipboard && self.path.trim().is_empty() {
            self.error = Some("Choose a file".into());
            return;
        }
        match self.source.clone() {
            ExportSource::Rows { columns, rows, .. } => {
                let result = if to_clipboard {
                    tabularium_tools::export::export_to_string(&columns, &rows, &opts).map(|s| {
                        ctx.egui.copy_text(s);
                        format!("Copied {} rows to the clipboard", rows.len())
                    })
                } else {
                    tabularium_tools::export::export_to_file(&columns, &rows, &opts, &path)
                        .map(|_| format!("Exported {} rows to {}", rows.len(), path.display()))
                };
                match result {
                    Ok(msg) => {
                        ctx.notify(NoticeLevel::Success, msg);
                        self.done = true;
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            ExportSource::Table { ds, object } => {
                let sql = format!("SELECT * FROM {}", object.qualified(dialect));
                self.spawn_query_export(ctx, ds, sql, opts, path, to_clipboard);
            }
            ExportSource::Query { ds, sql } => {
                self.spawn_query_export(ctx, ds, sql, opts, path, to_clipboard);
            }
        }
    }

    fn spawn_query_export(
        &mut self,
        ctx: &mut Ctx,
        ds: uuid::Uuid,
        sql: String,
        opts: ExportOptions,
        path: PathBuf,
        to_clipboard: bool,
    ) {
        let Some(source) = ctx.sources.get(ds) else { return };
        let conn = source.conn.clone();
        let egui_ctx = ctx.egui.clone();
        self.task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await.map_err(|e| e.to_string())?;
            let r = c.execute(&sql, &ExecOptions::unlimited()).await.map_err(|e| e.to_string())?;
            let n = r.rows.len();
            if to_clipboard {
                let s = tabularium_tools::export::export_to_string(&r.columns, &r.rows, &opts).map_err(|e| e.to_string())?;
                egui_ctx.copy_text(s);
                return Ok(format!("Copied {n} rows to the clipboard"));
            }
            if opts.format.is_binary() {
                tabularium_tools::export::export_to_file(&r.columns, &r.rows, &opts, &path).map_err(|e| e.to_string())?;
            } else {
                // Stream in chunks so huge results don't need a second full copy in memory.
                let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
                let mut exporter = StreamingExporter::new(r.columns.clone(), opts, Box::new(std::io::BufWriter::new(file)))
                    .map_err(|e| e.to_string())?;
                for chunk in r.rows.chunks(5000) {
                    exporter.write_rows(chunk).map_err(|e| e.to_string())?;
                }
                exporter.finish().map_err(|e| e.to_string())?;
            }
            Ok(format!("Exported {n} rows to {}", path.display()))
        }));
    }
}

impl Dialog for ExportDialog {
    fn show(&mut self, ctx: &mut Ctx) -> bool {
        if self.done {
            return false;
        }
        if !self.initialized {
            self.initialized = true;
            self.null_text = String::new();
            self.path = self.default_path(ctx);
        }
        if let Some(r) = poll_slot(&mut self.task) {
            match r {
                Ok(msg) => {
                    ctx.notify(NoticeLevel::Success, msg);
                    return false;
                }
                Err(e) => self.error = Some(e),
            }
        }
        let palette = Palette::current(ctx.egui);
        let mut open = true;
        let mut go = false;
        let what = match &self.source {
            ExportSource::Rows { rows, .. } => format!("{} rows from the result", rows.len()),
            ExportSource::Table { object, .. } => format!("table {}", object.display_name()),
            ExportSource::Query { sql, .. } => format!("query: {}", sql.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect::<String>()),
        };
        let egui_ctx = ctx.egui.clone();
        egui::Window::new("Export data").open(&mut open).collapsible(false).resizable(false).default_width(480.0).show(&egui_ctx, |ui| {
            ui.weak(format!("Exporting {what}"));
            ui.separator();
            egui::Grid::new("export_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Format:");
                let before = self.format;
                egui::ComboBox::from_id_salt("export_format").selected_text(self.format.label()).show_ui(ui, |ui| {
                    for f in ExportFormat::ALL {
                        ui.selectable_value(&mut self.format, *f, f.label());
                    }
                });
                if before != self.format {
                    // Keep the file name but switch the extension.
                    let p = PathBuf::from(&self.path);
                    self.path = p.with_extension(self.format.extension()).to_string_lossy().into_owned();
                    if self.format.is_binary() {
                        self.destination = Destination::File;
                    }
                }
                ui.end_row();
                match self.format {
                    ExportFormat::Csv | ExportFormat::Tsv => {
                        ui.label("Delimiter:");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.delimiter).hint_text(if self.format == ExportFormat::Csv { "," } else { "\\t" }).desired_width(40.0));
                            ui.checkbox(&mut self.quote_all, "Quote all values");
                        });
                        ui.end_row();
                    }
                    ExportFormat::SqlInsert | ExportFormat::SqlInsertMultiRow | ExportFormat::SqlUpdate => {
                        ui.label("Table name:");
                        ui.text_edit_singleline(&mut self.table_name);
                        ui.end_row();
                        if self.format == ExportFormat::SqlInsertMultiRow {
                            ui.label("Rows per INSERT:");
                            ui.add(egui::DragValue::new(&mut self.rows_per_insert).range(1..=100_000));
                            ui.end_row();
                        }
                        if self.format == ExportFormat::SqlUpdate {
                            ui.label("Key columns:");
                            ui.add(egui::TextEdit::singleline(&mut self.key_columns).hint_text("id, other_id"));
                            ui.end_row();
                        }
                    }
                    ExportFormat::Json => {
                        ui.label("");
                        ui.checkbox(&mut self.pretty, "Pretty print");
                        ui.end_row();
                    }
                    _ => {}
                }
                if !matches!(self.format, ExportFormat::Json | ExportFormat::JsonLines | ExportFormat::SqlInsert | ExportFormat::SqlInsertMultiRow | ExportFormat::SqlUpdate | ExportFormat::Xml) {
                    ui.label("");
                    ui.checkbox(&mut self.include_header, "Include column header");
                    ui.end_row();
                }
                ui.label("NULL as:");
                ui.add(egui::TextEdit::singleline(&mut self.null_text).hint_text("(empty)").desired_width(80.0));
                ui.end_row();
                ui.label("Destination:");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.destination, Destination::File, "File");
                    ui.add_enabled_ui(!self.format.is_binary(), |ui| {
                        ui.radio_value(&mut self.destination, Destination::Clipboard, "Clipboard");
                    });
                });
                ui.end_row();
                if self.destination == Destination::File {
                    ui.label("File:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(300.0));
                        if ui.button("…").clicked() {
                            let name = PathBuf::from(&self.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            if let Some(p) = rfd::FileDialog::new()
                                .add_filter(self.format.label(), &[self.format.extension()])
                                .set_file_name(name)
                                .save_file()
                            {
                                self.path = p.to_string_lossy().into_owned();
                            }
                        }
                    });
                    ui.end_row();
                }
            });
            if let Some(e) = &self.error {
                ui.colored_label(palette.error, e);
            }
            ui.separator();
            ui.horizontal(|ui| {
                let busy = self.task.is_some();
                if ui.add_enabled(!busy, egui::Button::new("Export")).clicked() {
                    go = true;
                }
                if busy {
                    ui.spinner();
                    ui.weak("exporting…");
                }
            });
        });
        if go {
            self.start(ctx);
        }
        open && !self.done
    }
}

fn directories_home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}
