use super::*;
use chrono::NaiveDate;

fn cols() -> Vec<ColumnMeta> {
    vec![ColumnMeta::new("id", "int4"), ColumnMeta::new("name", "text"), ColumnMeta::new("price", "numeric")]
}

fn rows() -> Vec<Vec<Value>> {
    vec![
        vec![Value::Int(1), "plain".into(), Value::Decimal("9.50".into())],
        vec![Value::Int(2), "a,b \"q\"\nline2".into(), Value::Null],
        vec![Value::Int(3), "ünïcødé 日本".into(), Value::Float(-1.25)],
    ]
}

fn export(format: ExportFormat, f: impl FnOnce(&mut ExportOptions)) -> String {
    let mut o = ExportOptions::new(format);
    f(&mut o);
    export_to_string(&cols(), &rows(), &o).unwrap()
}

#[test]
fn format_metadata() {
    assert_eq!(ExportFormat::ALL.len(), 12);
    for f in ExportFormat::ALL {
        assert!(!f.label().is_empty() && !f.extension().is_empty());
    }
    assert!(ExportFormat::Xlsx.is_binary());
    assert!(!ExportFormat::Csv.is_binary());
    let o = ExportOptions::new(ExportFormat::Csv);
    assert!(o.include_header && o.pretty && o.rows_per_insert == 100 && o.null_text.is_empty());
}

#[test]
fn csv_and_tsv() {
    assert_eq!(
        export(ExportFormat::Csv, |_| {}),
        "id,name,price\n1,plain,9.50\n2,\"a,b \"\"q\"\"\nline2\",\n3,ünïcødé 日本,-1.25\n"
    );
    assert_eq!(
        export(ExportFormat::Csv, |o| {
            o.quote_all = true;
            o.include_header = false;
            o.null_text = "NULL".into();
            o.delimiter = Some(';');
        }),
        "\"1\";\"plain\";\"9.50\"\n\"2\";\"a,b \"\"q\"\"\nline2\";\"NULL\"\n\"3\";\"ünïcødé 日本\";\"-1.25\"\n"
    );
    assert_eq!(
        export(ExportFormat::Tsv, |_| {}),
        "id\tname\tprice\n1\tplain\t9.50\n2\t\"a,b \"\"q\"\"\nline2\"\t\n3\tünïcødé 日本\t-1.25\n"
    );
    let mut o = ExportOptions::new(ExportFormat::Csv);
    o.delimiter = Some('§');
    assert!(export_to_string(&cols(), &rows(), &o).is_err());
}

#[test]
fn bytes_dates_and_date_format() {
    let cols = vec![ColumnMeta::new("b", "bytea"), ColumnMeta::new("d", "date"), ColumnMeta::new("t", "timestamp")];
    let dt = NaiveDate::from_ymd_opt(2024, 3, 9).unwrap().and_hms_opt(13, 5, 0).unwrap();
    let rows = vec![vec![Value::Bytes(vec![0, 255, 16]), Value::Date(dt.date()), Value::DateTime(dt)]];
    let mut o = ExportOptions::new(ExportFormat::Csv);
    assert_eq!(export_to_string(&cols, &rows, &o).unwrap(), "b,d,t\n0x00FF10,2024-03-09,2024-03-09 13:05:00\n");
    o.date_format = Some("%d.%m.%Y %H:%M".into());
    // the date has no time fields: falls back to ISO
    assert_eq!(export_to_string(&cols, &rows, &o).unwrap(), "b,d,t\n0x00FF10,2024-03-09,09.03.2024 13:05\n");
    o.format = ExportFormat::JsonLines;
    assert_eq!(
        export_to_string(&cols, &rows, &o).unwrap(),
        "{\"b\":\"AP8Q\",\"d\":\"2024-03-09\",\"t\":\"09.03.2024 13:05\"}\n"
    );
}

#[test]
fn json_pretty_compact_and_duplicates() {
    assert_eq!(
        export(ExportFormat::Json, |o| o.pretty = false),
        r#"[{"id":1,"name":"plain","price":9.5},{"id":2,"name":"a,b \"q\"\nline2","price":null},{"id":3,"name":"ünïcødé 日本","price":-1.25}]"#
    );
    let pretty = export(ExportFormat::Json, |_| {});
    assert!(pretty.starts_with("[\n  {\n    \"id\": 1,\n    \"name\": \"plain\",\n"), "{pretty}");
    assert!(pretty.ends_with("  }\n]\n"));
    let parsed: serde_json::Value = serde_json::from_str(&pretty).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 3);
    // duplicate column names keep all data
    let cols = vec![ColumnMeta::new("a", ""), ColumnMeta::new("a", ""), ColumnMeta::new("a_2", "")];
    let rows = vec![vec![Value::Int(1), Value::Int(2), Value::Int(3)]];
    let o = ExportOptions { pretty: false, ..ExportOptions::new(ExportFormat::Json) };
    assert_eq!(export_to_string(&cols, &rows, &o).unwrap(), r#"[{"a":1,"a_3":2,"a_2":3}]"#);
    // empty result
    assert_eq!(export_to_string(&cols, &[], &o).unwrap(), "[]");
    let o = ExportOptions::new(ExportFormat::Json);
    assert_eq!(export_to_string(&cols, &[], &o).unwrap(), "[]\n");
}

#[test]
fn sql_formats() {
    let t = Some(ObjectRef::table(Some("public"), "Items"));
    assert_eq!(
        export(ExportFormat::SqlInsert, |o| {
            o.dialect = Dialect::Postgres;
            o.table = t.clone();
        }),
        "INSERT INTO public.\"Items\" (id, name, price) VALUES (1, 'plain', 9.50);\n\
         INSERT INTO public.\"Items\" (id, name, price) VALUES (2, 'a,b \"q\"\nline2', NULL);\n\
         INSERT INTO public.\"Items\" (id, name, price) VALUES (3, 'ünïcødé 日本', -1.25);\n"
    );
    assert_eq!(
        export(ExportFormat::SqlInsertMultiRow, |o| {
            o.dialect = Dialect::MySql;
            o.rows_per_insert = 2;
        }),
        "INSERT INTO my_table (id, name, price) VALUES\n  (1, 'plain', 9.50),\n  (2, 'a,b \"q\"\nline2', NULL);\n\
         INSERT INTO my_table (id, name, price) VALUES\n  (3, 'ünïcødé 日本', -1.25);\n"
    );
    assert_eq!(
        export(ExportFormat::SqlUpdate, |o| {
            o.dialect = Dialect::MsSql;
            o.key_columns = vec!["id".into()];
            o.table = Some(ObjectRef::table(Some("dbo"), "items"));
        }),
        "UPDATE dbo.items SET name = N'plain', price = 9.50 WHERE id = 1;\n\
         UPDATE dbo.items SET name = N'a,b \"q\"\nline2', price = NULL WHERE id = 2;\n\
         UPDATE dbo.items SET name = N'ünïcødé 日本', price = -1.25 WHERE id = 3;\n"
    );
    // NULL key -> IS NULL
    let o = ExportOptions { key_columns: vec!["price".into()], ..ExportOptions::new(ExportFormat::SqlUpdate) };
    let s = export_to_string(&cols(), &rows()[1..2], &o).unwrap();
    assert_eq!(s, "UPDATE my_table SET id = 2, name = 'a,b \"q\"\nline2' WHERE price IS NULL;\n");
    let o = ExportOptions::new(ExportFormat::SqlUpdate);
    assert!(export_to_string(&cols(), &rows(), &o).is_err());
}

#[test]
fn markdown() {
    assert_eq!(
        export(ExportFormat::Markdown, |o| o.null_text = "NULL".into()),
        "|  id | name             | price |\n\
         | --: | ---------------- | ----: |\n\
         |   1 | plain            |  9.50 |\n\
         |   2 | a,b \"q\"<br>line2 |  NULL |\n\
         |   3 | ünïcødé 日本     | -1.25 |\n"
    );
    let cols = vec![ColumnMeta::new("a|b", "text")];
    let rows = vec![vec![Value::from("x|y")]];
    let s = export_to_string(&cols, &rows, &ExportOptions::new(ExportFormat::Markdown)).unwrap();
    assert_eq!(s, "| a\\|b |\n| ---- |\n| x\\|y |\n");
}

#[test]
fn text_table() {
    assert_eq!(
        export(ExportFormat::TextTable, |_| {}),
        "+----+----------------+-------+\n\
         | id | name           | price |\n\
         +----+----------------+-------+\n\
         |  1 | plain          |  9.50 |\n\
         |  2 | a,b \"q\"\\nline2 |       |\n\
         |  3 | ünïcødé 日本   | -1.25 |\n\
         +----+----------------+-------+\n"
    );
}

#[test]
fn html() {
    let cols = vec![ColumnMeta::new("<b>", "text"), ColumnMeta::new("n", "int")];
    let rows = vec![vec![Value::from("a & <script>\"'"), Value::Null]];
    let o = ExportOptions { table: Some(ObjectRef::table(None, "t&t")), ..ExportOptions::new(ExportFormat::Html) };
    let s = export_to_string(&cols, &rows, &o).unwrap();
    assert!(s.starts_with("<!DOCTYPE html>\n<html>"));
    assert!(s.contains("<title>t&amp;t</title>"));
    assert!(s.contains("<thead>\n<tr><th>&lt;b&gt;</th><th>n</th></tr>\n</thead>"));
    assert!(s.contains("<tr><td>a &amp; &lt;script&gt;&quot;&#39;</td><td class=\"null\"></td></tr>"));
    assert!(s.ends_with("</tbody>\n</table>\n</body>\n</html>\n"));
}

#[test]
fn xml() {
    let cols = vec![
        ColumnMeta::new("id", "int"),
        ColumnMeta::new("first name", "text"),
        ColumnMeta::new("1st", "text"),
        ColumnMeta::new("xmlthing", "text"),
        ColumnMeta::new("id", "int"),
        ColumnMeta::new("bin", "bytea"),
    ];
    let rows = vec![vec![
        Value::Int(1),
        Value::from("a<b & \"c\"\u{1}"),
        Value::Null,
        Value::from("x"),
        Value::Int(2),
        Value::Bytes(vec![1, 2]),
    ]];
    let s = export_to_string(&cols, &rows, &ExportOptions::new(ExportFormat::Xml)).unwrap();
    assert_eq!(
        s,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<data xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n  <row>\n    <id>1</id>\n    <first_name>a&lt;b &amp; &quot;c&quot;</first_name>\n    <_1st xsi:nil=\"true\"/>\n    <_xmlthing>x</_xmlthing>\n    <id_2>2</id_2>\n    <bin>AQI=</bin>\n  </row>\n</data>\n"
    );
    let o = ExportOptions { pretty: false, ..ExportOptions::new(ExportFormat::Xml) };
    let s = export_to_string(&cols[..1], &[vec![Value::Int(5)]], &o).unwrap();
    assert!(s.ends_with("<data xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"><row><id>5</id></row></data>"));
}

#[test]
fn streaming_matches_one_shot() {
    #[derive(Clone)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for &format in ExportFormat::ALL {
        if format.is_binary() || format == ExportFormat::SqlUpdate {
            continue;
        }
        let mut o = ExportOptions::new(format);
        o.rows_per_insert = 2;
        let buf = Shared(Default::default());
        let mut s = StreamingExporter::new(cols(), o.clone(), Box::new(buf.clone())).unwrap();
        let all = rows();
        s.write_rows(&all[..1]).unwrap();
        s.write_rows(&all[1..]).unwrap();
        s.write_rows(&[]).unwrap();
        s.finish().unwrap();
        let streamed = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(streamed, export_to_string(&cols(), &all, &o).unwrap(), "{format:?}");
    }
    assert!(StreamingExporter::new(cols(), ExportOptions::new(ExportFormat::Xlsx), Box::new(std::io::sink())).is_err());
}

#[test]
fn xlsx_round_trip() {
    use calamine::{open_workbook, Data, Reader, Xlsx};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.xlsx");
    let cols = vec![
        ColumnMeta::new("id", "int"),
        ColumnMeta::new("name", "text"),
        ColumnMeta::new("price", "numeric"),
        ColumnMeta::new("ok", "bool"),
        ColumnMeta::new("day", "date"),
        ColumnMeta::new("at", "timestamp"),
        ColumnMeta::new("big", "bigint"),
    ];
    let day = NaiveDate::from_ymd_opt(2023, 12, 31).unwrap();
    let at = day.and_hms_opt(8, 30, 15).unwrap();
    let rows = vec![
        vec![
            Value::Int(1),
            "ünï, \"q\"".into(),
            Value::Decimal("12.5".into()),
            Value::Bool(true),
            Value::Date(day),
            Value::DateTime(at),
            Value::Int(i64::MAX),
        ],
        vec![Value::Int(2), Value::Null, Value::Float(0.25), Value::Bool(false), Value::Null, Value::Null, Value::Null],
    ];
    let o = ExportOptions { table: Some(ObjectRef::table(None, "my/sheet")), ..ExportOptions::new(ExportFormat::Xlsx) };
    export_to_file(&cols, &rows, &o, &path).unwrap();
    assert!(export_to_string(&cols, &rows, &o).is_err());

    let mut wb: Xlsx<_> = open_workbook(&path).unwrap();
    assert_eq!(wb.sheet_names(), vec!["my_sheet".to_string()]);
    let range = wb.worksheet_range("my_sheet").unwrap();
    let r: Vec<Vec<Data>> = range.rows().map(|r| r.to_vec()).collect();
    assert_eq!(r[0][0], Data::String("id".into()));
    assert_eq!(r[1][0], Data::Float(1.0));
    assert_eq!(r[1][1], Data::String("ünï, \"q\"".into()));
    assert_eq!(r[1][2], Data::Float(12.5));
    assert_eq!(r[1][3], Data::Bool(true));
    match &r[1][4] {
        Data::DateTime(d) => assert_eq!(d.as_datetime().unwrap().date(), day),
        other => panic!("{other:?}"),
    }
    match &r[1][5] {
        Data::DateTime(d) => assert_eq!(d.as_datetime().unwrap(), at),
        other => panic!("{other:?}"),
    }
    // too large for f64 precision -> stored as text
    assert_eq!(r[1][6], Data::String(i64::MAX.to_string()));
    assert_eq!(r[2][1], Data::Empty);

    // via the importer
    let imported = crate::import::read_file(&path, &crate::import::ImportOptions::new(crate::import::ImportFormat::Xlsx)).unwrap();
    assert_eq!(imported.columns, vec!["id", "name", "price", "ok", "day", "at", "big"]);
    assert_eq!(imported.rows[0][0], Value::Int(1));
    assert_eq!(imported.rows[0][4], Value::Date(day));
    assert_eq!(imported.rows[0][5], Value::DateTime(at));
    assert_eq!(imported.rows[1][2], Value::Float(0.25));
    assert_eq!(imported.rows[1][4], Value::Null);

    let bytes = export_xlsx_to_buffer(&cols, &rows, &o).unwrap();
    assert!(bytes.starts_with(b"PK"));
    let mut sink = Vec::new();
    export_to_writer(&cols, &rows, &o, &mut sink).unwrap();
    assert!(sink.starts_with(b"PK"));
}

#[test]
fn csv_round_trip_through_import() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.csv");
    let o = ExportOptions { null_text: "\\N".into(), ..ExportOptions::new(ExportFormat::Csv) };
    export_to_file(&cols(), &rows(), &o, &path).unwrap();
    let mut io = crate::import::ImportOptions::new(crate::import::ImportFormat::Csv);
    io.null_text = Some("\\N".into());
    let data = crate::import::read_file(&path, &io).unwrap();
    assert_eq!(data.columns, vec!["id", "name", "price"]);
    let expected: Vec<Vec<Value>> = rows()
        .into_iter()
        .map(|r| r.into_iter().map(|v| if v.is_null() { Value::Null } else { Value::Text(v.to_string()) }).collect())
        .collect();
    assert_eq!(data.rows, expected);
}

#[test]
fn json_round_trip_through_import() {
    let s = export(ExportFormat::Json, |_| {});
    let data = crate::import::read_str(&s, &crate::import::ImportOptions::new(crate::import::ImportFormat::Json)).unwrap();
    assert_eq!(data.columns, vec!["id", "name", "price"]);
    assert_eq!(data.rows[0], vec![Value::Int(1), Value::from("plain"), Value::Float(9.5)]);
    assert_eq!(data.rows[1], vec![Value::Int(2), Value::from("a,b \"q\"\nline2"), Value::Null]);
    let s = export(ExportFormat::JsonLines, |_| {});
    let data = crate::import::read_str(&s, &crate::import::ImportOptions::new(crate::import::ImportFormat::JsonLines)).unwrap();
    assert_eq!(data.rows.len(), 3);
    assert_eq!(data.rows[2][1], Value::from("ünïcødé 日本"));
}
