//! Query console: SQL editor bound to its own database session, with multiple result
//! tabs, transaction control, execution plans, parameters and editable results.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use tabularium_core::{
    driver::{first_keyword, ExecOptions},
    Error, ObjectRef, StatementResult, TableInfo, TransactionMode,
};
use tabularium_sql::analyze::{find_parameters, single_table_select, substitute_parameters};
use tabularium_sql::split::{split_statements, statement_at};
use tabularium_tools::changes::change_statements_with_table;
use tabularium_tools::history::HistoryEntry;
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, ExportSource, NoticeLevel, TabView};
use crate::editor::{CodeEditor, EditorEnv};
use crate::grid::{GridEvent, ResultGrid};
use crate::runtime::{poll_slot, Pending, SharedConn};
use crate::theme::Palette;

enum ExecEvent {
    Started(usize),
    Done { index: usize, sql: String, offset: usize, result: Result<StatementResult, Error> },
    Finished { in_transaction: bool },
}

enum ResultView {
    Grid {
        grid: Box<ResultGrid>,
        summary: String,
        /// Table the result is editable against.
        table: Option<TableInfo>,
        sql: String,
    },
    Plan { text: String, summary: String },
    Message { text: String, error: bool },
}

struct ResultTab {
    title: String,
    view: ResultView,
    pinned: bool,
}

struct ParamPrompt {
    statements: Vec<(String, usize)>,
    names: Vec<String>,
    values: Vec<String>,
    quote: Vec<bool>,
    explain: Option<bool>,
}

struct Confirm {
    message: String,
    statements: Vec<(String, usize)>,
    explain: Option<bool>,
}

pub struct ConsoleTab {
    id: u64,
    pub ds: Uuid,
    title: String,
    pub editor: CodeEditor,
    session: Arc<SharedConn>,
    results: Vec<ResultTab>,
    active_result: usize,
    output: Vec<(String, NoticeLevel)>,
    running: Option<mpsc::Receiver<ExecEvent>>,
    run_started: Option<Instant>,
    current_statement: Option<usize>,
    tx_mode: TransactionMode,
    in_transaction: bool,
    pending_tx: Option<Pending<Result<bool, Error>>>,
    databases: Vec<String>,
    schemas: Vec<String>,
    current_db: Option<String>,
    current_schema: Option<String>,
    nav_loading: Option<Pending<Result<(Vec<String>, Vec<String>, Option<String>, Option<String>), Error>>>,
    nav_loaded: bool,
    switch_task: Option<Pending<Result<(), Error>>>,
    describe_tasks: Vec<Pending<Option<TableInfo>>>,
    submit_task: Option<Pending<Result<usize, Error>>>,
    pub file: Option<PathBuf>,
    pub saved_text: String,
    params: Option<ParamPrompt>,
    confirm: Option<Confirm>,
    param_values: HashMap<String, String>,
    max_rows: usize,
    results_height: f32,
    close_prompt: bool,
    force_close: bool,
}

impl ConsoleTab {
    pub fn new(id: u64, ds: Uuid, ds_name: &str, config: tabularium_core::ConnectionConfig, text: String, ordinal: usize) -> Self {
        let dialect = config.dialect();
        let tx_mode = if config.auto_commit { TransactionMode::Auto } else { TransactionMode::Manual };
        let title = if ordinal <= 1 { format!("{ds_name} [console]") } else { format!("{ds_name} [console {ordinal}]") };
        Self {
            id,
            ds,
            title,
            editor: CodeEditor::new(("console", id), text.clone(), dialect),
            session: SharedConn::new(config),
            results: vec![],
            active_result: 0,
            output: vec![],
            running: None,
            run_started: None,
            current_statement: None,
            tx_mode,
            in_transaction: false,
            pending_tx: None,
            databases: vec![],
            schemas: vec![],
            current_db: None,
            current_schema: None,
            nav_loading: None,
            nav_loaded: false,
            switch_task: None,
            describe_tasks: vec![],
            submit_task: None,
            file: None,
            saved_text: text,
            params: None,
            confirm: None,
            param_values: HashMap::new(),
            max_rows: 500,
            results_height: 260.0,
            close_prompt: false,
            force_close: false,
        }
    }

    pub fn with_file(mut self, path: PathBuf) -> Self {
        self.title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.file = Some(path);
        self
    }

    pub fn session(&self) -> &Arc<SharedConn> {
        &self.session
    }

    pub fn is_dirty(&self) -> bool {
        self.file.is_some() && self.editor.text != self.saved_text
    }

    fn log(&mut self, level: NoticeLevel, msg: impl Into<String>) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        self.output.push((format!("[{ts}] {}", msg.into()), level));
        if self.output.len() > 2000 {
            self.output.drain(..500);
        }
    }

    fn is_running(&self) -> bool {
        self.running.is_some()
    }

    // ----- execution ------------------------------------------------------------------------------

    /// Statements to run: selection (split), else statement under caret, else whole script.
    fn collect_statements(&self, whole_script: bool) -> Vec<(String, usize)> {
        let dialect = self.editor.dialect;
        if let (Some((a, _)), Some(sel)) = (self.editor.selection(), self.editor.selected_text()) {
            return split_statements(sel, dialect).into_iter().map(|s| (s.text, a + s.start)).collect();
        }
        if whole_script {
            return split_statements(&self.editor.text, dialect).into_iter().map(|s| (s.text, s.start)).collect();
        }
        statement_at(&self.editor.text, self.editor.cursor(), dialect)
            .map(|s| vec![(s.text, s.start)])
            .unwrap_or_default()
    }

    fn request_run(&mut self, ctx: &mut Ctx, whole_script: bool, explain: Option<bool>) {
        if self.is_running() {
            return;
        }
        let statements = self.collect_statements(whole_script);
        if statements.is_empty() {
            ctx.info("Nothing to execute");
            return;
        }
        // Query parameters.
        let mut names: Vec<String> = vec![];
        for (s, _) in &statements {
            for p in find_parameters(s, self.editor.dialect) {
                if !names.contains(&p.name) {
                    names.push(p.name);
                }
            }
        }
        if !names.is_empty() {
            let values: Vec<String> = names.iter().map(|n| self.param_values.get(n).cloned().unwrap_or_default()).collect();
            let quote = values.iter().map(|v| v.parse::<f64>().is_err() && v.to_ascii_uppercase() != "NULL").collect();
            self.params = Some(ParamPrompt { statements, names, values, quote, explain });
            return;
        }
        self.check_and_run(ctx, statements, explain);
    }

    fn check_and_run(&mut self, ctx: &mut Ctx, statements: Vec<(String, usize)>, explain: Option<bool>) {
        if ctx.settings.confirm_dangerous_statements && explain.is_none() {
            let dangerous: Vec<&String> = statements.iter().map(|(s, _)| s).filter(|s| is_unbounded_dml(s, self.editor.dialect)).collect();
            if !dangerous.is_empty() {
                self.confirm = Some(Confirm {
                    message: format!(
                        "{} statement(s) will modify every row of the table (no WHERE clause):\n\n{}",
                        dangerous.len(),
                        dangerous.iter().map(|s| truncate_line(s, 100)).collect::<Vec<_>>().join("\n")
                    ),
                    statements,
                    explain,
                });
                return;
            }
        }
        self.run(ctx, statements, explain);
    }

    fn run(&mut self, ctx: &mut Ctx, statements: Vec<(String, usize)>, explain: Option<bool>) {
        let (tx, rx) = mpsc::channel();
        let session = self.session.clone();
        let max_rows = ctx.settings.console_max_rows.max(1);
        self.max_rows = max_rows;
        let egui_ctx = ctx.egui.clone();
        // Drop unpinned result tabs, like DataGrip does on re-run.
        self.results.retain(|r| r.pinned);
        self.active_result = 0;
        self.editor.error_range = None;
        self.run_started = Some(Instant::now());
        let count = statements.len();
        ctx.rt.spawn(async move {
            let conn = match session.get().await {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(ExecEvent::Done { index: 0, sql: String::new(), offset: 0, result: Err(e) });
                    let _ = tx.send(ExecEvent::Finished { in_transaction: false });
                    egui_ctx.request_repaint();
                    return;
                }
            };
            for (index, (sql, offset)) in statements.into_iter().enumerate() {
                let _ = tx.send(ExecEvent::Started(index));
                egui_ctx.request_repaint();
                let result = match explain {
                    Some(analyze) => conn.explain(&sql, analyze).await,
                    None => conn.execute(&sql, &ExecOptions::with_limit(max_rows)).await,
                };
                let failed = result.is_err();
                if let Err(e) = &result {
                    session.reset_if_broken(e).await;
                }
                let _ = tx.send(ExecEvent::Done { index, sql, offset, result });
                egui_ctx.request_repaint();
                if failed {
                    break;
                }
            }
            let in_tx = match session.current().await {
                Some(c) => c.in_transaction(),
                None => false,
            };
            let _ = tx.send(ExecEvent::Finished { in_transaction: in_tx });
            egui_ctx.request_repaint();
        });
        self.running = Some(rx);
        self.log(NoticeLevel::Info, format!("Executing {count} statement(s)…"));
    }

    fn poll_execution(&mut self, ctx: &mut Ctx) {
        let Some(rx) = &self.running else { return };
        let mut events = vec![];
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        for ev in events {
            match ev {
                ExecEvent::Started(i) => self.current_statement = Some(i),
                ExecEvent::Done { index, sql, offset, result } => self.on_statement_done(ctx, index, sql, offset, result),
                ExecEvent::Finished { in_transaction } => {
                    self.running = None;
                    self.current_statement = None;
                    self.in_transaction = in_transaction;
                    if let Some(t) = self.run_started.take() {
                        self.log(NoticeLevel::Info, format!("Finished in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0));
                    }
                    if !self.nav_loaded {
                        self.load_navigation(ctx);
                    }
                }
            }
        }
    }

    fn on_statement_done(&mut self, ctx: &mut Ctx, _index: usize, sql: String, offset: usize, result: Result<StatementResult, Error>) {
        let ds_name = ctx.sources.name(self.ds);
        match result {
            Ok(res) => {
                let summary = res.summary();
                for n in &res.notices {
                    self.log(NoticeLevel::Warning, n.clone());
                }
                self.log(NoticeLevel::Success, format!("{} … {summary}", truncate_line(&sql, 80)));
                if ctx.settings.record_history {
                    ctx.act(AppAction::RecordHistory(HistoryEntry {
                        timestamp: chrono::Utc::now(),
                        connection: ds_name,
                        database: self.current_db.clone(),
                        sql: sql.clone(),
                        duration_ms: res.elapsed.as_millis() as u64,
                        rows: res.rows_affected.or(Some(res.rows.len() as u64)),
                        error: None,
                    }));
                }
                let first = first_keyword(&sql).to_ascii_uppercase();
                if matches!(first.as_str(), "CREATE" | "DROP" | "ALTER" | "RENAME" | "COMMENT") {
                    ctx.act(AppAction::RefreshExplorer(self.ds));
                    ctx.act(AppAction::InvalidateCatalog(self.ds));
                }
                if matches!(first.as_str(), "USE" | "SET") {
                    self.nav_loaded = false;
                }
                let is_plan = first == "EXPLAIN" || res.columns.len() == 1 && res.columns[0].name.eq_ignore_ascii_case("QUERY PLAN");
                if res.has_result_set() && is_plan {
                    let text = plan_text(&res);
                    let n = self.results.len() + 1;
                    self.results.push(ResultTab { title: format!("Plan {n}"), view: ResultView::Plan { text, summary }, pinned: false });
                    self.active_result = self.results.len() - 1;
                } else if res.has_result_set() {
                    let n = self.results.iter().filter(|r| matches!(r.view, ResultView::Grid { .. })).count() + 1;
                    let mut grid = ResultGrid::new(("console_result", self.id, n, Instant::now()), res.columns, res.rows);
                    grid.dialect = self.editor.dialect;
                    grid.null_text = ctx.settings.null_text.clone();
                    grid.max_cell_chars = ctx.settings.max_cell_chars;
                    grid.font_size = ctx.settings.grid_font_size;
                    let truncated = if res.truncated { format!(" (first {} rows)", self.max_rows) } else { String::new() };
                    // Editable when it is a plain single-table SELECT.
                    if let Some(t) = single_table_select(&sql, self.editor.dialect) {
                        let object = ObjectRef::table(t.schema.as_deref(), t.name.clone());
                        grid.table = Some(object.clone());
                        self.describe_for_edit(ctx, object, n);
                    }
                    self.results.push(ResultTab {
                        title: format!("Result {n}"),
                        view: ResultView::Grid { grid: Box::new(grid), summary: format!("{summary}{truncated}"), table: None, sql },
                        pinned: false,
                    });
                    self.active_result = self.results.len() - 1;
                } else if self.results.iter().all(|r| !matches!(r.view, ResultView::Grid { .. })) {
                    self.results.retain(|r| !matches!(r.view, ResultView::Message { .. }) || r.pinned);
                    self.results.push(ResultTab { title: "Result".into(), view: ResultView::Message { text: summary, error: false }, pinned: false });
                    self.active_result = self.results.len() - 1;
                }
            }
            Err(e) => {
                let msg = e.to_string();
                self.log(NoticeLevel::Error, format!("{} … {msg}", truncate_line(&sql, 80)));
                if let Error::Database(db) = &e {
                    if let Some(pos) = db.position {
                        // 1-based character offset within the statement.
                        let byte_in_stmt = sql.char_indices().nth(pos.saturating_sub(1)).map(|(i, _)| i).unwrap_or(0);
                        let start = offset + byte_in_stmt;
                        let end = self.editor.text[start.min(self.editor.text.len())..]
                            .find(|c: char| c.is_whitespace() || c == ',' || c == ')')
                            .map(|l| start + l.max(1))
                            .unwrap_or(self.editor.text.len());
                        self.editor.error_range = Some((start, end));
                    } else {
                        self.editor.error_range = Some((offset, offset + sql.len()));
                    }
                } else if !sql.is_empty() {
                    self.editor.error_range = Some((offset, offset + sql.len()));
                }
                if ctx.settings.record_history && !sql.is_empty() {
                    ctx.act(AppAction::RecordHistory(HistoryEntry {
                        timestamp: chrono::Utc::now(),
                        connection: ds_name,
                        database: self.current_db.clone(),
                        sql: sql.clone(),
                        duration_ms: 0,
                        rows: None,
                        error: Some(msg.clone()),
                    }));
                }
                self.results.push(ResultTab { title: "Error".into(), view: ResultView::Message { text: msg, error: true }, pinned: false });
                self.active_result = self.results.len() - 1;
            }
        }
    }

    fn describe_for_edit(&mut self, ctx: &mut Ctx, object: ObjectRef, _n: usize) {
        let session = self.session.clone();
        self.describe_tasks.push(Pending::spawn(ctx.rt, ctx.egui, async move {
            let conn = session.get().await.ok()?;
            let mut object = object;
            if object.schema.is_none() {
                object.schema = conn.current_schema().await.ok().flatten();
            }
            let info = conn.describe_table(&object).await.ok()?;
            Some(info)
        }));
    }

    fn poll_describes(&mut self) {
        let mut done = vec![];
        self.describe_tasks.retain_mut(|p| match p.poll() {
            Some(r) => {
                done.push(r);
                false
            }
            None => true,
        });
        for info in done.into_iter().flatten() {
            for r in &mut self.results {
                if let ResultView::Grid { grid, table, .. } = &mut r.view {
                    let matches = grid.table.as_ref().is_some_and(|t| t.name.eq_ignore_ascii_case(&info.object.name));
                    if matches && table.is_none() {
                        // Only editable when every result column is a real column of the table
                        // and rows can be identified.
                        let all_cols = grid.columns.iter().all(|c| info.column(&c.name).is_some());
                        if all_cols {
                            grid.editable = true;
                            grid.table = Some(info.object.clone());
                            *table = Some(info.clone());
                        }
                    }
                }
            }
        }
    }

    fn submit_result_changes(&mut self, ctx: &mut Ctx) {
        let Some(tab) = self.results.get(self.active_result) else { return };
        let ResultView::Grid { grid, table: Some(table), .. } = &tab.view else { return };
        let changes = grid.pending_changes(table);
        if changes.is_empty() {
            return;
        }
        let statements = change_statements_with_table(table, &changes, self.editor.dialect);
        for s in &statements {
            self.log(NoticeLevel::Info, s.clone());
        }
        let session = self.session.clone();
        self.submit_task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let conn = session.get().await?;
            let mut affected = 0;
            for s in &statements {
                let r = conn.execute(s, &ExecOptions::default()).await?;
                affected += r.rows_affected.unwrap_or(0) as usize;
            }
            Ok(affected)
        }));
    }

    fn load_navigation(&mut self, ctx: &mut Ctx) {
        if self.nav_loading.is_some() {
            return;
        }
        self.nav_loaded = true;
        let session = self.session.clone();
        self.nav_loading = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let conn = session.get().await?;
            let dbs = conn.list_databases().await.unwrap_or_default();
            let schemas = conn.list_schemas().await.unwrap_or_default();
            let cur_schema = conn.current_schema().await.ok().flatten();
            Ok((dbs, schemas, conn.current_database(), cur_schema))
        }));
    }

    fn switch_database(&mut self, ctx: &mut Ctx, db: String) {
        let session = self.session.clone();
        self.current_db = Some(db.clone());
        self.switch_task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let conn = session.get().await?;
            conn.use_database(&db).await
        }));
    }

    fn switch_schema(&mut self, ctx: &mut Ctx, schema: String) {
        let session = self.session.clone();
        let dialect = self.editor.dialect;
        self.current_schema = Some(schema.clone());
        self.switch_task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let conn = session.get().await?;
            let sql = match dialect {
                tabularium_core::Dialect::Postgres => format!("SET search_path TO {}", dialect.quote_ident_if_needed(&schema)),
                tabularium_core::Dialect::MySql => format!("USE {}", dialect.quote_ident(&schema)),
                tabularium_core::Dialect::MsSql => return Err(Error::Unsupported("switching default schema".into())),
                _ => return Ok(()),
            };
            conn.execute(&sql, &ExecOptions::default()).await.map(|_| ())
        }));
    }

    fn set_tx_mode(&mut self, ctx: &mut Ctx, mode: TransactionMode) {
        self.tx_mode = mode;
        let session = self.session.clone();
        self.pending_tx = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let conn = session.get().await?;
            conn.set_transaction_mode(mode).await?;
            Ok(conn.in_transaction())
        }));
    }

    fn end_transaction(&mut self, ctx: &mut Ctx, commit: bool) {
        let session = self.session.clone();
        self.log(NoticeLevel::Info, if commit { "COMMIT" } else { "ROLLBACK" });
        self.pending_tx = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let conn = session.get().await?;
            if commit { conn.commit().await? } else { conn.rollback().await? }
            Ok(conn.in_transaction())
        }));
    }

    fn cancel(&mut self, ctx: &mut Ctx) {
        match self.session.cancel_handle() {
            Some(h) => {
                self.log(NoticeLevel::Warning, "Cancelling…");
                ctx.rt.spawn(async move {
                    if let Err(e) = h.cancel().await {
                        log::warn!("cancel failed: {e}");
                    }
                });
            }
            None => {
                // No server-side cancel: drop the session; the running task ends with an error.
                self.log(NoticeLevel::Warning, "Driver can't cancel; closing session");
                let s = self.session.clone();
                ctx.rt.spawn(async move { s.disconnect().await });
                self.running = None;
                self.current_statement = None;
            }
        }
    }

    fn format_sql(&mut self, ctx: &mut Ctx) {
        let opts = ctx.settings.format_options();
        let dialect = self.editor.dialect;
        if let (Some((a, b)), Some(sel)) = (self.editor.selection(), self.editor.selected_text().map(str::to_string)) {
            let formatted = tabularium_sql::format::format_sql(&sel, dialect, &opts);
            self.editor.replace_range(a, b, &formatted);
        } else {
            let formatted = tabularium_sql::format::format_sql(&self.editor.text, dialect, &opts);
            self.editor.set_text(formatted);
        }
    }

    pub fn save_file(&mut self, ctx: &mut Ctx, save_as: bool) {
        let path = match (&self.file, save_as) {
            (Some(p), false) => Some(p.clone()),
            _ => rfd::FileDialog::new().add_filter("SQL", &["sql"]).set_file_name("query.sql").save_file(),
        };
        let Some(path) = path else { return };
        match std::fs::write(&path, &self.editor.text) {
            Ok(()) => {
                self.saved_text = self.editor.text.clone();
                self.title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ctx.info(format!("Saved {}", path.display()));
                self.file = Some(path);
            }
            Err(e) => ctx.error(format!("Saving {}: {e}", path.display())),
        }
    }

    /// Fill in columns of tables referenced by the statement at the caret (for completion).
    fn ensure_catalog_columns(&mut self, ctx: &mut Ctx) {
        let Some(ds) = ctx.sources.get(self.ds) else { return };
        let Some(stmt) = statement_at(&self.editor.text, self.editor.cursor(), self.editor.dialect) else { return };
        let refs = tabularium_sql::analyze::referenced_tables(&stmt.text, self.editor.dialect);
        let catalog = ds.catalog.clone();
        let missing: Vec<ObjectRef> = {
            let cat = catalog.lock().unwrap();
            refs.iter()
                .filter_map(|r| {
                    let t = cat.find_table(r.schema.as_deref(), &r.name)?;
                    if t.columns.is_empty() { Some(ObjectRef::new(t.schema.as_deref(), t.name.clone(), t.kind)) } else { None }
                })
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let conn = ds.conn.clone();
        // Mark as "loading" by inserting a placeholder column to avoid duplicate requests.
        {
            let mut cat = catalog.lock().unwrap();
            for m in &missing {
                if let Some(t) = cat.tables.iter_mut().find(|t| t.name == m.name && t.schema == m.schema) {
                    t.columns.push(tabularium_sql::catalog::CatalogColumn { name: String::new(), data_type: String::new() });
                }
            }
        }
        let egui_ctx = ctx.egui.clone();
        ctx.rt.spawn(async move {
            let Ok(c) = conn.get().await else { return };
            for m in missing {
                let info = c.describe_table(&m).await;
                let mut cat = catalog.lock().unwrap();
                if let Some(t) = cat.tables.iter_mut().find(|t| t.name == m.name && t.schema == m.schema) {
                    t.columns = match info {
                        Ok(info) => info
                            .columns
                            .into_iter()
                            .map(|c| tabularium_sql::catalog::CatalogColumn { name: c.name, data_type: c.data_type })
                            .collect(),
                        Err(_) => vec![],
                    };
                }
            }
            egui_ctx.request_repaint();
        });
    }

    // ----- UI ---------------------------------------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        let running = self.is_running();
        ui.horizontal(|ui| {
            if ui.add_enabled(!running, egui::Button::new("▶ Run")).on_hover_text("Execute statement at caret or selection (Ctrl+Enter)").clicked() {
                self.request_run(ctx, false, None);
            }
            if ui.add_enabled(!running, egui::Button::new("▶▶ Script")).on_hover_text("Execute whole script (Ctrl+Shift+Enter)").clicked() {
                self.request_run(ctx, true, None);
            }
            if ui.add_enabled(running, egui::Button::new("■ Stop")).on_hover_text("Cancel running statement").clicked() {
                self.cancel(ctx);
            }
            ui.menu_button("Explain", |ui| {
                if ui.button("Explain plan").clicked() {
                    self.request_run(ctx, false, Some(false));
                    ui.close();
                }
                if ui.button("Explain analyze (executes!)").clicked() {
                    self.request_run(ctx, false, Some(true));
                    ui.close();
                }
            });
            ui.separator();
            ui.label("Tx:");
            let mut mode = self.tx_mode;
            egui::ComboBox::from_id_salt(("txmode", self.id))
                .width(70.0)
                .selected_text(if mode == TransactionMode::Auto { "Auto" } else { "Manual" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut mode, TransactionMode::Auto, "Auto");
                    ui.selectable_value(&mut mode, TransactionMode::Manual, "Manual");
                });
            if mode != self.tx_mode {
                self.set_tx_mode(ctx, mode);
            }
            let tx_active = self.in_transaction;
            if ui.add_enabled(tx_active && !running, egui::Button::new("✔ Commit")).clicked() {
                self.end_transaction(ctx, true);
            }
            if ui.add_enabled(tx_active && !running, egui::Button::new("↶ Rollback")).clicked() {
                self.end_transaction(ctx, false);
            }
            if tx_active {
                ui.colored_label(Palette::current(ui.ctx()).warning, "● TX");
            }
            ui.separator();
            if ui.button("Format").on_hover_text("Reformat SQL (Ctrl+Alt+L)").clicked() {
                self.format_sql(ctx);
            }
            ui.separator();
            // Database / schema switchers.
            if !self.databases.is_empty() {
                let mut sel = self.current_db.clone().unwrap_or_default();
                egui::ComboBox::from_id_salt(("db", self.id)).width(120.0).selected_text(&sel).show_ui(ui, |ui| {
                    for d in &self.databases {
                        ui.selectable_value(&mut sel, d.clone(), d);
                    }
                });
                if Some(&sel) != self.current_db.as_ref() && !sel.is_empty() {
                    self.switch_database(ctx, sel);
                }
            }
            if !self.schemas.is_empty() && self.editor.dialect != tabularium_core::Dialect::MySql {
                let mut sel = self.current_schema.clone().unwrap_or_default();
                egui::ComboBox::from_id_salt(("schema", self.id)).width(120.0).selected_text(&sel).show_ui(ui, |ui| {
                    for s in &self.schemas {
                        ui.selectable_value(&mut sel, s.clone(), s);
                    }
                });
                if Some(&sel) != self.current_schema.as_ref() && !sel.is_empty() {
                    self.switch_schema(ctx, sel);
                }
            }
            if running {
                ui.spinner();
                if let Some(t) = self.run_started {
                    ui.weak(format!("{:.1}s", t.elapsed().as_secs_f32()));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                }
            }
        });
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        let palette = Palette::current(ui.ctx());
        ui.horizontal(|ui| {
            let output_selected = self.active_result >= self.results.len();
            if ui.selectable_label(output_selected, "Output").clicked() {
                self.active_result = usize::MAX;
            }
            let mut close = None;
            for (i, r) in self.results.iter_mut().enumerate() {
                let label = if r.pinned { format!("📌 {}", r.title) } else { r.title.clone() };
                let resp = ui.selectable_label(self.active_result == i, label);
                if resp.clicked() {
                    self.active_result = i;
                }
                resp.context_menu(|ui| {
                    if ui.button(if r.pinned { "Unpin" } else { "Pin tab" }).clicked() {
                        r.pinned = !r.pinned;
                        ui.close();
                    }
                    if ui.button("Close").clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
                if resp.middle_clicked() {
                    close = Some(i);
                }
            }
            if let Some(i) = close {
                self.results.remove(i);
                if self.active_result >= self.results.len() {
                    self.active_result = self.results.len().saturating_sub(1);
                }
            }
        });
        ui.separator();
        if self.active_result >= self.results.len() {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for (line, level) in &self.output {
                    let color = match level {
                        NoticeLevel::Error => palette.error,
                        NoticeLevel::Warning => palette.warning,
                        NoticeLevel::Success => palette.text,
                        NoticeLevel::Info => palette.comment,
                    };
                    ui.label(egui::RichText::new(line).monospace().color(color));
                }
            });
            return;
        }
        let ds = self.ds;
        let submitting = self.submit_task.is_some();
        let mut submit = false;
        let tab = &mut self.results[self.active_result];
        match &mut tab.view {
            ResultView::Grid { grid, summary, table, sql } => {
                ui.horizontal(|ui| {
                    ui.weak(summary.as_str());
                    ui.separator();
                    if grid.editable && table.is_some() {
                        if ui.small_button("＋").on_hover_text("Add row").clicked() {
                            grid.add_row();
                        }
                        if ui.small_button("－").on_hover_text("Delete selected rows").clicked() {
                            grid.delete_selected_rows();
                        }
                        let pending = grid.has_pending_changes();
                        if ui.add_enabled(pending && !submitting, egui::Button::new(format!("Submit ({})", grid.pending_count())).small()).clicked() {
                            submit = true;
                        }
                        if ui.add_enabled(pending, egui::Button::new("Revert").small()).clicked() {
                            grid.revert();
                        }
                        ui.separator();
                    }
                    if ui.small_button("Transpose").clicked() {
                        grid.transposed = !grid.transposed;
                    }
                    if ui.small_button("Value").on_hover_text("Value viewer").clicked() {
                        grid.show_value_viewer = !grid.show_value_viewer;
                    }
                    if ui.small_button("🔍").on_hover_text("Search in results (Ctrl+F)").clicked() {
                        grid.toggle_search();
                    }
                    if ui.small_button("Export…").clicked() {
                        ctx.act(AppAction::Export(ExportSource::Rows {
                            columns: grid.columns.clone(),
                            rows: grid.display_rows(),
                            table: grid.table.clone(),
                        }));
                    }
                    if ui.small_button("Export all rows…").on_hover_text("Re-run the query without row limit and export").clicked() {
                        ctx.act(AppAction::Export(ExportSource::Query { ds, sql: sql.clone() }));
                    }
                    if let Some(agg) = grid.selection_aggregates() {
                        ui.separator();
                        ui.weak(agg);
                    }
                });
                for ev in grid.show(ui) {
                    if let GridEvent::ExportRequested = ev {
                        ctx.act(AppAction::Export(ExportSource::Rows {
                            columns: grid.columns.clone(),
                            rows: grid.display_rows(),
                            table: grid.table.clone(),
                        }));
                    }
                }
            }
            ResultView::Plan { text, summary } => {
                ui.weak(summary.as_str());
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    plan_ui(ui, text, &palette);
                });
            }
            ResultView::Message { text, error } => {
                let color = if *error { palette.error } else { palette.text };
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.label(egui::RichText::new(text.as_str()).monospace().color(color));
                });
            }
        }
        if submit {
            self.submit_result_changes(ctx);
        }
    }

    fn dialogs(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        let mut run_with: Option<(Vec<(String, usize)>, Option<bool>)> = None;
        if let Some(p) = &mut self.params {
            let mut open = true;
            let mut go = false;
            egui::Window::new("Query parameters").collapsible(false).resizable(false).open(&mut open).show(ui.ctx(), |ui| {
                egui::Grid::new(("params", self.id)).num_columns(3).show(ui, |ui| {
                    for i in 0..p.names.len() {
                        ui.monospace(&p.names[i]);
                        let r = ui.text_edit_singleline(&mut p.values[i]);
                        if i == 0 && !r.has_focus() && ui.memory(|m| m.focused().is_none()) {
                            r.request_focus();
                        }
                        ui.checkbox(&mut p.quote[i], "as string").on_hover_text("Wrap in quotes as a string literal");
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Execute").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        go = true;
                    }
                });
            });
            if go {
                let dialect = self.editor.dialect;
                let mut map = HashMap::new();
                for i in 0..p.names.len() {
                    let raw = p.values[i].clone();
                    self.param_values.insert(p.names[i].clone(), raw.clone());
                    let lit = if p.quote[i] { dialect.quote_string(&raw) } else { raw };
                    map.insert(p.names[i].clone(), lit);
                }
                let stmts = p.statements.iter().map(|(s, o)| (substitute_parameters(s, dialect, &map), *o)).collect();
                run_with = Some((stmts, p.explain));
                open = false;
            }
            if !open {
                self.params = None;
            }
        }
        if let Some(c) = &self.confirm {
            let mut decision = None;
            egui::Window::new("Confirm data modification").collapsible(false).resizable(false).show(ui.ctx(), |ui| {
                ui.label(&c.message);
                ui.horizontal(|ui| {
                    if ui.button("Execute").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
            match decision {
                Some(true) => {
                    let c = self.confirm.take().unwrap();
                    self.run(ctx, c.statements, c.explain);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }
        if let Some((stmts, explain)) = run_with {
            self.check_and_run(ctx, stmts, explain);
        }
        if self.close_prompt {
            let mut decision = None;
            egui::Window::new("Uncommitted transaction").collapsible(false).resizable(false).show(ui.ctx(), |ui| {
                ui.label("This console has an open transaction. What should happen to it?");
                ui.horizontal(|ui| {
                    if ui.button("Commit").clicked() {
                        decision = Some(Some(true));
                    }
                    if ui.button("Rollback").clicked() {
                        decision = Some(Some(false));
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(None);
                    }
                });
            });
            match decision {
                Some(Some(commit)) => {
                    self.end_transaction(ctx, commit);
                    self.in_transaction = false;
                    self.close_prompt = false;
                    self.force_close = true;
                }
                Some(None) => self.close_prompt = false,
                None => {}
            }
        }
    }

    fn poll_tasks(&mut self, ctx: &mut Ctx) {
        self.poll_execution(ctx);
        self.poll_describes();
        if let Some(r) = poll_slot(&mut self.pending_tx) {
            match r {
                Ok(in_tx) => self.in_transaction = in_tx,
                Err(e) => {
                    self.log(NoticeLevel::Error, e.to_string());
                    ctx.error(e);
                }
            }
        }
        if let Some(r) = poll_slot(&mut self.nav_loading) {
            match r {
                Ok((dbs, schemas, db, schema)) => {
                    self.databases = dbs;
                    self.schemas = schemas;
                    self.current_db = db;
                    self.current_schema = schema;
                }
                Err(e) => self.log(NoticeLevel::Error, e.to_string()),
            }
        }
        if let Some(r) = poll_slot(&mut self.switch_task) {
            match r {
                Ok(()) => {
                    self.nav_loaded = false;
                    self.load_navigation(ctx);
                }
                Err(e) => {
                    self.log(NoticeLevel::Error, e.to_string());
                    ctx.error(e);
                }
            }
        }
        if let Some(r) = poll_slot(&mut self.submit_task) {
            match r {
                Ok(n) => {
                    self.log(NoticeLevel::Success, format!("Submitted changes: {n} row(s) affected"));
                    if let Some(ResultTab { view: ResultView::Grid { grid, .. }, .. }) = self.results.get_mut(self.active_result) {
                        grid.commit_pending();
                    }
                    let s = self.session.clone();
                    let egui_ctx = ctx.egui.clone();
                    let (tx, rx) = mpsc::channel();
                    ctx.rt.spawn(async move {
                        let in_tx = s.current().await.map(|c| c.in_transaction()).unwrap_or(false);
                        let _ = tx.send(ExecEvent::Finished { in_transaction: in_tx });
                        egui_ctx.request_repaint();
                    });
                    self.running = Some(rx);
                }
                Err(e) => {
                    self.log(NoticeLevel::Error, format!("Submit failed: {e}"));
                    ctx.error(format!("Submit failed: {e}"));
                }
            }
        }
    }

    fn shortcuts(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        use egui::{Key, Modifiers};
        let (run, run_script, format, save, save_as, cancel) = ui.input_mut(|i| {
            let run_script = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter);
            let run = i.consume_key(Modifiers::COMMAND, Key::Enter);
            let format = i.consume_key(Modifiers::COMMAND | Modifiers::ALT, Key::L)
                || i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::F);
            let save_as = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::S);
            let save = i.consume_key(Modifiers::COMMAND, Key::S);
            let cancel = i.consume_key(Modifiers::COMMAND, Key::F2);
            (run, run_script, format, save, save_as, cancel)
        });
        if run_script {
            self.request_run(ctx, true, None);
        } else if run {
            self.request_run(ctx, false, None);
        }
        if format {
            self.format_sql(ctx);
        }
        if save_as {
            self.save_file(ctx, true);
        } else if save {
            self.save_file(ctx, false);
        }
        if cancel && self.is_running() {
            self.cancel(ctx);
        }
    }
}

impl TabView for ConsoleTab {
    fn title(&self) -> String {
        let dirty = if self.is_dirty() { " •" } else { "" };
        format!("{}{dirty}", self.title)
    }

    fn data_source(&self) -> Option<Uuid> {
        Some(self.ds)
    }

    fn status(&self) -> Option<String> {
        let line = crate::editor::line_of(&self.editor.text, self.editor.cursor()) + 1;
        let tx = if self.in_transaction { " · transaction open" } else { "" };
        let conn = if self.session.is_connected() { "connected" } else { "not connected" };
        let ver = self.session.server_version().map(|v| format!(" · {}", truncate_line(&v, 40))).unwrap_or_default();
        Some(format!("Ln {line} · {conn}{ver}{tx}"))
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        self.poll_tasks(ctx);
        // Keep the console's session config in sync with the data source.
        if let Some(ds) = ctx.sources.get(self.ds) {
            if ds.config != self.session.config() {
                self.session.set_config(ds.config.clone());
                self.editor.dialect = ds.config.dialect();
            }
            if ds.conn.has_session_password() && !self.session.has_session_password() {
                self.session.set_session_password(ds.conn.effective_config().password);
            }
        }
        self.shortcuts(ui, ctx);
        self.toolbar(ui, ctx);
        ui.separator();

        let available = ui.available_height();
        let show_results = !self.results.is_empty() || !self.output.is_empty();
        if show_results {
            self.results_height = self.results_height.clamp(80.0, (available - 80.0).max(80.0));
            egui::TopBottomPanel::bottom(egui::Id::new(("console_results", self.id)))
                .resizable(true)
                .default_height(self.results_height)
                .height_range(80.0..=(available - 60.0).max(81.0))
                .show_inside(ui, |ui| {
                    self.results_height = ui.available_height();
                    self.results_ui(ui, ctx);
                });
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE).show_inside(ui, |ui| {
            let ds = ctx.sources.get(self.ds);
            let catalog_arc = ds.map(|d| d.catalog.clone());
            let guard = catalog_arc.as_ref().map(|c| c.lock().unwrap());
            let env = EditorEnv {
                catalog: guard.as_deref(),
                font_size: ctx.settings.editor_font_size,
                auto_completion: ctx.settings.auto_completion,
                inspections: ctx.settings.inspections,
                line_numbers: ctx.settings.show_line_numbers,
            };
            let resp = self.editor.show(ui, &env);
            drop(guard);
            if resp.changed {
                self.ensure_catalog_columns(ctx);
            }
        });
        self.dialogs(ui, ctx);
    }

    fn can_close(&mut self) -> bool {
        if self.force_close || !self.in_transaction {
            let s = self.session.clone();
            // Closing the session is fire-and-forget.
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    rt.block_on(s.disconnect());
                }
            });
            return true;
        }
        self.close_prompt = true;
        false
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// `UPDATE`/`DELETE` without a WHERE clause.
fn is_unbounded_dml(sql: &str, dialect: tabularium_core::Dialect) -> bool {
    use tabularium_sql::lexer::{tokenize, TokenKind};
    let kw = first_keyword(sql).to_ascii_uppercase();
    if kw != "UPDATE" && kw != "DELETE" {
        return false;
    }
    !tokenize(sql, dialect)
        .iter()
        .any(|t| t.kind == TokenKind::Keyword && t.text(sql).eq_ignore_ascii_case("WHERE"))
}

fn truncate_line(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n { one.chars().take(n).collect::<String>() + "…" } else { one }
}

fn plan_text(res: &StatementResult) -> String {
    if res.columns.len() == 1 {
        return res.rows.iter().map(|r| r[0].display_text().unwrap_or_default()).collect::<Vec<_>>().join("\n");
    }
    // Tabular plans (MySQL classic EXPLAIN, SQLite EXPLAIN QUERY PLAN): render as text table.
    let opts = tabularium_tools::export::ExportOptions::new(tabularium_tools::export::ExportFormat::TextTable);
    tabularium_tools::export::export_to_string(&res.columns, &res.rows, &opts).unwrap_or_default()
}

/// Plan text with cost/time highlights.
fn plan_ui(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    for line in text.lines() {
        let mut job = egui::text::LayoutJob::default();
        let font = egui::FontId::monospace(12.5);
        let (main, detail) = match line.find("  (") {
            Some(i) => line.split_at(i),
            None => (line, ""),
        };
        let node_color = if main.contains("Seq Scan") || main.contains("Full scan") || main.contains("SCAN ") {
            palette.warning
        } else if main.trim_start().starts_with("->") || main.contains("Index") || main.contains("SEARCH") {
            palette.function
        } else {
            palette.text
        };
        job.append(main, 0.0, egui::TextFormat { font_id: font.clone(), color: node_color, ..Default::default() });
        job.append(detail, 0.0, egui::TextFormat { font_id: font, color: palette.comment, ..Default::default() });
        ui.label(job);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbounded_dml() {
        let d = tabularium_core::Dialect::Postgres;
        assert!(is_unbounded_dml("delete from t", d));
        assert!(is_unbounded_dml("UPDATE t SET a = 1", d));
        assert!(!is_unbounded_dml("delete from t where id = 1", d));
        assert!(!is_unbounded_dml("select * from t", d));
        assert!(!is_unbounded_dml("update t set a = 'where'  where id=1", d));
    }
}
