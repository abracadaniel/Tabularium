//! "Go to object" popup (Ctrl+N): fuzzy search over all known tables and views.

use tabularium_core::{ObjectKind, ObjectRef};
use uuid::Uuid;

use crate::ctx::{AppAction, Ctx};

pub struct GotoDialog {
    query: String,
    selected: usize,
    focus: bool,
}

struct Hit {
    ds: Uuid,
    ds_name: String,
    object: ObjectRef,
    score: i32,
}

impl GotoDialog {
    pub fn new() -> Self {
        Self { query: String::new(), selected: 0, focus: true }
    }

    fn hits(&self, ctx: &Ctx) -> Vec<Hit> {
        let mut hits = vec![];
        let q = self.query.trim().to_lowercase();
        for ds in &ctx.sources.list {
            let cat = ds.catalog.lock().unwrap();
            for t in &cat.tables {
                let Some(score) = fuzzy_score(&t.name, &q) else { continue };
                hits.push(Hit {
                    ds: ds.config.id,
                    ds_name: ds.config.name.clone(),
                    object: ObjectRef::new(t.schema.as_deref(), t.name.clone(), t.kind),
                    score,
                });
            }
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.object.name.len().cmp(&b.object.name.len())));
        hits.truncate(100);
        hits
    }

    pub fn show(&mut self, ctx: &mut Ctx) -> bool {
        let mut keep = true;
        let hits = self.hits(ctx);
        let mut chosen: Option<usize> = None;
        let egui_ctx = ctx.egui.clone();
        egui::Window::new("Go to object")
            .title_bar(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .fixed_size([520.0, 360.0])
            .show(&egui_ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Table or view name (connect a data source to index its objects)")
                        .desired_width(f32::INFINITY),
                );
                if self.focus {
                    r.request_focus();
                    self.focus = false;
                }
                if r.changed() {
                    self.selected = 0;
                }
                ui.input(|i| {
                    if i.key_pressed(egui::Key::ArrowDown) {
                        self.selected = (self.selected + 1).min(hits.len().saturating_sub(1));
                    }
                    if i.key_pressed(egui::Key::ArrowUp) {
                        self.selected = self.selected.saturating_sub(1);
                    }
                    if i.key_pressed(egui::Key::Enter) && !hits.is_empty() {
                        chosen = Some(self.selected);
                    }
                    if i.key_pressed(egui::Key::Escape) {
                        keep = false;
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, h) in hits.iter().enumerate() {
                        let icon = crate::explorer::kind_icon(h.object.kind);
                        let r = ui.selectable_label(
                            i == self.selected,
                            format!("{icon} {}    — {}", h.object.display_name(), h.ds_name),
                        );
                        if i == self.selected {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            chosen = Some(i);
                        }
                    }
                    if hits.is_empty() {
                        ui.weak("No matches");
                    }
                });
            });
        if let Some(i) = chosen {
            let h = &hits[i];
            if h.object.kind == ObjectKind::Table || h.object.kind.has_data() {
                ctx.act(AppAction::OpenTableData { ds: h.ds, object: h.object.clone(), filter: None });
            } else {
                ctx.act(AppAction::OpenDdl { ds: h.ds, object: h.object.clone(), signature: None });
            }
            keep = false;
        }
        keep
    }
}

/// Subsequence match score: higher is better; contiguous and prefix matches rank first.
pub fn fuzzy_score(candidate: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let c = candidate.to_lowercase();
    if c == query {
        return Some(1000);
    }
    if c.starts_with(query) {
        return Some(800 - c.len() as i32);
    }
    if let Some(pos) = c.find(query) {
        return Some(600 - pos as i32);
    }
    let mut score = 0;
    let mut chars = c.char_indices();
    let mut last = None;
    for qc in query.chars() {
        let (idx, _) = chars.by_ref().find(|(_, ch)| *ch == qc)?;
        score += match last {
            Some(l) if idx == l + 1 => 10,
            _ => 1,
        };
        last = Some(idx);
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn scores() {
        assert!(fuzzy_score("users", "users") > fuzzy_score("users_roles", "users"));
        assert!(fuzzy_score("order_items", "oi").is_some());
        assert!(fuzzy_score("orders", "xyz").is_none());
        assert!(fuzzy_score("customer_orders", "ord") > fuzzy_score("o_r_d", "ord"));
    }
}
