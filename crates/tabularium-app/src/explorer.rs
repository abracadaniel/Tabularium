//! Database explorer tree: data sources → schemas → object folders → objects → details.

use std::collections::{BTreeMap, HashMap};

use egui::collapsing_header::CollapsingState;
use egui::RichText;
use tabularium_core::{
    Dialect, DriverKind, Error, ObjectInfo, ObjectKind, ObjectRef, TableInfo,
};
use tabularium_sql::catalog::{CatalogColumn, CatalogTable};
use tabularium_tools::ddl;
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, DataSource};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

enum Loadable<T> {
    Loading(Pending<Result<T, Error>>),
    Loaded(T),
    Failed(String),
}

impl<T: Send + 'static> Loadable<T> {
    fn poll(&mut self) -> bool {
        if let Loadable::Loading(p) = self {
            if let Some(r) = p.poll() {
                *self = match r {
                    Ok(v) => Loadable::Loaded(v),
                    Err(e) => Loadable::Failed(e.to_string()),
                };
                return true;
            }
        }
        false
    }
}

const ALL_KINDS: &[ObjectKind] = &[
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::ForeignTable,
    ObjectKind::Sequence,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
    ObjectKind::Type,
    ObjectKind::Event,
    ObjectKind::Synonym,
];

struct SchemaNav {
    databases: Vec<String>,
    current_db: Option<String>,
    schemas: Vec<String>,
    default_schema: Option<String>,
}

#[derive(Default)]
struct DsTree {
    nav: Option<Loadable<SchemaNav>>,
    objects: HashMap<String, Loadable<Vec<ObjectInfo>>>,
    details: HashMap<ObjectRef, Loadable<TableInfo>>,
    switch_db: Option<Pending<Result<(), Error>>>,
}

pub struct Explorer {
    filter: String,
    trees: HashMap<Uuid, DsTree>,
    selected: Option<(Uuid, ObjectRef)>,
    gen_tasks: Vec<Pending<Result<(Uuid, String), Error>>>,
    connect_tasks: HashMap<Uuid, Pending<Result<(), Error>>>,
}

impl Default for Explorer {
    fn default() -> Self {
        Self::new()
    }
}

impl Explorer {
    pub fn new() -> Self {
        Self { filter: String::new(), trees: HashMap::new(), selected: None, gen_tasks: vec![], connect_tasks: HashMap::new() }
    }

    /// Drop cached nodes so they reload on next expand.
    pub fn refresh(&mut self, ds: Uuid) {
        self.trees.remove(&ds);
    }

    pub fn selected(&self) -> Option<&(Uuid, ObjectRef)> {
        self.selected.as_ref()
    }

    /// Connect a data source (explorer session) and load its top level.
    pub fn connect(&mut self, ctx: &mut Ctx, ds: Uuid) {
        let Some(source) = ctx.sources.get(ds) else { return };
        let conn = source.conn.clone();
        self.trees.remove(&ds);
        self.connect_tasks.insert(ds, Pending::spawn(ctx.rt, ctx.egui, async move { conn.get().await.map(|_| ()) }));
    }

    pub fn is_connecting(&self, ds: Uuid) -> bool {
        self.connect_tasks.contains_key(&ds)
    }

    fn poll(&mut self, ctx: &mut Ctx) {
        let mut connected = vec![];
        self.connect_tasks.retain(|id, p| match p.poll() {
            Some(r) => {
                connected.push((*id, r));
                false
            }
            None => true,
        });
        for (id, r) in connected {
            match r {
                Ok(()) => ctx.info(format!("Connected to {}", ctx.sources.name(id))),
                Err(e) => ctx.error(format!("{}: {e}", ctx.sources.name(id))),
            }
        }
        let mut finished = vec![];
        self.gen_tasks.retain_mut(|p| match p.poll() {
            Some(r) => {
                finished.push(r);
                false
            }
            None => true,
        });
        for r in finished {
            match r {
                Ok((ds, sql)) => ctx.act(AppAction::OpenConsole { ds, sql: Some(sql), run: false }),
                Err(e) => ctx.error(e),
            }
        }
        for (ds_id, tree) in self.trees.iter_mut() {
            let Some(source) = ctx.sources.get(*ds_id) else { continue };
            if let Some(nav) = &mut tree.nav {
                if nav.poll() {
                    if let Loadable::Loaded(n) = nav {
                        let mut cat = source.catalog.lock().unwrap();
                        cat.schemas = n.schemas.clone();
                        cat.default_schema = n.default_schema.clone();
                    }
                }
            }
            for (schema, objs) in tree.objects.iter_mut() {
                if objs.poll() {
                    if let Loadable::Loaded(list) = objs {
                        update_catalog(source, schema, list);
                    }
                }
            }
            for d in tree.details.values_mut() {
                d.poll();
            }
            if let Some(r) = poll_slot(&mut tree.switch_db) {
                match r {
                    Ok(()) => {
                        tree.nav = None;
                        tree.objects.clear();
                        tree.details.clear();
                    }
                    Err(e) => ctx.error(e),
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        self.poll(ctx);
        let palette = Palette::current(ui.ctx());
        ui.horizontal(|ui| {
            ui.menu_button("＋", |ui| {
                for kind in DriverKind::ALL {
                    if ui.button(kind.label()).clicked() {
                        ctx.act(AppAction::NewDataSource(kind));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("From URL…").clicked() {
                    ctx.act(AppAction::NewDataSourceFromUrl);
                    ui.close();
                }
            })
            .response
            .on_hover_text("New data source");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("🔍 Filter objects")
                    .desired_width(ui.available_width() - 4.0),
            );
        });
        ui.separator();
        let filter = self.filter.trim().to_lowercase();

        // Group data sources by folder.
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, ds) in ctx.sources.list.iter().enumerate() {
            groups.entry(ds.config.group.clone().unwrap_or_default()).or_default().push(i);
        }
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            if ctx.sources.list.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.weak("No data sources yet");
                    if ui.button("Add data source…").clicked() {
                        ctx.act(AppAction::NewDataSource(DriverKind::Postgres));
                    }
                });
            }
            for (group, idxs) in groups {
                if group.is_empty() {
                    for i in idxs {
                        self.data_source_ui(ui, ctx, i, &filter, &palette);
                    }
                } else {
                    CollapsingState::load_with_default_open(ui.ctx(), egui::Id::new(("group", &group)), true)
                        .show_header(ui, |ui| {
                            ui.label(RichText::new(format!("📁 {group}")).strong());
                        })
                        .body(|ui| {
                            for i in idxs {
                                self.data_source_ui(ui, ctx, i, &filter, &palette);
                            }
                        });
                }
            }
        });
    }

    fn data_source_ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx, idx: usize, filter: &str, palette: &Palette) {
        let source = &ctx.sources.list[idx];
        let id = source.config.id;
        let name = source.config.name.clone();
        let connected = source.conn.is_connected();
        let color = source.color();
        let driver = source.config.driver;
        let connecting = self.is_connecting(id);
        let state_id = egui::Id::new(("ds", id));
        let mut state = CollapsingState::load_with_default_open(ui.ctx(), state_id, false);
        if !filter.is_empty() && connected {
            state.set_open(true);
        }
        let header = state.show_header(ui, |ui| {
            if let Some(c) = color {
                ui.colored_label(c, "■");
            }
            let icon = driver_icon(driver);
            let mut text = RichText::new(format!("{icon} {name}"));
            if connected {
                text = text.strong();
            }
            let r = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
            if connecting {
                ui.spinner();
            } else if connected {
                ui.colored_label(palette.success, "●").on_hover_text("Connected");
            }
            let r = r.on_hover_text(format!(
                "{}\n{}{}",
                driver.label(),
                ctx.sources.get(id).map(|d| d.config.describe()).unwrap_or_default(),
                ctx.sources.get(id).and_then(|d| d.conn.server_version()).map(|v| format!("\n{v}")).unwrap_or_default()
            ));
            if r.double_clicked() {
                ctx.act(AppAction::OpenConsole { ds: id, sql: None, run: false });
            }
            r.context_menu(|ui| self.ds_menu(ui, ctx, id, connected));
        });
        header.body(|ui| {
            let tree = self.trees.entry(id).or_default();
            if tree.nav.is_none() {
                let Some(source) = ctx.sources.get(id) else { return };
                let conn = source.conn.clone();
                tree.nav = Some(Loadable::Loading(Pending::spawn(ctx.rt, ctx.egui, async move {
                    let c = conn.get().await?;
                    let databases = c.list_databases().await.unwrap_or_default();
                    let schemas = c.list_schemas().await?;
                    let default_schema = c.current_schema().await.ok().flatten();
                    Ok(SchemaNav { databases, current_db: c.current_database(), schemas, default_schema })
                })));
            }
            let nav = match tree.nav.as_ref() {
                Some(Loadable::Loaded(n)) => n,
                Some(Loadable::Failed(e)) => {
                    let e = e.clone();
                    ui.colored_label(palette.error, e);
                    if ui.small_button("Retry").clicked() {
                        tree.nav = None;
                    }
                    return;
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("connecting…");
                    });
                    return;
                }
            };
            let dialect = driver.dialect();
            let schemas = nav.schemas.clone();
            let default_schema = nav.default_schema.clone();
            let databases = nav.databases.clone();
            let current_db = nav.current_db.clone();
            // Databases (switch the explorer session's current database).
            if databases.len() > 1 && dialect != Dialect::MySql {
                CollapsingState::load_with_default_open(ui.ctx(), egui::Id::new(("dbs", id)), false)
                    .show_header(ui, |ui| {
                        ui.label(format!("🗄 databases ({})", databases.len()));
                    })
                    .body(|ui| {
                        for db in &databases {
                            let current = current_db.as_deref() == Some(db.as_str());
                            let text = if current { RichText::new(format!("{db} (current)")).strong() } else { RichText::new(db) };
                            let r = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                            r.context_menu(|ui| {
                                if ui.button("Make current").clicked() {
                                    if let Some(source) = ctx.sources.get(id) {
                                        let conn = source.conn.clone();
                                        let db = db.clone();
                                        let tree = self.trees.entry(id).or_default();
                                        tree.switch_db = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
                                            let c = conn.get().await?;
                                            c.use_database(&db).await
                                        }));
                                    }
                                    ui.close();
                                }
                            });
                            if r.double_clicked() && !current {
                                if let Some(source) = ctx.sources.get(id) {
                                    let conn = source.conn.clone();
                                    let db = db.clone();
                                    let tree = self.trees.entry(id).or_default();
                                    tree.switch_db = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
                                        let c = conn.get().await?;
                                        c.use_database(&db).await
                                    }));
                                }
                            }
                        }
                    });
            }
            if schemas.is_empty() {
                self.schema_ui(ui, ctx, id, None, true, filter, palette);
            } else {
                for s in &schemas {
                    let is_default = default_schema.as_deref() == Some(s.as_str());
                    self.schema_ui(ui, ctx, id, Some(s.clone()), is_default, filter, palette);
                }
            }
        });
    }

    fn ds_menu(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx, id: Uuid, connected: bool) {
        if connected {
            if ui.button("Disconnect").clicked() {
                ctx.act(AppAction::Disconnect(id));
                self.trees.remove(&id);
                ui.close();
            }
        } else if ui.button("Connect").clicked() {
            self.connect(ctx, id);
            ui.close();
        }
        if ui.button("New query console (Ctrl+Shift+Q)").clicked() {
            ctx.act(AppAction::OpenConsole { ds: id, sql: None, run: false });
            ui.close();
        }
        if ui.button("⟳ Refresh").clicked() {
            self.trees.remove(&id);
            ctx.act(AppAction::InvalidateCatalog(id));
            ui.close();
        }
        ui.separator();
        if ui.button("Properties…").clicked() {
            ctx.act(AppAction::EditDataSource(id));
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            ctx.act(AppAction::DuplicateDataSource(id));
            ui.close();
        }
        if ui.button("Copy connection URL").clicked() {
            if let Some(s) = ctx.sources.get(id) {
                ctx.act(AppAction::CopyToClipboard(s.config.to_url(false)));
            }
            ui.close();
        }
        ui.separator();
        if ui.button("Sessions / process list").clicked() {
            ctx.act(AppAction::OpenSessions(id));
            ui.close();
        }
        if ui.button("Users & roles").clicked() {
            ctx.act(AppAction::OpenRoles(id));
            ui.close();
        }
        if ui.button("Compare schemas…").clicked() {
            ctx.act(AppAction::OpenSchemaCompare { ds: Some(id), schema: None });
            ui.close();
        }
        if ui.button("Full-text search in data…").clicked() {
            ctx.act(AppAction::OpenFindInDb { ds: id, schema: None });
            ui.close();
        }
        if ui.button("Dump database…").clicked() {
            ctx.act(AppAction::Dump { ds: id, restore: false });
            ui.close();
        }
        if ui.button("Restore from dump…").clicked() {
            ctx.act(AppAction::Dump { ds: id, restore: true });
            ui.close();
        }
        ui.separator();
        if ui.button("Delete data source").clicked() {
            let name = ctx.sources.name(id);
            ctx.act(AppAction::Confirm {
                title: "Delete data source".into(),
                message: format!("Delete data source \"{name}\"? This doesn't affect the database."),
                then: Box::new(AppAction::DeleteDataSource(id)),
            });
            ui.close();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn schema_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut Ctx,
        ds: Uuid,
        schema: Option<String>,
        is_default: bool,
        filter: &str,
        palette: &Palette,
    ) {
        let key = schema.clone().unwrap_or_default();
        let label = schema.clone().unwrap_or_else(|| "objects".into());
        let mut state = CollapsingState::load_with_default_open(ui.ctx(), egui::Id::new(("schema", ds, &key)), is_default);
        let tree = self.trees.entry(ds).or_default();
        if !filter.is_empty() && matches!(tree.objects.get(&key), Some(Loadable::Loaded(_))) {
            state.set_open(true);
        }
        // Default schema objects load eagerly so completion knows table names.
        if is_default && !tree.objects.contains_key(&key) {
            self.load_objects(ctx, ds, schema.clone());
        }
        let header = state.show_header(ui, |ui| {
            let text = if is_default { RichText::new(format!("◆ {label}")).strong() } else { RichText::new(format!("◇ {label}")) };
            let r = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
            r.context_menu(|ui| {
                if ui.button("New query console").clicked() {
                    let dialect = ctx.sources.get(ds).map(|d| d.config.dialect()).unwrap_or_default();
                    let sql = match (&schema, dialect) {
                        (Some(s), Dialect::Postgres) => Some(format!("SET search_path TO {};\n", dialect.quote_ident_if_needed(s))),
                        (Some(s), Dialect::MySql) => Some(format!("USE {};\n", dialect.quote_ident(s))),
                        _ => None,
                    };
                    ctx.act(AppAction::OpenConsole { ds, sql, run: false });
                    ui.close();
                }
                if ui.button("⟳ Refresh").clicked() {
                    self.trees.entry(ds).or_default().objects.remove(&key);
                    ui.close();
                }
                ui.separator();
                if ui.button("New table…").clicked() {
                    ctx.act(AppAction::OpenModifyTable { ds, object: None, schema: schema.clone() });
                    ui.close();
                }
                if ui.button("Import data from file…").clicked() {
                    ctx.act(AppAction::Import { ds, schema: schema.clone(), table: None });
                    ui.close();
                }
                ui.separator();
                if ui.button("ER diagram").clicked() {
                    ctx.act(AppAction::OpenErDiagram { ds, schema: schema.clone() });
                    ui.close();
                }
                if ui.button("Compare with…").clicked() {
                    ctx.act(AppAction::OpenSchemaCompare { ds: Some(ds), schema: schema.clone() });
                    ui.close();
                }
                if ui.button("Full-text search in data…").clicked() {
                    ctx.act(AppAction::OpenFindInDb { ds, schema: schema.clone() });
                    ui.close();
                }
                if ui.button("Generate DDL for schema").clicked() {
                    self.generate_schema_ddl(ctx, ds, schema.clone());
                    ui.close();
                }
                if ui.button("Copy name").clicked() {
                    ctx.act(AppAction::CopyToClipboard(label.clone()));
                    ui.close();
                }
            });
        });
        header.body(|ui| {
            let tree = self.trees.entry(ds).or_default();
            if !tree.objects.contains_key(&key) {
                self.load_objects(ctx, ds, schema.clone());
            }
            let tree = self.trees.entry(ds).or_default();
            let objects = match tree.objects.get(&key) {
                Some(Loadable::Loaded(o)) => o.clone(),
                Some(Loadable::Failed(e)) => {
                    let e = e.clone();
                    ui.colored_label(palette.error, e);
                    if ui.small_button("Retry").clicked() {
                        tree.objects.remove(&key);
                    }
                    return;
                }
                _ => {
                    ui.spinner();
                    return;
                }
            };
            let mut by_kind: BTreeMap<ObjectKind, Vec<ObjectInfo>> = BTreeMap::new();
            for o in objects {
                if filter.is_empty() || o.object.name.to_lowercase().contains(filter) {
                    by_kind.entry(o.object.kind).or_default().push(o);
                }
            }
            if by_kind.is_empty() {
                ui.weak(if filter.is_empty() { "empty" } else { "no matches" });
            }
            for (kind, mut list) in by_kind {
                list.sort_by(|a, b| a.object.name.to_lowercase().cmp(&b.object.name.to_lowercase()));
                let mut st = CollapsingState::load_with_default_open(
                    ui.ctx(),
                    egui::Id::new(("folder", ds, &key, kind)),
                    kind == ObjectKind::Table,
                );
                if !filter.is_empty() {
                    st.set_open(true);
                }
                st.show_header(ui, |ui| {
                    ui.label(RichText::new(format!("{} {} ({})", kind_icon(kind), kind.folder_label(), list.len())).weak());
                })
                .body(|ui| {
                    for o in &list {
                        self.object_ui(ui, ctx, ds, o, palette);
                    }
                });
            }
        });
    }

    fn load_objects(&mut self, ctx: &mut Ctx, ds: Uuid, schema: Option<String>) {
        let Some(source) = ctx.sources.get(ds) else { return };
        let conn = source.conn.clone();
        let key = schema.clone().unwrap_or_default();
        let tree = self.trees.entry(ds).or_default();
        tree.objects.insert(
            key,
            Loadable::Loading(Pending::spawn(ctx.rt, ctx.egui, async move {
                let c = conn.get().await?;
                c.list_objects(schema.as_deref(), ALL_KINDS).await
            })),
        );
    }

    fn object_ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx, ds: Uuid, info: &ObjectInfo, palette: &Palette) {
        let object = info.object.clone();
        let selected = self.selected.as_ref().is_some_and(|(d, o)| *d == ds && *o == object);
        let label = match (&info.signature, &info.parent) {
            (Some(sig), _) if matches!(object.kind, ObjectKind::Function | ObjectKind::Procedure) => format!("{}{sig}", object.name),
            _ => object.name.clone(),
        };
        let hover = {
            let mut h = format!("{} {}", object.kind.label(), object.display_name());
            if let Some(p) = &info.parent {
                h.push_str(&format!("\non {p}"));
            }
            if let Some(n) = info.row_estimate {
                h.push_str(&format!("\n≈ {n} rows"));
            }
            if let Some(c) = &info.comment {
                h.push_str(&format!("\n{c}"));
            }
            h
        };
        let header_ui = |ui: &mut egui::Ui, this: &mut Self, ctx: &mut Ctx| {
            let r = ui.selectable_label(selected, RichText::new(format!("{} {label}", kind_icon(object.kind))));
            if let Some(c) = &info.comment {
                ui.weak(truncate(c, 40));
            }
            let r = r.on_hover_text(hover.clone());
            if r.clicked() {
                this.selected = Some((ds, object.clone()));
            }
            if r.double_clicked() {
                if object.kind.has_data() {
                    ctx.act(AppAction::OpenTableData { ds, object: object.clone(), filter: None });
                } else {
                    ctx.act(AppAction::OpenDdl { ds, object: object.clone(), signature: info.signature.clone() });
                }
            }
            r.context_menu(|ui| this.object_menu(ui, ctx, ds, info));
        };
        if !object.kind.has_columns() {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                header_ui(ui, self, ctx);
            });
            return;
        }
        let st = CollapsingState::load_with_default_open(ui.ctx(), egui::Id::new(("obj", ds, &object)), false);
        st.show_header(ui, |ui| header_ui(ui, self, ctx)).body(|ui| {
            let tree = self.trees.entry(ds).or_default();
            if !tree.details.contains_key(&object) {
                if let Some(source) = ctx.sources.get(ds) {
                    let conn = source.conn.clone();
                    let o = object.clone();
                    tree.details.insert(
                        object.clone(),
                        Loadable::Loading(Pending::spawn(ctx.rt, ctx.egui, async move {
                            let c = conn.get().await?;
                            c.describe_table(&o).await
                        })),
                    );
                }
            }
            match tree.details.get(&object) {
                Some(Loadable::Loaded(t)) => table_details_ui(ui, ds, t, palette),
                Some(Loadable::Failed(e)) => {
                    ui.colored_label(palette.error, e.clone());
                }
                _ => {
                    ui.spinner();
                }
            }
        });
    }

    fn object_menu(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx, ds: Uuid, info: &ObjectInfo) {
        let object = &info.object;
        let dialect = ctx.sources.get(ds).map(|d| d.config.dialect()).unwrap_or_default();
        if object.kind.has_data() && ui.button("Open data (F4)").clicked() {
            ctx.act(AppAction::OpenTableData { ds, object: object.clone(), filter: None });
            ui.close();
        }
        if ui.button("Show DDL").clicked() {
            ctx.act(AppAction::OpenDdl { ds, object: object.clone(), signature: info.signature.clone() });
            ui.close();
        }
        if ui.button("New console here").clicked() {
            let sql = if object.kind.has_data() {
                Some(format!("SELECT * FROM {};\n", object.qualified(dialect)))
            } else {
                None
            };
            ctx.act(AppAction::OpenConsole { ds, sql, run: false });
            ui.close();
        }
        ui.menu_button("Copy", |ui| {
            if ui.button("Name").clicked() {
                ctx.act(AppAction::CopyToClipboard(object.name.clone()));
                ui.close();
            }
            if ui.button("Qualified name").clicked() {
                ctx.act(AppAction::CopyToClipboard(object.qualified(dialect)));
                ui.close();
            }
        });
        if object.kind.has_data() {
            ui.menu_button("Generate SQL", |ui| {
                for (label, which) in [
                    ("SELECT", GenKind::Select),
                    ("INSERT", GenKind::Insert),
                    ("UPDATE", GenKind::Update),
                    ("DELETE", GenKind::Delete),
                    ("CREATE (DDL)", GenKind::Ddl),
                ] {
                    if ui.button(label).clicked() {
                        self.generate(ctx, ds, object.clone(), which);
                        ui.close();
                    }
                }
            });
        }
        if object.kind == ObjectKind::Table {
            ui.separator();
            if ui.button("Modify table…").clicked() {
                ctx.act(AppAction::OpenModifyTable { ds, object: Some(object.clone()), schema: object.schema.clone() });
                ui.close();
            }
            if ui.button("Import data…").clicked() {
                ctx.act(AppAction::Import { ds, schema: object.schema.clone(), table: Some(object.clone()) });
                ui.close();
            }
            if ui.button("Generate test data…").clicked() {
                ctx.act(AppAction::OpenDataGenerator { ds, object: object.clone() });
                ui.close();
            }
            if ui.button("Copy table to…").clicked() {
                ctx.act(AppAction::CopyTable { ds, object: object.clone() });
                ui.close();
            }
        }
        if object.kind.has_data() && ui.button("Export data…").clicked() {
            ctx.act(AppAction::Export(crate::ctx::ExportSource::Table { ds, object: object.clone() }));
            ui.close();
        }
        if object.kind == ObjectKind::MaterializedView && ui.button("Refresh materialized view").clicked() {
            ctx.act(AppAction::RunStatements {
                ds,
                statements: vec![format!("REFRESH MATERIALIZED VIEW {}", object.qualified(dialect))],
                refresh: false,
            });
            ui.close();
        }
        ui.separator();
        if object.kind == ObjectKind::Table && ui.button("Truncate…").clicked() {
            ctx.act(AppAction::Confirm {
                title: "Truncate table".into(),
                message: format!("Delete ALL rows from {}?", object.display_name()),
                then: Box::new(AppAction::RunStatements { ds, statements: vec![ddl::truncate_ddl(object, dialect)], refresh: false }),
            });
            ui.close();
        }
        if ui.button("Drop…").clicked() {
            let stmt = ddl::drop_ddl(object, dialect, info.signature.as_deref(), false);
            ctx.act(AppAction::Confirm {
                title: format!("Drop {}", object.kind.label()),
                message: format!("Execute:\n\n{stmt}"),
                then: Box::new(AppAction::RunStatements { ds, statements: vec![stmt], refresh: true }),
            });
            ui.close();
        }
    }

    fn generate(&mut self, ctx: &mut Ctx, ds: Uuid, object: ObjectRef, which: GenKind) {
        let Some(source) = ctx.sources.get(ds) else { return };
        let conn = source.conn.clone();
        let dialect = source.config.dialect();
        self.gen_tasks.push(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await?;
            let t = c.describe_table(&object).await?;
            let name = object.qualified(dialect);
            let cols: Vec<String> = t.columns.iter().map(|c| dialect.quote_ident_if_needed(&c.name)).collect();
            let key: Vec<String> = t.row_identity().unwrap_or_else(|| t.columns.iter().take(1).map(|c| c.name.clone()).collect());
            let where_clause = key.iter().map(|k| format!("{} = ?", dialect.quote_ident_if_needed(k))).collect::<Vec<_>>().join(" AND ");
            let sql = match which {
                GenKind::Select => format!("SELECT {}\nFROM {name};\n", cols.join(",\n       ")),
                GenKind::Insert => {
                    let insertable: Vec<&tabularium_core::ColumnInfo> =
                        t.columns.iter().filter(|c| !c.auto_increment && c.generated.is_none()).collect();
                    format!(
                        "INSERT INTO {name} ({})\nVALUES ({});\n",
                        insertable.iter().map(|c| dialect.quote_ident_if_needed(&c.name)).collect::<Vec<_>>().join(", "),
                        insertable.iter().map(|c| format!(":{}", c.name)).collect::<Vec<_>>().join(", ")
                    )
                }
                GenKind::Update => format!(
                    "UPDATE {name}\nSET {}\nWHERE {where_clause};\n",
                    t.columns
                        .iter()
                        .filter(|c| !key.contains(&c.name) && c.generated.is_none())
                        .map(|c| format!("{} = :{}", dialect.quote_ident_if_needed(&c.name), c.name))
                        .collect::<Vec<_>>()
                        .join(",\n    ")
                ),
                GenKind::Delete => format!("DELETE FROM {name}\nWHERE {where_clause};\n"),
                GenKind::Ddl => ddl::create_table_ddl(&t, dialect),
            };
            Ok((ds, sql))
        }));
    }

    fn generate_schema_ddl(&mut self, ctx: &mut Ctx, ds: Uuid, schema: Option<String>) {
        let Some(source) = ctx.sources.get(ds) else { return };
        let conn = source.conn.clone();
        let dialect = source.config.dialect();
        self.gen_tasks.push(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await?;
            let snap = c.snapshot_schema(schema.as_deref()).await?;
            Ok((ds, ddl::snapshot_ddl(&snap, dialect)))
        }));
    }
}

#[derive(Clone, Copy)]
enum GenKind {
    Select,
    Insert,
    Update,
    Delete,
    Ddl,
}

fn table_details_ui(ui: &mut egui::Ui, ds: Uuid, t: &TableInfo, palette: &Palette) {
    let pk: Vec<&String> = t.primary_key.iter().flat_map(|k| k.columns.iter()).collect();
    let fk_cols: Vec<&String> = t.foreign_keys.iter().flat_map(|f| f.columns.iter()).collect();
    CollapsingState::load_with_default_open(ui.ctx(), egui::Id::new(("cols", ds, &t.object)), true)
        .show_header(ui, |ui| {
            ui.weak(format!("columns ({})", t.columns.len()));
        })
        .body(|ui| {
            for c in &t.columns {
                ui.horizontal(|ui| {
                    let icon = if pk.contains(&&c.name) {
                        "🔑"
                    } else if fk_cols.contains(&&c.name) {
                        "🔗"
                    } else {
                        "▫"
                    };
                    ui.label(icon);
                    let r = ui.label(&c.name);
                    let mut ty = c.data_type.clone();
                    if !c.nullable {
                        ty.push_str(" not null");
                    }
                    ui.label(RichText::new(ty).color(palette.comment));
                    let mut tip = format!("{} {}", c.name, c.data_type);
                    if let Some(d) = &c.default {
                        tip.push_str(&format!("\ndefault {d}"));
                    }
                    if c.auto_increment {
                        tip.push_str("\nauto increment");
                    }
                    if let Some(g) = &c.generated {
                        tip.push_str(&format!("\ngenerated as {g}"));
                    }
                    if let Some(cm) = &c.comment {
                        tip.push_str(&format!("\n{cm}"));
                    }
                    r.on_hover_text(tip);
                });
            }
        });
    let mut keys: Vec<String> = vec![];
    if let Some(pk) = &t.primary_key {
        keys.push(format!("🔑 {} ({})", pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into()), pk.columns.join(", ")));
    }
    for u in &t.unique_keys {
        keys.push(format!("◈ {} ({})", u.name.clone().unwrap_or_else(|| "UNIQUE".into()), u.columns.join(", ")));
    }
    simple_folder(ui, ("keys", ds, &t.object), "keys", &keys);
    let fks: Vec<String> = t
        .foreign_keys
        .iter()
        .map(|f| {
            format!(
                "🔗 {} ({}) → {}{}({})",
                f.name.clone().unwrap_or_default(),
                f.columns.join(", "),
                f.ref_schema.as_ref().map(|s| format!("{s}.")).unwrap_or_default(),
                f.ref_table,
                f.ref_columns.join(", ")
            )
        })
        .collect();
    simple_folder(ui, ("fks", ds, &t.object), "foreign keys", &fks);
    let idx: Vec<String> = t
        .indexes
        .iter()
        .map(|i| format!("{} {} ({}){}", if i.unique { "◈" } else { "▸" }, i.name, i.columns.join(", "), i.method.as_ref().map(|m| format!(" {m}")).unwrap_or_default()))
        .collect();
    simple_folder(ui, ("idx", ds, &t.object), "indexes", &idx);
    let checks: Vec<String> = t.checks.iter().map(|c| format!("✓ {} {}", c.name.clone().unwrap_or_default(), c.expression)).collect();
    simple_folder(ui, ("checks", ds, &t.object), "checks", &checks);
    let trg: Vec<String> = t.triggers.iter().map(|tr| format!("⚡ {} {} {}", tr.name, tr.timing, tr.events)).collect();
    simple_folder(ui, ("trg", ds, &t.object), "triggers", &trg);
}

fn simple_folder(ui: &mut egui::Ui, id: impl std::hash::Hash, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    CollapsingState::load_with_default_open(ui.ctx(), egui::Id::new(id), false)
        .show_header(ui, |ui| {
            ui.weak(format!("{label} ({})", items.len()));
        })
        .body(|ui| {
            for i in items {
                ui.label(i);
            }
        });
}

fn update_catalog(source: &DataSource, schema: &str, list: &[ObjectInfo]) {
    let mut cat = source.catalog.lock().unwrap();
    let schema_opt = if schema.is_empty() { None } else { Some(schema.to_string()) };
    cat.tables.retain(|t| t.schema != schema_opt);
    for o in list {
        if o.object.kind.has_columns() {
            cat.tables.push(CatalogTable {
                schema: o.object.schema.clone(),
                name: o.object.name.clone(),
                kind: o.object.kind,
                columns: vec![],
            });
        } else if matches!(o.object.kind, ObjectKind::Function | ObjectKind::Procedure) && !cat.functions.contains(&o.object.name) {
            cat.functions.push(o.object.name.clone());
        }
    }
}

/// Reload names of all tables in all schemas into the catalog (for completion).
pub async fn load_catalog(source_conn: std::sync::Arc<crate::runtime::SharedConn>) -> Result<tabularium_sql::catalog::SchemaCatalog, Error> {
    let c = source_conn.get().await?;
    let mut cat = tabularium_sql::catalog::SchemaCatalog { default_schema: c.current_schema().await.ok().flatten(), ..Default::default() };
    cat.schemas = c.list_schemas().await.unwrap_or_default();
    let schemas: Vec<Option<String>> = if cat.schemas.is_empty() { vec![None] } else { cat.schemas.iter().cloned().map(Some).collect() };
    for s in schemas.iter().take(50) {
        let objs = c
            .list_objects(s.as_deref(), &[ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView, ObjectKind::Function])
            .await
            .unwrap_or_default();
        for o in objs {
            if o.object.kind == ObjectKind::Function {
                if !cat.functions.contains(&o.object.name) {
                    cat.functions.push(o.object.name);
                }
            } else {
                cat.tables.push(CatalogTable { schema: o.object.schema, name: o.object.name, kind: o.object.kind, columns: vec![] });
            }
        }
    }
    // Columns for the default schema's tables (bounded) so completion works immediately.
    let default = cat.default_schema.clone();
    let mut described = 0;
    for t in cat.tables.iter_mut().filter(|t| t.schema == default || default.is_none()) {
        if described >= 200 {
            break;
        }
        if let Ok(info) = c.describe_table(&ObjectRef::new(t.schema.as_deref(), t.name.clone(), t.kind)).await {
            t.columns = info.columns.into_iter().map(|c| CatalogColumn { name: c.name, data_type: c.data_type }).collect();
        }
        described += 1;
    }
    Ok(cat)
}

pub fn driver_icon(kind: DriverKind) -> &'static str {
    match kind {
        DriverKind::Postgres | DriverKind::CockroachDb | DriverKind::Redshift => "🐘",
        DriverKind::MySql | DriverKind::MariaDb | DriverKind::TiDb => "🐬",
        DriverKind::Sqlite => "🪶",
        DriverKind::MsSql => "🗔",
    }
}

pub fn kind_icon(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Database => "🗄",
        ObjectKind::Schema => "◆",
        ObjectKind::Table => "▦",
        ObjectKind::View => "👁",
        ObjectKind::MaterializedView => "▣",
        ObjectKind::ForeignTable => "⇄",
        ObjectKind::Sequence => "#",
        ObjectKind::Function => "ƒ",
        ObjectKind::Procedure => "⚙",
        ObjectKind::Trigger => "⚡",
        ObjectKind::Index => "▸",
        ObjectKind::Type => "T",
        ObjectKind::Event => "⏰",
        ObjectKind::Synonym => "≡",
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n).collect::<String>() + "…" } else { s.to_string() }
}
