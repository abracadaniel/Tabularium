//! Generate realistic-looking test data for a table.

use std::collections::HashMap;

use tabularium_core::{driver::ExecOptions, ObjectRef, TableInfo, TransactionMode, Value};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, Dialog, NoticeLevel};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

type Prepared = (TableInfo, HashMap<String, Vec<Value>>);

pub struct DataGenDialog {
    ds: Uuid,
    object: ObjectRef,
    count: usize,
    seed: u64,
    prepare: Option<Pending<Result<Prepared, String>>>,
    prepared: Option<Prepared>,
    preview: Option<(Vec<String>, Vec<Vec<Value>>)>,
    task: Option<Pending<Result<usize, String>>>,
    error: Option<String>,
    started: bool,
}

impl DataGenDialog {
    pub fn new(ds: Uuid, object: ObjectRef) -> Self {
        Self {
            ds,
            object,
            count: 100,
            seed: 42,
            prepare: None,
            prepared: None,
            preview: None,
            task: None,
            error: None,
            started: false,
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.started = true;
        let Some(src) = ctx.sources.get(self.ds) else { return };
        let conn = src.conn.clone();
        let dialect = src.config.dialect();
        let object = self.object.clone();
        self.prepare = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await.map_err(|e| e.to_string())?;
            let info = c.describe_table(&object).await.map_err(|e| e.to_string())?;
            // Existing key values for foreign key columns.
            let mut fk_values = HashMap::new();
            for fk in &info.foreign_keys {
                for (col, ref_col) in fk.columns.iter().zip(&fk.ref_columns) {
                    let rt = ObjectRef::table(fk.ref_schema.as_deref().or(object.schema.as_deref()), fk.ref_table.clone());
                    let sql = format!(
                        "SELECT DISTINCT {} FROM {}",
                        dialect.quote_ident_if_needed(ref_col),
                        rt.qualified(dialect)
                    );
                    if let Ok(r) = c.execute(&sql, &ExecOptions::with_limit(1000)).await {
                        fk_values.insert(col.clone(), r.rows.into_iter().filter_map(|mut row| row.drain(..).next()).collect());
                    }
                }
            }
            Ok((info, fk_values))
        }));
    }

    fn regenerate_preview(&mut self) {
        if let Some((info, fks)) = &self.prepared {
            self.preview = Some(tabularium_tools::datagen::generate_rows(info, self.count.min(20), self.seed, fks));
        }
    }

    fn start(&mut self, ctx: &mut Ctx) {
        let Some((info, fks)) = self.prepared.clone() else { return };
        let Some(src) = ctx.sources.get(self.ds) else { return };
        let cfg = src.conn.effective_config();
        let dialect = src.config.dialect();
        let (cols, rows) = tabularium_tools::datagen::generate_rows(&info, self.count, self.seed, &fks);
        let types: Vec<String> =
            cols.iter().map(|c| info.column(c).map(|col| col.data_type.clone()).unwrap_or_default()).collect();
        let statements = tabularium_tools::import::insert_statements(&info.object, &cols, &rows, Some(&types), dialect, 200);
        let n = rows.len();
        self.error = None;
        self.task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = tabularium_drivers::connect(&cfg).await.map_err(|e| e.to_string())?;
            c.set_transaction_mode(TransactionMode::Manual).await.map_err(|e| e.to_string())?;
            for s in &statements {
                if let Err(e) = c.execute(s, &ExecOptions::default()).await {
                    let _ = c.rollback().await;
                    return Err(e.to_string());
                }
            }
            c.commit().await.map_err(|e| e.to_string())?;
            let _ = c.close().await;
            Ok(n)
        }));
    }
}

impl Dialog for DataGenDialog {
    fn show(&mut self, ctx: &mut Ctx) -> bool {
        if !self.started {
            self.load(ctx);
        }
        if let Some(r) = poll_slot(&mut self.prepare) {
            match r {
                Ok(p) => {
                    self.prepared = Some(p);
                    self.regenerate_preview();
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(r) = poll_slot(&mut self.task) {
            match r {
                Ok(n) => {
                    ctx.notify(NoticeLevel::Success, format!("Inserted {n} generated rows into {}", self.object.display_name()));
                    ctx.act(AppAction::OpenTableData { ds: self.ds, object: self.object.clone(), filter: None });
                    return false;
                }
                Err(e) => self.error = Some(e),
            }
        }
        let palette = Palette::current(ctx.egui);
        let egui_ctx = ctx.egui.clone();
        let mut open = true;
        let mut go = false;
        let mut regen = false;
        egui::Window::new(format!("Generate data for {}", self.object.display_name()))
            .open(&mut open)
            .default_width(640.0)
            .show(&egui_ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Rows:");
                    regen |= ui.add(egui::DragValue::new(&mut self.count).range(1..=1_000_000)).changed();
                    ui.label("Seed:");
                    regen |= ui.add(egui::DragValue::new(&mut self.seed)).changed();
                    if ui.button("🎲").on_hover_text("New random seed").clicked() {
                        self.seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
                        regen = true;
                    }
                });
                if self.prepare.is_some() {
                    ui.spinner();
                }
                if let Some((cols, rows)) = &self.preview {
                    ui.weak("Preview (first 20 rows):");
                    egui::ScrollArea::both().max_height(300.0).show(ui, |ui| {
                        egui::Grid::new("datagen_preview").striped(true).show(ui, |ui| {
                            for c in cols {
                                ui.strong(c);
                            }
                            ui.end_row();
                            for r in rows {
                                for v in r {
                                    ui.label(v.display_text().unwrap_or_else(|| "NULL".into()));
                                }
                                ui.end_row();
                            }
                        });
                    });
                }
                if let Some(e) = &self.error {
                    ui.colored_label(palette.error, e);
                }
                ui.horizontal(|ui| {
                    let busy = self.task.is_some();
                    if ui.add_enabled(!busy && self.prepared.is_some(), egui::Button::new(format!("Insert {} rows", self.count))).clicked() {
                        go = true;
                    }
                    if busy {
                        ui.spinner();
                    }
                });
            });
        if regen {
            self.regenerate_preview();
        }
        if go {
            self.start(ctx);
        }
        open
    }
}
