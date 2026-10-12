//! Create / modify table dialog: edit columns, keys, indexes, foreign keys and checks
//! with a live preview of the CREATE or ALTER statements.

use tabularium_core::{
    driver::ExecOptions, CheckInfo, ColumnInfo, Dialect, Error, FkAction, ForeignKeyInfo, IndexInfo, KeyInfo, ObjectRef,
    TableInfo,
};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, TabView};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

#[derive(Clone)]
struct ColumnRow {
    original: Option<String>,
    col: ColumnInfo,
    pk: bool,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Page {
    Columns,
    Indexes,
    ForeignKeys,
    Checks,
    Options,
}

pub struct TableDesignerTab {
    ds: Uuid,
    original: Option<TableInfo>,
    schema: Option<String>,
    name: String,
    comment: String,
    columns: Vec<ColumnRow>,
    indexes: Vec<(String, String, bool)>,
    fks: Vec<ForeignKeyInfo>,
    fk_columns_text: Vec<(String, String)>,
    checks: Vec<CheckInfo>,
    page: Page,
    load: Option<Pending<Result<TableInfo, Error>>>,
    exec: Option<Pending<Result<(), (usize, Error)>>>,
    error: Option<String>,
    started: bool,
    target: Option<ObjectRef>,
}

const COMMON_TYPES: &[(Dialect, &[&str])] = &[
    (Dialect::Postgres, &["integer", "bigint", "smallint", "serial", "bigserial", "numeric(10,2)", "real", "double precision", "boolean", "text", "varchar(255)", "char(1)", "date", "time", "timestamp", "timestamptz", "interval", "uuid", "json", "jsonb", "bytea", "inet", "integer[]", "text[]"]),
    (Dialect::MySql, &["int", "bigint", "smallint", "tinyint(1)", "decimal(10,2)", "float", "double", "varchar(255)", "char(1)", "text", "longtext", "date", "time", "datetime", "timestamp", "json", "blob", "enum('a','b')"]),
    (Dialect::Sqlite, &["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC", "BOOLEAN", "DATE", "DATETIME"]),
    (Dialect::MsSql, &["int", "bigint", "smallint", "bit", "decimal(10,2)", "float", "nvarchar(255)", "nvarchar(max)", "varchar(255)", "date", "datetime2", "datetimeoffset", "uniqueidentifier", "varbinary(max)"]),
];

impl TableDesignerTab {
    pub fn new(_id: u64, ds: Uuid, object: Option<ObjectRef>, schema: Option<String>) -> Self {
        let mut t = Self {
            ds,
            original: None,
            schema: schema.clone(),
            name: String::new(),
            comment: String::new(),
            columns: vec![],
            indexes: vec![],
            fks: vec![],
            fk_columns_text: vec![],
            checks: vec![],
            page: Page::Columns,
            load: None,
            exec: None,
            error: None,
            started: object.is_none(),
            target: object,
        };
        if t.target.is_none() {
            t.name = "new_table".into();
            t.columns.push(ColumnRow {
                original: None,
                col: ColumnInfo { name: "id".into(), data_type: "integer".into(), nullable: false, auto_increment: true, ordinal: 1, ..Default::default() },
                pk: true,
            });
        }
        t
    }

    fn dialect(&self, ctx: &Ctx) -> Dialect {
        ctx.sources.get(self.ds).map(|d| d.config.dialect()).unwrap_or_default()
    }

    fn set_from(&mut self, info: TableInfo) {
        let pk: Vec<String> = info.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        self.schema = info.object.schema.clone();
        self.name = info.object.name.clone();
        self.comment = info.comment.clone().unwrap_or_default();
        self.columns = info
            .columns
            .iter()
            .map(|c| ColumnRow { original: Some(c.name.clone()), col: c.clone(), pk: pk.contains(&c.name) })
            .collect();
        self.indexes = info
            .indexes
            .iter()
            .filter(|i| !i.primary)
            .map(|i| (i.name.clone(), i.columns.join(", "), i.unique))
            .collect();
        self.fks = info.foreign_keys.clone();
        self.fk_columns_text = info.foreign_keys.iter().map(|f| (f.columns.join(", "), f.ref_columns.join(", "))).collect();
        self.checks = info.checks.clone();
        self.original = Some(info);
    }

    /// Table as currently edited (with the *new* name).
    fn build(&self) -> TableInfo {
        let mut t = self.original.clone().unwrap_or_else(|| TableInfo::new(ObjectRef::table(self.schema.as_deref(), self.name.clone())));
        t.object = ObjectRef::table(self.schema.as_deref(), self.name.trim().to_string());
        t.columns = self.columns.iter().enumerate().map(|(i, r)| ColumnInfo { ordinal: i as u32 + 1, ..r.col.clone() }).collect();
        let pk: Vec<String> = self.columns.iter().filter(|r| r.pk).map(|r| r.col.name.clone()).collect();
        t.primary_key = if pk.is_empty() {
            None
        } else {
            Some(KeyInfo { name: self.original.as_ref().and_then(|o| o.primary_key.as_ref()).and_then(|k| k.name.clone()), columns: pk })
        };
        let split = |s: &str| s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect::<Vec<_>>();
        // Keep native definitions of untouched indexes so the diff doesn't see false changes.
        t.indexes = self
            .indexes
            .iter()
            .filter(|(n, c, _)| !n.trim().is_empty() && !c.trim().is_empty())
            .map(|(name, cols, unique)| {
                let original = self.original.as_ref().and_then(|o| o.indexes.iter().find(|i| &i.name == name));
                match original {
                    Some(o) if o.columns == split(cols) && o.unique == *unique => o.clone(),
                    _ => IndexInfo { name: name.trim().into(), columns: split(cols), unique: *unique, ..Default::default() },
                }
            })
            .chain(self.original.iter().flat_map(|o| o.indexes.iter().filter(|i| i.primary).cloned()))
            .collect();
        t.foreign_keys = self
            .fks
            .iter()
            .zip(&self.fk_columns_text)
            .filter(|(f, _)| !f.ref_table.trim().is_empty())
            .map(|(f, (cols, refs))| ForeignKeyInfo { columns: split(cols), ref_columns: split(refs), ..f.clone() })
            .collect();
        t.checks = self.checks.iter().filter(|c| !c.expression.trim().is_empty()).cloned().collect();
        t.comment = if self.comment.trim().is_empty() { None } else { Some(self.comment.clone()) };
        t
    }

    fn statements(&self, dialect: Dialect) -> Vec<String> {
        let new = self.build();
        let Some(old) = &self.original else {
            return tabularium_sql::split::split_statements(&tabularium_tools::ddl::create_table_ddl(&new, dialect), dialect)
                .into_iter()
                .map(|s| s.text)
                .collect();
        };
        let mut out = vec![];
        let old_name = old.object.qualified(dialect);
        // Renames first, so the structural diff compares like with like.
        let mut renamed_old = old.clone();
        for r in &self.columns {
            if let Some(orig) = &r.original {
                if orig != &r.col.name && !r.col.name.trim().is_empty() {
                    out.push(match dialect {
                        Dialect::MsSql => format!(
                            "EXEC sp_rename '{}.{}', '{}', 'COLUMN'",
                            old.object.display_name().replace('\'', "''"),
                            orig.replace('\'', "''"),
                            r.col.name.replace('\'', "''")
                        ),
                        _ => format!(
                            "ALTER TABLE {old_name} RENAME COLUMN {} TO {}",
                            dialect.quote_ident_if_needed(orig),
                            dialect.quote_ident_if_needed(&r.col.name)
                        ),
                    });
                    for c in &mut renamed_old.columns {
                        if &c.name == orig {
                            c.name = r.col.name.clone();
                        }
                    }
                    let rename = |cols: &mut Vec<String>| cols.iter_mut().filter(|c| *c == orig).for_each(|c| *c = r.col.name.clone());
                    if let Some(pk) = &mut renamed_old.primary_key {
                        rename(&mut pk.columns);
                    }
                    for i in &mut renamed_old.indexes {
                        rename(&mut i.columns);
                    }
                    for u in &mut renamed_old.unique_keys {
                        rename(&mut u.columns);
                    }
                    for f in &mut renamed_old.foreign_keys {
                        rename(&mut f.columns);
                    }
                }
            }
        }
        let mut new_same_name = new.clone();
        new_same_name.object = old.object.clone();
        out.extend(tabularium_tools::ddl::alter_table_ddl(&renamed_old, &new_same_name, dialect));
        if new.object.name != old.object.name {
            out.push(match dialect {
                Dialect::MsSql => format!("EXEC sp_rename '{}', '{}'", old.object.display_name().replace('\'', "''"), new.object.name.replace('\'', "''")),
                Dialect::MySql => format!("RENAME TABLE {old_name} TO {}", new.object.qualified(dialect)),
                _ => format!("ALTER TABLE {old_name} RENAME TO {}", dialect.quote_ident_if_needed(&new.object.name)),
            });
        }
        out
    }

    fn execute(&mut self, ctx: &mut Ctx) {
        let dialect = self.dialect(ctx);
        let statements = self.statements(dialect);
        if statements.is_empty() {
            ctx.info("No changes");
            return;
        }
        let Some(src) = ctx.sources.get(self.ds) else { return };
        let conn = src.conn.clone();
        self.error = None;
        self.exec = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await.map_err(|e| (0, e))?;
            for (i, s) in statements.iter().enumerate() {
                c.execute(s, &ExecOptions::default()).await.map_err(|e| (i, e))?;
            }
            Ok(())
        }));
    }

    fn columns_ui(&mut self, ui: &mut egui::Ui, dialect: Dialect) {
        let types: &[&str] = COMMON_TYPES.iter().find(|(d, _)| *d == dialect).map(|(_, t)| *t).unwrap_or(&[]);
        let mut remove = None;
        let mut swap = None;
        egui::ScrollArea::both().max_height(ui.available_height() - 40.0).show(ui, |ui| {
            egui::Grid::new("designer_cols").striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
                for h in ["", "Name", "Type", "PK", "Not null", "Auto inc.", "Default", "Comment", ""] {
                    ui.strong(h);
                }
                ui.end_row();
                let n = self.columns.len();
                for (i, r) in self.columns.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.add_enabled(i > 0, egui::Button::new("▲").small()).clicked() {
                            swap = Some((i, i - 1));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("▼").small()).clicked() {
                            swap = Some((i, i + 1));
                        }
                    });
                    ui.add_sized([140.0, 20.0], egui::TextEdit::singleline(&mut r.col.name));
                    ui.horizontal(|ui| {
                        ui.add_sized([130.0, 20.0], egui::TextEdit::singleline(&mut r.col.data_type).font(egui::TextStyle::Monospace));
                        egui::ComboBox::from_id_salt(("type", i)).width(18.0).selected_text("").show_ui(ui, |ui| {
                            for t in types {
                                if ui.selectable_label(r.col.data_type == *t, *t).clicked() {
                                    r.col.data_type = t.to_string();
                                }
                            }
                        });
                    });
                    if ui.checkbox(&mut r.pk, "").changed() && r.pk {
                        r.col.nullable = false;
                    }
                    let mut not_null = !r.col.nullable;
                    if ui.checkbox(&mut not_null, "").changed() {
                        r.col.nullable = !not_null;
                    }
                    ui.checkbox(&mut r.col.auto_increment, "");
                    let mut def = r.col.default.clone().unwrap_or_default();
                    if ui.add_sized([120.0, 20.0], egui::TextEdit::singleline(&mut def).font(egui::TextStyle::Monospace)).changed() {
                        r.col.default = if def.trim().is_empty() { None } else { Some(def) };
                    }
                    let mut cm = r.col.comment.clone().unwrap_or_default();
                    if ui.add_sized([160.0, 20.0], egui::TextEdit::singleline(&mut cm)).changed() {
                        r.col.comment = if cm.trim().is_empty() { None } else { Some(cm) };
                    }
                    if ui.small_button("×").on_hover_text("Remove column").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
        });
        if let Some((a, b)) = swap {
            self.columns.swap(a, b);
        }
        if let Some(i) = remove {
            self.columns.remove(i);
        }
        if ui.button("+ Add column").clicked() {
            let n = self.columns.len() + 1;
            self.columns.push(ColumnRow {
                original: None,
                col: ColumnInfo { name: format!("column_{n}"), data_type: types.get(5).unwrap_or(&"text").to_string(), nullable: true, ..Default::default() },
                pk: false,
            });
        }
    }

    fn indexes_ui(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        egui::Grid::new("designer_idx").striped(true).show(ui, |ui| {
            for h in ["Name", "Columns (comma separated)", "Unique", ""] {
                ui.strong(h);
            }
            ui.end_row();
            for (i, (name, cols, unique)) in self.indexes.iter_mut().enumerate() {
                ui.add_sized([180.0, 20.0], egui::TextEdit::singleline(name));
                ui.add_sized([260.0, 20.0], egui::TextEdit::singleline(cols));
                ui.checkbox(unique, "");
                if ui.small_button("×").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.indexes.remove(i);
        }
        if ui.button("+ Add index").clicked() {
            let first = self.columns.first().map(|c| c.col.name.clone()).unwrap_or_default();
            self.indexes.push((format!("{}_{}_idx", self.name, first), first, false));
        }
    }

    fn fks_ui(&mut self, ui: &mut egui::Ui, tables: &[String]) {
        let mut remove = None;
        egui::Grid::new("designer_fk").striped(true).show(ui, |ui| {
            for h in ["Name", "Columns", "References table", "Ref. columns", "On delete", "On update", ""] {
                ui.strong(h);
            }
            ui.end_row();
            for (i, (fk, (cols, refs))) in self.fks.iter_mut().zip(self.fk_columns_text.iter_mut()).enumerate() {
                let mut name = fk.name.clone().unwrap_or_default();
                if ui.add_sized([140.0, 20.0], egui::TextEdit::singleline(&mut name)).changed() {
                    fk.name = if name.is_empty() { None } else { Some(name) };
                }
                ui.add_sized([120.0, 20.0], egui::TextEdit::singleline(cols));
                ui.horizontal(|ui| {
                    ui.add_sized([120.0, 20.0], egui::TextEdit::singleline(&mut fk.ref_table));
                    egui::ComboBox::from_id_salt(("fk_table", i)).width(18.0).selected_text("").show_ui(ui, |ui| {
                        for t in tables {
                            if ui.selectable_label(&fk.ref_table == t, t).clicked() {
                                fk.ref_table = t.clone();
                            }
                        }
                    });
                });
                ui.add_sized([120.0, 20.0], egui::TextEdit::singleline(refs));
                for (j, action) in [&mut fk.on_delete, &mut fk.on_update].into_iter().enumerate() {
                    egui::ComboBox::from_id_salt(("fk_act", i, j)).width(90.0).selected_text(action.sql()).show_ui(ui, |ui| {
                        for a in [FkAction::NoAction, FkAction::Restrict, FkAction::Cascade, FkAction::SetNull, FkAction::SetDefault] {
                            ui.selectable_value(action, a, a.sql());
                        }
                    });
                }
                if ui.small_button("×").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.fks.remove(i);
            self.fk_columns_text.remove(i);
        }
        if ui.button("+ Add foreign key").clicked() {
            self.fks.push(ForeignKeyInfo { name: Some(format!("{}_fk_{}", self.name, self.fks.len() + 1)), ..Default::default() });
            self.fk_columns_text.push((String::new(), "id".into()));
        }
    }

    fn checks_ui(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        egui::Grid::new("designer_chk").striped(true).show(ui, |ui| {
            ui.strong("Name");
            ui.strong("Expression");
            ui.strong("");
            ui.end_row();
            for (i, c) in self.checks.iter_mut().enumerate() {
                let mut name = c.name.clone().unwrap_or_default();
                if ui.add_sized([160.0, 20.0], egui::TextEdit::singleline(&mut name)).changed() {
                    c.name = if name.is_empty() { None } else { Some(name) };
                }
                ui.add_sized([360.0, 20.0], egui::TextEdit::singleline(&mut c.expression).font(egui::TextStyle::Monospace));
                if ui.small_button("×").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.checks.remove(i);
        }
        if ui.button("+ Add check").clicked() {
            self.checks.push(CheckInfo { name: None, expression: String::new() });
        }
    }
}

impl TabView for TableDesignerTab {
    fn title(&self) -> String {
        if self.original.is_some() || self.target.is_some() {
            format!("Modify {}", self.name)
        } else {
            "New table".into()
        }
    }

    fn data_source(&self) -> Option<Uuid> {
        Some(self.ds)
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        if !self.started {
            self.started = true;
            if let (Some(src), Some(obj)) = (ctx.sources.get(self.ds), self.target.clone()) {
                let conn = src.conn.clone();
                self.load = Some(Pending::spawn(ctx.rt, ctx.egui, async move { conn.get().await?.describe_table(&obj).await }));
            }
        }
        if let Some(r) = poll_slot(&mut self.load) {
            match r {
                Ok(info) => self.set_from(info),
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        if let Some(r) = poll_slot(&mut self.exec) {
            match r {
                Ok(()) => {
                    ctx.info(format!("Table {} saved", self.name));
                    ctx.act(AppAction::RefreshExplorer(self.ds));
                    ctx.act(AppAction::InvalidateCatalog(self.ds));
                    // Reload the definition so further edits diff against the new state.
                    self.target = Some(ObjectRef::table(self.schema.as_deref(), self.name.clone()));
                    self.started = false;
                }
                Err((i, e)) => self.error = Some(format!("Statement {} failed: {e}", i + 1)),
            }
        }
        let dialect = self.dialect(ctx);
        let palette = Palette::current(ui.ctx());
        let tables: Vec<String> = ctx
            .sources
            .get(self.ds)
            .map(|s| s.catalog.lock().unwrap().tables.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("Table:");
            ui.add_sized([200.0, 20.0], egui::TextEdit::singleline(&mut self.name));
            if let Some(s) = &self.schema {
                ui.weak(format!("in schema {s}"));
            }
            ui.label("Comment:");
            ui.add_sized([260.0, 20.0], egui::TextEdit::singleline(&mut self.comment));
            if self.load.is_some() {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            for (p, label) in [
                (Page::Columns, format!("Columns ({})", self.columns.len())),
                (Page::Indexes, format!("Indexes ({})", self.indexes.len())),
                (Page::ForeignKeys, format!("Foreign keys ({})", self.fks.len())),
                (Page::Checks, format!("Checks ({})", self.checks.len())),
                (Page::Options, "SQL preview".to_string()),
            ] {
                ui.selectable_value(&mut self.page, p, label);
            }
        });
        ui.separator();
        let statements = self.statements(dialect);
        egui::TopBottomPanel::bottom("designer_actions").show_inside(ui, |ui| {
            if let Some(e) = &self.error {
                ui.colored_label(palette.error, e);
            }
            ui.horizontal(|ui| {
                let busy = self.exec.is_some();
                let label = if self.original.is_some() { "Apply changes" } else { "Create table" };
                if ui.add_enabled(!busy && !statements.is_empty(), egui::Button::new(label)).clicked() {
                    self.execute(ctx);
                }
                if ui.add_enabled(!statements.is_empty(), egui::Button::new("Open in console")).clicked() {
                    ctx.act(AppAction::OpenConsole { ds: self.ds, sql: Some(statements.join(";\n") + ";\n"), run: false });
                }
                if self.original.is_some() && ui.button("Reset").clicked() {
                    if let Some(o) = self.original.clone() {
                        self.set_from(o);
                    }
                }
                ui.weak(format!("{} statement(s)", statements.len()));
                if busy {
                    ui.spinner();
                }
            });
        });
        match self.page {
            Page::Columns => self.columns_ui(ui, dialect),
            Page::Indexes => self.indexes_ui(ui),
            Page::ForeignKeys => self.fks_ui(ui, &tables),
            Page::Checks => self.checks_ui(ui),
            Page::Options => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let text = if statements.is_empty() { "-- no changes".to_string() } else { statements.join(";\n\n") + ";" };
                    let mut t = text.as_str();
                    ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
                });
            }
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing() -> TableInfo {
        let mut t = TableInfo::new(ObjectRef::table(Some("public"), "users"));
        t.columns = vec![
            ColumnInfo { name: "id".into(), data_type: "integer".into(), nullable: false, ordinal: 1, ..Default::default() },
            ColumnInfo { name: "name".into(), data_type: "text".into(), nullable: true, ordinal: 2, ..Default::default() },
        ];
        t.primary_key = Some(KeyInfo { name: Some("users_pkey".into()), columns: vec!["id".into()] });
        t
    }

    #[test]
    fn unchanged_table_has_no_statements() {
        let mut d = TableDesignerTab::new(1, Uuid::nil(), Some(ObjectRef::table(Some("public"), "users")), None);
        d.set_from(existing());
        assert!(d.statements(Dialect::Postgres).is_empty(), "{:?}", d.statements(Dialect::Postgres));
    }

    #[test]
    fn rename_column_is_not_drop_add() {
        let mut d = TableDesignerTab::new(1, Uuid::nil(), Some(ObjectRef::table(Some("public"), "users")), None);
        d.set_from(existing());
        d.columns[1].col.name = "full_name".into();
        let s = d.statements(Dialect::Postgres);
        assert_eq!(s, vec!["ALTER TABLE public.users RENAME COLUMN name TO full_name".to_string()]);
    }

    #[test]
    fn new_table_creates() {
        let d = TableDesignerTab::new(1, Uuid::nil(), None, Some("public".into()));
        let s = d.statements(Dialect::Postgres);
        assert_eq!(s.len(), 1);
        assert!(s[0].starts_with("CREATE TABLE public.new_table"), "{}", s[0]);
    }
}
