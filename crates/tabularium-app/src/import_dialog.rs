//! Import data from CSV/TSV/JSON/XLSX files into a new or existing table.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tabularium_core::{driver::ExecOptions, ObjectKind, ObjectRef, TableInfo, TransactionMode, Value};
use tabularium_tools::import::{self, ImportFormat, ImportOptions, ImportedData};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, Dialog, NoticeLevel};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Target {
    New,
    Existing,
}

pub struct ImportDialog {
    ds: Uuid,
    schema: Option<String>,
    path: String,
    format: ImportFormat,
    has_header: bool,
    delimiter: String,
    sheet: String,
    null_text: String,
    skip_rows: usize,
    trim: bool,
    preview: Option<ImportedData>,
    preview_error: Option<String>,
    target: Target,
    new_table: String,
    types: Vec<String>,
    existing: Option<ObjectRef>,
    existing_info: Option<TableInfo>,
    describe: Option<Pending<Result<TableInfo, String>>>,
    /// file column index -> target column name ("" = skip)
    mapping: Vec<String>,
    use_transaction: bool,
    batch_size: usize,
    task: Option<Pending<Result<usize, String>>>,
    progress: Arc<AtomicUsize>,
    total: usize,
    error: Option<String>,
}

impl ImportDialog {
    pub fn new(ds: Uuid, schema: Option<String>, table: Option<ObjectRef>) -> Self {
        Self {
            ds,
            schema,
            path: String::new(),
            format: ImportFormat::Csv,
            has_header: true,
            delimiter: String::new(),
            sheet: String::new(),
            null_text: String::new(),
            skip_rows: 0,
            trim: true,
            preview: None,
            preview_error: None,
            target: if table.is_some() { Target::Existing } else { Target::New },
            new_table: String::new(),
            types: vec![],
            existing: table,
            existing_info: None,
            describe: None,
            mapping: vec![],
            use_transaction: true,
            batch_size: 500,
            task: None,
            progress: Arc::new(AtomicUsize::new(0)),
            total: 0,
            error: None,
        }
    }

    fn options(&self, max_rows: Option<usize>) -> ImportOptions {
        let mut o = ImportOptions::new(self.format);
        o.has_header = self.has_header;
        o.delimiter = match self.delimiter.as_str() {
            "" => None,
            "\\t" | "tab" => Some('\t'),
            s => s.chars().next(),
        };
        o.sheet = if self.sheet.trim().is_empty() { None } else { Some(self.sheet.trim().to_string()) };
        o.null_text = if self.null_text.is_empty() { None } else { Some(self.null_text.clone()) };
        o.skip_rows = self.skip_rows;
        o.max_rows = max_rows;
        o.trim = self.trim;
        o
    }

    fn pick_file(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Data files", &["csv", "tsv", "txt", "json", "jsonl", "ndjson", "xlsx", "xls", "ods"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.set_path(p);
        }
    }

    fn set_path(&mut self, p: PathBuf) {
        self.path = p.to_string_lossy().into_owned();
        if let Some(f) = ImportFormat::from_path(&p) {
            self.format = f;
        }
        if matches!(self.format, ImportFormat::Csv | ImportFormat::Tsv) {
            if let Ok(bytes) = std::fs::read(&p) {
                let sample = String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)]).into_owned();
                let (delim, header) = import::sniff_csv(&sample);
                self.delimiter = if delim == '\t' { "\\t".into() } else { delim.to_string() };
                self.has_header = header;
            }
        }
        if self.new_table.is_empty() {
            self.new_table = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase().replace(|c: char| !c.is_alphanumeric(), "_"))
                .unwrap_or_else(|| "imported".into());
        }
        self.reload_preview();
    }

    fn reload_preview(&mut self) {
        self.preview_error = None;
        match import::read_file(std::path::Path::new(&self.path), &self.options(Some(200))) {
            Ok(data) => {
                self.types = import::infer_column_types(&data, tabularium_core::Dialect::Generic);
                self.preview = Some(data);
                self.auto_map();
            }
            Err(e) => {
                self.preview = None;
                self.preview_error = Some(e.to_string());
            }
        }
    }

    fn auto_map(&mut self) {
        let Some(data) = &self.preview else { return };
        self.mapping = data
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| match &self.existing_info {
                Some(info) => info
                    .column(c)
                    .map(|col| col.name.clone())
                    .or_else(|| (!self.has_header).then(|| info.columns.get(i).map(|col| col.name.clone())).flatten())
                    .unwrap_or_default(),
                None => c.clone(),
            })
            .collect();
    }

    fn start(&mut self, ctx: &mut Ctx) {
        self.error = None;
        let Some(source) = ctx.sources.get(self.ds) else { return };
        let dialect = source.config.dialect();
        let cfg = source.conn.effective_config();
        let data = match import::read_file(std::path::Path::new(&self.path), &self.options(None)) {
            Ok(d) => d,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let (table, create_sql, columns, types, keep): (ObjectRef, Option<String>, Vec<String>, Vec<String>, Vec<usize>) =
            match self.target {
                Target::New => {
                    let name = self.new_table.trim();
                    if name.is_empty() {
                        self.error = Some("Enter a table name".into());
                        return;
                    }
                    let table = ObjectRef::table(self.schema.as_deref(), name);
                    let keep: Vec<usize> = (0..data.columns.len()).filter(|i| self.mapping.get(*i).is_some_and(|m| !m.is_empty())).collect();
                    let cols: Vec<String> = keep.iter().map(|&i| self.mapping[i].clone()).collect();
                    let types: Vec<String> = keep.iter().map(|&i| self.types.get(i).cloned().unwrap_or_else(|| "text".into())).collect();
                    let ddl = import::create_table_sql(&table, &cols, &types, dialect);
                    (table, Some(ddl), cols, types, keep)
                }
                Target::Existing => {
                    let (Some(table), Some(info)) = (self.existing.clone(), self.existing_info.clone()) else {
                        self.error = Some("Choose a target table".into());
                        return;
                    };
                    let keep: Vec<usize> = (0..data.columns.len()).filter(|i| self.mapping.get(*i).is_some_and(|m| !m.is_empty())).collect();
                    let cols: Vec<String> = keep.iter().map(|&i| self.mapping[i].clone()).collect();
                    let types: Vec<String> = cols.iter().map(|c| info.column(c).map(|col| col.data_type.clone()).unwrap_or_else(|| "text".into())).collect();
                    (table, None, cols, types, keep)
                }
            };
        if columns.is_empty() {
            self.error = Some("No columns mapped".into());
            return;
        }
        let rows: Vec<Vec<Value>> = data.rows.iter().map(|r| keep.iter().map(|&i| r.get(i).cloned().unwrap_or(Value::Null)).collect()).collect();
        let statements = import::insert_statements(&table, &columns, &rows, Some(&types), dialect, self.batch_size.max(1));
        let batch = self.batch_size.max(1);
        self.total = rows.len();
        self.progress.store(0, Ordering::Relaxed);
        let progress = self.progress.clone();
        let use_tx = self.use_transaction;
        let total = rows.len();
        self.task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            // A dedicated session so the import transaction doesn't affect other tools.
            let conn = tabularium_drivers::connect(&cfg).await.map_err(|e| e.to_string())?;
            if let Some(ddl) = create_sql {
                conn.execute(&ddl, &ExecOptions::default()).await.map_err(|e| format!("CREATE TABLE failed: {e}"))?;
            }
            if use_tx {
                conn.set_transaction_mode(TransactionMode::Manual).await.map_err(|e| e.to_string())?;
            }
            for (i, s) in statements.iter().enumerate() {
                if let Err(e) = conn.execute(s, &ExecOptions::default()).await {
                    if use_tx {
                        let _ = conn.rollback().await;
                    }
                    let _ = conn.close().await;
                    return Err(format!("batch {} failed (rows {}–{}): {e}", i + 1, i * batch + 1, ((i + 1) * batch).min(total)));
                }
                progress.store(((i + 1) * batch).min(total), Ordering::Relaxed);
            }
            if use_tx {
                conn.commit().await.map_err(|e| e.to_string())?;
            }
            let _ = conn.close().await;
            Ok(total)
        }));
    }
}

impl Dialog for ImportDialog {
    fn show(&mut self, ctx: &mut Ctx) -> bool {
        if let Some(r) = poll_slot(&mut self.task) {
            match r {
                Ok(n) => {
                    ctx.notify(NoticeLevel::Success, format!("Imported {n} rows"));
                    ctx.act(AppAction::RefreshExplorer(self.ds));
                    ctx.act(AppAction::InvalidateCatalog(self.ds));
                    return false;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(r) = poll_slot(&mut self.describe) {
            match r {
                Ok(info) => {
                    self.existing_info = Some(info);
                    self.auto_map();
                }
                Err(e) => self.error = Some(e),
            }
        }
        if self.target == Target::Existing && self.existing.is_some() && self.existing_info.is_none() && self.describe.is_none() {
            if let (Some(source), Some(obj)) = (ctx.sources.get(self.ds), self.existing.clone()) {
                let conn = source.conn.clone();
                self.describe = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
                    let c = conn.get().await.map_err(|e| e.to_string())?;
                    c.describe_table(&obj).await.map_err(|e| e.to_string())
                }));
            }
        }
        let palette = Palette::current(ctx.egui);
        let egui_ctx = ctx.egui.clone();
        let tables: Vec<(Option<String>, String)> = ctx
            .sources
            .get(self.ds)
            .map(|s| {
                let cat = s.catalog.lock().unwrap();
                cat.tables.iter().filter(|t| t.kind == ObjectKind::Table).map(|t| (t.schema.clone(), t.name.clone())).collect()
            })
            .unwrap_or_default();
        let mut open = true;
        let mut go = false;
        let mut reload = false;
        egui::Window::new(format!("Import data into {}", ctx.sources.name(self.ds)))
            .open(&mut open)
            .default_width(720.0)
            .resizable(true)
            .show(&egui_ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("File:");
                    ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(420.0));
                    if ui.button("…").clicked() {
                        self.pick_file();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Format:");
                    egui::ComboBox::from_id_salt("imp_fmt").selected_text(self.format.label()).show_ui(ui, |ui| {
                        for f in [ImportFormat::Csv, ImportFormat::Tsv, ImportFormat::Json, ImportFormat::JsonLines, ImportFormat::Xlsx] {
                            if ui.selectable_value(&mut self.format, f, f.label()).clicked() {
                                reload = true;
                            }
                        }
                    });
                    match self.format {
                        ImportFormat::Csv | ImportFormat::Tsv => {
                            reload |= ui.checkbox(&mut self.has_header, "Header row").changed();
                            ui.label("Delimiter:");
                            reload |= ui.add(egui::TextEdit::singleline(&mut self.delimiter).desired_width(30.0)).lost_focus();
                            ui.label("Skip rows:");
                            reload |= ui.add(egui::DragValue::new(&mut self.skip_rows).range(0..=10_000)).changed();
                        }
                        ImportFormat::Xlsx => {
                            reload |= ui.checkbox(&mut self.has_header, "Header row").changed();
                            ui.label("Sheet:");
                            reload |= ui.add(egui::TextEdit::singleline(&mut self.sheet).hint_text("first").desired_width(100.0)).lost_focus();
                        }
                        _ => {}
                    }
                    ui.label("NULL text:");
                    reload |= ui.add(egui::TextEdit::singleline(&mut self.null_text).hint_text("(none)").desired_width(60.0)).lost_focus();
                    reload |= ui.checkbox(&mut self.trim, "Trim").changed();
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Target:");
                    ui.radio_value(&mut self.target, Target::New, "New table");
                    ui.radio_value(&mut self.target, Target::Existing, "Existing table");
                    match self.target {
                        Target::New => {
                            ui.add(egui::TextEdit::singleline(&mut self.new_table).hint_text("table name").desired_width(200.0));
                            if let Some(s) = &self.schema {
                                ui.weak(format!("in {s}"));
                            }
                        }
                        Target::Existing => {
                            let label = self.existing.as_ref().map(|o| o.display_name()).unwrap_or_else(|| "choose…".into());
                            egui::ComboBox::from_id_salt("imp_target").selected_text(label).width(220.0).show_ui(ui, |ui| {
                                for (s, n) in &tables {
                                    let obj = ObjectRef::table(s.as_deref(), n.clone());
                                    let selected = self.existing.as_ref() == Some(&obj);
                                    if ui.selectable_label(selected, obj.display_name()).clicked() {
                                        self.existing = Some(obj);
                                        self.existing_info = None;
                                    }
                                }
                            });
                        }
                    }
                });
                if let Some(e) = &self.preview_error {
                    ui.colored_label(palette.error, e);
                }
                if let Some(data) = &self.preview {
                    ui.weak(format!("Preview: {} columns, first {} rows", data.columns.len(), data.rows.len()));
                    let target_cols: Vec<String> = self.existing_info.as_ref().map(|i| i.columns.iter().map(|c| c.name.clone()).collect()).unwrap_or_default();
                    egui::ScrollArea::both().max_height(320.0).show(ui, |ui| {
                        egui::Grid::new("imp_preview").striped(true).show(ui, |ui| {
                            for (i, c) in data.columns.iter().enumerate() {
                                ui.vertical(|ui| {
                                    ui.strong(c);
                                    if let Some(m) = self.mapping.get_mut(i) {
                                        match self.target {
                                            Target::New => {
                                                ui.add(egui::TextEdit::singleline(m).desired_width(110.0).hint_text("skip"));
                                                if let Some(t) = self.types.get_mut(i) {
                                                    ui.add(egui::TextEdit::singleline(t).desired_width(110.0).font(egui::TextStyle::Monospace));
                                                }
                                            }
                                            Target::Existing => {
                                                egui::ComboBox::from_id_salt(("map", i))
                                                    .width(110.0)
                                                    .selected_text(if m.is_empty() { "skip".to_string() } else { m.clone() })
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(m, String::new(), "skip");
                                                        for t in &target_cols {
                                                            ui.selectable_value(m, t.clone(), t);
                                                        }
                                                    });
                                            }
                                        }
                                    }
                                });
                            }
                            ui.end_row();
                            for row in data.rows.iter().take(50) {
                                for v in row {
                                    match v.display_text() {
                                        Some(t) => ui.label(t.chars().take(40).collect::<String>()),
                                        None => ui.label(egui::RichText::new("NULL").italics().color(palette.null)),
                                    };
                                }
                                ui.end_row();
                            }
                        });
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.use_transaction, "Single transaction (roll back on error)");
                    ui.label("Batch size:");
                    ui.add(egui::DragValue::new(&mut self.batch_size).range(1..=10_000));
                });
                if let Some(e) = &self.error {
                    ui.colored_label(palette.error, e);
                }
                ui.horizontal(|ui| {
                    let busy = self.task.is_some();
                    if ui.add_enabled(!busy && self.preview.is_some(), egui::Button::new("Import")).clicked() {
                        go = true;
                    }
                    if busy {
                        let done = self.progress.load(Ordering::Relaxed);
                        let frac = if self.total == 0 { 0.0 } else { done as f32 / self.total as f32 };
                        ui.add(egui::ProgressBar::new(frac).text(format!("{done} / {}", self.total)).desired_width(240.0));
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                    }
                });
            });
        if reload && !self.path.is_empty() {
            self.reload_preview();
        }
        if go {
            self.start(ctx);
        }
        open
    }
}
