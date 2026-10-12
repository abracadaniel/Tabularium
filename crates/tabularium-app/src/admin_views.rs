//! Server administration views: session manager (process list) and users & roles.

use std::time::{Duration, Instant};

use tabularium_core::{Error, RoleInfo, SessionInfo};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, TabView};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

pub struct SessionsTab {
    ds: Uuid,
    sessions: Vec<SessionInfo>,
    load: Option<Pending<Result<Vec<SessionInfo>, Error>>>,
    kill: Option<Pending<Result<(), Error>>>,
    error: Option<String>,
    auto_refresh: bool,
    last: Option<Instant>,
    filter: String,
    selected: Option<String>,
}

impl SessionsTab {
    pub fn new(_id: u64, ds: Uuid) -> Self {
        Self {
            ds,
            sessions: vec![],
            load: None,
            kill: None,
            error: None,
            auto_refresh: false,
            last: None,
            filter: String::new(),
            selected: None,
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let Some(s) = ctx.sources.get(self.ds) else { return };
        let conn = s.conn.clone();
        self.last = Some(Instant::now());
        self.load = Some(Pending::spawn(ctx.rt, ctx.egui, async move { conn.get().await?.list_sessions().await }));
    }
}

impl TabView for SessionsTab {
    fn title(&self) -> String {
        "Sessions".into()
    }

    fn data_source(&self) -> Option<Uuid> {
        Some(self.ds)
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        if self.last.is_none() {
            self.refresh(ctx);
        }
        if let Some(r) = poll_slot(&mut self.load) {
            match r {
                Ok(s) => {
                    self.sessions = s;
                    self.error = None;
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        if let Some(r) = poll_slot(&mut self.kill) {
            match r {
                Ok(()) => {
                    ctx.info("Session terminated");
                    self.refresh(ctx);
                }
                Err(e) => ctx.error(e),
            }
        }
        if self.auto_refresh && self.load.is_none() && self.last.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            self.refresh(ctx);
        }
        if self.auto_refresh {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        let palette = Palette::current(ui.ctx());
        ui.horizontal(|ui| {
            if ui.button("⟳ Refresh").clicked() {
                self.refresh(ctx);
            }
            ui.checkbox(&mut self.auto_refresh, "Auto-refresh (3 s)");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter").desired_width(200.0));
            let can_kill = self.selected.is_some() && self.kill.is_none();
            if ui.add_enabled(can_kill, egui::Button::new("Terminate session")).clicked() {
                if let Some(id) = self.selected.clone() {
                    let Some(s) = ctx.sources.get(self.ds) else { return };
                    let conn = s.conn.clone();
                    self.kill = Some(Pending::spawn(ctx.rt, ctx.egui, async move { conn.get().await?.kill_session(&id).await }));
                }
            }
            if self.load.is_some() {
                ui.spinner();
            }
            ui.weak(format!("{} sessions", self.sessions.len()));
        });
        if let Some(e) = &self.error {
            ui.colored_label(palette.error, e);
        }
        ui.separator();
        let filter = self.filter.to_lowercase();
        let rows: Vec<&SessionInfo> = self
            .sessions
            .iter()
            .filter(|s| {
                filter.is_empty()
                    || [&s.user, &s.database, &s.client, &s.state, &s.query]
                        .iter()
                        .any(|f| f.as_ref().is_some_and(|v| v.to_lowercase().contains(&filter)))
                    || s.id.contains(&filter)
            })
            .collect();
        let mut clicked = None;
        let mut open_query = None;
        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .column(egui_extras::Column::initial(70.0))
            .column(egui_extras::Column::initial(100.0))
            .column(egui_extras::Column::initial(100.0))
            .column(egui_extras::Column::initial(140.0))
            .column(egui_extras::Column::initial(80.0))
            .column(egui_extras::Column::initial(150.0))
            .column(egui_extras::Column::remainder())
            .header(22.0, |mut h| {
                for t in ["ID", "User", "Database", "Client", "State", "Started", "Query"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let s = rows[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(s.id.as_str()));
                    for v in [
                        Some(&s.id),
                        s.user.as_ref(),
                        s.database.as_ref(),
                        s.client.as_ref(),
                        s.state.as_ref(),
                        s.started.as_ref(),
                    ] {
                        row.col(|ui| {
                            ui.label(v.cloned().unwrap_or_default());
                        });
                    }
                    row.col(|ui| {
                        let q: String = s.query.clone().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
                        ui.monospace(q);
                    });
                    let r = row.response();
                    if r.clicked() {
                        clicked = Some(s.id.clone());
                    }
                    if r.double_clicked() {
                        open_query = s.query.clone();
                    }
                });
            });
        if let Some(id) = clicked {
            self.selected = Some(id);
        }
        if let Some(q) = open_query {
            ctx.act(AppAction::OpenConsole { ds: self.ds, sql: Some(q), run: false });
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

pub struct RolesTab {
    ds: Uuid,
    roles: Vec<RoleInfo>,
    load: Option<Pending<Result<Vec<RoleInfo>, Error>>>,
    error: Option<String>,
    started: bool,
}

impl RolesTab {
    pub fn new(_id: u64, ds: Uuid) -> Self {
        Self { ds, roles: vec![], load: None, error: None, started: false }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.started = true;
        let Some(s) = ctx.sources.get(self.ds) else { return };
        let conn = s.conn.clone();
        self.load = Some(Pending::spawn(ctx.rt, ctx.egui, async move { conn.get().await?.list_roles().await }));
    }
}

impl TabView for RolesTab {
    fn title(&self) -> String {
        "Users & roles".into()
    }

    fn data_source(&self) -> Option<Uuid> {
        Some(self.ds)
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        if !self.started {
            self.refresh(ctx);
        }
        if let Some(r) = poll_slot(&mut self.load) {
            match r {
                Ok(v) => {
                    self.roles = v;
                    self.error = None;
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        let palette = Palette::current(ui.ctx());
        let dialect = ctx.sources.get(self.ds).map(|d| d.config.dialect()).unwrap_or_default();
        ui.horizontal(|ui| {
            if ui.button("⟳ Refresh").clicked() {
                self.refresh(ctx);
            }
            if ui.button("New user…").on_hover_text("Open a console with a CREATE USER template").clicked() {
                let sql = match dialect {
                    tabularium_core::Dialect::Postgres => "CREATE ROLE new_user WITH LOGIN PASSWORD 'change-me';\nGRANT CONNECT ON DATABASE postgres TO new_user;\n",
                    tabularium_core::Dialect::MySql => "CREATE USER 'new_user'@'%' IDENTIFIED BY 'change-me';\nGRANT SELECT ON *.* TO 'new_user'@'%';\n",
                    tabularium_core::Dialect::MsSql => "CREATE LOGIN new_user WITH PASSWORD = 'Change-me1!';\nCREATE USER new_user FOR LOGIN new_user;\n",
                    _ => "-- this database has no users\n",
                };
                ctx.act(AppAction::OpenConsole { ds: self.ds, sql: Some(sql.into()), run: false });
            }
            if self.load.is_some() {
                ui.spinner();
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(palette.error, e);
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("roles").striped(true).num_columns(4).spacing([16.0, 4.0]).show(ui, |ui| {
                for t in ["Name", "Login", "Superuser", "Member of"] {
                    ui.strong(t);
                }
                ui.end_row();
                for r in &self.roles {
                    ui.label(&r.name);
                    ui.label(if r.can_login { "✔" } else { "" });
                    ui.label(if r.superuser { "✔" } else { "" });
                    ui.label(r.member_of.join(", "));
                    ui.end_row();
                }
            });
        });
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
