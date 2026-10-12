//! ER diagram of a schema: draggable table boxes with columns and foreign-key links,
//! automatic layout, pan/zoom, saved positions and SVG export.

use std::collections::HashMap;

use egui::{pos2, vec2, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use tabularium_core::{Error, ObjectKind, SchemaSnapshot, TableInfo};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx, TabView};
use crate::runtime::{poll_slot, Pending};
use crate::theme::Palette;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnMode {
    All,
    Keys,
    None,
}

const ROW_H: f32 = 18.0;
const HEADER_H: f32 = 24.0;
const CHAR_W: f32 = 7.2;

pub struct ErDiagramTab {
    ds: Uuid,
    schema: Option<String>,
    snapshot: Option<SchemaSnapshot>,
    load: Option<Pending<Result<SchemaSnapshot, Error>>>,
    error: Option<String>,
    positions: HashMap<String, Pos2>,
    pan: Vec2,
    zoom: f32,
    dragging: Option<String>,
    selected: Option<String>,
    mode: ColumnMode,
    show_types: bool,
    filter: String,
    started: bool,
}

impl ErDiagramTab {
    pub fn new(_id: u64, ds: Uuid, schema: Option<String>) -> Self {
        Self {
            ds,
            schema,
            snapshot: None,
            load: None,
            error: None,
            positions: HashMap::new(),
            pan: vec2(20.0, 20.0),
            zoom: 1.0,
            dragging: None,
            selected: None,
            mode: ColumnMode::All,
            show_types: true,
            filter: String::new(),
            started: false,
        }
    }

    fn layout_file(&self, ctx: &Ctx) -> std::path::PathBuf {
        let schema = self.schema.clone().unwrap_or_else(|| "default".into());
        let safe: String = schema.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        ctx.dirs.data.join("diagrams").join(format!("{}_{safe}.json", self.ds))
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.started = true;
        let Some(src) = ctx.sources.get(self.ds) else { return };
        let conn = src.conn.clone();
        let schema = self.schema.clone();
        self.load = Some(Pending::spawn(ctx.rt, ctx.egui, async move {
            let c = conn.get().await?;
            let schema = match schema {
                Some(s) => Some(s),
                None => c.current_schema().await.ok().flatten(),
            };
            let mut snap = SchemaSnapshot { schema: schema.clone(), ..Default::default() };
            for o in c.list_objects(schema.as_deref(), &[ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView]).await? {
                snap.tables.push(c.describe_table(&o.object).await?);
            }
            Ok(snap)
        }));
        // Saved positions.
        if let Ok(data) = std::fs::read_to_string(self.layout_file(ctx)) {
            if let Ok(saved) = serde_json::from_str::<HashMap<String, (f32, f32)>>(&data) {
                self.positions = saved.into_iter().map(|(k, (x, y))| (k, pos2(x, y))).collect();
            }
        }
    }

    fn save_layout(&self, ctx: &Ctx) {
        let saved: HashMap<&String, (f32, f32)> = self.positions.iter().map(|(k, p)| (k, (p.x, p.y))).collect();
        if let Ok(data) = serde_json::to_vec_pretty(&saved) {
            let _ = tabularium_core::config::write_atomic(&self.layout_file(ctx), &data);
        }
    }

    fn visible_columns<'a>(&self, t: &'a TableInfo) -> Vec<&'a tabularium_core::ColumnInfo> {
        let keys: Vec<&String> = t
            .primary_key
            .iter()
            .flat_map(|k| k.columns.iter())
            .chain(t.foreign_keys.iter().flat_map(|f| f.columns.iter()))
            .collect();
        match self.mode {
            ColumnMode::All => t.columns.iter().collect(),
            ColumnMode::Keys => t.columns.iter().filter(|c| keys.contains(&&c.name)).collect(),
            ColumnMode::None => vec![],
        }
    }

    fn box_size(&self, t: &TableInfo) -> Vec2 {
        let cols = self.visible_columns(t);
        let mut chars = t.object.name.chars().count() + 4;
        for c in &cols {
            let n = c.name.chars().count() + if self.show_types { c.data_type.chars().count() + 3 } else { 0 } + 4;
            chars = chars.max(n);
        }
        vec2((chars as f32 * CHAR_W).clamp(120.0, 420.0), HEADER_H + cols.len() as f32 * ROW_H + 6.0)
    }

    /// Grid layout ordered by connectivity, then a few force-directed iterations.
    fn auto_layout(&mut self) {
        let Some(snap) = &self.snapshot else { return };
        let names: Vec<String> = snap.tables.iter().map(|t| t.object.name.clone()).collect();
        let mut degree: HashMap<&str, usize> = HashMap::new();
        for t in &snap.tables {
            for fk in &t.foreign_keys {
                *degree.entry(t.object.name.as_str()).or_default() += 1;
                *degree.entry(fk.ref_table.as_str()).or_default() += 1;
            }
        }
        let mut order: Vec<usize> = (0..names.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(degree.get(names[i].as_str()).copied().unwrap_or(0)));
        let cols = (names.len() as f32).sqrt().ceil().max(1.0) as usize;
        let sizes: HashMap<&str, Vec2> = snap.tables.iter().map(|t| (t.object.name.as_str(), self.box_size(t))).collect();
        let mut pos: HashMap<String, Pos2> = HashMap::new();
        let (mut x, mut y, mut row_h) = (0.0, 0.0, 0.0f32);
        for (k, &i) in order.iter().enumerate() {
            if k > 0 && k % cols == 0 {
                x = 0.0;
                y += row_h + 60.0;
                row_h = 0.0;
            }
            let s = sizes[names[i].as_str()];
            pos.insert(names[i].clone(), pos2(x, y));
            x += s.x + 60.0;
            row_h = row_h.max(s.y);
        }
        // Spring embedding: pull related tables together, push overlapping boxes apart.
        let edges: Vec<(String, String)> = snap
            .tables
            .iter()
            .flat_map(|t| t.foreign_keys.iter().map(move |f| (t.object.name.clone(), f.ref_table.clone())))
            .filter(|(a, b)| a != b)
            .collect();
        for _ in 0..120 {
            let mut force: HashMap<String, Vec2> = names.iter().map(|n| (n.clone(), Vec2::ZERO)).collect();
            for (a, b) in &edges {
                let (Some(pa), Some(pb)) = (pos.get(a), pos.get(b)) else { continue };
                let d = *pb - *pa;
                let f = d * 0.01;
                *force.get_mut(a).unwrap() += f;
                *force.get_mut(b).unwrap() -= f;
            }
            for i in 0..names.len() {
                for j in (i + 1)..names.len() {
                    let (a, b) = (&names[i], &names[j]);
                    let ra = Rect::from_min_size(pos[a], sizes[a.as_str()]).expand(30.0);
                    let rb = Rect::from_min_size(pos[b], sizes[b.as_str()]).expand(30.0);
                    if ra.intersects(rb) {
                        let mut d = rb.center() - ra.center();
                        if d.length() < 1.0 {
                            d = vec2(1.0, 0.5);
                        }
                        let push = d.normalized() * 8.0;
                        *force.get_mut(a).unwrap() -= push;
                        *force.get_mut(b).unwrap() += push;
                    }
                }
            }
            for n in &names {
                let f = force[n];
                let p = pos.get_mut(n).unwrap();
                *p += vec2(f.x.clamp(-20.0, 20.0), f.y.clamp(-20.0, 20.0));
            }
        }
        // Normalize to positive coordinates.
        let min_x = pos.values().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let min_y = pos.values().map(|p| p.y).fold(f32::INFINITY, f32::min);
        for p in pos.values_mut() {
            *p -= vec2(min_x, min_y);
        }
        self.positions = pos;
    }

    fn export_svg(&self) -> String {
        let Some(snap) = &self.snapshot else { return String::new() };
        let mut out = String::new();
        let mut max = Vec2::ZERO;
        for t in &snap.tables {
            if let Some(p) = self.positions.get(&t.object.name) {
                let s = self.box_size(t);
                max = max.max(p.to_vec2() + s);
            }
        }
        out.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" font-family=\"monospace\" font-size=\"12\">\n",
            max.x + 40.0,
            max.y + 40.0
        ));
        out.push_str("<rect width=\"100%\" height=\"100%\" fill=\"white\"/>\n");
        for (a, b) in self.edges() {
            out.push_str(&format!(
                "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#6a7a90\" stroke-width=\"1.5\"/>\n",
                a.x + 20.0,
                a.y + 20.0,
                b.x + 20.0,
                b.y + 20.0
            ));
            out.push_str(&format!("<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3\" fill=\"#6a7a90\"/>\n", b.x + 20.0, b.y + 20.0));
        }
        for t in &snap.tables {
            let Some(p) = self.positions.get(&t.object.name) else { continue };
            let s = self.box_size(t);
            let (x, y) = (p.x + 20.0, p.y + 20.0);
            out.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"4\" fill=\"#f7f9fc\" stroke=\"#3c5a99\"/>\n",
                s.x, s.y
            ));
            out.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{HEADER_H}\" rx=\"4\" fill=\"#3c5a99\"/>\n",
                s.x
            ));
            out.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"white\" font-weight=\"bold\">{}</text>\n",
                x + 8.0,
                y + 16.0,
                xml_escape(&t.object.name)
            ));
            let pk: Vec<&String> = t.primary_key.iter().flat_map(|k| k.columns.iter()).collect();
            for (i, c) in self.visible_columns(t).iter().enumerate() {
                let marker = if pk.contains(&&c.name) { "🔑 " } else { "" };
                let ty = if self.show_types { format!("  {}", c.data_type) } else { String::new() };
                out.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#222\">{}{}<tspan fill=\"#888\">{}</tspan></text>\n",
                    x + 8.0,
                    y + HEADER_H + 13.0 + i as f32 * ROW_H,
                    marker,
                    xml_escape(&c.name),
                    xml_escape(&ty)
                ));
            }
        }
        out.push_str("</svg>\n");
        out
    }

    /// World-space endpoints of each FK link (child box edge → parent box edge).
    fn edges(&self) -> Vec<(Pos2, Pos2)> {
        let Some(snap) = &self.snapshot else { return vec![] };
        let rects: HashMap<&str, Rect> = snap
            .tables
            .iter()
            .filter_map(|t| self.positions.get(&t.object.name).map(|p| (t.object.name.as_str(), Rect::from_min_size(*p, self.box_size(t)))))
            .collect();
        let mut out = vec![];
        for t in &snap.tables {
            for fk in &t.foreign_keys {
                let (Some(a), Some(b)) = (rects.get(t.object.name.as_str()), rects.get(fk.ref_table.as_str())) else { continue };
                out.push(edge_points(*a, *b));
            }
        }
        out
    }
}

fn edge_points(a: Rect, b: Rect) -> (Pos2, Pos2) {
    if a == b {
        return (a.right_center(), a.right_center() + vec2(20.0, -20.0));
    }
    let pa = if b.center().x > a.right() {
        a.right_center()
    } else if b.center().x < a.left() {
        a.left_center()
    } else if b.center().y > a.center().y {
        a.center_bottom()
    } else {
        a.center_top()
    };
    let pb = if a.center().x > b.right() {
        b.right_center()
    } else if a.center().x < b.left() {
        b.left_center()
    } else if a.center().y > b.center().y {
        b.center_bottom()
    } else {
        b.center_top()
    };
    (pa, pb)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

impl TabView for ErDiagramTab {
    fn title(&self) -> String {
        format!("Diagram: {}", self.schema.clone().unwrap_or_else(|| "schema".into()))
    }

    fn data_source(&self) -> Option<Uuid> {
        Some(self.ds)
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut Ctx) {
        if !self.started {
            self.load(ctx);
        }
        if let Some(r) = poll_slot(&mut self.load) {
            match r {
                Ok(s) => {
                    if self.schema.is_none() {
                        self.schema = s.schema.clone();
                    }
                    let missing = s.tables.iter().any(|t| !self.positions.contains_key(&t.object.name));
                    self.snapshot = Some(s);
                    if missing {
                        self.auto_layout();
                    }
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        let palette = Palette::current(ui.ctx());
        ui.horizontal(|ui| {
            if ui.button("⟳ Reload").clicked() {
                self.load(ctx);
            }
            if ui.button("Auto layout").clicked() {
                self.auto_layout();
                self.save_layout(ctx);
            }
            ui.label("Columns:");
            ui.selectable_value(&mut self.mode, ColumnMode::All, "all");
            ui.selectable_value(&mut self.mode, ColumnMode::Keys, "keys");
            ui.selectable_value(&mut self.mode, ColumnMode::None, "none");
            ui.checkbox(&mut self.show_types, "types");
            ui.separator();
            if ui.button("−").clicked() {
                self.zoom = (self.zoom / 1.2).max(0.2);
            }
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            if ui.button("+").clicked() {
                self.zoom = (self.zoom * 1.2).min(3.0);
            }
            if ui.button("Fit").clicked() {
                self.pan = vec2(20.0, 20.0);
                self.zoom = 1.0;
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Highlight").desired_width(140.0));
            if ui.button("Export SVG…").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("SVG", &["svg"]).set_file_name("diagram.svg").save_file() {
                    match std::fs::write(&p, self.export_svg()) {
                        Ok(()) => ctx.info(format!("Saved {}", p.display())),
                        Err(e) => ctx.error(e),
                    }
                }
            }
            if self.load.is_some() {
                ui.spinner();
                ui.weak("loading schema…");
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(palette.error, e);
        }
        let Some(snap) = self.snapshot.clone() else { return };

        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let canvas = resp.rect;
        painter.rect_filled(canvas, 0.0, ui.visuals().extreme_bg_color);
        // Zoom with ctrl+wheel / pinch around the pointer, pan with wheel.
        if resp.hovered() {
            let (zoom_delta, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            if zoom_delta != 1.0 {
                if let Some(p) = resp.hover_pos() {
                    let before = (p - canvas.min - self.pan) / self.zoom;
                    self.zoom = (self.zoom * zoom_delta).clamp(0.2, 3.0);
                    self.pan = p - canvas.min - before * self.zoom;
                }
            } else {
                self.pan += scroll;
            }
        }
        let to_screen = |p: Pos2, pan: Vec2, zoom: f32| canvas.min + pan + p.to_vec2() * zoom;
        let z = self.zoom;
        let rects: HashMap<String, Rect> = snap
            .tables
            .iter()
            .filter_map(|t| {
                self.positions.get(&t.object.name).map(|p| {
                    let s = self.box_size(t) * z;
                    (t.object.name.clone(), Rect::from_min_size(to_screen(*p, self.pan, z), s))
                })
            })
            .collect();

        // Interaction: drag boxes or pan.
        if resp.drag_started() {
            let p = resp.interact_pointer_pos().unwrap_or_default();
            self.dragging = rects.iter().find(|(_, r)| r.contains(p)).map(|(n, _)| n.clone());
        }
        if resp.dragged() {
            let d = resp.drag_delta();
            match &self.dragging {
                Some(n) => {
                    if let Some(p) = self.positions.get_mut(n) {
                        *p += d / z;
                    }
                }
                None => self.pan += d,
            }
        }
        if resp.drag_stopped() {
            if self.dragging.is_some() {
                self.save_layout(ctx);
            }
            self.dragging = None;
        }
        let hovered_table = resp.hover_pos().and_then(|p| rects.iter().find(|(_, r)| r.contains(p)).map(|(n, _)| n.clone()));
        if resp.clicked() {
            self.selected = hovered_table.clone();
        }
        if resp.double_clicked() {
            if let Some(n) = &hovered_table {
                if let Some(t) = snap.tables.iter().find(|t| &t.object.name == n) {
                    ctx.act(AppAction::OpenTableData { ds: self.ds, object: t.object.clone(), filter: None });
                }
            }
        }
        let menu_table = hovered_table.clone();
        resp.context_menu(|ui| {
            if let Some(n) = &menu_table {
                if let Some(t) = snap.tables.iter().find(|t| &t.object.name == n) {
                    if ui.button("Open data").clicked() {
                        ctx.act(AppAction::OpenTableData { ds: self.ds, object: t.object.clone(), filter: None });
                        ui.close();
                    }
                    if ui.button("Show DDL").clicked() {
                        ctx.act(AppAction::OpenDdl { ds: self.ds, object: t.object.clone(), signature: None });
                        ui.close();
                    }
                }
            } else if ui.button("Auto layout").clicked() {
                self.auto_layout();
                ui.close();
            }
        });

        // Related tables of the selection are highlighted.
        let related: Vec<String> = match &self.selected {
            Some(sel) => snap
                .tables
                .iter()
                .filter(|t| &t.object.name == sel || t.foreign_keys.iter().any(|f| &f.ref_table == sel))
                .map(|t| t.object.name.clone())
                .chain(snap.tables.iter().filter(|t| &t.object.name == sel).flat_map(|t| t.foreign_keys.iter().map(|f| f.ref_table.clone())))
                .collect(),
            None => vec![],
        };

        let painter = painter.with_clip_rect(canvas);
        // Links first so boxes cover them.
        for t in &snap.tables {
            for fk in &t.foreign_keys {
                let (Some(a), Some(b)) = (rects.get(&t.object.name), rects.get(&fk.ref_table)) else { continue };
                let (pa, pb) = edge_points(*a, *b);
                let active = self.selected.as_deref() == Some(t.object.name.as_str()) || self.selected.as_deref() == Some(fk.ref_table.as_str());
                let color = if active { palette.function } else { palette.comment };
                let width: f32 = if active { 2.0 } else { 1.2 };
                let mid_x = (pa.x + pb.x) / 2.0;
                let pts = vec![pa, pos2(mid_x, pa.y), pos2(mid_x, pb.y), pb];
                painter.add(egui::Shape::line(pts, Stroke::new(width, color)));
                // "Many" side marker at the child, "one" marker at the parent.
                painter.circle_filled(pb, 3.5 * z.max(0.6), color);
                painter.line_segment([pa + vec2(0.0, -5.0 * z), pa + vec2(0.0, 5.0 * z)], Stroke::new(width, color));
            }
        }
        let filter = self.filter.to_lowercase();
        let font = FontId::monospace(12.0 * z);
        let header_font = FontId::proportional(13.0 * z);
        for t in &snap.tables {
            let Some(r) = rects.get(&t.object.name) else { continue };
            if !r.intersects(canvas) {
                continue;
            }
            let is_sel = self.selected.as_deref() == Some(t.object.name.as_str());
            let dim = !related.is_empty() && !related.contains(&t.object.name);
            let matches = !filter.is_empty() && t.object.name.to_lowercase().contains(&filter);
            let header_color = match t.object.kind {
                ObjectKind::View | ObjectKind::MaterializedView => Color32::from_rgb(0x5a, 0x7a, 0x4a),
                _ => Color32::from_rgb(0x3c, 0x5a, 0x99),
            };
            let alpha: f32 = if dim { 0.35 } else { 1.0 };
            painter.rect_filled(*r, 4.0 * z, ui.visuals().window_fill.gamma_multiply(alpha.max(0.6)));
            let header = Rect::from_min_size(r.min, vec2(r.width(), HEADER_H * z));
            painter.rect_filled(header, egui::CornerRadius { nw: (4.0 * z) as u8, ne: (4.0 * z) as u8, sw: 0, se: 0 }, header_color.gamma_multiply(alpha));
            let border = if is_sel || matches { Stroke::new(2.0_f32, palette.function) } else { Stroke::new(1.0_f32, header_color.gamma_multiply(alpha)) };
            painter.rect_stroke(*r, 4.0 * z, border, egui::StrokeKind::Outside);
            painter.text(header.left_center() + vec2(8.0 * z, 0.0), Align2::LEFT_CENTER, &t.object.name, header_font.clone(), Color32::WHITE.gamma_multiply(alpha));
            if z < 0.45 {
                continue;
            }
            let pk: Vec<&String> = t.primary_key.iter().flat_map(|k| k.columns.iter()).collect();
            let fks: Vec<&String> = t.foreign_keys.iter().flat_map(|f| f.columns.iter()).collect();
            for (i, c) in self.visible_columns(t).iter().enumerate() {
                let y = r.top() + (HEADER_H + 3.0 + i as f32 * ROW_H + ROW_H / 2.0) * z;
                let icon = if pk.contains(&&c.name) {
                    "🔑"
                } else if fks.contains(&&c.name) {
                    "🔗"
                } else if c.nullable {
                    "◇"
                } else {
                    "◆"
                };
                painter.text(pos2(r.left() + 6.0 * z, y), Align2::LEFT_CENTER, icon, font.clone(), palette.comment.gamma_multiply(alpha));
                painter.text(pos2(r.left() + 22.0 * z, y), Align2::LEFT_CENTER, &c.name, font.clone(), palette.text.gamma_multiply(alpha));
                if self.show_types {
                    painter.text(pos2(r.right() - 6.0 * z, y), Align2::RIGHT_CENTER, &c.data_type, font.clone(), palette.comment.gamma_multiply(alpha));
                }
            }
        }
        if let Some(n) = hovered_table {
            if self.dragging.is_none() {
                if let Some(t) = snap.tables.iter().find(|t| t.object.name == n) {
                    resp.on_hover_text(format!(
                        "{} {}\n{} columns, {} foreign keys{}",
                        t.object.kind.label(),
                        t.object.display_name(),
                        t.columns.len(),
                        t.foreign_keys.len(),
                        t.comment.as_ref().map(|c| format!("\n{c}")).unwrap_or_default()
                    ));
                }
            }
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
