//! Copy a table (structure and/or data) to another schema or data source, converting
//! column types between dialects.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tabularium_core::{driver::ExecOptions, ObjectRef, TransactionMode};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, Dialog, NoticeLevel};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

pub struct CopyTableDialog {
    ds: Uuid,
    object: ObjectRef,
    target_ds: Uuid,
    target_schema: String,
    target_name: String,
    create: bool,
    copy_data: bool,
    include_fks: bool,
    task: Option<Pending<Result<usize, String>>>,
    progress: Arc<AtomicUsize>,
    error: Option<String>,
}

impl CopyTableDialog {
    pub fn new(ds: Uuid, object: ObjectRef) -> Self {
        Self {
            ds,
            target_ds: ds,
            target_schema: object.schema.clone().unwrap_or_default(),
            target_name: format!("{}_copy", object.name),
            object,
            create: true,
            copy_data: true,
            include_fks: false,
            task: None,
            progress: Arc::new(AtomicUsize::new(0)),
            error: None,
        }
    }

    fn start(&mut self, ctx: &mut Ctx) {
        let (Some(src), Some(dst)) = (ctx.sources.get(self.ds), ctx.sources.get(self.target_ds)) else { return };
        let src_conn = src.conn.clone();
        let src_dialect = src.config.dialect();
        let dst_cfg = dst.conn.effective_config();
        let dst_dialect = dst.config.dialect();
        let object = self.object.clone();
        let schema = if self.target_schema.trim().is_empty() { None } else { Some(self.target_schema.trim().to_string()) };
        let target = ObjectRef::table(schema.as_deref(), self.target_name.trim().to_string());
        let (create, copy_data, include_fks) = (self.create, self.copy_data, self.include_fks);
        let progress = self.progress.clone();
        progress.store(0, Ordering::Relaxed);
        self.error = None;
        self.task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let s = src_conn.get().await.map_err(|e| e.to_string())?;
            let mut info = s.describe_table(&object).await.map_err(|e| e.to_string())?;
            let dst = tabularium_drivers::connect(&dst_cfg).await.map_err(|e| e.to_string())?;
            if create {
                info.object = target.clone();
                for c in &mut info.columns {
                    c.data_type = tabularium_tools::ddl::convert_type(&c.data_type, src_dialect, dst_dialect);
                    if src_dialect != dst_dialect {
                        // Dialect-specific defaults (nextval(...), now()) rarely translate.
                        c.default = None;
                        c.collation = None;
                    }
                }
                info.triggers.clear();
                if !include_fks {
                    info.foreign_keys.clear();
                }
                if src_dialect != dst_dialect {
                    info.checks.clear();
                    for i in &mut info.indexes {
                        i.definition = None;
                    }
                }
                // Index names are schema-unique in some databases; suffix them.
                for i in &mut info.indexes {
                    i.name = format!("{}_{}", target.name, i.name);
                }
                let ddl = tabularium_tools::ddl::create_table_ddl(&info, dst_dialect);
                for stmt in tabularium_sql::split::split_statements(&ddl, dst_dialect) {
                    dst.execute(&stmt.text, &ExecOptions::default()).await.map_err(|e| format!("{e}\n{}", stmt.text))?;
                }
            }
            let mut copied = 0;
            if copy_data {
                let r = s
                    .execute(&format!("SELECT * FROM {}", object.qualified(src_dialect)), &ExecOptions::unlimited())
                    .await
                    .map_err(|e| e.to_string())?;
                let cols: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
                let types: Vec<String> = cols
                    .iter()
                    .map(|c| info.column(c).map(|col| col.data_type.clone()).unwrap_or_else(|| "text".into()))
                    .collect();
                dst.set_transaction_mode(TransactionMode::Manual).await.map_err(|e| e.to_string())?;
                for chunk in r.rows.chunks(500) {
                    for stmt in tabularium_tools::import::insert_statements(&target, &cols, chunk, Some(&types), dst_dialect, 500) {
                        if let Err(e) = dst.execute(&stmt, &ExecOptions::default()).await {
                            let _ = dst.rollback().await;
                            return Err(e.to_string());
                        }
                    }
                    copied += chunk.len();
                    progress.store(copied, Ordering::Relaxed);
                }
                dst.commit().await.map_err(|e| e.to_string())?;
            }
            let _ = dst.close().await;
            Ok(copied)
        }));
    }
}

impl Dialog for CopyTableDialog {
    fn show(&mut self, ctx: &mut Ctx) -> bool {
        if let Some(r) = poll_slot(&mut self.task) {
            match r {
                Ok(n) => {
                    ctx.notify(NoticeLevel::Success, format!("Copied {} ({n} rows)", self.object.display_name()));
                    ctx.act(AppAction::RefreshExplorer(self.target_ds));
                    ctx.act(AppAction::InvalidateCatalog(self.target_ds));
                    return false;
                }
                Err(e) => self.error = Some(e),
            }
        }
        let palette = Palette::current(ctx.egui);
        let egui_ctx = ctx.egui.clone();
        let mut open = true;
        let mut go = false;
        egui::Window::new(format!("Copy {}", self.object.display_name())).open(&mut open).collapsible(false).show(&egui_ctx, |ui| {
            egui::Grid::new("copy_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Target data source:");
                egui::ComboBox::from_id_salt("copy_ds").selected_text(ctx.sources.name(self.target_ds)).show_ui(ui, |ui| {
                    for d in &ctx.sources.list {
                        ui.selectable_value(&mut self.target_ds, d.config.id, &d.config.name);
                    }
                });
                ui.end_row();
                ui.label("Schema:");
                ui.text_edit_singleline(&mut self.target_schema);
                ui.end_row();
                ui.label("Table name:");
                ui.text_edit_singleline(&mut self.target_name);
                ui.end_row();
                ui.label("");
                ui.vertical(|ui| {
                    ui.checkbox(&mut self.create, "Create table");
                    ui.add_enabled(self.create, egui::Checkbox::new(&mut self.include_fks, "Include foreign keys"));
                    ui.checkbox(&mut self.copy_data, "Copy data");
                });
                ui.end_row();
            });
            if let Some(e) = &self.error {
                ui.colored_label(palette.error, e);
            }
            ui.horizontal(|ui| {
                let busy = self.task.is_some();
                if ui.add_enabled(!busy && !self.target_name.trim().is_empty(), egui::Button::new("Copy")).clicked() {
                    go = true;
                }
                if busy {
                    ui.spinner();
                    ui.weak(format!("{} rows copied", self.progress.load(Ordering::Relaxed)));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                }
            });
        });
        if go {
            self.start(ctx);
        }
        open
    }
}
