//! Application shell: menu bar, explorer, tab strip, status bar, dialogs, notifications
//! and processing of [`AppAction`]s.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tabularium_core::config::{AppDirs, ConnectionStore};
use tabularium_core::{driver::ExecOptions, ConnectionConfig, DriverKind, Error};
use tabularium_tools::history::QueryHistory;
use uuid::Uuid;

use crate::connection_dialog::{ConnectionDialog, DialogResult};
use crate::console::ConsoleTab;
use crate::ctx::{AppAction, Ctx, DataSource, DataSources, Dialog, NoticeLevel, TabView};
use crate::explorer::Explorer;
use crate::runtime::Pending;
use crate::settings::Settings;
use crate::text_view::TextTab;
use crate::theme::{self, Palette};

struct Tab {
    id: u64,
    view: Box<dyn TabView>,
}

struct Notice {
    level: NoticeLevel,
    text: String,
    at: Instant,
}

struct ConfirmDialog {
    title: String,
    message: String,
    then: Option<AppAction>,
}

struct PasswordPrompt {
    ds: Uuid,
    password: String,
    then: Option<AppAction>,
}

struct UrlPrompt {
    url: String,
    error: Option<String>,
}

pub struct TabulariumApp {
    rt: tokio::runtime::Runtime,
    dirs: AppDirs,
    settings: Settings,
    settings_applied: Option<Settings>,
    store: ConnectionStore,
    sources: DataSources,
    history: QueryHistory,
    explorer: Explorer,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    actions: Vec<AppAction>,
    notices: Vec<Notice>,
    connection_dialog: Option<(ConnectionDialog, bool)>,
    confirm: Option<ConfirmDialog>,
    password_prompt: Option<PasswordPrompt>,
    url_prompt: Option<UrlPrompt>,
    dialogs: Vec<Box<dyn Dialog>>,
    settings_dialog: Option<crate::settings_dialog::SettingsDialog>,
    goto: Option<crate::goto::GotoDialog>,
    background: Vec<Pending<Box<dyn FnOnce(&mut TabulariumApp) + Send>>>,
    show_explorer: bool,
    show_about: bool,
    exiting: bool,
}

impl TabulariumApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime) -> Self {
        let dirs = AppDirs::discover();
        let settings = Settings::load(&dirs.settings_file());
        let store = match ConnectionStore::load(&dirs.connections_file()) {
            Ok(s) => s,
            Err(e) => {
                log::error!("loading connections: {e}");
                ConnectionStore::default()
            }
        };
        let history = QueryHistory::open(dirs.history_file()).unwrap_or_else(|e| {
            log::error!("opening history: {e}");
            QueryHistory::in_memory()
        });
        let mut sources = DataSources::default();
        for cfg in &store.connections {
            sources.list.push(DataSource::new(cfg.clone()));
        }
        setup_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, &settings);
        let mut app = Self {
            rt,
            dirs,
            settings_applied: Some(settings.clone()),
            settings,
            store,
            sources,
            history,
            explorer: Explorer::new(),
            tabs: vec![],
            active: 0,
            next_id: 1,
            actions: vec![],
            notices: vec![],
            connection_dialog: None,
            confirm: None,
            password_prompt: None,
            url_prompt: None,
            dialogs: vec![],
            settings_dialog: None,
            goto: None,
            background: vec![],
            show_explorer: true,
            show_about: false,
            exiting: false,
        };
        if app.settings.restore_consoles {
            app.restore_consoles();
        }
        // Files passed on the command line.
        for arg in std::env::args().skip(1) {
            let p = PathBuf::from(&arg);
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) {
                app.actions.push(AppAction::OpenFile(p));
            } else if ["db", "sqlite", "sqlite3", "db3"].iter().any(|e| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(e))) {
                let cfg = ConnectionConfig::sqlite(p.to_string_lossy());
                let id = cfg.id;
                app.add_data_source(cfg);
                app.actions.push(AppAction::OpenConsole { ds: id, sql: None, run: false });
            }
        }
        app
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn notify(&mut self, level: NoticeLevel, text: impl Into<String>) {
        let text = text.into();
        match level {
            NoticeLevel::Error => log::error!("{text}"),
            NoticeLevel::Warning => log::warn!("{text}"),
            _ => log::info!("{text}"),
        }
        self.notices.push(Notice { level, text, at: Instant::now() });
        if self.notices.len() > 6 {
            self.notices.remove(0);
        }
    }

    fn save_store(&mut self) {
        self.store.connections = self.sources.list.iter().map(|d| d.config.clone()).collect();
        if let Err(e) = self.store.save(&self.dirs.connections_file()) {
            self.notify(NoticeLevel::Error, format!("Saving data sources: {e}"));
        }
    }

    fn add_data_source(&mut self, cfg: ConnectionConfig) {
        self.sources.list.push(DataSource::new(cfg));
        self.save_store();
    }

    /// Start a background task whose result mutates the app.
    fn spawn_app<F>(&mut self, ctx: &egui::Context, fut: F)
    where
        F: std::future::Future<Output = Box<dyn FnOnce(&mut TabulariumApp) + Send>> + Send + 'static,
    {
        self.background.push(Pending::spawn(self.rt.handle(), ctx, fut));
    }

    fn needs_password(&self, ds: Uuid) -> bool {
        let Some(s) = self.sources.get(ds) else { return false };
        !s.config.driver.is_file_based()
            && !s.config.save_password
            && s.config.password.as_deref().unwrap_or("").is_empty()
            && !s.conn.has_session_password()
            && !s.conn.is_connected()
    }

    // ----- actions ----------------------------------------------------------------------------------

    fn process_actions(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut self.actions);
        for action in actions {
            self.handle_action(ctx, action);
        }
    }

    fn handle_action(&mut self, ctx: &egui::Context, action: AppAction) {
        // Actions that open a session first ask for a password when it isn't saved.
        let ds_needing_session = match &action {
            AppAction::OpenConsole { ds, .. }
            | AppAction::OpenTableData { ds, .. }
            | AppAction::OpenDdl { ds, .. }
            | AppAction::Connect(ds)
            | AppAction::OpenErDiagram { ds, .. }
            | AppAction::OpenSessions(ds)
            | AppAction::OpenRoles(ds) => Some(*ds),
            _ => None,
        };
        if let Some(ds) = ds_needing_session {
            if self.needs_password(ds) {
                self.password_prompt = Some(PasswordPrompt { ds, password: String::new(), then: Some(action) });
                return;
            }
        }
        match action {
            AppAction::OpenConsole { ds, sql, run } => {
                let Some(source) = self.sources.get(ds) else { return };
                let cfg = source.config.clone();
                let name = cfg.name.clone();
                let password = source.conn.effective_config().password;
                let ordinal = self
                    .tabs
                    .iter_mut()
                    .filter_map(|t| t.view.as_any_mut().downcast_mut::<ConsoleTab>())
                    .filter(|c| c.ds == ds)
                    .count()
                    + 1;
                // Reuse an empty console of this data source when just opening one.
                if sql.is_none() && !run {
                    if let Some(idx) = self.tabs.iter_mut().position(|t| {
                        t.view.as_any_mut().downcast_mut::<ConsoleTab>().is_some_and(|c| c.ds == ds && c.file.is_none())
                    }) {
                        self.active = idx;
                        return;
                    }
                }
                let id = self.next_id();
                let mut tab = ConsoleTab::new(id, ds, &name, cfg, sql.unwrap_or_default(), ordinal);
                if let Some(pw) = password {
                    tab.session().set_session_password(Some(pw));
                }
                tab.editor.request_focus = true;
                self.tabs.push(Tab { id, view: Box::new(tab) });
                self.active = self.tabs.len() - 1;
                if run {
                    ctx.input_mut(|i| {
                        i.events.push(egui::Event::Key {
                            key: egui::Key::Enter,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                        })
                    });
                }
                self.ensure_catalog(ctx, ds);
            }
            AppAction::OpenTableData { ds, object, filter } => {
                // Focus an existing tab for the same table.
                if filter.is_none() {
                    if let Some(idx) = self.tabs.iter_mut().position(|t| {
                        t.view
                            .as_any_mut()
                            .downcast_mut::<crate::table_data::TableDataTab>()
                            .is_some_and(|tt| tt.ds == ds && tt.object == object)
                    }) {
                        self.active = idx;
                        return;
                    }
                }
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(crate::table_data::TableDataTab::new(id, ds, object, filter)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenDdl { ds, object, signature } => {
                let dialect = self.sources.get(ds).map(|d| d.config.dialect()).unwrap_or_default();
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(TextTab::ddl(id, ds, object, signature, dialect)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenText { title, text, ds } => {
                let dialect = ds.and_then(|d| self.sources.get(d)).map(|d| d.config.dialect()).unwrap_or_default();
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(TextTab::text(id, title, text, ds, dialect)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenFile(path) => self.open_file(path),
            AppAction::NewDataSource(kind) => {
                self.connection_dialog = Some((ConnectionDialog::new(ConnectionConfig::new(kind), true), true));
            }
            AppAction::NewDataSourceFromUrl => self.url_prompt = Some(UrlPrompt { url: String::new(), error: None }),
            AppAction::EditDataSource(id) => {
                if let Some(s) = self.sources.get(id) {
                    self.connection_dialog = Some((ConnectionDialog::new(s.config.clone(), false), false));
                }
            }
            AppAction::DuplicateDataSource(id) => {
                if let Some(s) = self.sources.get(id) {
                    let mut cfg = s.config.clone();
                    cfg.id = Uuid::new_v4();
                    cfg.name = format!("{} (copy)", cfg.name);
                    self.add_data_source(cfg);
                }
            }
            AppAction::DeleteDataSource(id) => {
                if let Some(idx) = self.sources.list.iter().position(|d| d.config.id == id) {
                    let ds = self.sources.list.remove(idx);
                    let conn = ds.conn.clone();
                    self.rt.spawn(async move { conn.disconnect().await });
                    self.explorer.refresh(id);
                    self.save_store();
                }
            }
            AppAction::Connect(id) => {
                let mut c = Ctx {
                    rt: self.rt.handle(),
                    egui: ctx,
                    actions: &mut self.actions,
                    sources: &self.sources,
                    settings: &self.settings,
                    dirs: &self.dirs,
                    history: &self.history,
                };
                self.explorer.connect(&mut c, id);
                self.ensure_catalog(ctx, id);
            }
            AppAction::Disconnect(id) => {
                if let Some(s) = self.sources.get(id) {
                    let conn = s.conn.clone();
                    self.rt.spawn(async move { conn.disconnect().await });
                    s.catalog.lock().unwrap().tables.clear();
                }
                self.explorer.refresh(id);
            }
            AppAction::RefreshExplorer(id) => self.explorer.refresh(id),
            AppAction::InvalidateCatalog(id) => {
                if let Some(s) = self.sources.get(id) {
                    *s.catalog.lock().unwrap() = Default::default();
                }
                self.ensure_catalog(ctx, id);
            }
            AppAction::OpenSchemaCompare { ds, schema } => {
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(crate::schema_compare::SchemaCompareTab::new(id, ds, schema)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenErDiagram { ds, schema } => {
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(crate::er_diagram::ErDiagramTab::new(id, ds, schema)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenSessions(ds) => {
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(crate::admin_views::SessionsTab::new(id, ds)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenRoles(ds) => {
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(crate::admin_views::RolesTab::new(id, ds)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenHistory => {
                if let Some(idx) = self
                    .tabs
                    .iter_mut()
                    .position(|t| t.view.as_any_mut().downcast_mut::<crate::history_view::HistoryTab>().is_some())
                {
                    self.active = idx;
                    return;
                }
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(crate::history_view::HistoryTab::new(id)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenFindInDb { ds, schema } => {
                let id = self.next_id();
                self.tabs.push(Tab { id, view: Box::new(crate::find_in_db::FindInDbTab::new(id, ds, schema)) });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenModifyTable { ds, object, schema } => {
                let id = self.next_id();
                self.tabs.push(Tab {
                    id,
                    view: Box::new(crate::table_designer::TableDesignerTab::new(id, ds, object, schema)),
                });
                self.active = self.tabs.len() - 1;
            }
            AppAction::OpenDataGenerator { ds, object } => {
                self.dialogs.push(Box::new(crate::datagen_dialog::DataGenDialog::new(ds, object)));
            }
            AppAction::Export(source) => self.dialogs.push(Box::new(crate::export_dialog::ExportDialog::new(source))),
            AppAction::Import { ds, schema, table } => {
                self.dialogs.push(Box::new(crate::import_dialog::ImportDialog::new(ds, schema, table)));
            }
            AppAction::CopyTable { ds, object } => {
                self.dialogs.push(Box::new(crate::copy_table::CopyTableDialog::new(ds, object)));
            }
            AppAction::RecordHistory(entry) => {
                if let Err(e) = self.history.add(entry) {
                    log::warn!("recording history: {e}");
                }
            }
            AppAction::ClearHistory => {
                if let Err(e) = self.history.clear() {
                    self.notify(NoticeLevel::Error, format!("Clearing history: {e}"));
                }
            }
            AppAction::Notify(level, text) => self.notify(level, text),
            AppAction::Confirm { title, message, then } => {
                self.confirm = Some(ConfirmDialog { title, message, then: Some(*then) });
            }
            AppAction::RunStatements { ds, statements, refresh } => {
                let Some(s) = self.sources.get(ds) else { return };
                let conn = s.conn.clone();
                self.spawn_app(ctx, async move {
                    let result: Result<usize, Error> = async {
                        let c = conn.get().await?;
                        for st in &statements {
                            c.execute(st, &ExecOptions::default()).await?;
                        }
                        Ok(statements.len())
                    }
                    .await;
                    Box::new(move |app: &mut TabulariumApp| match result {
                        Ok(n) => {
                            app.notify(NoticeLevel::Success, format!("Executed {n} statement(s)"));
                            if refresh {
                                app.explorer.refresh(ds);
                                app.actions.push(AppAction::InvalidateCatalog(ds));
                            }
                        }
                        Err(e) => app.notify(NoticeLevel::Error, e.to_string()),
                    }) as Box<dyn FnOnce(&mut TabulariumApp) + Send>
                });
            }
            AppAction::OpenSettings => self.settings_dialog = Some(crate::settings_dialog::SettingsDialog::new(&self.settings)),
            AppAction::CopyToClipboard(text) => {
                ctx.copy_text(text);
                self.notify(NoticeLevel::Info, "Copied to clipboard");
            }
        }
    }

    /// Load object names for completion once per data source.
    fn ensure_catalog(&mut self, ctx: &egui::Context, ds: Uuid) {
        let Some(s) = self.sources.get(ds) else { return };
        {
            let cat = s.catalog.lock().unwrap();
            if !cat.tables.is_empty() || !cat.schemas.is_empty() {
                return;
            }
        }
        let conn = s.conn.clone();
        let catalog = s.catalog.clone();
        let egui_ctx = ctx.clone();
        self.rt.spawn(async move {
            match crate::explorer::load_catalog(conn).await {
                Ok(c) => {
                    *catalog.lock().unwrap() = c;
                    egui_ctx.request_repaint();
                }
                Err(e) => log::warn!("loading completion catalog: {e}"),
            }
        });
    }

    fn open_file(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                // Attach to the data source of the active tab, else the first one.
                let ds = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.view.data_source())
                    .or_else(|| self.sources.list.first().map(|d| d.config.id));
                let Some(ds) = ds else {
                    self.notify(NoticeLevel::Warning, "Create a data source first to open SQL files");
                    return;
                };
                let Some(source) = self.sources.get(ds) else { return };
                let cfg = source.config.clone();
                let id = self.next_id();
                let tab = ConsoleTab::new(id, ds, &cfg.name.clone(), cfg, text, 1).with_file(path.clone());
                self.tabs.push(Tab { id, view: Box::new(tab) });
                self.active = self.tabs.len() - 1;
                self.settings.add_recent_file(&path.to_string_lossy());
                self.settings.save(&self.dirs.settings_file());
            }
            Err(e) => self.notify(NoticeLevel::Error, format!("Opening {}: {e}", path.display())),
        }
    }

    // ----- console persistence ---------------------------------------------------------------------

    fn consoles_file(&self) -> PathBuf {
        self.dirs.consoles_dir().join("open_consoles.json")
    }

    fn save_consoles(&mut self) {
        #[derive(serde::Serialize)]
        struct Saved {
            ds: Uuid,
            text: String,
            file: Option<PathBuf>,
        }
        let mut saved = vec![];
        for t in &mut self.tabs {
            if let Some(c) = t.view.as_any_mut().downcast_mut::<ConsoleTab>() {
                saved.push(Saved { ds: c.ds, text: c.editor.text.clone(), file: c.file.clone() });
            }
        }
        if let Ok(data) = serde_json::to_vec_pretty(&saved) {
            let _ = tabularium_core::config::write_atomic(&self.consoles_file(), &data);
        }
    }

    fn restore_consoles(&mut self) {
        #[derive(serde::Deserialize)]
        struct Saved {
            ds: Uuid,
            text: String,
            file: Option<PathBuf>,
        }
        let Ok(data) = std::fs::read_to_string(self.consoles_file()) else { return };
        let Ok(saved) = serde_json::from_str::<Vec<Saved>>(&data) else { return };
        for (n, s) in saved.into_iter().enumerate() {
            let Some(source) = self.sources.get(s.ds) else { continue };
            let cfg = source.config.clone();
            let id = self.next_id();
            let mut tab = ConsoleTab::new(id, s.ds, &cfg.name.clone(), cfg, s.text.clone(), n + 1);
            if let Some(f) = s.file {
                // Keep unsaved edits but remember the file's saved contents for the dirty marker.
                let on_disk = std::fs::read_to_string(&f).unwrap_or_default();
                tab = tab.with_file(f);
                tab.saved_text = on_disk;
            }
            self.tabs.push(Tab { id, view: Box::new(tab) });
        }
        self.active = 0;
    }

    // ----- UI -----------------------------------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                ui.menu_button("New data source", |ui| {
                    for kind in DriverKind::ALL {
                        if ui.button(kind.label()).clicked() {
                            self.actions.push(AppAction::NewDataSource(kind));
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button("From URL…").clicked() {
                        self.actions.push(AppAction::NewDataSourceFromUrl);
                        ui.close();
                    }
                });
                if ui.button("New query console   Ctrl+Shift+Q").clicked() {
                    self.new_console_for_context();
                    ui.close();
                }
                ui.separator();
                if ui.button("Open SQL file…   Ctrl+O").clicked() {
                    self.pick_and_open_file();
                    ui.close();
                }
                ui.menu_button("Open recent", |ui| {
                    if self.settings.recent_files.is_empty() {
                        ui.weak("No recent files");
                    }
                    for f in self.settings.recent_files.clone() {
                        if ui.button(&f).clicked() {
                            self.actions.push(AppAction::OpenFile(f.into()));
                            ui.close();
                        }
                    }
                });
                if ui.button("Open SQLite database…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("SQLite", &["db", "sqlite", "sqlite3", "db3"]).pick_file() {
                        let cfg = ConnectionConfig::sqlite(p.to_string_lossy());
                        let id = cfg.id;
                        self.add_data_source(cfg);
                        self.actions.push(AppAction::OpenConsole { ds: id, sql: None, run: false });
                    }
                    ui.close();
                }
                ui.separator();
                if ui.button("Import data sources…").clicked() {
                    self.import_data_sources();
                    ui.close();
                }
                if ui.button("Export data sources…").clicked() {
                    self.export_data_sources();
                    ui.close();
                }
                ui.separator();
                if ui.button("Settings…   Ctrl+,").clicked() {
                    self.actions.push(AppAction::OpenSettings);
                    ui.close();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                if ui.checkbox(&mut self.show_explorer, "Database explorer   Alt+1").clicked() {
                    ui.close();
                }
                if ui.button("Query history").clicked() {
                    self.actions.push(AppAction::OpenHistory);
                    ui.close();
                }
                if ui.button("Go to object…   Ctrl+N").clicked() {
                    self.goto = Some(crate::goto::GotoDialog::new());
                    ui.close();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Zoom");
                    if ui.button("−").clicked() {
                        self.settings.ui_scale = (self.settings.ui_scale - 0.1).max(0.6);
                    }
                    ui.label(format!("{:.0}%", self.settings.ui_scale * 100.0));
                    if ui.button("+").clicked() {
                        self.settings.ui_scale = (self.settings.ui_scale + 0.1).min(2.5);
                    }
                });
            });
            ui.menu_button("Database", |ui| {
                let ds = self.context_ds();
                ui.add_enabled_ui(ds.is_some(), |ui| {
                    let ds = ds.unwrap_or_default();
                    if ui.button("Connect").clicked() {
                        self.actions.push(AppAction::Connect(ds));
                        ui.close();
                    }
                    if ui.button("Disconnect").clicked() {
                        self.actions.push(AppAction::Disconnect(ds));
                        ui.close();
                    }
                    if ui.button("Data source properties…").clicked() {
                        self.actions.push(AppAction::EditDataSource(ds));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("ER diagram").clicked() {
                        self.actions.push(AppAction::OpenErDiagram { ds, schema: None });
                        ui.close();
                    }
                    if ui.button("Sessions / process list").clicked() {
                        self.actions.push(AppAction::OpenSessions(ds));
                        ui.close();
                    }
                    if ui.button("Users & roles").clicked() {
                        self.actions.push(AppAction::OpenRoles(ds));
                        ui.close();
                    }
                    if ui.button("Full-text search in data…").clicked() {
                        self.actions.push(AppAction::OpenFindInDb { ds, schema: None });
                        ui.close();
                    }
                    if ui.button("Import data from file…").clicked() {
                        self.actions.push(AppAction::Import { ds, schema: None, table: None });
                        ui.close();
                    }
                    if ui.button("New table…").clicked() {
                        self.actions.push(AppAction::OpenModifyTable { ds, object: None, schema: None });
                        ui.close();
                    }
                });
            });
            ui.menu_button("Tools", |ui| {
                if ui.button("Compare schemas…").clicked() {
                    self.actions.push(AppAction::OpenSchemaCompare { ds: self.context_ds(), schema: None });
                    ui.close();
                }
                if ui.button("Query history").clicked() {
                    self.actions.push(AppAction::OpenHistory);
                    ui.close();
                }
            });
            ui.menu_button("Window", |ui| {
                if ui.button("Close tab   Ctrl+W").clicked() {
                    self.close_tab(self.active);
                    ui.close();
                }
                if ui.button("Close all tabs").clicked() {
                    let mut i = self.tabs.len();
                    while i > 0 {
                        i -= 1;
                        self.close_tab(i);
                    }
                    ui.close();
                }
                ui.separator();
                let titles: Vec<String> = self.tabs.iter().map(|t| t.view.title()).collect();
                for (i, t) in titles.iter().enumerate() {
                    if ui.selectable_label(i == self.active, t).clicked() {
                        self.active = i;
                        ui.close();
                    }
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard shortcuts").clicked() {
                    self.actions.push(AppAction::OpenText {
                        title: "Keyboard shortcuts".into(),
                        text: SHORTCUTS.into(),
                        ds: None,
                    });
                    ui.close();
                }
                if ui.button("About Tabularium").clicked() {
                    self.show_about = true;
                    ui.close();
                }
            });
        });
    }

    /// Data source of the active tab or the explorer selection.
    fn context_ds(&self) -> Option<Uuid> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.view.data_source())
            .or_else(|| self.explorer.selected().map(|(d, _)| *d))
            .or_else(|| (self.sources.list.len() == 1).then(|| self.sources.list[0].config.id))
    }

    fn new_console_for_context(&mut self) {
        match self.context_ds() {
            Some(ds) => {
                // Always a fresh console.
                self.actions.push(AppAction::OpenConsole { ds, sql: Some(String::new()), run: false })
            }
            None => self.notify(NoticeLevel::Warning, "Select a data source first"),
        }
    }

    fn pick_and_open_file(&mut self) {
        if let Some(p) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).add_filter("All files", &["*"]).pick_file() {
            self.actions.push(AppAction::OpenFile(p));
        }
    }

    fn import_data_sources(&mut self) {
        let Some(p) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() else { return };
        match ConnectionStore::load(&p) {
            Ok(store) => {
                let mut n = 0;
                for mut cfg in store.connections {
                    if self.sources.get(cfg.id).is_some() {
                        cfg.id = Uuid::new_v4();
                    }
                    self.sources.list.push(DataSource::new(cfg));
                    n += 1;
                }
                self.save_store();
                self.notify(NoticeLevel::Success, format!("Imported {n} data source(s)"));
            }
            Err(e) => self.notify(NoticeLevel::Error, format!("Import failed: {e}")),
        }
    }

    fn export_data_sources(&mut self) {
        let Some(p) = rfd::FileDialog::new().add_filter("JSON", &["json"]).set_file_name("datasources.json").save_file() else {
            return;
        };
        let mut store = ConnectionStore { connections: self.sources.list.iter().map(|d| d.config.clone()).collect() };
        // Never export passwords.
        for c in &mut store.connections {
            c.password = None;
            c.save_password = false;
        }
        match store.save(&p) {
            Ok(()) => self.notify(NoticeLevel::Success, format!("Exported to {}", p.display())),
            Err(e) => self.notify(NoticeLevel::Error, e.to_string()),
        }
    }

    fn close_tab(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        if !tab.view.can_close() {
            self.active = idx;
            return;
        }
        self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        } else if self.active > idx {
            self.active -= 1;
        }
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::current(ui.ctx());
        let dark = ui.visuals().dark_mode;
        let mut close = None;
        let mut activate = None;
        egui::ScrollArea::horizontal().id_salt("tab_strip").show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let selected = i == self.active;
                    let color = tab.view.data_source().and_then(|d| self.sources.get(d)).and_then(|d| d.color());
                    let frame = egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .corner_radius(egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 })
                        .fill(match (selected, color) {
                            (true, Some(c)) => theme::tint(c, dark).gamma_multiply(1.6),
                            (true, None) => ui.visuals().extreme_bg_color,
                            (false, Some(c)) => theme::tint(c, dark),
                            (false, None) => egui::Color32::TRANSPARENT,
                        });
                    let r = frame
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let title = tab.view.title();
                                let text = if selected { egui::RichText::new(&title).strong() } else { egui::RichText::new(&title) };
                                let label = ui.add(egui::Label::new(text).sense(egui::Sense::click()).selectable(false));
                                if label.clicked() {
                                    activate = Some(i);
                                }
                                if label.middle_clicked() {
                                    close = Some(i);
                                }
                                label.context_menu(|ui| {
                                    if ui.button("Close").clicked() {
                                        close = Some(i);
                                        ui.close();
                                    }
                                });
                                if ui.add(egui::Button::new(egui::RichText::new("✕").small()).frame(false)).clicked() {
                                    close = Some(i);
                                }
                            });
                        })
                        .response;
                    if selected {
                        ui.painter().hline(r.rect.x_range(), r.rect.bottom(), egui::Stroke::new(2.0_f32, palette.number));
                    }
                }
            });
        });
        if let Some(i) = activate {
            self.active = i;
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(tab) = self.tabs.get(self.active) {
                if let Some(ds) = tab.view.data_source().and_then(|d| self.sources.get(d)) {
                    if let Some(c) = ds.color() {
                        ui.colored_label(c, "■");
                    }
                    ui.label(&ds.config.name);
                    if ds.config.read_only {
                        ui.weak("read-only");
                    }
                    ui.separator();
                }
                if let Some(s) = tab.view.status() {
                    ui.weak(s);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("Tabularium {}", env!("CARGO_PKG_VERSION")));
                let running = self.background.len();
                if running > 0 {
                    ui.spinner();
                }
            });
        });
    }

    fn notifications_ui(&mut self, ctx: &egui::Context) {
        self.notices.retain(|n| n.at.elapsed() < Duration::from_secs(if n.level == NoticeLevel::Error { 12 } else { 5 }));
        if self.notices.is_empty() {
            return;
        }
        let palette = Palette::current(ctx);
        let mut dismiss = None;
        egui::Area::new(egui::Id::new("notifications"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -36.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                for (i, n) in self.notices.iter().enumerate() {
                    let color = match n.level {
                        NoticeLevel::Error => palette.error,
                        NoticeLevel::Warning => palette.warning,
                        NoticeLevel::Success => palette.success,
                        NoticeLevel::Info => palette.info,
                    };
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(color, "●");
                            ui.add(egui::Label::new(&n.text).wrap());
                            if ui.small_button("✕").clicked() {
                                dismiss = Some(i);
                            }
                        });
                    });
                }
            });
        if let Some(i) = dismiss {
            self.notices.remove(i);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn modal_dialogs(&mut self, ctx: &egui::Context) {
        // Connection dialog.
        if let Some((dialog, is_new)) = &mut self.connection_dialog {
            match dialog.show(ctx, self.rt.handle()) {
                DialogResult::Open => {}
                DialogResult::Cancel => self.connection_dialog = None,
                DialogResult::Save(cfg) => {
                    let is_new = *is_new;
                    let id = cfg.id;
                    if is_new {
                        self.add_data_source(cfg);
                    } else if let Some(ds) = self.sources.get_mut(id) {
                        ds.conn.set_config(cfg.clone());
                        ds.config = cfg;
                        let conn = ds.conn.clone();
                        self.rt.spawn(async move { conn.disconnect().await });
                        self.explorer.refresh(id);
                        self.save_store();
                    }
                    self.connection_dialog = None;
                }
            }
        }
        // Confirmation.
        if let Some(c) = &mut self.confirm {
            let mut decision = None;
            egui::Window::new(&c.title).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.add(egui::Label::new(&c.message).wrap());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        decision = Some(false);
                    }
                });
            });
            match decision {
                Some(true) => {
                    if let Some(a) = c.then.take() {
                        self.actions.push(a);
                    }
                    self.confirm = None;
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }
        // Password prompt.
        if let Some(p) = &mut self.password_prompt {
            let mut decision = None;
            let name = self.sources.name(p.ds);
            egui::Window::new(format!("Password for {name}")).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut p.password).password(true).hint_text("password"));
                r.request_focus();
                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
            match decision {
                Some(true) => {
                    if let Some(s) = self.sources.get(p.ds) {
                        s.conn.set_session_password(Some(p.password.clone()));
                    }
                    if let Some(a) = p.then.take() {
                        self.actions.push(a);
                    }
                    self.password_prompt = None;
                }
                Some(false) => self.password_prompt = None,
                None => {}
            }
        }
        // URL prompt.
        if let Some(p) = &mut self.url_prompt {
            let mut decision = None;
            egui::Window::new("New data source from URL").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut p.url).hint_text("postgres://user:pass@host:5432/db").desired_width(380.0));
                if let Some(e) = &p.error {
                    ui.colored_label(Palette::current(ctx).error, e);
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
            match decision {
                Some(true) => match ConnectionConfig::from_url(p.url.trim()) {
                    Ok(cfg) => {
                        self.url_prompt = None;
                        self.connection_dialog = Some((ConnectionDialog::new(cfg, true), true));
                    }
                    Err(e) => p.error = Some(e.to_string()),
                },
                Some(false) => self.url_prompt = None,
                None => {}
            }
        }
        if self.show_about {
            egui::Window::new("About Tabularium").collapsible(false).resizable(false).open(&mut self.show_about).show(ctx, |ui| {
                ui.heading("Tabularium");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.label("A cross-platform database IDE written in Rust.");
                ui.label("Drivers: PostgreSQL, MySQL, MariaDB, SQLite, SQL Server, CockroachDB, Redshift, TiDB");
                ui.weak(format!("Config: {}", self.dirs.config.display()));
                ui.weak(format!("Data: {}", self.dirs.data.display()));
            });
        }
        // Settings.
        if let Some(d) = &mut self.settings_dialog {
            if !d.show(ctx, &mut self.settings) {
                self.settings_dialog = None;
                self.settings.save(&self.dirs.settings_file());
            }
        }
    }

    fn global_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        let (new_console, goto, close, next, prev, open, settings, explorer, history, f4) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Q),
                i.consume_key(Modifiers::COMMAND, Key::N),
                i.consume_key(Modifiers::COMMAND, Key::W),
                i.consume_key(Modifiers::CTRL, Key::Tab) && !i.modifiers.shift,
                i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab),
                i.consume_key(Modifiers::COMMAND, Key::O),
                i.consume_key(Modifiers::COMMAND, Key::Comma),
                i.consume_key(Modifiers::ALT, Key::Num1),
                i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::H),
                i.key_pressed(Key::F4),
            )
        });
        if new_console {
            self.new_console_for_context();
        }
        if goto {
            self.goto = Some(crate::goto::GotoDialog::new());
        }
        if close && !self.tabs.is_empty() {
            self.close_tab(self.active);
        }
        if next && !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
        if prev && !self.tabs.is_empty() {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }
        if open {
            self.pick_and_open_file();
        }
        if settings {
            self.actions.push(AppAction::OpenSettings);
        }
        if explorer {
            self.show_explorer = !self.show_explorer;
        }
        if history {
            self.actions.push(AppAction::OpenHistory);
        }
        if f4 && !ctx.wants_keyboard_input() {
            if let Some((ds, obj)) = self.explorer.selected().cloned() {
                if obj.kind.has_data() {
                    self.actions.push(AppAction::OpenTableData { ds, object: obj, filter: None });
                } else {
                    self.actions.push(AppAction::OpenDdl { ds, object: obj, signature: None });
                }
            }
        }
    }

    fn welcome_ui(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.2);
            ui.heading(egui::RichText::new("Tabularium").size(32.0));
            ui.weak("Database IDE");
            ui.add_space(20.0);
            if ui.button("➕ New data source").clicked() {
                self.actions.push(AppAction::NewDataSource(DriverKind::Postgres));
            }
            if ui.button("🔗 Data source from URL").clicked() {
                self.actions.push(AppAction::NewDataSourceFromUrl);
            }
            if ui.button("📂 Open SQL file").clicked() {
                self.pick_and_open_file();
            }
            if ui.button("🪶 Open SQLite database").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("SQLite", &["db", "sqlite", "sqlite3", "db3"]).pick_file() {
                    let cfg = ConnectionConfig::sqlite(p.to_string_lossy());
                    let id = cfg.id;
                    self.add_data_source(cfg);
                    self.actions.push(AppAction::OpenConsole { ds: id, sql: None, run: false });
                }
            }
            ui.add_space(20.0);
            ui.weak("Ctrl+Shift+Q  new console   ·   Ctrl+N  go to object   ·   Ctrl+Enter  run statement");
        });
    }
}

impl eframe::App for TabulariumApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.settings_applied.as_ref() != Some(&self.settings) {
            theme::apply(ctx, &self.settings);
            self.settings_applied = Some(self.settings.clone());
        }
        // Background task completions.
        let mut done = vec![];
        self.background.retain_mut(|p| match p.poll() {
            Some(f) => {
                done.push(f);
                false
            }
            None => true,
        });
        for f in done {
            f(self);
        }

        self.global_shortcuts(ctx);

        egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu_bar(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));

        if self.show_explorer {
            egui::SidePanel::left("explorer").resizable(true).default_width(300.0).width_range(160.0..=800.0).show(ctx, |ui| {
                let mut c = Ctx {
                    rt: self.rt.handle(),
                    egui: ctx,
                    actions: &mut self.actions,
                    sources: &self.sources,
                    settings: &self.settings,
                    dirs: &self.dirs,
                    history: &self.history,
                };
                self.explorer.ui(ui, &mut c);
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.tabs.is_empty() {
                self.welcome_ui(ui);
                return;
            }
            self.tab_strip(ui);
            ui.separator();
            let active = self.active.min(self.tabs.len() - 1);
            let mut c = Ctx {
                rt: self.rt.handle(),
                egui: ctx,
                actions: &mut self.actions,
                sources: &self.sources,
                settings: &self.settings,
                dirs: &self.dirs,
                history: &self.history,
            };
            let tab = &mut self.tabs[active];
            ui.push_id(tab.id, |ui| tab.view.ui(ui, &mut c));
        });

        // Tool dialogs.
        let mut dialogs = std::mem::take(&mut self.dialogs);
        {
            let mut c = Ctx {
                rt: self.rt.handle(),
                egui: ctx,
                actions: &mut self.actions,
                sources: &self.sources,
                settings: &self.settings,
                dirs: &self.dirs,
                history: &self.history,
            };
            dialogs.retain_mut(|d| d.show(&mut c));
            if let Some(g) = &mut self.goto {
                if !g.show(&mut c) {
                    self.goto = None;
                }
            }
        }
        dialogs.append(&mut self.dialogs);
        self.dialogs = dialogs;

        self.modal_dialogs(ctx);
        self.notifications_ui(ctx);
        self.process_actions(ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.exiting {
            self.exiting = true;
            self.save_consoles();
            self.settings.save(&self.dirs.settings_file());
            let mut c = Ctx {
                rt: self.rt.handle(),
                egui: ctx,
                actions: &mut self.actions,
                sources: &self.sources,
                settings: &self.settings,
                dirs: &self.dirs,
                history: &self.history,
            };
            for t in &mut self.tabs {
                t.view.on_exit(&mut c);
            }
        }
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.save_consoles();
        self.settings.save(&self.dirs.settings_file());
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(60)
    }
}

/// Load fonts with broad Unicode coverage when available on the system, keeping egui's
/// built-ins as fallback.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates: &[&str] = &[
        // Windows
        "C:\\Windows\\Fonts\\seguisym.ttf",
        "C:\\Windows\\Fonts\\seguiemj.ttf",
        // macOS
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        // Linux
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
    ];
    for (i, path) in candidates.iter().enumerate() {
        if let Ok(data) = std::fs::read(path) {
            let name = format!("fallback{i}");
            fonts.font_data.insert(name.clone(), std::sync::Arc::new(egui::FontData::from_owned(data)));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(family).or_default().push(name.clone());
            }
        }
    }
    ctx.set_fonts(fonts);
}

const SHORTCUTS: &str = "\
-- Tabularium keyboard shortcuts (Cmd on macOS instead of Ctrl)

-- General
Ctrl+Shift+Q        New query console for the current data source
Ctrl+N              Go to object (table, view, routine...)
Ctrl+O              Open SQL file
Ctrl+W              Close tab
Ctrl+Tab            Next tab (Ctrl+Shift+Tab previous)
Ctrl+,              Settings
Ctrl+Shift+H        Query history
Alt+1               Toggle database explorer
F4                  Open selected explorer object (data or DDL)

-- Console / editor
Ctrl+Enter          Execute statement at caret (or selection)
Ctrl+Shift+Enter    Execute whole script
Ctrl+F2             Cancel running statement
Ctrl+Space          Code completion
Tab                 Expand live template (sel, ins, upd, ...)
Ctrl+Alt+L          Reformat SQL (also Ctrl+Shift+F)
Ctrl+/              Toggle line comment
Ctrl+D              Duplicate line
Ctrl+F / F3         Find / find next
Ctrl+S              Save file (Ctrl+Shift+S save as)

-- Data editor / result grid
F5                  Reload table data
Ctrl+Enter          Submit pending changes
Enter / F2          Edit cell
Insert              Add row
Ctrl+Delete         Delete selected rows
Ctrl+C / Ctrl+V     Copy / paste cells (TSV)
Ctrl+A              Select all cells
Ctrl+F              Search in results
";
