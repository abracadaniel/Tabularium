//! Read-only SQL view: object DDL loaded from the server, or generated text.

use tabularium_core::{Dialect, Error, ObjectKind, ObjectRef};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, TabView};
use crate::editor::{CodeEditor, EditorEnv};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

pub struct TextTab {
    title: String,
    ds: Option<Uuid>,
    object: Option<(ObjectRef, Option<String>)>,
    editor: CodeEditor,
    loading: Option<Pending<Result<String, Error>>>,
    error: Option<String>,
    started: bool,
}

impl TextTab {
    /// DDL of a database object, fetched from the server.
    pub fn ddl(id: u64, ds: Uuid, object: ObjectRef, signature: Option<String>, dialect: Dialect) -> Self {
        Self {
            title: format!("{} [DDL]", object.name),
            ds: Some(ds),
            object: Some((object, signature)),
            editor: CodeEditor::new(("text", id), String::new(), dialect).read_only(),
            loading: None,
            error: None,
            started: false,
        }
    }

    /// Static text (generated SQL, diffs, ...).
    pub fn text(id: u64, title: String, text: String, ds: Option<Uuid>, dialect: Dialect) -> Self {
        Self {
            title,
            ds,
            object: None,
            editor: CodeEditor::new(("text", id), text, dialect).read_only(),
            loading: None,
            error: None,
            started: true,
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.started = true;
        let (Some(ds), Some((object, signature))) = (self.ds, self.object.clone()) else { return };
        let Some(source) = ctx.sources.get(ds) else { return };
        let conn = source.conn.clone();
        self.error = None;
        self.loading = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await?;
            match object.kind {
                ObjectKind::Function | ObjectKind::Procedure => {
                    Ok(c.describe_routine(&object, signature.as_deref()).await?.definition)
                }
                _ => c.object_ddl(&object).await,
            }
        }));
    }
}

impl TabView for TextTab {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn data_source(&self) -> Option<Uuid> {
        self.ds
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        if !self.started {
            self.load(ctx);
        }
        if let Some(r) = poll_slot(&mut self.loading) {
            match r {
                Ok(t) => self.editor.set_text(t),
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Copy").clicked() {
                ctx.act(AppAction::CopyToClipboard(self.editor.text.clone()));
            }
            if let Some(ds) = self.ds {
                if ui.button("Open in console").clicked() {
                    ctx.act(AppAction::OpenConsole { ds, sql: Some(self.editor.text.clone()), run: false });
                }
            }
            if self.object.is_some() && ui.button("⟳ Refresh").clicked() {
                self.load(ctx);
            }
            if ui.button("Save as…").clicked() {
                if let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).set_file_name("ddl.sql").save_file() {
                    match std::fs::write(&path, &self.editor.text) {
                        Ok(()) => ctx.info(format!("Saved {}", path.display())),
                        Err(e) => ctx.error(e),
                    }
                }
            }
            if self.loading.is_some() {
                ui.spinner();
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(Palette::current(ui.ctx()).error, e);
        }
        ui.separator();
        let env = EditorEnv {
            catalog: None,
            font_size: ctx.settings.editor_font_size,
            auto_completion: false,
            inspections: false,
            line_numbers: ctx.settings.show_line_numbers,
        };
        self.editor.show(ui, &env);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
