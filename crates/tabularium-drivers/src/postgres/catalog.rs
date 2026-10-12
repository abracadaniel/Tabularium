//! PostgreSQL introspection (pg_catalog based) and DDL generation.

use tabularium_core::model::*;
use tabularium_core::{Dialect, Error, Result};

use super::PostgresConnection;
use crate::common::*;

const D: Dialect = Dialect::Postgres;

fn qi(s: &str) -> String {
    D.quote_ident_if_needed(s)
}

fn qn(schema: &str, name: &str) -> String {
    D.qualified(Some(schema), name)
}

fn lit(s: &str) -> String {
    D.quote_string(s)
}

/// Constraint as reported by `pg_get_constraintdef`.
pub(crate) struct ConstraintDef {
    pub name: String,
    pub contype: String,
    pub definition: String,
}

/// `describe_table` result plus what DDL generation needs.
pub(crate) struct TableDetails {
    pub info: TableInfo,
    pub relkind: String,
    pub constraints: Vec<ConstraintDef>,
    /// column name -> attidentity ('a' always / 'd' by default)
    pub identity: Vec<(String, String)>,
    /// Names of indexes that implement constraints (no separate CREATE INDEX).
    pub constraint_indexes: Vec<String>,
    pub partition_key: Option<String>,
    /// Columns backed by an owned sequence (`serial`): (column, serial type name).
    pub serial: Vec<(String, String)>,
    /// (parent qualified name, bound expression) for partitions.
    pub partition_of: Option<(String, String)>,
    /// (server, options) for foreign tables.
    pub foreign: Option<(String, Vec<String>)>,
}

fn parse_fk_action(code: &str) -> FkAction {
    match code {
        "r" => FkAction::Restrict,
        "c" => FkAction::Cascade,
        "n" => FkAction::SetNull,
        "d" => FkAction::SetDefault,
        _ => FkAction::NoAction,
    }
}

/// `CHECK ((a > 0)) NOT VALID` -> `a > 0`
fn check_expression(def: &str) -> String {
    let d = def.trim();
    let d = d.strip_suffix("NOT VALID").map(str::trim).unwrap_or(d);
    let d = d.strip_suffix("NO INHERIT").map(str::trim).unwrap_or(d);
    let inner = d.strip_prefix("CHECK").map(str::trim).unwrap_or(d);
    let mut s = inner;
    // strip balanced outer parentheses (pg prints two levels)
    for _ in 0..2 {
        if s.starts_with('(') && s.ends_with(')') && outer_parens_balanced(s) {
            s = s[1..s.len() - 1].trim();
        }
    }
    s.to_string()
}

fn strip_outer_parens(s: &str) -> String {
    let t = s.trim();
    if t.starts_with('(') && t.ends_with(')') && outer_parens_balanced(t) {
        t[1..t.len() - 1].trim().to_string()
    } else {
        t.to_string()
    }
}

fn outer_parens_balanced(s: &str) -> bool {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != s.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Timing and events from `pg_trigger.tgtype`.
fn trigger_timing_events(tgtype: i64) -> (String, String) {
    let timing = if tgtype & 64 != 0 {
        "INSTEAD OF"
    } else if tgtype & 2 != 0 {
        "BEFORE"
    } else {
        "AFTER"
    };
    let mut ev = vec![];
    if tgtype & 4 != 0 {
        ev.push("INSERT");
    }
    if tgtype & 8 != 0 {
        ev.push("DELETE");
    }
    if tgtype & 16 != 0 {
        ev.push("UPDATE");
    }
    if tgtype & 32 != 0 {
        ev.push("TRUNCATE");
    }
    (timing.to_string(), ev.join(" OR "))
}

fn ensure_semicolon(s: &str) -> String {
    let t = s.trim_end();
    if t.ends_with(';') {
        t.to_string()
    } else {
        format!("{t};")
    }
}

fn normalize_sig(s: &str) -> String {
    s.trim().trim_start_matches('(').trim_end_matches(')').split_whitespace().collect::<Vec<_>>().join(" ")
}

impl PostgresConnection {
    fn v(&self) -> u32 {
        self.server_version_num()
    }

    async fn schema_or_current(&self, schema: Option<&str>) -> Result<String> {
        match schema.filter(|s| !s.is_empty()) {
            Some(s) => Ok(s.to_string()),
            None => {
                let rows = self.query("SELECT current_schema()", &[]).await?;
                Ok(rows.first().and_then(|r| c_str(r, 0)).unwrap_or_else(|| "public".into()))
            }
        }
    }

    pub(crate) async fn catalog_list_databases(&self) -> Result<Vec<String>> {
        let rows = self
            .query("SELECT datname::text FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY 1", &[])
            .await?;
        Ok(rows.iter().map(|r| c_string(r, 0)).collect())
    }

    pub(crate) async fn catalog_list_schemas(&self) -> Result<Vec<String>> {
        let rows = self
            .query(
                "SELECT nspname::text FROM pg_namespace \
                 WHERE nspname NOT LIKE 'pg\\_temp\\_%' AND nspname NOT LIKE 'pg\\_toast\\_temp\\_%' \
                   AND ($1::text = 'true' OR (nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
                        AND nspname <> 'crdb_internal')) \
                 ORDER BY 1",
                &[if self.show_system() { "true" } else { "false" }],
            )
            .await?;
        let mut out: Vec<String> = rows.iter().map(|r| c_string(r, 0)).collect();
        if !self.cfg.schema_filter.is_empty() {
            out.retain(|s| self.cfg.schema_filter.iter().any(|f| f == s));
        }
        Ok(out)
    }

    fn not_extension_member(&self, oid_expr: &str, classid: &str) -> String {
        if self.show_system() {
            "TRUE".into()
        } else {
            format!(
                "NOT EXISTS (SELECT 1 FROM pg_depend dep WHERE dep.classid = '{classid}'::regclass \
                 AND dep.objid = {oid_expr} AND dep.deptype = 'e')"
            )
        }
    }

    pub(crate) async fn catalog_list_objects(&self, schema: Option<&str>, kinds: &[ObjectKind]) -> Result<Vec<ObjectInfo>> {
        let schema = self.schema_or_current(schema).await?;
        let mut out = vec![];
        let mut relkinds = vec![];
        for k in kinds {
            match k {
                ObjectKind::Table => relkinds.extend(["r", "p"]),
                ObjectKind::View => relkinds.push("v"),
                ObjectKind::MaterializedView => relkinds.push("m"),
                ObjectKind::ForeignTable => relkinds.push("f"),
                ObjectKind::Sequence => relkinds.push("S"),
                _ => {}
            }
        }
        if !relkinds.is_empty() {
            let partition_filter = if self.v() >= 100000 { "AND NOT c.relispartition" } else { "" };
            let sql = format!(
                "SELECT c.relname::text, c.relkind::text, obj_description(c.oid, 'pg_class'), c.reltuples::int8 \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1::text AND c.relkind::text = ANY(string_to_array($2::text, ',')) {partition_filter} \
                   AND {} \
                 ORDER BY 1",
                self.not_extension_member("c.oid", "pg_class")
            );
            let rows = self.query(&sql, &[&schema, &relkinds.join(",")]).await?;
            for r in rows {
                let kind = match c_string(&r, 1).as_str() {
                    "r" | "p" => ObjectKind::Table,
                    "v" => ObjectKind::View,
                    "m" => ObjectKind::MaterializedView,
                    "f" => ObjectKind::ForeignTable,
                    "S" => ObjectKind::Sequence,
                    _ => continue,
                };
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&schema), c_string(&r, 0), kind));
                info.comment = c_nonempty(&r, 2);
                if kind.has_data() {
                    info.row_estimate = c_i64(&r, 3).filter(|n| *n >= 0);
                }
                out.push(info);
            }
        }

        let want_fn = kinds.contains(&ObjectKind::Function);
        let want_proc = kinds.contains(&ObjectKind::Procedure);
        if want_fn || want_proc {
            let kind_expr = if self.v() >= 110000 {
                "p.prokind::text"
            } else {
                "CASE WHEN p.proisagg THEN 'a' WHEN p.proiswindow THEN 'w' ELSE 'f' END"
            };
            let sql = format!(
                "SELECT p.proname::text, {kind_expr}, pg_get_function_identity_arguments(p.oid), \
                        obj_description(p.oid, 'pg_proc') \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = $1::text AND {} ORDER BY 1, 3",
                self.not_extension_member("p.oid", "pg_proc")
            );
            for r in self.query(&sql, &[&schema]).await? {
                let kind = if c_string(&r, 1) == "p" { ObjectKind::Procedure } else { ObjectKind::Function };
                if (kind == ObjectKind::Function && !want_fn) || (kind == ObjectKind::Procedure && !want_proc) {
                    continue;
                }
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&schema), c_string(&r, 0), kind));
                info.signature = Some(format!("({})", c_string(&r, 2)));
                info.comment = c_nonempty(&r, 3);
                out.push(info);
            }
        }

        if kinds.contains(&ObjectKind::Trigger) {
            let rows = self
                .query(
                    "SELECT t.tgname::text, c.relname::text, obj_description(t.oid, 'pg_trigger') \
                     FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1::text AND NOT t.tgisinternal ORDER BY 2, 1",
                    &[&schema],
                )
                .await?;
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&schema), c_string(&r, 0), ObjectKind::Trigger));
                info.parent = c_str(&r, 1);
                info.comment = c_nonempty(&r, 2);
                out.push(info);
            }
        }

        if kinds.contains(&ObjectKind::Index) {
            let rows = self
                .query(
                    "SELECT ic.relname::text, tc.relname::text, obj_description(ic.oid, 'pg_class') \
                     FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid \
                     JOIN pg_class tc ON tc.oid = i.indrelid \
                     JOIN pg_namespace n ON n.oid = ic.relnamespace \
                     WHERE n.nspname = $1::text ORDER BY 2, 1",
                    &[&schema],
                )
                .await?;
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&schema), c_string(&r, 0), ObjectKind::Index));
                info.parent = c_str(&r, 1);
                info.comment = c_nonempty(&r, 2);
                out.push(info);
            }
        }

        if kinds.contains(&ObjectKind::Type) {
            let sql = format!(
                "SELECT t.typname::text, t.typtype::text, obj_description(t.oid, 'pg_type') \
                 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
                 WHERE n.nspname = $1::text \
                   AND (t.typtype IN ('e', 'd', 'r') \
                        OR (t.typtype = 'c' AND (SELECT c.relkind FROM pg_class c WHERE c.oid = t.typrelid) = 'c') \
                        OR (t.typtype = 'b' AND t.typcategory <> 'A')) \
                   AND {} ORDER BY 1",
                self.not_extension_member("t.oid", "pg_type")
            );
            for r in self.query(&sql, &[&schema]).await? {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&schema), c_string(&r, 0), ObjectKind::Type));
                info.comment = c_nonempty(&r, 2);
                out.push(info);
            }
        }

        if kinds.contains(&ObjectKind::Schema) {
            for s in self.catalog_list_schemas().await? {
                out.push(ObjectInfo::new(ObjectRef::new(None, s, ObjectKind::Schema)));
            }
        }
        Ok(out)
    }

    /// Resolve a relation to (oid, relkind, comment, reltuples).
    async fn resolve_relation(&self, schema: &str, name: &str) -> Result<(String, String, Option<String>, Option<i64>)> {
        let rows = self
            .query(
                "SELECT c.oid::int8, c.relkind::text, obj_description(c.oid, 'pg_class'), c.reltuples::int8 \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1::text AND c.relname = $2::text",
                &[schema, name],
            )
            .await?;
        let r = rows.first().ok_or_else(|| not_found("relation", &format!("{schema}.{name}")))?;
        Ok((c_string(r, 0), c_string(r, 1), c_nonempty(r, 2), c_i64(r, 3).filter(|n| *n >= 0)))
    }

    pub(crate) async fn catalog_describe_table(&self, object: &ObjectRef) -> Result<TableDetails> {
        let schema = self.schema_or_current(object.schema.as_deref()).await?;
        let (oid, relkind, comment, reltuples) = self.resolve_relation(&schema, &object.name).await?;
        let kind = match relkind.as_str() {
            "v" => ObjectKind::View,
            "m" => ObjectKind::MaterializedView,
            "f" => ObjectKind::ForeignTable,
            "S" => ObjectKind::Sequence,
            _ => ObjectKind::Table,
        };
        let mut info = TableInfo::new(ObjectRef::new(Some(&schema), object.name.clone(), kind));
        info.comment = comment;
        info.row_estimate = reltuples;
        let mut details = TableDetails {
            info,
            relkind: relkind.clone(),
            constraints: vec![],
            identity: vec![],
            constraint_indexes: vec![],
            partition_key: None,
            serial: vec![],
            partition_of: None,
            foreign: None,
        };

        // ---- columns
        let identity = if self.v() >= 100000 { "a.attidentity::text" } else { "''" };
        let generated = if self.v() >= 120000 { "a.attgenerated::text" } else { "''" };
        let sql = format!(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                    pg_get_expr(d.adbin, d.adrelid), {identity}, {generated}, \
                    col_description(a.attrelid, a.attnum), \
                    CASE WHEN a.attcollation <> 0 AND a.attcollation <> t.typcollation THEN co.collname::text END, \
                    a.attnum::int8, \
                    pg_get_serial_sequence(a.attrelid::regclass::text, a.attname) IS NOT NULL, \
                    format_type(a.atttypid, NULL) \
             FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             LEFT JOIN pg_collation co ON co.oid = a.attcollation \
             WHERE a.attrelid = $1::text::oid AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum"
        );
        let mut attnums: Vec<(i64, String)> = vec![];
        for (i, r) in self.query(&sql, &[&oid]).await?.iter().enumerate() {
            let name = c_string(r, 0);
            let default = c_str(r, 3);
            let ident = c_string(r, 4);
            let gen = c_string(r, 5);
            let is_generated = gen == "s" || gen == "v";
            let auto_increment =
                matches!(ident.as_str(), "a" | "d") || default.as_deref().is_some_and(|d| d.starts_with("nextval("));
            if matches!(ident.as_str(), "a" | "d") {
                details.identity.push((name.clone(), ident.clone()));
            } else if c_bool(r, 9) && default.as_deref().is_some_and(|d| d.starts_with("nextval(")) {
                let serial = match c_string(r, 10).as_str() {
                    "smallint" => Some("smallserial"),
                    "integer" => Some("serial"),
                    "bigint" => Some("bigserial"),
                    _ => None,
                };
                if let Some(t) = serial {
                    details.serial.push((name.clone(), t.to_string()));
                }
            }
            attnums.push((c_i64(r, 8).unwrap_or(0), name.clone()));
            details.info.columns.push(ColumnInfo {
                name,
                data_type: c_string(r, 1),
                nullable: c_bool(r, 2),
                default: if is_generated { None } else { default.clone() },
                auto_increment,
                generated: if is_generated { default.map(|g| strip_outer_parens(&g)) } else { None },
                comment: c_nonempty(r, 6),
                collation: c_str(r, 7),
                ordinal: i as u32 + 1,
            });
        }

        // ---- constraints
        let rows = self
            .query(
                "SELECT con.conname::text, con.contype::text, pg_get_constraintdef(con.oid, true), \
                        con.conkey::text, fn.nspname::text, fc.relname::text, con.confrelid::int8, \
                        con.confkey::text, con.confupdtype::text, con.confdeltype::text, ic.relname::text \
                 FROM pg_constraint con \
                 LEFT JOIN pg_class fc ON fc.oid = con.confrelid \
                 LEFT JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
                 LEFT JOIN pg_class ic ON ic.oid = con.conindid AND con.contype IN ('p', 'u', 'x') \
                 WHERE con.conrelid = $1::text::oid \
                 ORDER BY CASE con.contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'c' THEN 2 WHEN 'x' THEN 3 ELSE 4 END, \
                          con.conname",
                &[&oid],
            )
            .await?;
        let col_names = |keys: &[String], nums: &[(i64, String)]| -> Vec<String> {
            keys.iter()
                .filter_map(|k| k.trim().parse::<i64>().ok())
                .map(|n| nums.iter().find(|(a, _)| *a == n).map(|(_, s)| s.clone()).unwrap_or_else(|| format!("#{n}")))
                .collect()
        };
        for r in &rows {
            let name = c_string(r, 0);
            let contype = c_string(r, 1);
            let def = c_string(r, 2);
            let keys = col_names(&c_list(r, 3), &attnums);
            if let Some(idx) = c_str(r, 10) {
                details.constraint_indexes.push(idx);
            }
            match contype.as_str() {
                "p" => details.info.primary_key = Some(KeyInfo { name: Some(name.clone()), columns: keys }),
                "u" => details.info.unique_keys.push(KeyInfo { name: Some(name.clone()), columns: keys }),
                "c" => details.info.checks.push(CheckInfo { name: Some(name.clone()), expression: check_expression(&def) }),
                "f" => {
                    let ref_oid = c_string(r, 6);
                    let ref_attnums: Vec<(i64, String)> = self
                        .query(
                            "SELECT attnum::int8, attname::text FROM pg_attribute \
                             WHERE attrelid = $1::text::oid AND attnum > 0",
                            &[&ref_oid],
                        )
                        .await?
                        .iter()
                        .map(|a| (c_i64(a, 0).unwrap_or(0), c_string(a, 1)))
                        .collect();
                    details.info.foreign_keys.push(ForeignKeyInfo {
                        name: Some(name.clone()),
                        columns: keys,
                        ref_schema: c_str(r, 4),
                        ref_table: c_string(r, 5),
                        ref_columns: col_names(&c_list(r, 7), &ref_attnums),
                        on_update: parse_fk_action(&c_string(r, 8)),
                        on_delete: parse_fk_action(&c_string(r, 9)),
                    });
                }
                _ => {}
            }
            details.constraints.push(ConstraintDef { name, contype, definition: def });
        }

        // ---- indexes
        let natts = if self.v() >= 110000 { "i.indnkeyatts" } else { "i.indnatts" };
        let sql = format!(
            "SELECT ic.relname::text, i.indisunique, i.indisprimary, am.amname::text, \
                    pg_get_expr(i.indpred, i.indrelid), pg_get_indexdef(i.indexrelid), \
                    ARRAY(SELECT pg_get_indexdef(i.indexrelid, k, true) FROM generate_series(1, {natts}::int) k ORDER BY k)::text[] \
             FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid JOIN pg_am am ON am.oid = ic.relam \
             WHERE i.indrelid = $1::text::oid ORDER BY i.indisprimary DESC, ic.relname"
        );
        for r in self.query(&sql, &[&oid]).await? {
            details.info.indexes.push(IndexInfo {
                name: c_string(&r, 0),
                columns: c_list(&r, 6),
                unique: c_bool(&r, 1),
                primary: c_bool(&r, 2),
                method: c_str(&r, 3),
                predicate: c_str(&r, 4),
                definition: c_str(&r, 5),
            });
        }

        // ---- triggers
        let rows = self
            .query(
                "SELECT t.tgname::text, pg_get_triggerdef(t.oid, true), t.tgtype::int8 \
                 FROM pg_trigger t WHERE t.tgrelid = $1::text::oid AND NOT t.tgisinternal ORDER BY 1",
                &[&oid],
            )
            .await?;
        for r in rows {
            let (timing, events) = trigger_timing_events(c_i64(&r, 2).unwrap_or(0));
            details.info.triggers.push(TriggerInfo { name: c_string(&r, 0), timing, events, definition: c_str(&r, 1) });
        }

        // ---- view definition / partitioning / foreign table
        match relkind.as_str() {
            "v" | "m" => {
                let rows = self.query("SELECT pg_get_viewdef($1::text::oid, true)", &[&oid]).await?;
                details.info.view_definition =
                    rows.first().and_then(|r| c_str(r, 0)).map(|s| s.trim().trim_end_matches(';').to_string());
            }
            "f" => {
                let rows = self
                    .query(
                        "SELECT s.srvname::text, ft.ftoptions::text FROM pg_foreign_table ft \
                         JOIN pg_foreign_server s ON s.oid = ft.ftserver WHERE ft.ftrelid = $1::text::oid",
                        &[&oid],
                    )
                    .await?;
                if let Some(r) = rows.first() {
                    details.foreign = Some((c_string(r, 0), c_list(r, 1)));
                }
            }
            _ => {}
        }
        if self.v() >= 100000 && matches!(relkind.as_str(), "r" | "p" | "f") {
            let rows = self
                .query(
                    "SELECT CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END, \
                            CASE WHEN c.relispartition THEN pg_get_expr(c.relpartbound, c.oid) END, \
                            (SELECT quote_ident(pn.nspname) || '.' || quote_ident(pc.relname) \
                               FROM pg_inherits inh JOIN pg_class pc ON pc.oid = inh.inhparent \
                               JOIN pg_namespace pn ON pn.oid = pc.relnamespace \
                              WHERE inh.inhrelid = c.oid LIMIT 1) \
                     FROM pg_class c WHERE c.oid = $1::text::oid",
                    &[&oid],
                )
                .await?;
            if let Some(r) = rows.first() {
                details.partition_key = c_str(r, 0);
                if let (Some(bound), Some(parent)) = (c_str(r, 1), c_str(r, 2)) {
                    details.partition_of = Some((parent, bound));
                }
            }
        }
        Ok(details)
    }

    pub(crate) async fn catalog_describe_routine(&self, object: &ObjectRef, signature: Option<&str>) -> Result<RoutineInfo> {
        let schema = self.schema_or_current(object.schema.as_deref()).await?;
        let kind_expr = if self.v() >= 110000 { "p.prokind::text" } else { "CASE WHEN p.proisagg THEN 'a' ELSE 'f' END" };
        let sql = format!(
            "SELECT p.oid::int8, pg_get_function_identity_arguments(p.oid), pg_get_function_result(p.oid), \
                    l.lanname::text, {kind_expr} \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace JOIN pg_language l ON l.oid = p.prolang \
             WHERE n.nspname = $1::text AND p.proname = $2::text ORDER BY 2"
        );
        let rows = self.query(&sql, &[&schema, &object.name]).await?;
        let wanted = signature.map(normalize_sig);
        let row = rows
            .iter()
            .find(|r| wanted.as_deref().is_none_or(|w| normalize_sig(&c_string(r, 1)) == w))
            .or_else(|| rows.first())
            .ok_or_else(|| not_found("routine", &format!("{schema}.{}", object.name)))?;
        let oid = c_string(row, 0);
        let kind = c_string(row, 4);
        let definition = self.routine_definition(&oid, &kind, &schema, &object.name, &c_string(row, 1)).await?;
        Ok(RoutineInfo {
            object: ObjectRef::new(
                Some(&schema),
                object.name.clone(),
                if kind == "p" { ObjectKind::Procedure } else { ObjectKind::Function },
            ),
            signature: Some(format!("({})", c_string(row, 1))),
            return_type: c_str(row, 2),
            language: c_str(row, 3),
            definition,
        })
    }

    async fn routine_definition(&self, oid: &str, kind: &str, schema: &str, name: &str, args: &str) -> Result<String> {
        if kind == "a" {
            // pg_get_functiondef rejects aggregates; give a readable stub instead.
            let rows = self
                .query(
                    "SELECT a.aggtransfn::regproc::text, format_type(a.aggtranstype, NULL), \
                            NULLIF(a.aggfinalfn::oid, 0)::regproc::text, a.agginitval \
                     FROM pg_aggregate a WHERE a.aggfnoid = $1::text::oid",
                    &[oid],
                )
                .await?;
            let mut s = format!("CREATE AGGREGATE {}({args}) (\n", qn(schema, name));
            if let Some(r) = rows.first() {
                s.push_str(&format!("    SFUNC = {},\n    STYPE = {}", c_string(r, 0), c_string(r, 1)));
                if let Some(f) = c_str(r, 2) {
                    s.push_str(&format!(",\n    FINALFUNC = {f}"));
                }
                if let Some(init) = c_str(r, 3) {
                    s.push_str(&format!(",\n    INITCOND = {}", lit(&init)));
                }
            }
            s.push_str("\n);");
            return Ok(s);
        }
        let rows = self.query("SELECT pg_get_functiondef($1::text::oid)", &[oid]).await?;
        Ok(rows.first().map(|r| ensure_semicolon(&c_string(r, 0))).unwrap_or_default())
    }

    pub(crate) async fn catalog_object_ddl(&self, object: &ObjectRef) -> Result<String> {
        match object.kind {
            ObjectKind::Table | ObjectKind::ForeignTable => {
                let d = self.catalog_describe_table(object).await?;
                Ok(table_ddl(&d))
            }
            ObjectKind::View | ObjectKind::MaterializedView => {
                let d = self.catalog_describe_table(object).await?;
                Ok(view_ddl(&d))
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let schema = self.schema_or_current(object.schema.as_deref()).await?;
                let kind_expr =
                    if self.v() >= 110000 { "p.prokind::text" } else { "CASE WHEN p.proisagg THEN 'a' ELSE 'f' END" };
                let sql = format!(
                    "SELECT p.oid::int8, {kind_expr}, pg_get_function_identity_arguments(p.oid) \
                     FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                     WHERE n.nspname = $1::text AND p.proname = $2::text ORDER BY 3"
                );
                let rows = self.query(&sql, &[&schema, &object.name]).await?;
                if rows.is_empty() {
                    return Err(not_found(object.kind.label(), &object.display_name()));
                }
                let mut parts = vec![];
                for r in rows {
                    parts.push(
                        self.routine_definition(&c_string(&r, 0), &c_string(&r, 1), &schema, &object.name, &c_string(&r, 2))
                            .await?,
                    );
                }
                Ok(parts.join("\n\n") + "\n")
            }
            ObjectKind::Sequence => self.sequence_ddl(object).await,
            ObjectKind::Index => {
                let schema = self.schema_or_current(object.schema.as_deref()).await?;
                let rows = self
                    .query(
                        "SELECT pg_get_indexdef(c.oid), obj_description(c.oid, 'pg_class') FROM pg_class c \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1::text AND c.relname = $2::text AND c.relkind IN ('i', 'I')",
                        &[&schema, &object.name],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| not_found("index", &object.display_name()))?;
                let mut s = ensure_semicolon(&c_string(r, 0)) + "\n";
                if let Some(c) = c_nonempty(r, 1) {
                    s.push_str(&format!("COMMENT ON INDEX {} IS {};\n", qn(&schema, &object.name), lit(&c)));
                }
                Ok(s)
            }
            ObjectKind::Trigger => {
                let schema = self.schema_or_current(object.schema.as_deref()).await?;
                let rows = self
                    .query(
                        "SELECT pg_get_triggerdef(t.oid, true) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1::text AND t.tgname = $2::text AND NOT t.tgisinternal ORDER BY c.relname",
                        &[&schema, &object.name],
                    )
                    .await?;
                if rows.is_empty() {
                    return Err(not_found("trigger", &object.display_name()));
                }
                Ok(rows.iter().map(|r| ensure_semicolon(&c_string(r, 0))).collect::<Vec<_>>().join("\n") + "\n")
            }
            ObjectKind::Type => self.type_ddl(object).await,
            ObjectKind::Schema => {
                let rows = self
                    .query(
                        "SELECT pg_get_userbyid(nspowner)::text, obj_description(oid, 'pg_namespace') \
                         FROM pg_namespace WHERE nspname = $1::text",
                        &[&object.name],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| not_found("schema", &object.name))?;
                let mut s = format!("CREATE SCHEMA {} AUTHORIZATION {};\n", qi(&object.name), qi(&c_string(r, 0)));
                if let Some(c) = c_nonempty(r, 1) {
                    s.push_str(&format!("COMMENT ON SCHEMA {} IS {};\n", qi(&object.name), lit(&c)));
                }
                Ok(s)
            }
            ObjectKind::Database => {
                let rows = self
                    .query(
                        "SELECT pg_get_userbyid(datdba)::text, pg_encoding_to_char(encoding)::text, \
                                datcollate::text, datctype::text \
                         FROM pg_database WHERE datname = $1::text",
                        &[&object.name],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| not_found("database", &object.name))?;
                Ok(format!(
                    "CREATE DATABASE {}\n    OWNER = {}\n    ENCODING = {}\n    LC_COLLATE = {}\n    LC_CTYPE = {};\n",
                    qi(&object.name),
                    qi(&c_string(r, 0)),
                    lit(&c_string(r, 1)),
                    lit(&c_string(r, 2)),
                    lit(&c_string(r, 3))
                ))
            }
            other => Err(Error::Unsupported(format!("DDL for {} objects", other.label()))),
        }
    }

    async fn sequence_ddl(&self, object: &ObjectRef) -> Result<String> {
        let schema = self.schema_or_current(object.schema.as_deref()).await?;
        let rows = if self.v() >= 100000 {
            self.query(
                "SELECT data_type::text, start_value::text, min_value::text, max_value::text, increment_by::text, \
                        cycle, cache_size::text \
                 FROM pg_sequences WHERE schemaname = $1::text AND sequencename = $2::text",
                &[&schema, &object.name],
            )
            .await?
        } else {
            let sql = format!(
                "SELECT 'bigint', start_value::text, min_value::text, max_value::text, increment_by::text, \
                        is_cycled, cache_value::text FROM {}",
                qn(&schema, &object.name)
            );
            self.query(&sql, &[]).await?
        };
        let r = rows.first().ok_or_else(|| not_found("sequence", &object.display_name()))?;
        let mut s = format!(
            "CREATE SEQUENCE {}\n    AS {}\n    INCREMENT BY {}\n    MINVALUE {}\n    MAXVALUE {}\n    START WITH {}\n    CACHE {}\n    {};\n",
            qn(&schema, &object.name),
            c_string(r, 0),
            c_string(r, 4),
            c_string(r, 2),
            c_string(r, 3),
            c_string(r, 1),
            c_string(r, 6),
            if c_bool(r, 5) { "CYCLE" } else { "NO CYCLE" }
        );
        if self.v() < 100000 {
            s = s.replace("    AS bigint\n", "");
        }
        // OWNED BY
        let owned = self
            .query(
                "SELECT quote_ident(tn.nspname) || '.' || quote_ident(t.relname) || '.' || quote_ident(a.attname) \
                 FROM pg_class s JOIN pg_namespace sn ON sn.oid = s.relnamespace \
                 JOIN pg_depend d ON d.objid = s.oid AND d.classid = 'pg_class'::regclass AND d.deptype IN ('a', 'i') \
                 JOIN pg_class t ON t.oid = d.refobjid JOIN pg_namespace tn ON tn.oid = t.relnamespace \
                 JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = d.refobjsubid \
                 WHERE sn.nspname = $1::text AND s.relname = $2::text AND s.relkind = 'S'",
                &[&schema, &object.name],
            )
            .await
            .unwrap_or_default();
        if let Some(o) = owned.first().and_then(|r| c_str(r, 0)) {
            s.push_str(&format!("ALTER SEQUENCE {} OWNED BY {o};\n", qn(&schema, &object.name)));
        }
        Ok(s)
    }

    async fn type_ddl(&self, object: &ObjectRef) -> Result<String> {
        let schema = self.schema_or_current(object.schema.as_deref()).await?;
        let rows = self
            .query(
                "SELECT t.oid::int8, t.typtype::text, t.typrelid::int8, format_type(t.typbasetype, t.typtypmod), \
                        t.typnotnull, t.typdefault, obj_description(t.oid, 'pg_type') \
                 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
                 WHERE n.nspname = $1::text AND t.typname = $2::text",
                &[&schema, &object.name],
            )
            .await?;
        let r = rows.first().ok_or_else(|| not_found("type", &object.display_name()))?;
        let oid = c_string(r, 0);
        let name = qn(&schema, &object.name);
        let mut s = match c_string(r, 1).as_str() {
            "e" => {
                let labels = self
                    .query("SELECT enumlabel::text FROM pg_enum WHERE enumtypid = $1::text::oid ORDER BY enumsortorder", &[&oid])
                    .await?;
                let labels: Vec<String> = labels.iter().map(|l| lit(&c_string(l, 0))).collect();
                format!("CREATE TYPE {name} AS ENUM ({});\n", labels.join(", "))
            }
            "c" => {
                let attrs = self
                    .query(
                        "SELECT attname::text, format_type(atttypid, atttypmod) FROM pg_attribute \
                         WHERE attrelid = $1::text::oid AND attnum > 0 AND NOT attisdropped ORDER BY attnum",
                        &[&c_string(r, 2)],
                    )
                    .await?;
                let cols: Vec<String> =
                    attrs.iter().map(|a| format!("    {} {}", qi(&c_string(a, 0)), c_string(a, 1))).collect();
                format!("CREATE TYPE {name} AS (\n{}\n);\n", cols.join(",\n"))
            }
            "d" => {
                let mut s = format!("CREATE DOMAIN {name} AS {}", c_string(r, 3));
                if let Some(def) = c_str(r, 5) {
                    s.push_str(&format!(" DEFAULT {def}"));
                }
                if c_bool(r, 4) {
                    s.push_str(" NOT NULL");
                }
                let checks = self
                    .query(
                        "SELECT conname::text, pg_get_constraintdef(oid, true) FROM pg_constraint \
                         WHERE contypid = $1::text::oid ORDER BY conname",
                        &[&oid],
                    )
                    .await?;
                for c in checks {
                    s.push_str(&format!("\n    CONSTRAINT {} {}", qi(&c_string(&c, 0)), c_string(&c, 1)));
                }
                s.push_str(";\n");
                s
            }
            "r" => {
                let rng = self
                    .query("SELECT format_type(rngsubtype, NULL) FROM pg_range WHERE rngtypid = $1::text::oid", &[&oid])
                    .await?;
                format!("CREATE TYPE {name} AS RANGE (SUBTYPE = {});\n", rng.first().map(|x| c_string(x, 0)).unwrap_or_default())
            }
            other => format!("-- {name} is a base type (typtype '{other}'); its definition is provided by C functions\n"),
        };
        if let Some(c) = c_nonempty(r, 6) {
            s.push_str(&format!("COMMENT ON TYPE {name} IS {};\n", lit(&c)));
        }
        Ok(s)
    }

    pub(crate) async fn catalog_list_roles(&self) -> Result<Vec<RoleInfo>> {
        let rows = self
            .query(
                "SELECT r.rolname::text, r.rolsuper, r.rolcanlogin, \
                        ARRAY(SELECT b.rolname::text FROM pg_auth_members m JOIN pg_roles b ON m.roleid = b.oid \
                              WHERE m.member = r.oid ORDER BY 1)::text[] \
                 FROM pg_roles r WHERE $1::text = 'true' OR r.rolname NOT LIKE 'pg\\_%' ORDER BY 1",
                &[if self.show_system() { "true" } else { "false" }],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| RoleInfo { name: c_string(r, 0), superuser: c_bool(r, 1), can_login: c_bool(r, 2), member_of: c_list(r, 3) })
            .collect())
    }

    pub(crate) async fn catalog_list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let filter = if self.v() >= 100000 { "WHERE backend_type = 'client backend'" } else { "" };
        let sql = format!(
            "SELECT pid::text, usename::text, datname::text, \
                    NULLIF(concat_ws(' ', NULLIF(application_name, ''), client_addr::text), ''), \
                    state, query, backend_start::text \
             FROM pg_stat_activity {filter} ORDER BY backend_start"
        );
        let rows = self.query(&sql, &[]).await?;
        Ok(rows
            .iter()
            .map(|r| SessionInfo {
                id: c_string(r, 0),
                user: c_str(r, 1),
                database: c_str(r, 2),
                client: c_str(r, 3),
                state: c_str(r, 4),
                query: c_str(r, 5),
                started: c_str(r, 6),
            })
            .collect())
    }
}

/// `CREATE TABLE` (or `CREATE FOREIGN TABLE`) with constraints, indexes, comments and
/// triggers.
pub(crate) fn table_ddl(d: &TableDetails) -> String {
    let t = &d.info;
    let schema = t.object.schema.as_deref().unwrap_or("public");
    let name = qn(schema, &t.object.name);
    let mut s = String::new();
    let foreign = d.relkind == "f";
    if let Some((parent, bound)) = &d.partition_of {
        s.push_str(&format!("CREATE TABLE {name} PARTITION OF {parent}\n    {bound}"));
        let cons: Vec<String> = d
            .constraints
            .iter()
            .filter(|c| c.contype != "f")
            .map(|c| format!("    CONSTRAINT {} {}", qi(&c.name), c.definition))
            .collect();
        if !cons.is_empty() {
            s = format!("CREATE TABLE {name} PARTITION OF {parent} (\n{}\n)\n    {bound}", cons.join(",\n"));
        }
    } else {
        s.push_str(&format!("CREATE {}TABLE {name} (\n", if foreign { "FOREIGN " } else { "" }));
        let mut lines: Vec<String> = vec![];
        for c in &t.columns {
            let serial = d.serial.iter().find(|(n, _)| n == &c.name).map(|(_, t)| t.as_str());
            let mut line = format!("    {} {}", qi(&c.name), serial.unwrap_or(&c.data_type));
            if let Some(coll) = &c.collation {
                line.push_str(&format!(" COLLATE {}", D.quote_ident(coll)));
            }
            if let Some(g) = &c.generated {
                line.push_str(&format!(" GENERATED ALWAYS AS ({g}) STORED"));
            } else if let Some((_, ident)) = d.identity.iter().find(|(n, _)| n == &c.name) {
                line.push_str(if ident == "a" {
                    " GENERATED ALWAYS AS IDENTITY"
                } else {
                    " GENERATED BY DEFAULT AS IDENTITY"
                });
            } else if let Some(def) = c.default.as_ref().filter(|_| serial.is_none()) {
                line.push_str(&format!(" DEFAULT {def}"));
            }
            if !c.nullable {
                line.push_str(" NOT NULL");
            }
            lines.push(line);
        }
        for c in &d.constraints {
            if c.contype == "n" {
                continue; // NOT NULL constraints (PG 18) are part of the column line
            }
            lines.push(format!("    CONSTRAINT {} {}", qi(&c.name), c.definition));
        }
        s.push_str(&lines.join(",\n"));
        s.push_str("\n)");
        if let Some(pk) = &d.partition_key {
            s.push_str(&format!("\nPARTITION BY {pk}"));
        }
        if let Some((server, opts)) = &d.foreign {
            s.push_str(&format!("\nSERVER {}", qi(server)));
            if !opts.is_empty() {
                let o: Vec<String> = opts
                    .iter()
                    .map(|kv| match kv.split_once('=') {
                        Some((k, v)) => format!("{} {}", qi(k), lit(v)),
                        None => kv.clone(),
                    })
                    .collect();
                s.push_str(&format!("\nOPTIONS ({})", o.join(", ")));
            }
        }
    }
    s.push_str(";\n");

    let idx: Vec<&IndexInfo> =
        t.indexes.iter().filter(|i| !d.constraint_indexes.contains(&i.name) && i.definition.is_some()).collect();
    if !idx.is_empty() {
        s.push('\n');
        for i in idx {
            s.push_str(&ensure_semicolon(i.definition.as_deref().unwrap_or_default()));
            s.push('\n');
        }
    }
    let mut comments = vec![];
    if let Some(c) = &t.comment {
        comments.push(format!("COMMENT ON {}TABLE {name} IS {};", if foreign { "FOREIGN " } else { "" }, lit(c)));
    }
    for col in &t.columns {
        if let Some(c) = &col.comment {
            comments.push(format!("COMMENT ON COLUMN {name}.{} IS {};", qi(&col.name), lit(c)));
        }
    }
    if !comments.is_empty() {
        s.push('\n');
        s.push_str(&comments.join("\n"));
        s.push('\n');
    }
    if !t.triggers.is_empty() {
        s.push('\n');
        for tr in &t.triggers {
            if let Some(def) = &tr.definition {
                s.push_str(&ensure_semicolon(def));
                s.push('\n');
            }
        }
    }
    s
}

/// `CREATE OR REPLACE VIEW` / `CREATE MATERIALIZED VIEW`.
pub(crate) fn view_ddl(d: &TableDetails) -> String {
    let t = &d.info;
    let schema = t.object.schema.as_deref().unwrap_or("public");
    let name = qn(schema, &t.object.name);
    let body = t.view_definition.as_deref().unwrap_or("SELECT NULL").trim();
    let mut s = if d.relkind == "m" {
        format!("CREATE MATERIALIZED VIEW {name} AS\n{body}\nWITH DATA;\n")
    } else {
        format!("CREATE OR REPLACE VIEW {name} AS\n{body};\n")
    };
    if d.relkind == "m" {
        for i in &t.indexes {
            if let Some(def) = &i.definition {
                s.push_str(&ensure_semicolon(def));
                s.push('\n');
            }
        }
    }
    let kw = if d.relkind == "m" { "MATERIALIZED VIEW" } else { "VIEW" };
    if let Some(c) = &t.comment {
        s.push_str(&format!("COMMENT ON {kw} {name} IS {};\n", lit(c)));
    }
    for col in &t.columns {
        if let Some(c) = &col.comment {
            s.push_str(&format!("COMMENT ON COLUMN {name}.{} IS {};\n", qi(&col.name), lit(c)));
        }
    }
    for tr in &t.triggers {
        if let Some(def) = &tr.definition {
            s.push_str(&ensure_semicolon(def));
            s.push('\n');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_expr() {
        assert_eq!(check_expression("CHECK ((total >= 0))"), "total >= 0");
        assert_eq!(check_expression("CHECK (((a > 0) AND (b > 0)))"), "(a > 0) AND (b > 0)");
        assert_eq!(check_expression("CHECK ((a > 0)) NOT VALID"), "a > 0");
        assert_eq!(check_expression("CHECK ((a > 0) OR (b > 0))"), "(a > 0) OR (b > 0)");
    }

    #[test]
    fn trigger_bits() {
        assert_eq!(trigger_timing_events(2 | 4 | 16 | 1), ("BEFORE".into(), "INSERT OR UPDATE".into()));
        assert_eq!(trigger_timing_events(64 | 8), ("INSTEAD OF".into(), "DELETE".into()));
        assert_eq!(trigger_timing_events(4), ("AFTER".into(), "INSERT".into()));
    }

    #[test]
    fn sig_normalization() {
        assert_eq!(normalize_sig("(a integer,  b text)"), "a integer, b text");
        assert_eq!(normalize_sig("a integer, b text"), "a integer, b text");
    }
}
