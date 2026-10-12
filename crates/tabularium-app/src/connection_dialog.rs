//! Data source properties dialog.

use tabularium_core::{ConnectionConfig, DriverKind, Error, SshAuth, SslMode};

use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Page {
    General,
    Ssh,
    Ssl,
    Options,
}

pub enum DialogResult {
    Open,
    Cancel,
    Save(ConnectionConfig),
}

pub struct ConnectionDialog {
    pub cfg: ConnectionConfig,
    is_new: bool,
    page: Page,
    password: String,
    url: String,
    url_error: Option<String>,
    test: Option<Pending<Result<String, Error>>>,
    test_result: Option<Result<String, String>>,
    ssh_auth_kind: usize,
    ssh_password: String,
    ssh_key: String,
    ssh_passphrase: String,
    options_text: String,
    schema_filter: String,
    port_text: String,
}

impl ConnectionDialog {
    pub fn new(cfg: ConnectionConfig, is_new: bool) -> Self {
        let (ssh_auth_kind, ssh_password, ssh_key, ssh_passphrase) = match &cfg.ssh.auth {
            SshAuth::Password(p) => (0, p.clone(), String::new(), String::new()),
            SshAuth::Key { path, passphrase } => {
                (1, String::new(), path.to_string_lossy().into_owned(), passphrase.clone().unwrap_or_default())
            }
            SshAuth::Agent => (2, String::new(), String::new(), String::new()),
        };
        Self {
            password: cfg.password.clone().unwrap_or_default(),
            url: cfg.url.clone().unwrap_or_default(),
            options_text: cfg.options.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"),
            schema_filter: cfg.schema_filter.join(", "),
            port_text: cfg.port.to_string(),
            cfg,
            is_new,
            page: Page::General,
            url_error: None,
            test: None,
            test_result: None,
            ssh_auth_kind,
            ssh_password,
            ssh_key,
            ssh_passphrase,
        }
    }

    /// Assemble the config from the form fields.
    fn build(&self) -> ConnectionConfig {
        let mut c = self.cfg.clone();
        c.password = if self.password.is_empty() { None } else { Some(self.password.clone()) };
        c.url = if self.url.trim().is_empty() { None } else { Some(self.url.trim().to_string()) };
        c.port = self.port_text.trim().parse().unwrap_or(c.driver.default_port());
        c.ssh.auth = match self.ssh_auth_kind {
            0 => SshAuth::Password(self.ssh_password.clone()),
            1 => SshAuth::Key {
                path: self.ssh_key.clone().into(),
                passphrase: if self.ssh_passphrase.is_empty() { None } else { Some(self.ssh_passphrase.clone()) },
            },
            _ => SshAuth::Agent,
        };
        c.options = self
            .options_text
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                if l.is_empty() || l.starts_with('#') {
                    return None;
                }
                let (k, v) = l.split_once('=').unwrap_or((l, ""));
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .collect();
        c.schema_filter =
            self.schema_filter.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        c
    }

    pub fn show(&mut self, ctx: &egui::Context, rt: &tokio::runtime::Handle) -> DialogResult {
        let mut result = DialogResult::Open;
        if let Some(r) = poll_slot(&mut self.test) {
            self.test_result = Some(r.map_err(|e| e.to_string()));
        }
        let palette = Palette::current(ctx);
        let title = if self.is_new { "New data source" } else { "Data source properties" };
        let mut open = true;
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.name).desired_width(260.0));
                    ui.label("Driver:");
                    let before = self.cfg.driver;
                    egui::ComboBox::from_id_salt("driver").selected_text(self.cfg.driver.label()).show_ui(ui, |ui| {
                        for k in tabularium_drivers::supported_drivers() {
                            ui.selectable_value(&mut self.cfg.driver, k, k.label());
                        }
                    });
                    if before != self.cfg.driver {
                        // Adopt defaults of the new driver when the old ones were untouched.
                        if self.port_text == before.default_port().to_string() {
                            self.port_text = self.cfg.driver.default_port().to_string();
                        }
                        if self.cfg.user == before.default_user() {
                            self.cfg.user = self.cfg.driver.default_user().to_string();
                        }
                        if self.cfg.name == before.label() {
                            self.cfg.name = self.cfg.driver.label().to_string();
                        }
                        if self.cfg.driver.is_file_based() {
                            self.cfg.host.clear();
                        } else if self.cfg.host.is_empty() {
                            self.cfg.host = "localhost".into();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    for (p, label) in [(Page::General, "General"), (Page::Ssh, "SSH"), (Page::Ssl, "SSL"), (Page::Options, "Options")] {
                        if self.cfg.driver.is_file_based() && matches!(p, Page::Ssh | Page::Ssl) {
                            continue;
                        }
                        ui.selectable_value(&mut self.page, p, label);
                    }
                });
                ui.separator();
                match self.page {
                    Page::General => self.general_ui(ui, &palette),
                    Page::Ssh => self.ssh_ui(ui),
                    Page::Ssl => self.ssl_ui(ui),
                    Page::Options => self.options_ui(ui),
                }
                ui.separator();
                ui.horizontal(|ui| {
                    let testing = self.test.is_some();
                    if ui.add_enabled(!testing, egui::Button::new("Test Connection")).clicked() {
                        let cfg = self.build();
                        self.test_result = None;
                        self.test = Some(Pending::spawn(rt, ctx, async move { tabularium_drivers::test_connection(&cfg).await }));
                    }
                    if testing {
                        ui.spinner();
                    }
                    match &self.test_result {
                        Some(Ok(v)) => {
                            ui.colored_label(palette.success, format!("✔ {}", truncate(v, 60))).on_hover_text(v);
                        }
                        Some(Err(e)) => {
                            ui.colored_label(palette.error, format!("✖ {}", truncate(e, 60))).on_hover_text(e);
                        }
                        None => {}
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            result = DialogResult::Cancel;
                        }
                        if ui.button(egui::RichText::new("OK").strong()).clicked() {
                            result = DialogResult::Save(self.build());
                        }
                    });
                });
            });
        if !open {
            result = DialogResult::Cancel;
        }
        result
    }

    fn general_ui(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        egui::Grid::new("conn_general").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            if self.cfg.driver.is_file_based() {
                ui.label("File:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.database).hint_text("path/to/database.db or :memory:").desired_width(320.0));
                    if ui.button("…").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("SQLite", &["db", "sqlite", "sqlite3", "db3"])
                            .add_filter("All files", &["*"])
                            .pick_file()
                        {
                            self.cfg.database = p.to_string_lossy().into_owned();
                            if self.cfg.name == DriverKind::Sqlite.label() {
                                self.cfg.name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            }
                        }
                    }
                    if ui.button("New…").on_hover_text("Create a new database file").clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("SQLite", &["db", "sqlite"]).set_file_name("database.db").save_file() {
                            self.cfg.database = p.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.end_row();
            } else {
                ui.label("Host:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.host).desired_width(260.0));
                    ui.label("Port:");
                    ui.add(egui::TextEdit::singleline(&mut self.port_text).desired_width(60.0));
                });
                ui.end_row();
                ui.label("User:");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.user).desired_width(260.0));
                ui.end_row();
                ui.label("Password:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(200.0));
                    ui.checkbox(&mut self.cfg.save_password, "Save password");
                });
                ui.end_row();
                ui.label("Database:");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.database).desired_width(260.0));
                ui.end_row();
            }
            ui.label("URL:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.url).hint_text("optional: overrides the fields above").desired_width(320.0));
                if ui.button("Parse").on_hover_text("Fill the fields from the URL").clicked() {
                    match ConnectionConfig::from_url(self.url.trim()) {
                        Ok(parsed) => {
                            let keep_id = self.cfg.id;
                            let keep_name = self.cfg.name.clone();
                            self.cfg = ConnectionConfig { id: keep_id, name: keep_name, ..parsed };
                            self.password = self.cfg.password.clone().unwrap_or_default();
                            self.port_text = self.cfg.port.to_string();
                            self.url.clear();
                            self.url_error = None;
                        }
                        Err(e) => self.url_error = Some(e.to_string()),
                    }
                }
            });
            ui.end_row();
            if let Some(e) = &self.url_error {
                ui.label("");
                ui.colored_label(palette.error, e);
                ui.end_row();
            }
            ui.label("Comment:");
            let mut comment = self.cfg.comment.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut comment).desired_width(320.0)).changed() {
                self.cfg.comment = if comment.is_empty() { None } else { Some(comment) };
            }
            ui.end_row();
        });
    }

    fn ssh_ui(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.cfg.ssh.enabled, "Use SSH tunnel");
        ui.add_enabled_ui(self.cfg.ssh.enabled, |ui| {
            egui::Grid::new("conn_ssh").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Proxy host:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.ssh.host).desired_width(220.0));
                    ui.label("Port:");
                    let mut port = self.cfg.ssh.port.to_string();
                    if ui.add(egui::TextEdit::singleline(&mut port).desired_width(50.0)).changed() {
                        self.cfg.ssh.port = port.parse().unwrap_or(22);
                    }
                });
                ui.end_row();
                ui.label("Proxy user:");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.ssh.user).desired_width(220.0));
                ui.end_row();
                ui.label("Auth type:");
                egui::ComboBox::from_id_salt("ssh_auth")
                    .selected_text(["Password", "Key pair", "SSH agent"][self.ssh_auth_kind])
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.ssh_auth_kind, 0, "Password");
                        ui.selectable_value(&mut self.ssh_auth_kind, 1, "Key pair");
                        ui.selectable_value(&mut self.ssh_auth_kind, 2, "SSH agent");
                    });
                ui.end_row();
                match self.ssh_auth_kind {
                    0 => {
                        ui.label("Password:");
                        ui.add(egui::TextEdit::singleline(&mut self.ssh_password).password(true));
                        ui.end_row();
                    }
                    1 => {
                        ui.label("Private key:");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.ssh_key).desired_width(240.0));
                            if ui.button("…").clicked() {
                                if let Some(p) = rfd::FileDialog::new().pick_file() {
                                    self.ssh_key = p.to_string_lossy().into_owned();
                                }
                            }
                        });
                        ui.end_row();
                        ui.label("Passphrase:");
                        ui.add(egui::TextEdit::singleline(&mut self.ssh_passphrase).password(true));
                        ui.end_row();
                    }
                    _ => {}
                }
            });
        });
    }

    fn ssl_ui(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("conn_ssl").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Mode:");
            egui::ComboBox::from_id_salt("ssl_mode").selected_text(self.cfg.ssl.mode.label()).show_ui(ui, |ui| {
                for m in SslMode::ALL {
                    ui.selectable_value(&mut self.cfg.ssl.mode, m, m.label());
                }
            });
            ui.end_row();
            for (label, field) in [
                ("CA file:", &mut self.cfg.ssl.ca_cert),
                ("Client certificate:", &mut self.cfg.ssl.client_cert),
                ("Client key:", &mut self.cfg.ssl.client_key),
            ] {
                ui.label(label);
                ui.horizontal(|ui| {
                    let mut text = field.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                    if ui.add(egui::TextEdit::singleline(&mut text).desired_width(260.0)).changed() {
                        *field = if text.is_empty() { None } else { Some(text.into()) };
                    }
                    if ui.button("…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            *field = Some(p);
                        }
                    }
                });
                ui.end_row();
            }
        });
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("conn_options").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Behaviour:");
            ui.vertical(|ui| {
                ui.checkbox(&mut self.cfg.read_only, "Read-only (reject modifying statements)");
                ui.checkbox(&mut self.cfg.auto_commit, "Auto-commit by default");
                ui.checkbox(&mut self.cfg.show_system_objects, "Show system schemas and objects");
            });
            ui.end_row();
            ui.label("Folder:");
            let mut group = self.cfg.group.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut group).hint_text("e.g. Production/EU").desired_width(240.0)).changed() {
                self.cfg.group = if group.trim().is_empty() { None } else { Some(group.trim().to_string()) };
            }
            ui.end_row();
            ui.label("Color:");
            ui.horizontal(|ui| {
                let mut has = self.cfg.color.is_some();
                ui.checkbox(&mut has, "");
                if has {
                    let mut c = self.cfg.color.unwrap_or([200, 60, 60]);
                    ui.color_edit_button_srgb(&mut c);
                    self.cfg.color = Some(c);
                    for (name, rgb) in [("prod", [200, 60, 60]), ("test", [220, 160, 40]), ("dev", [60, 160, 80])] {
                        if ui.small_button(name).clicked() {
                            self.cfg.color = Some(rgb);
                        }
                    }
                } else {
                    self.cfg.color = None;
                }
            });
            ui.end_row();
            ui.label("Schemas:");
            ui.add(egui::TextEdit::singleline(&mut self.schema_filter).hint_text("comma separated; empty = all").desired_width(240.0));
            ui.end_row();
            ui.label("Connect timeout (s):");
            let mut t = self.cfg.connect_timeout_secs.to_string();
            if ui.add(egui::TextEdit::singleline(&mut t).desired_width(50.0)).changed() {
                self.cfg.connect_timeout_secs = t.parse().unwrap_or(15);
            }
            ui.end_row();
            ui.label("Startup SQL:");
            let mut init = self.cfg.init_sql.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::multiline(&mut init).code_editor().desired_rows(3).desired_width(320.0)).changed() {
                self.cfg.init_sql = if init.trim().is_empty() { None } else { Some(init) };
            }
            ui.end_row();
            ui.label("Driver options:");
            ui.add(egui::TextEdit::multiline(&mut self.options_text).code_editor().hint_text("key=value per line").desired_rows(3).desired_width(320.0));
            ui.end_row();
        });
    }
}

fn truncate(s: &str, n: usize) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() > n { one.chars().take(n).collect::<String>() + "…" } else { one }
}
