//! User preferences, persisted as `settings.json` in the config directory.

use serde::{Deserialize, Serialize};
use tabularium_core::config::write_atomic;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    Dark,
    Light,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeywordCaseChoice {
    Upper,
    Lower,
    Preserve,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub ui_scale: f32,
    pub editor_font_size: f32,
    pub grid_font_size: f32,
    /// Rows fetched per page in the table data editor.
    pub page_size: u64,
    /// Max rows fetched by console queries.
    pub console_max_rows: usize,
    pub auto_completion: bool,
    pub inspections: bool,
    pub show_line_numbers: bool,
    pub format_indent: usize,
    pub format_keyword_case: KeywordCaseChoice,
    pub format_comma_first: bool,
    /// Ask before running UPDATE/DELETE without WHERE.
    pub confirm_dangerous_statements: bool,
    /// Text shown for NULL cells.
    pub null_text: String,
    pub record_history: bool,
    pub restore_consoles: bool,
    pub recent_files: Vec<String>,
    /// Max characters shown in a grid cell before truncating.
    pub max_cell_chars: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            ui_scale: 1.0,
            editor_font_size: 13.5,
            grid_font_size: 12.5,
            page_size: 500,
            console_max_rows: 500,
            auto_completion: true,
            inspections: true,
            show_line_numbers: true,
            format_indent: 4,
            format_keyword_case: KeywordCaseChoice::Upper,
            format_comma_first: false,
            confirm_dangerous_statements: true,
            null_text: "<null>".into(),
            record_history: true,
            restore_consoles: true,
            recent_files: vec![],
            max_cell_chars: 200,
        }
    }
}

impl Settings {
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) {
        match serde_json::to_vec_pretty(self) {
            Ok(data) => {
                if let Err(e) = write_atomic(path, &data) {
                    log::error!("saving settings: {e}");
                }
            }
            Err(e) => log::error!("serializing settings: {e}"),
        }
    }

    pub fn format_options(&self) -> tabularium_sql::format::FormatOptions {
        use tabularium_sql::format::{FormatOptions, KeywordCase};
        FormatOptions {
            indent: self.format_indent,
            keyword_case: match self.format_keyword_case {
                KeywordCaseChoice::Upper => KeywordCase::Upper,
                KeywordCaseChoice::Lower => KeywordCase::Lower,
                KeywordCaseChoice::Preserve => KeywordCase::Preserve,
            },
            comma_first: self.format_comma_first,
            ..FormatOptions::default()
        }
    }

    pub fn add_recent_file(&mut self, path: &str) {
        self.recent_files.retain(|p| p != path);
        self.recent_files.insert(0, path.to_string());
        self.recent_files.truncate(15);
    }
}
