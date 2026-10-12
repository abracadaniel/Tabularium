use super::*;

fn opts(format: ImportFormat) -> ImportOptions {
    ImportOptions::new(format)
}

#[test]
fn format_from_path() {
    assert_eq!(ImportFormat::from_path(Path::new("a/b.CSV")), Some(ImportFormat::Csv));
    assert_eq!(ImportFormat::from_path(Path::new("x.ndjson")), Some(ImportFormat::JsonLines));
    assert_eq!(ImportFormat::from_path(Path::new("x.ods")), Some(ImportFormat::Xlsx));
    assert_eq!(ImportFormat::from_path(Path::new("x.tsv")), Some(ImportFormat::Tsv));
    assert_eq!(ImportFormat::from_path(Path::new("x")), None);
    assert_eq!(ImportFormat::Xlsx.label(), "Excel");
}

#[test]
fn csv_basic_bom_ragged_and_null() {
    let text = "\u{FEFF}id;name;note\n1;\"a;b\";x\n2;NULL\n3;\"multi\nline\";y;extra\n";
    let mut o = opts(ImportFormat::Csv);
    o.null_text = Some("NULL".into());
    let d = read_str(text, &o).unwrap();
    assert_eq!(d.columns, vec!["id", "name", "note", "column_4"]);
    assert_eq!(d.rows.len(), 3);
    assert_eq!(d.rows[0], vec!["1".into(), "a;b".into(), "x".into(), Value::Null]);
    assert_eq!(d.rows[1], vec!["2".into(), Value::Null, Value::Null, Value::Null]);
    assert_eq!(d.rows[2][1], Value::from("multi\nline"));
    assert_eq!(d.rows[2][3], Value::from("extra"));
}

#[test]
fn csv_options() {
    let text = "# comment line\nh1,h2,h1\n a , b ,c\n1,2,3\n4,5,6\n";
    let mut o = opts(ImportFormat::Csv);
    o.skip_rows = 1;
    o.trim = true;
    o.max_rows = Some(2);
    o.delimiter = Some(',');
    let d = read_str(text, &o).unwrap();
    assert_eq!(d.columns, vec!["h1", "h2", "h1_2"]);
    assert_eq!(d.rows, vec![vec!["a".into(), "b".into(), "c".into()], vec!["1".into(), "2".into(), "3".into()]]);
    let o = ImportOptions { has_header: false, quote: '\'', ..opts(ImportFormat::Csv) };
    let d = read_str("'x,y',z\n", &o).unwrap();
    assert_eq!(d.columns, vec!["column_1", "column_2"]);
    assert_eq!(d.rows, vec![vec!["x,y".into(), "z".into()]]);
    let d = read_str("a\tb\n1\t2\n", &opts(ImportFormat::Tsv)).unwrap();
    assert_eq!(d.columns, vec!["a", "b"]);
}

#[test]
fn sniffing() {
    assert_eq!(sniff_csv("id,name\n1,Alice\n2,Bob\n"), (',', true));
    assert_eq!(sniff_csv("id;name;city\n1;Alice;Paris\n"), (';', true));
    assert_eq!(sniff_csv("a\tb\tc\n1\t2\t3\n"), ('\t', true));
    assert_eq!(sniff_csv("a|b\n1|2\n"), ('|', true));
    assert_eq!(sniff_csv("1,Alice\n2,Bob\n"), (',', false));
    assert_eq!(sniff_csv("alice,paris\nbob,london\nalice,rome\n"), (',', false));
    assert_eq!(sniff_csv("name,city\nalice,paris\nbob,london\n"), (',', true));
    // commas inside quoted fields don't fool the semicolon detection
    assert_eq!(sniff_csv("a;b\n\"1,2\";3\n\"4,5\";6\n"), (';', true));
}

#[test]
fn json_inputs() {
    let d = read_str(r#"[{"a":1,"b":"x"},{"b":"y","c":[1,2],"a":2.5},{"d":null,"e":true}]"#, &opts(ImportFormat::Json)).unwrap();
    assert_eq!(d.columns, vec!["a", "b", "c", "d", "e"]);
    assert_eq!(d.rows[0], vec![Value::Int(1), "x".into(), Value::Null, Value::Null, Value::Null]);
    assert_eq!(d.rows[1][0], Value::Float(2.5));
    assert_eq!(d.rows[1][2], Value::Json(serde_json::json!([1, 2])));
    assert_eq!(d.rows[2][4], Value::Bool(true));

    let d = read_str(r#"[["id","name"],[1,"a"],[2,"b",true]]"#, &opts(ImportFormat::Json)).unwrap();
    assert_eq!(d.columns, vec!["id", "name", "column_3"]);
    assert_eq!(d.rows[1], vec![Value::Int(2), "b".into(), Value::Bool(true)]);

    let d = read_str(r#"{"rows":[{"x":1}]}"#, &opts(ImportFormat::Json)).unwrap();
    assert_eq!(d.columns, vec!["x"]);
    let d = read_str("{\"x\":1}\n\n{\"y\":18446744073709551615}\n", &opts(ImportFormat::JsonLines)).unwrap();
    assert_eq!(d.columns, vec!["x", "y"]);
    assert_eq!(d.rows[1][1], Value::UInt(u64::MAX));
    assert!(read_str("{\"x\":1}\nnope\n", &opts(ImportFormat::JsonLines)).is_err());
    assert!(read_str("x", &opts(ImportFormat::Xlsx)).is_err());
}

#[test]
fn type_inference() {
    let text = "i,big,dec,f,b,d,ts,s,mixed,empty,zip\n\
                1,1,1.5,1e3,true,2024-01-02,2024-01-02 10:00:00,hello,1,,01234\n\
                -2,9999999999,-12.25,2.5,false,2024-02-03,2024-01-02T10:00:00,world wide,x,,00042\n";
    let d = read_str(text, &opts(ImportFormat::Csv)).unwrap();
    assert_eq!(
        infer_column_types(&d, Dialect::Postgres),
        vec![
            "integer",
            "bigint",
            "numeric(4,2)",
            "double precision",
            "boolean",
            "date",
            "timestamp",
            "varchar(16)",
            "varchar(16)",
            "text",
            "varchar(16)"
        ]
    );
    let my = infer_column_types(&d, Dialect::MySql);
    assert_eq!(my[0], "int");
    assert_eq!(my[2], "decimal(4,2)");
    assert_eq!(my[6], "datetime");
    assert_eq!(my[9], "varchar(255)");
    let ms = infer_column_types(&d, Dialect::MsSql);
    assert_eq!(ms[4], "bit");
    assert_eq!(ms[7], "nvarchar(16)");
    let json = ImportedData {
        columns: vec!["j".into(), "long".into()],
        rows: vec![vec![Value::Json(serde_json::json!({})), Value::Text("x".repeat(5000))]],
    };
    assert_eq!(infer_column_types(&json, Dialect::Postgres), vec!["jsonb", "text"]);
    assert_eq!(infer_column_types(&json, Dialect::MsSql), vec!["nvarchar(max)", "nvarchar(max)"]);
}

#[test]
fn sql_generation() {
    let t = ObjectRef::table(Some("public"), "Imported");
    let cols = vec!["id".to_string(), "name".to_string(), "ok".to_string()];
    let types = vec!["integer".to_string(), "varchar(16)".to_string(), "boolean".to_string()];
    assert_eq!(
        create_table_sql(&t, &cols, &types, Dialect::Postgres),
        "CREATE TABLE public.\"Imported\" (\n    id integer,\n    name varchar(16),\n    ok boolean\n)"
    );
    let rows = vec![
        vec![Value::from("1"), Value::from("O'Hara"), Value::from("true")],
        vec![Value::from(""), Value::from(""), Value::Null],
        vec![Value::from("3"), Value::Null, Value::from("f")],
    ];
    let stmts = insert_statements(&t, &cols, &rows, Some(&types), Dialect::Postgres, 2);
    assert_eq!(
        stmts,
        vec![
            "INSERT INTO public.\"Imported\" (id, name, ok) VALUES\n  (1, 'O''Hara', TRUE),\n  (NULL, '', NULL)",
            "INSERT INTO public.\"Imported\" (id, name, ok) VALUES\n  (3, NULL, FALSE)",
        ]
    );
    let stmts = insert_statements(&t, &cols, &rows[..1], None, Dialect::MySql, 0);
    assert_eq!(stmts, vec!["INSERT INTO public.Imported (id, name, ok) VALUES\n  ('1', 'O''Hara', 'true')"]);
    let many: Vec<Vec<Value>> = (0..2500).map(|i| vec![Value::Int(i)]).collect();
    assert_eq!(insert_statements(&t, &cols[..1], &many, None, Dialect::MsSql, 5000).len(), 3);
}

#[test]
fn spreadsheet_sheets_and_options() {
    use rust_xlsxwriter::Workbook;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.xlsx");
    let mut wb = Workbook::new();
    let s1 = wb.add_worksheet();
    s1.set_name("First").unwrap();
    s1.write_string(0, 0, "a").unwrap();
    let s2 = wb.add_worksheet();
    s2.set_name("Data").unwrap();
    s2.write_string(0, 0, "title row").unwrap();
    s2.write_string(1, 0, "x").unwrap();
    s2.write_string(1, 1, "y").unwrap();
    s2.write_number(2, 0, 1.5).unwrap();
    s2.write_string(2, 1, "-").unwrap();
    s2.write_number(3, 0, 2.0).unwrap();
    wb.save(&path).unwrap();
    assert_eq!(sheet_names(&path).unwrap(), vec!["First", "Data"]);
    let mut o = opts(ImportFormat::Xlsx);
    o.sheet = Some("data".into());
    o.skip_rows = 1;
    o.null_text = Some("-".into());
    let d = read_file(&path, &o).unwrap();
    assert_eq!(d.columns, vec!["x", "y"]);
    assert_eq!(d.rows, vec![vec![Value::Float(1.5), Value::Null], vec![Value::Int(2), Value::Null]]);
    o.sheet = Some("missing".into());
    assert!(read_file(&path, &o).is_err());
}
