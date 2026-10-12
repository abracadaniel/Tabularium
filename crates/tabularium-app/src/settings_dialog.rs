//! Preferences window.

use crate::settings::{KeywordCaseChoice, Settings, ThemeChoice};

pub struct SettingsDialog {
    draft: Settings,
}

impl SettingsDialog {
    pub fn new(current: &Settings) -> Self {
        Self { draft: current.clone() }
    }

    /// Returns `false` when closed. Changes apply live on "Apply"/"OK".
    pub fn show(&mut self, ctx: &egui::Context, settings: &mut Settings) -> bool {
        let mut open = true;
        let mut close = false;
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(true).default_width(460.0).show(ctx, |ui| {
            let d = &mut self.draft;
            egui::Grid::new("settings_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.strong("Appearance");
                ui.end_row();
                ui.label("Theme");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut d.theme, ThemeChoice::Dark, "Dark");
                    ui.selectable_value(&mut d.theme, ThemeChoice::Light, "Light");
                    ui.selectable_value(&mut d.theme, ThemeChoice::System, "System");
                });
                ui.end_row();
                ui.label("UI scale");
                ui.add(egui::Slider::new(&mut d.ui_scale, 0.6..=2.5).step_by(0.05));
                ui.end_row();
                ui.label("Editor font size");
                ui.add(egui::Slider::new(&mut d.editor_font_size, 9.0..=28.0).step_by(0.5));
                ui.end_row();
                ui.label("Grid font size");
                ui.add(egui::Slider::new(&mut d.grid_font_size, 9.0..=24.0).step_by(0.5));
                ui.end_row();

                ui.strong("Editor");
                ui.end_row();
                ui.label("");
                ui.vertical(|ui| {
                    ui.checkbox(&mut d.auto_completion, "Show completion automatically while typing");
                    ui.checkbox(&mut d.inspections, "Run inspections (syntax errors, unresolved objects…)");
                    ui.checkbox(&mut d.show_line_numbers, "Show line numbers");
                    ui.checkbox(&mut d.restore_consoles, "Restore consoles on startup");
                });
                ui.end_row();
                ui.label("Formatter");
                ui.horizontal(|ui| {
                    ui.label("indent");
                    ui.add(egui::DragValue::new(&mut d.format_indent).range(1..=8));
                    egui::ComboBox::from_id_salt("kwcase")
                        .selected_text(match d.format_keyword_case {
                            KeywordCaseChoice::Upper => "UPPER keywords",
                            KeywordCaseChoice::Lower => "lower keywords",
                            KeywordCaseChoice::Preserve => "keep case",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut d.format_keyword_case, KeywordCaseChoice::Upper, "UPPER keywords");
                            ui.selectable_value(&mut d.format_keyword_case, KeywordCaseChoice::Lower, "lower keywords");
                            ui.selectable_value(&mut d.format_keyword_case, KeywordCaseChoice::Preserve, "keep case");
                        });
                    ui.checkbox(&mut d.format_comma_first, "comma first");
                });
                ui.end_row();

                ui.strong("Data");
                ui.end_row();
                ui.label("Table page size");
                ui.add(egui::DragValue::new(&mut d.page_size).range(1..=100_000));
                ui.end_row();
                ui.label("Console max rows");
                ui.add(egui::DragValue::new(&mut d.console_max_rows).range(1..=1_000_000));
                ui.end_row();
                ui.label("NULL text");
                ui.text_edit_singleline(&mut d.null_text);
                ui.end_row();
                ui.label("Max cell characters");
                ui.add(egui::DragValue::new(&mut d.max_cell_chars).range(10..=10_000));
                ui.end_row();
                ui.label("");
                ui.vertical(|ui| {
                    ui.checkbox(&mut d.confirm_dangerous_statements, "Confirm UPDATE/DELETE without WHERE");
                    ui.checkbox(&mut d.record_history, "Record query history");
                });
                ui.end_row();
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    *settings = self.draft.clone();
                    close = true;
                }
                if ui.button("Apply").clicked() {
                    *settings = self.draft.clone();
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                if ui.button("Restore defaults").clicked() {
                    let recent = self.draft.recent_files.clone();
                    self.draft = Settings { recent_files: recent, ..Settings::default() };
                }
            });
        });
        open && !close
    }
}
