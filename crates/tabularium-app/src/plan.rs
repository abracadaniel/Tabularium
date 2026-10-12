//! Visual query plan for PostgreSQL `EXPLAIN (FORMAT JSON)` output.

use egui::RichText;
use serde_json::Value as J;

use crate::theme::Palette;

#[derive(Debug, Clone)]
pub struct PlanNode {
    pub node_type: String,
    pub detail: String,
    pub total_cost: f64,
    pub startup_cost: f64,
    pub plan_rows: f64,
    pub actual_rows: Option<f64>,
    pub actual_time: Option<f64>,
    pub loops: Option<f64>,
    pub extra: Vec<(String, String)>,
    pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub root: PlanNode,
    pub planning_time: Option<f64>,
    pub execution_time: Option<f64>,
}

impl PlanNode {
    fn from_json(v: &J) -> Option<Self> {
        let o = v.as_object()?;
        let s = |k: &str| o.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let f = |k: &str| o.get(k).and_then(|x| x.as_f64());
        let mut detail = vec![];
        if let Some(r) = s("Relation Name") {
            let mut t = format!("on {r}");
            if let Some(a) = s("Alias").filter(|a| a != &r) {
                t.push_str(&format!(" {a}"));
            }
            detail.push(t);
        }
        if let Some(i) = s("Index Name") {
            detail.push(format!("using {i}"));
        }
        if let Some(j) = s("Join Type") {
            detail.push(format!("{j} join"));
        }
        if let Some(st) = s("Strategy") {
            detail.push(st);
        }
        let mut extra = vec![];
        for key in [
            "Filter", "Index Cond", "Hash Cond", "Merge Cond", "Join Filter", "Recheck Cond", "Sort Key", "Group Key",
            "Rows Removed by Filter", "Sort Method", "Sort Space Used", "Shared Hit Blocks", "Shared Read Blocks", "Workers Planned",
        ] {
            if let Some(val) = o.get(key) {
                let text = match val {
                    J::String(s) => s.clone(),
                    J::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(", "),
                    other => other.to_string(),
                };
                if text != "0" {
                    extra.push((key.to_string(), text));
                }
            }
        }
        let children = o
            .get("Plans")
            .and_then(|p| p.as_array())
            .map(|a| a.iter().filter_map(PlanNode::from_json).collect())
            .unwrap_or_default();
        Some(PlanNode {
            node_type: s("Node Type").unwrap_or_else(|| "?".into()),
            detail: detail.join(" "),
            total_cost: f("Total Cost").unwrap_or(0.0),
            startup_cost: f("Startup Cost").unwrap_or(0.0),
            plan_rows: f("Plan Rows").unwrap_or(0.0),
            actual_rows: f("Actual Rows"),
            actual_time: f("Actual Total Time"),
            loops: f("Actual Loops"),
            extra,
            children,
        })
    }

    /// Cost of this node excluding its children.
    fn self_cost(&self) -> f64 {
        (self.total_cost - self.children.iter().map(|c| c.total_cost).sum::<f64>()).max(0.0)
    }
}

impl Plan {
    /// Parse the single JSON value returned by `EXPLAIN (FORMAT JSON)`.
    pub fn parse(text: &str) -> Option<Plan> {
        let v: J = serde_json::from_str(text).ok()?;
        let first = v.as_array().and_then(|a| a.first()).unwrap_or(&v);
        let root = PlanNode::from_json(first.get("Plan")?)?;
        Some(Plan {
            root,
            planning_time: first.get("Planning Time").and_then(|x| x.as_f64()),
            execution_time: first.get("Execution Time").and_then(|x| x.as_f64()),
        })
    }

    pub fn ui(&self, ui: &mut egui::Ui, palette: &Palette) {
        ui.horizontal(|ui| {
            ui.label(format!("Total cost: {:.2}", self.root.total_cost));
            if let Some(t) = self.planning_time {
                ui.label(format!("· planning {t:.3} ms"));
            }
            if let Some(t) = self.execution_time {
                ui.label(format!("· execution {t:.3} ms"));
            }
        });
        ui.separator();
        let total = self.root.total_cost.max(1e-9);
        node_ui(ui, &self.root, total, palette, &mut 0);
    }
}

fn node_ui(ui: &mut egui::Ui, n: &PlanNode, total: f64, palette: &Palette, counter: &mut usize) {
    *counter += 1;
    let id = ui.make_persistent_id(("plan_node", *counter));
    let frac = (n.self_cost() / total).clamp(0.0, 1.0) as f32;
    let color = if frac > 0.5 {
        palette.error
    } else if frac > 0.2 {
        palette.warning
    } else {
        palette.success
    };
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
        .show_header(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(60.0, 10.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2.0, ui.visuals().faint_bg_color);
            let mut fill = rect;
            fill.set_width(rect.width() * frac.max(0.02));
            ui.painter().rect_filled(fill, 2.0, color);
            let warn = n.node_type == "Seq Scan";
            ui.label(RichText::new(&n.node_type).strong().color(if warn { palette.warning } else { palette.function }));
            if !n.detail.is_empty() {
                ui.label(&n.detail);
            }
            let mut stats = format!("cost {:.2}..{:.2}  rows {}", n.startup_cost, n.total_cost, n.plan_rows);
            if let Some(r) = n.actual_rows {
                stats.push_str(&format!("  actual rows {r}"));
                // Highlight large estimate errors.
                let est = n.plan_rows.max(1.0);
                if r > est * 10.0 || r * 10.0 < est {
                    stats.push_str(" ⚠");
                }
            }
            if let Some(t) = n.actual_time {
                stats.push_str(&format!("  {t:.3} ms"));
            }
            if let Some(l) = n.loops.filter(|l| *l > 1.0) {
                stats.push_str(&format!("  ×{l}"));
            }
            ui.weak(stats);
        })
        .body(|ui| {
            for (k, v) in &n.extra {
                ui.horizontal(|ui| {
                    ui.weak(format!("{k}:"));
                    ui.monospace(v);
                });
            }
            for c in &n.children {
                node_ui(ui, c, total, palette, counter);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pg_json_plan() {
        let text = r#"[{"Plan": {"Node Type": "Hash Join", "Join Type": "Inner", "Startup Cost": 1.0, "Total Cost": 10.0,
            "Plan Rows": 5, "Hash Cond": "(a.id = b.a_id)", "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "a", "Alias": "a", "Startup Cost": 0.0, "Total Cost": 4.0, "Plan Rows": 100},
              {"Node Type": "Hash", "Startup Cost": 2.0, "Total Cost": 3.0, "Plan Rows": 10, "Plans": [
                {"Node Type": "Index Scan", "Relation Name": "b", "Index Name": "b_pkey", "Startup Cost": 0.0, "Total Cost": 2.0, "Plan Rows": 10}]}]},
            "Planning Time": 0.1, "Execution Time": 0.5}]"#;
        let p = Plan::parse(text).unwrap();
        assert_eq!(p.root.node_type, "Hash Join");
        assert_eq!(p.root.children.len(), 2);
        assert_eq!(p.root.children[0].detail, "on a");
        assert_eq!(p.root.children[1].children[0].detail, "on b using b_pkey");
        assert_eq!(p.root.self_cost(), 3.0);
        assert_eq!(p.execution_time, Some(0.5));
        assert!(p.root.extra.iter().any(|(k, _)| k == "Hash Cond"));
    }
}
