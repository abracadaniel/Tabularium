//! Shared context handed to every UI component, plus the action queue components use to
//! ask the application for things they can't do themselves (open tabs, dialogs, ...).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tabularium_core::config::AppDirs;
use tabularium_core::{ConnectionConfig, DriverKind, ObjectRef};
use tabularium_sql::catalog::SchemaCatalog;
use tabularium_tools::history::{HistoryEntry, QueryHistory};
use tokio::runtime::Handle;
use uuid::Uuid;

use crate::runtime::SharedConn;
use crate::settings::Settings;

/// A configured data source and its shared "explorer" session.
pub struct DataSource {
    pub config: ConnectionConfig,
    /// Session used by the explorer, table editors and other tools (consoles open their own).
    pub conn: Arc<SharedConn>,
    /// Names (and lazily-loaded columns) of objects, used for completion and inspections.
    pub catalog: Arc<Mutex<SchemaCatalog>>,
}

impl DataSource {
    pub fn new(config: ConnectionConfig) -> Self {
        Self {
            conn: SharedConn::new(config.clone()),
            config,
            catalog: Arc::new(Mutex::new(SchemaCatalog::default())),
        }
    }

    pub fn color(&self) -> Option<egui::Color32> {
        self.config.color.map(|[r, g, b]| egui::Color32::from_rgb(r, g, b))
    }
}

#[derive(Default)]
pub struct DataSources {
    pub list: Vec<DataSource>,
}

impl DataSources {
    pub fn get(&self, id: Uuid) -> Option<&DataSource> {
        self.list.iter().find(|d| d.config.id == id)
    }
    pub fn get_mut(&mut self, id: Uuid) -> Option<&mut DataSource> {
        self.list.iter_mut().find(|d| d.config.id == id)
    }
    pub fn name(&self, id: Uuid) -> String {
        self.get(id).map(|d| d.config.name.clone()).unwrap_or_else(|| "<deleted>".into())
    }
}

/// What to export in the export dialog.
#[derive(Clone)]
pub enum ExportSource {
    /// Rows already in memory (a result grid).
    Rows {
        columns: Vec<tabularium_core::ColumnMeta>,
        rows: Vec<Vec<tabularium_core::Value>>,
        table: Option<ObjectRef>,
    },
    /// A whole table, streamed from the database.
    Table { ds: Uuid, object: ObjectRef },
    /// A query, executed and streamed.
    Query { ds: Uuid, sql: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// Requests from components to the application.
pub enum AppAction {
    OpenConsole { ds: Uuid, sql: Option<String>, run: bool },
    OpenTableData { ds: Uuid, object: ObjectRef, filter: Option<String> },
    OpenDdl { ds: Uuid, object: ObjectRef, signature: Option<String> },
    OpenText { title: String, text: String, ds: Option<Uuid> },
    OpenFile(PathBuf),
    NewDataSource(DriverKind),
    NewDataSourceFromUrl,
    EditDataSource(Uuid),
    DuplicateDataSource(Uuid),
    DeleteDataSource(Uuid),
    Connect(Uuid),
    Disconnect(Uuid),
    RefreshExplorer(Uuid),
    OpenSchemaCompare { ds: Option<Uuid>, schema: Option<String> },
    OpenErDiagram { ds: Uuid, schema: Option<String> },
    OpenSessions(Uuid),
    OpenRoles(Uuid),
    OpenHistory,
    OpenFindInDb { ds: Uuid, schema: Option<String> },
    OpenModifyTable { ds: Uuid, object: Option<ObjectRef>, schema: Option<String> },
    OpenDataGenerator { ds: Uuid, object: ObjectRef },
    Export(ExportSource),
    Import { ds: Uuid, schema: Option<String>, table: Option<ObjectRef> },
    CopyTable { ds: Uuid, object: ObjectRef },
    Dump { ds: Uuid, restore: bool },
    RecordHistory(HistoryEntry),
    ClearHistory,
    /// Re-read objects of a data source for completion.
    InvalidateCatalog(Uuid),
    Notify(NoticeLevel, String),
    /// Ask for confirmation, then perform the inner action.
    Confirm { title: String, message: String, then: Box<AppAction> },
    /// Run statements on a data source's explorer session (after optional confirmation),
    /// then refresh the explorer. Used for DROP/TRUNCATE etc.
    RunStatements { ds: Uuid, statements: Vec<String>, refresh: bool },
    OpenSettings,
    CopyToClipboard(String),
}

/// Per-frame context shared by all components.
pub struct Ctx<'a> {
    pub rt: &'a Handle,
    pub egui: &'a egui::Context,
    pub actions: &'a mut Vec<AppAction>,
    pub sources: &'a DataSources,
    pub settings: &'a Settings,
    pub dirs: &'a AppDirs,
    pub history: &'a QueryHistory,
}

impl Ctx<'_> {
    pub fn act(&mut self, action: AppAction) {
        self.actions.push(action);
    }
    pub fn notify(&mut self, level: NoticeLevel, msg: impl Into<String>) {
        self.actions.push(AppAction::Notify(level, msg.into()));
    }
    pub fn error(&mut self, msg: impl std::fmt::Display) {
        self.notify(NoticeLevel::Error, msg.to_string());
    }
    pub fn info(&mut self, msg: impl Into<String>) {
        self.notify(NoticeLevel::Info, msg);
    }
}

/// A document in the central tab strip.
pub trait TabView {
    fn title(&self) -> String;
    /// Data source the tab belongs to, for coloring and status bar.
    fn data_source(&self) -> Option<Uuid> {
        None
    }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx);
    /// Text shown in the status bar while the tab is active.
    fn status(&self) -> Option<String> {
        None
    }
    /// Return `false` to veto closing (e.g. uncommitted transaction); the tab should
    /// then show its own prompt.
    fn can_close(&mut self) -> bool {
        true
    }
    /// Persist state on exit (e.g. console contents).
    fn on_exit(&mut self, _ctx: &mut Ctx) {}
    /// Downcasting support so the app can find specific tabs.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// A modeless/modal window owned by the app (export, import, copy table, ...).
pub trait Dialog {
    /// Draw the dialog; return `false` once it should be closed.
    fn show(&mut self, ctx: &mut Ctx) -> bool;
}
