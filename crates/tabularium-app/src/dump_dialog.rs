//! Dump and restore through the databases' native tools (`pg_dump`, `pg_restore`,
//! `psql`, `mysqldump`, `mysql`). SQLite uses `VACUUM INTO` and needs no tools.

use std::path::PathBuf;
use std::process::Stdio;

use tabularium_core::{driver::ExecOptions, ConnectionConfig, Dialect, DriverKind};
use uuid::Uuid;

use crate::ctx::{Ctx, Dialog, NoticeLevel};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Mode {
    Dump,
    Restore,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Content {
    Everything,
    SchemaOnly,
    DataOnly,
}

pub struct DumpDialog {
    ds: Uuid,
    mode: Mode,
    path: String,
    content: Content,
    custom_format: bool,
    schema: String,
    tool_dir: String,
    task: Option<Pending<Result<String, String>>>,
    error: Option<String>,
    log: String,
}

impl DumpDialog {
    pub fn new(ds: Uuid, mode: Mode) -> Self {
        Self {
            ds,
            mode,
            path: String::new(),
            content: Content::Everything,
            custom_format: false,
            schema: String::new(),
            tool_dir: String::new(),
            task: None,
            error: None,
            log: String::new(),
        }
    }

    fn tool(&self, name: &str) -> PathBuf {
        let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
        if self.tool_dir.trim().is_empty() {
            PathBuf::from(exe)
        } else {
            PathBuf::from(self.tool_dir.trim()).join(exe)
        }
    }

    /// Program, arguments and environment for the external tool.
    fn command(&self, cfg: &ConnectionConfig) -> Result<(PathBuf, Vec<String>, Vec<(String, String)>), String> {
        let mut env = vec![];
        let path = self.path.trim().to_string();
        match cfg.driver.dialect() {
            Dialect::Postgres => {
                if let Some(pw) = cfg.password.clone().filter(|p| !p.is_empty()) {
                    env.push(("PGPASSWORD".into(), pw));
                }
                env.push(("PGSSLMODE".into(), cfg.ssl.mode.label().to_string()));
                let mut args = vec![
                    "-h".into(),
                    cfg.host.clone(),
                    "-p".into(),
                    cfg.port.to_string(),
                    "-U".into(),
                    cfg.user.clone(),
                ];
                match self.mode {
                    Mode::Dump => {
                        args.extend(["-d".into(), cfg.database.clone(), "-f".into(), path]);
                        if self.custom_format {
                            args.push("-Fc".into());
                        }
                        match self.content {
                            Content::SchemaOnly => args.push("--schema-only".into()),
                            Content::DataOnly => args.push("--data-only".into()),
                            Content::Everything => {}
                        }
                        if !self.schema.trim().is_empty() {
                            args.extend(["-n".into(), self.schema.trim().into()]);
                        }
                        Ok((self.tool("pg_dump"), args, env))
                    }
                    Mode::Restore => {
                        if self.custom_format || path.ends_with(".dump") || path.ends_with(".backup") {
                            args.extend(["-d".into(), cfg.database.clone(), "--no-owner".into(), path]);
                            Ok((self.tool("pg_restore"), args, env))
                        } else {
                            args.extend(["-d".into(), cfg.database.clone(), "-v".into(), "ON_ERROR_STOP=1".into(), "-f".into(), path]);
                            Ok((self.tool("psql"), args, env))
                        }
                    }
                }
            }
            Dialect::MySql => {
                if let Some(pw) = cfg.password.clone().filter(|p| !p.is_empty()) {
                    env.push(("MYSQL_PWD".into(), pw));
                }
                let mut args = vec![
                    format!("--host={}", cfg.host),
                    format!("--port={}", cfg.port),
                    format!("--user={}", cfg.user),
                ];
                match self.mode {
                    Mode::Dump => {
                        args.push(format!("--result-file={path}"));
                        args.extend(["--routines".into(), "--triggers".into(), "--events".into(), "--single-transaction".into()]);
                        match self.content {
                            Content::SchemaOnly => args.push("--no-data".into()),
                            Content::DataOnly => args.push("--no-create-info".into()),
                            Content::Everything => {}
                        }
                        let db = if self.schema.trim().is_empty() { cfg.database.clone() } else { self.schema.trim().to_string() };
                        if db.is_empty() {
                            return Err("Choose a database to dump".into());
                        }
                        args.push(db);
                        Ok((self.tool("mysqldump"), args, env))
                    }
                    Mode::Restore => {
                        args.push(cfg.database.clone());
                        // Input is fed through stdin (see run()).
                        Ok((self.tool("mysql"), args, env))
                    }
                }
            }
            _ => Err("No native dump tool for this database".into()),
        }
    }

    fn start(&mut self, ctx: &mut Ctx) {
        self.error = None;
        self.log.clear();
        let Some(src) = ctx.sources.get(self.ds) else { return };
        let cfg = src.conn.effective_config();
        if self.path.trim().is_empty() {
            self.error = Some("Choose a file".into());
            return;
        }
        if cfg.ssh.enabled {
            self.error = Some("Native tools can't use the SSH tunnel; dump/restore through a direct connection".into());
            return;
        }
        let path = PathBuf::from(self.path.trim());
        if cfg.driver == DriverKind::Sqlite {
            let conn = src.conn.clone();
            let mode = self.mode;
            self.task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
                let c = conn.get().await.map_err(|e| e.to_string())?;
                match mode {
                    Mode::Dump => {
                        let target = path.to_string_lossy().replace('\'', "''");
                        c.execute(&format!("VACUUM INTO '{target}'"), &ExecOptions::default()).await.map_err(|e| e.to_string())?;
                        Ok(format!("Database copied to {}", path.display()))
                    }
                    Mode::Restore => {
                        let script = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                        let stmts = tabularium_sql::split::split_statements(&script, Dialect::Sqlite);
                        let n = stmts.len();
                        for s in stmts {
                            c.execute(&s.text, &ExecOptions::default()).await.map_err(|e| format!("{e}\n{}", s.text))?;
                        }
                        Ok(format!("Executed {n} statements from {}", path.display()))
                    }
                }
            }));
            return;
        }
        let (program, args, env) = match self.command(&cfg) {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.log = format!("$ {} {}\n", program.display(), args.join(" "));
        let stdin_file = (self.mode == Mode::Restore && cfg.driver.dialect() == Dialect::MySql).then(|| path.clone());
        self.task = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let mut cmd = tokio::process::Command::new(&program);
            cmd.args(&args).envs(env).stdout(Stdio::piped()).stderr(Stdio::piped());
            if let Some(f) = &stdin_file {
                let file = std::fs::File::open(f).map_err(|e| e.to_string())?;
                cmd.stdin(Stdio::from(file));
            }
            let out = cmd
                .output()
                .await
                .map_err(|e| format!("Could not run {}: {e}. Install the client tools or set their folder.", program.display()))?;
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if out.status.success() {
                Ok(if stderr.trim().is_empty() { "Completed".into() } else { stderr })
            } else {
                Err(format!("{} exited with {}\n{stderr}", program.display(), out.status))
            }
        }));
    }
}

impl Dialog for DumpDialog {
    fn show(&mut self, ctx: &mut Ctx) -> bool {
        if let Some(r) = poll_slot(&mut self.task) {
            match r {
                Ok(msg) => {
                    let verb = if self.mode == Mode::Dump { "Dump" } else { "Restore" };
                    ctx.notify(NoticeLevel::Success, format!("{verb} finished"));
                    self.log.push_str(&msg);
                }
                Err(e) => self.error = Some(e),
            }
        }
        let Some(src) = ctx.sources.get(self.ds) else { return false };
        let driver = src.config.driver;
        let name = src.config.name.clone();
        let palette = Palette::current(ctx.egui);
        let egui_ctx = ctx.egui.clone();
        let mut open = true;
        let mut go = false;
        let title = if self.mode == Mode::Dump { format!("Dump {name}") } else { format!("Restore into {name}") };
        egui::Window::new(title).open(&mut open).collapsible(false).default_width(520.0).show(&egui_ctx, |ui| {
            egui::Grid::new("dump_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("File:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(320.0));
                    if ui.button("…").clicked() {
                        let dialog = rfd::FileDialog::new();
                        let picked = if self.mode == Mode::Dump {
                            let ext = if driver == DriverKind::Sqlite { "db" } else if self.custom_format { "dump" } else { "sql" };
                            dialog.set_file_name(format!("{name}.{ext}")).save_file()
                        } else {
                            dialog.pick_file()
                        };
                        if let Some(p) = picked {
                            self.path = p.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.end_row();
                if driver != DriverKind::Sqlite {
                    if self.mode == Mode::Dump {
                        ui.label("Content:");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.content, Content::Everything, "Schema and data");
                            ui.radio_value(&mut self.content, Content::SchemaOnly, "Schema only");
                            ui.radio_value(&mut self.content, Content::DataOnly, "Data only");
                        });
                        ui.end_row();
                        ui.label(if driver.dialect() == Dialect::MySql { "Database:" } else { "Schema:" });
                        ui.add(egui::TextEdit::singleline(&mut self.schema).hint_text("all / connection default"));
                        ui.end_row();
                    }
                    if driver.dialect() == Dialect::Postgres {
                        ui.label("");
                        ui.checkbox(&mut self.custom_format, "Custom (pg_dump -Fc) format");
                        ui.end_row();
                    }
                    ui.label("Tools folder:");
                    ui.add(egui::TextEdit::singleline(&mut self.tool_dir).hint_text("use PATH"));
                    ui.end_row();
                } else if self.mode == Mode::Restore {
                    ui.label("");
                    ui.weak("Runs the SQL script against the database.");
                    ui.end_row();
                }
            });
            if let Some(e) = &self.error {
                ui.colored_label(palette.error, e);
            }
            if !self.log.is_empty() {
                egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                    ui.monospace(&self.log);
                });
            }
            ui.horizontal(|ui| {
                let busy = self.task.is_some();
                let label = if self.mode == Mode::Dump { "Dump" } else { "Restore" };
                if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                    go = true;
                }
                if busy {
                    ui.spinner();
                }
            });
        });
        if go {
            self.start(ctx);
        }
        open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_dump_command_passes_password_via_env() {
        let mut cfg = ConnectionConfig::from_url("postgres://bob:secret@db:5433/shop").unwrap();
        cfg.password = Some("secret".into());
        let mut d = DumpDialog::new(Uuid::nil(), Mode::Dump);
        d.path = "/tmp/x.sql".into();
        d.content = Content::SchemaOnly;
        let (prog, args, env) = d.command(&cfg).unwrap();
        assert!(prog.to_string_lossy().starts_with("pg_dump"));
        assert!(args.contains(&"--schema-only".to_string()));
        assert!(!args.iter().any(|a| a.contains("secret")));
        assert!(env.contains(&("PGPASSWORD".to_string(), "secret".to_string())));
    }

    #[test]
    fn mysql_dump_needs_database() {
        let cfg = ConnectionConfig::from_url("mysql://root@localhost").unwrap();
        let mut d = DumpDialog::new(Uuid::nil(), Mode::Dump);
        d.path = "x.sql".into();
        assert!(d.command(&cfg).is_err());
        d.schema = "app".into();
        let (_, args, _) = d.command(&cfg).unwrap();
        assert_eq!(args.last().unwrap(), "app");
    }
}
