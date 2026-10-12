//! Colors and visuals.

use egui::{Color32, Visuals};
use tabularium_sql::lexer::TokenKind;

use crate::settings::{Settings, ThemeChoice};

#[derive(Clone, Copy)]
pub struct Palette {
    pub keyword: Color32,
    pub identifier: Color32,
    pub quoted: Color32,
    pub string: Color32,
    pub number: Color32,
    pub comment: Color32,
    pub operator: Color32,
    pub parameter: Color32,
    pub function: Color32,
    pub type_name: Color32,
    pub text: Color32,
    pub error: Color32,
    pub warning: Color32,
    pub info: Color32,
    pub null: Color32,
    pub current_statement: Color32,
    pub search_match: Color32,
    pub modified_cell: Color32,
    pub inserted_row: Color32,
    pub deleted_row: Color32,
    pub selection: Color32,
    pub gutter: Color32,
    pub success: Color32,
}

impl Palette {
    pub fn for_dark(dark: bool) -> Self {
        if dark {
            Palette {
                keyword: Color32::from_rgb(0xCC, 0x78, 0x32),
                identifier: Color32::from_rgb(0xA9, 0xB7, 0xC6),
                quoted: Color32::from_rgb(0x98, 0x76, 0xAA),
                string: Color32::from_rgb(0x6A, 0x87, 0x59),
                number: Color32::from_rgb(0x68, 0x97, 0xBB),
                comment: Color32::from_rgb(0x80, 0x80, 0x80),
                operator: Color32::from_rgb(0xA9, 0xB7, 0xC6),
                parameter: Color32::from_rgb(0xD0, 0xD0, 0xFF),
                function: Color32::from_rgb(0xFF, 0xC6, 0x6D),
                type_name: Color32::from_rgb(0xCC, 0x78, 0x32),
                text: Color32::from_rgb(0xA9, 0xB7, 0xC6),
                error: Color32::from_rgb(0xFF, 0x55, 0x55),
                warning: Color32::from_rgb(0xE0, 0xB0, 0x40),
                info: Color32::from_rgb(0x80, 0x90, 0xA0),
                null: Color32::from_rgb(0x70, 0x70, 0x70),
                current_statement: Color32::from_rgba_unmultiplied(0x40, 0x48, 0x58, 0x50),
                search_match: Color32::from_rgba_unmultiplied(0x32, 0x59, 0x3D, 0xC0),
                modified_cell: Color32::from_rgba_unmultiplied(0x3A, 0x4A, 0x7A, 0xC0),
                inserted_row: Color32::from_rgba_unmultiplied(0x2E, 0x5A, 0x2E, 0xA0),
                deleted_row: Color32::from_rgba_unmultiplied(0x6A, 0x2A, 0x2A, 0xA0),
                selection: Color32::from_rgba_unmultiplied(0x21, 0x42, 0x83, 0xC0),
                gutter: Color32::from_rgb(0x60, 0x66, 0x6E),
                success: Color32::from_rgb(0x60, 0xC0, 0x60),
            }
        } else {
            Palette {
                keyword: Color32::from_rgb(0x00, 0x00, 0x91),
                identifier: Color32::from_rgb(0x10, 0x10, 0x10),
                quoted: Color32::from_rgb(0x66, 0x0E, 0x7A),
                string: Color32::from_rgb(0x06, 0x7D, 0x17),
                number: Color32::from_rgb(0x17, 0x50, 0xEB),
                comment: Color32::from_rgb(0x8C, 0x8C, 0x8C),
                operator: Color32::from_rgb(0x20, 0x20, 0x20),
                parameter: Color32::from_rgb(0x87, 0x10, 0x94),
                function: Color32::from_rgb(0x00, 0x62, 0x7A),
                type_name: Color32::from_rgb(0x00, 0x00, 0x91),
                text: Color32::from_rgb(0x10, 0x10, 0x10),
                error: Color32::from_rgb(0xD0, 0x10, 0x10),
                warning: Color32::from_rgb(0xB0, 0x80, 0x00),
                info: Color32::from_rgb(0x70, 0x80, 0x90),
                null: Color32::from_rgb(0xA0, 0xA0, 0xA0),
                current_statement: Color32::from_rgba_unmultiplied(0xE8, 0xF0, 0xFF, 0x90),
                search_match: Color32::from_rgba_unmultiplied(0xFF, 0xE0, 0x80, 0xC0),
                modified_cell: Color32::from_rgba_unmultiplied(0xC8, 0xD8, 0xFF, 0xC0),
                inserted_row: Color32::from_rgba_unmultiplied(0xD0, 0xF5, 0xD0, 0xC0),
                deleted_row: Color32::from_rgba_unmultiplied(0xFF, 0xD0, 0xD0, 0xC0),
                selection: Color32::from_rgba_unmultiplied(0xA6, 0xD2, 0xFF, 0xC0),
                gutter: Color32::from_rgb(0xA0, 0xA0, 0xA0),
                success: Color32::from_rgb(0x20, 0x90, 0x20),
            }
        }
    }

    pub fn current(ctx: &egui::Context) -> Self {
        Self::for_dark(ctx.style().visuals.dark_mode)
    }

    pub fn token(&self, kind: TokenKind) -> Color32 {
        match kind {
            TokenKind::Keyword => self.keyword,
            TokenKind::Identifier => self.identifier,
            TokenKind::QuotedIdentifier => self.quoted,
            TokenKind::String => self.string,
            TokenKind::Number => self.number,
            TokenKind::Comment => self.comment,
            TokenKind::Operator | TokenKind::Punctuation => self.operator,
            TokenKind::Parameter => self.parameter,
            TokenKind::Function => self.function,
            TokenKind::Type => self.type_name,
            TokenKind::Whitespace | TokenKind::Unknown => self.text,
        }
    }
}

pub fn apply(ctx: &egui::Context, settings: &Settings) {
    let dark = match settings.theme {
        ThemeChoice::Dark => true,
        ThemeChoice::Light => false,
        ThemeChoice::System => ctx.system_theme().map(|t| t == egui::Theme::Dark).unwrap_or(true),
    };
    let mut visuals = if dark { Visuals::dark() } else { Visuals::light() };
    visuals.striped = true;
    visuals.indent_has_left_vline = true;
    if dark {
        visuals.panel_fill = Color32::from_rgb(0x2B, 0x2D, 0x30);
        visuals.window_fill = Color32::from_rgb(0x2B, 0x2D, 0x30);
        visuals.extreme_bg_color = Color32::from_rgb(0x1E, 0x1F, 0x22);
        visuals.faint_bg_color = Color32::from_rgb(0x32, 0x34, 0x38);
    }
    ctx.set_visuals(visuals);
    ctx.set_zoom_factor(settings.ui_scale.clamp(0.5, 3.0));
}

/// Subtle tint of a data source color for backgrounds.
pub fn tint(color: Color32, dark: bool) -> Color32 {
    let a = if dark { 0x30 } else { 0x40 };
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}
