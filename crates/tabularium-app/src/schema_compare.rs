//! Schema comparison and synchronization: diff two schemas (possibly on different
//! servers), review changes with side-by-side DDL, and generate a migration script.

use std::collections::HashMap;

use tabularium_core::{Error, SchemaSnapshot};
use tabularium_tools::diff::{diff_schemas, migration_script, DiffOptions, SchemaChange, SchemaDiff};
use tabularium_tools::textdiff::{diff_lines, DiffLine};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, TabView};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

/// Schemas of a data source and its current schema.
type SchemaList = (Vec<String>, Option<String>);

#[derive(Default)]
struct Side {
    ds: Option<Uuid>,
    schema: Option<String>,
    schemas: Vec<String>,
    schemas_for: Option<Uuid>,
    loading_schemas: Option<Pending<Result<SchemaList, Error>>>,
}

pub struct SchemaCompareTab {
    source: Side,
    target: Side,
    opts: DiffOptions,
    running: Option<Pending<Result<(SchemaSnapshot, SchemaSnapshot), Error>>>,
    snapshots: Option<(SchemaSnapshot, SchemaSnapshot)>,
    diff: Option<SchemaDiff>,
    included: Vec<bool>,
    selected: Option<usize>,
    filter: String,
    error: Option<String>,
}

impl SchemaCompareTab {
    pub fn new(_id: u64, ds: Option<Uuid>, schema: Option<String>) -> Self {
        Self {
            source: Side { ds, schema, ..Default::default() },
            target: Side { ds, ..Default::default() },
            opts: DiffOptions::default(),
            running: None,
            snapshots: None,
            diff: None,
            included: vec![],
            selected: None,
            filter: String::new(),
            error: None,
        }
    }

    fn side_ui(ui: &mut egui::Ui, ctx: &mut Ctx, side: &mut Side, label: &str, id: &str) {
        if let Some(Ok((schemas, current))) = poll_slot(&mut side.loading_schemas) {
            side.schemas = schemas;
            if side.schema.is_none() {
                side.schema = current.or_else(|| side.schemas.first().cloned());
            }
        }
        if side.ds.is_some() && side.schemas_for != side.ds {
            side.schemas_for = side.ds;
            side.schemas.clear();
            if let Some(src) = side.ds.and_then(|d| ctx.sources.get(d)) {
                let conn = src.conn.clone();
                side.loading_schemas = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
                    let c = conn.get().await?;
                    Ok((c.list_schemas().await?, c.current_schema().await.ok().flatten()))
                }));
            }
        }
        ui.vertical(|ui| {
            ui.strong(label);
            ui.horizontal(|ui| {
                let name = side.ds.map(|d| ctx.sources.name(d)).unwrap_or_else(|| "choose data source".into());
                egui::ComboBox::from_id_salt((id, "ds")).width(180.0).selected_text(name).show_ui(ui, |ui| {
                    for d in &ctx.sources.list {
                        if ui.selectable_label(side.ds == Some(d.config.id), &d.config.name).clicked() {
                            side.ds = Some(d.config.id);
                            side.schema = None;
                        }
                    }
                });
                if !side.schemas.is_empty() {
                    egui::ComboBox::from_id_salt((id, "schema"))
                        .width(140.0)
                        .selected_text(side.schema.clone().unwrap_or_default())
                        .show_ui(ui, |ui| {
                            for s in &side.schemas {
                                ui.selectable_value(&mut side.schema, Some(s.clone()), s);
                            }
                        });
                }
                if side.loading_schemas.is_some() {
                    ui.spinner();
                }
            });
        });
    }

    fn compare(&mut self, ctx: &mut Ctx) {
        let (Some(a), Some(b)) = (self.source.ds, self.target.ds) else {
            self.error = Some("Choose source and target".into());
            return;
        };
        let (Some(sa), Some(sb)) = (ctx.sources.get(a), ctx.sources.get(b)) else { return };
        let (ca, cb) = (sa.conn.clone(), sb.conn.clone());
        let (schema_a, schema_b) = (self.source.schema.clone(), self.target.schema.clone());
        self.error = None;
        self.diff = None;
        self.selected = None;
        self.running = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let x = ca.get().await?.snapshot_schema(schema_a.as_deref()).await?;
            let y = cb.get().await?.snapshot_schema(schema_b.as_deref()).await?;
            Ok((x, y))
        }));
    }

    fn selected_diff(&self) -> SchemaDiff {
        let Some(d) = &self.diff else { return SchemaDiff::default() };
        SchemaDiff {
            changes: d.changes.iter().zip(&self.included).filter(|(_, inc)| **inc).map(|(c, _)| c.clone()).collect(),
        }
    }

    /// DDL of an object on each side, for the side-by-side view.
    fn ddl_pair(&self, change: &SchemaChange, ctx: &Ctx) -> (String, String) {
        let Some((src, dst)) = &self.snapshots else { return (String::new(), String::new()) };
        let dialect_a = self.source.ds.and_then(|d| ctx.sources.get(d)).map(|d| d.config.dialect()).unwrap_or_default();
        let dialect_b = self.target.ds.and_then(|d| ctx.sources.get(d)).map(|d| d.config.dialect()).unwrap_or_default();
        let name = change.object_name();
        let short = name.rsplit('.').next().unwrap_or(&name).to_string();
        let table_ddl = |snap: &SchemaSnapshot, dialect| {
            snap.tables
                .iter()
                .find(|t| t.object.name == short)
                .map(|t| tabularium_tools::ddl::create_table_ddl(t, dialect))
                .or_else(|| snap.routines.iter().find(|r| r.object.name == short).map(|r| r.definition.clone()))
                .unwrap_or_default()
        };
        (table_ddl(src, dialect_a), table_ddl(dst, dialect_b))
    }
}

impl TabView for SchemaCompareTab {
    fn title(&self) -> String {
        "Schema compare".into()
    }

    fn data_source(&self) -> Option<Uuid> {
        self.target.ds
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        if let Some(r) = poll_slot(&mut self.running) {
            match r {
                Ok((a, b)) => {
                    let d = diff_schemas(&a, &b, &self.opts);
                    self.included = vec![true; d.changes.len()];
                    self.selected = if d.changes.is_empty() { None } else { Some(0) };
                    self.diff = Some(d);
                    self.snapshots = Some((a, b));
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        let palette = Palette::current(ui.ctx());
        ui.horizontal(|ui| {
            Self::side_ui(ui, ctx, &mut self.source, "Source (desired state)", "src");
            if ui.button("⇄").on_hover_text("Swap source and target").clicked() {
                std::mem::swap(&mut self.source, &mut self.target);
                self.diff = None;
            }
            Self::side_ui(ui, ctx, &mut self.target, "Target (will be changed)", "dst");
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.opts.ignore_case, "Ignore identifier case");
            ui.checkbox(&mut self.opts.ignore_comments, "Ignore comments");
            ui.checkbox(&mut self.opts.ignore_column_order, "Ignore column order");
            ui.checkbox(&mut self.opts.compare_indexes, "Indexes");
            ui.checkbox(&mut self.opts.compare_routines, "Routines");
            let busy = self.running.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Compare")).clicked() {
                self.compare(ctx);
            }
            if busy {
                ui.spinner();
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(palette.error, e);
        }
        ui.separator();
        let Some(diff) = self.diff.clone() else {
            ui.weak("Choose two schemas and press Compare.");
            return;
        };
        if diff.changes.is_empty() {
            ui.colored_label(palette.success, "✔ Schemas are identical");
            return;
        }
        let n_selected = self.included.iter().filter(|b| **b).count();
        ui.horizontal(|ui| {
            ui.label(format!("{} differences, {n_selected} selected", diff.changes.len()));
            if ui.small_button("Select all").clicked() {
                self.included.iter_mut().for_each(|b| *b = true);
            }
            if ui.small_button("Select none").clicked() {
                self.included.iter_mut().for_each(|b| *b = false);
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter").desired_width(160.0));
            let target_dialect = self.target.ds.and_then(|d| ctx.sources.get(d)).map(|d| d.config.dialect()).unwrap_or_default();
            if ui.add_enabled(n_selected > 0, egui::Button::new("Generate migration script")).clicked() {
                let script = migration_script(&self.selected_diff(), target_dialect, self.target.schema.as_deref());
                let text = script.join(";\n\n") + ";\n";
                match self.target.ds {
                    Some(ds) => ctx.act(AppAction::OpenConsole { ds, sql: Some(format!("-- Migration generated by Tabularium schema compare\n{text}")), run: false }),
                    None => ctx.act(AppAction::OpenText { title: "Migration".into(), text, ds: None }),
                }
            }
        });
        let changes: Vec<(usize, String, String)> = diff
            .changes
            .iter()
            .enumerate()
            .filter(|(_, c)| self.filter.is_empty() || c.object_name().to_lowercase().contains(&self.filter.to_lowercase()))
            .map(|(i, c)| (i, c.object_name(), c.description()))
            .collect();
        let selected_change = self.selected.and_then(|i| diff.changes.get(i)).cloned();
        egui::SidePanel::left("compare_list").resizable(true).default_width(360.0).show_inside(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut by_object: HashMap<String, Vec<(usize, String)>> = HashMap::new();
                let mut order = vec![];
                for (i, obj, desc) in &changes {
                    if !by_object.contains_key(obj) {
                        order.push(obj.clone());
                    }
                    by_object.entry(obj.clone()).or_default().push((*i, desc.clone()));
                }
                for obj in order {
                    for (i, desc) in &by_object[&obj] {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.included[*i], "");
                            let color = if desc.starts_with("Drop") || desc.starts_with("drop") {
                                palette.error
                            } else if desc.starts_with("Create") || desc.starts_with("create") || desc.starts_with("Add") {
                                palette.success
                            } else {
                                palette.warning
                            };
                            if ui.selectable_label(self.selected == Some(*i), egui::RichText::new(desc).color(color)).clicked() {
                                self.selected = Some(*i);
                            }
                        });
                    }
                }
            });
        });
        if let Some(change) = selected_change {
            let (a, b) = self.ddl_pair(&change, ctx);
            let target_dialect = self.target.ds.and_then(|d| ctx.sources.get(d)).map(|d| d.config.dialect()).unwrap_or_default();
            let stmts = migration_script(&SchemaDiff { changes: vec![change.clone()] }, target_dialect, self.target.schema.as_deref());
            egui::TopBottomPanel::bottom("compare_sql").resizable(true).default_height(150.0).show_inside(ui, |ui| {
                ui.weak("Statements for this change:");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let text = stmts.join(";\n") + ";";
                    let mut t = text.as_str();
                    ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
                });
            });
            ui.columns(2, |cols| {
                cols[0].strong("Source");
                cols[1].strong("Target");
            });
            egui::ScrollArea::both().show(ui, |ui| {
                let lines = diff_lines(&b, &a);
                ui.columns(2, |cols| {
                    for l in &lines {
                        let mono = |s: &str| egui::RichText::new(s.to_string()).monospace();
                        match l {
                            DiffLine::Same(s) => {
                                cols[0].label(mono(s));
                                cols[1].label(mono(s));
                            }
                            DiffLine::Added(s) => {
                                cols[0].label(mono(s).background_color(palette.inserted_row));
                                cols[1].label(mono(""));
                            }
                            DiffLine::Removed(s) => {
                                cols[0].label(mono(""));
                                cols[1].label(mono(s).background_color(palette.deleted_row));
                            }
                        }
                    }
                });
            });
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
