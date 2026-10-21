//! Generated spreadsheets: the columns, or the sheets and their columns,
//! declared at creation, the rows accepted in parts against them, and the
//! build to CSV, XLSX or ODS.

use std::io::{Cursor, Read};

use agrafe::jobs::JobKind;
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::support::*;

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const ODS: &str = "application/vnd.oasis.opendocument.spreadsheet";

fn columns() -> Value {
    json!([
        {"name": "Name"},
        {"name": "Amount (€)"},
        {"name": "Approved"},
        {"name": "Date", "type": "date"},
        {"name": "Created at", "type": "datetime"},
    ])
}

fn rows_blob(filename: &str, content_type: &str) -> Value {
    json!({
        "filename": filename,
        "content_type": content_type,
        "input_type": "rows",
        "columns": columns(),
    })
}

/// A workbook of three sheets: the sample's columns, then two narrower
/// ones.
fn workbook_blob(filename: &str, content_type: &str) -> Value {
    json!({
        "filename": filename,
        "content_type": content_type,
        "input_type": "rows",
        "sheets": [
            {"name": "Expenses", "columns": columns()},
            {"name": "Reviews & notes", "columns": [{"name": "Expense"}, {"name": "Reviews"}]},
            {"name": "Empty", "columns": [{"name": "Nothing"}]},
        ],
    })
}

/// A source of `lines`, each a JSON array.
fn source(lines: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line.to_string().as_bytes());
        out.push(b'\n');
    }
    out
}

/// Three rows as the backend would send them, verbatim: the `1.10` lexeme
/// is the point, and `json!` would normalise it.
const SAMPLE: [&str; 3] = [
    r#"["Lovelace", 1.10, true, "2026-10-17", "2026-10-17T08:44:26+02:00"]"#,
    r#"["=HYPERLINK(\"x\")", -5, false, null, null]"#,
    r#"["a,b \"c\"\nd", 12345678901234567890, null, "1850-01-01", "2026-10-17T12:00:00.5Z"]"#,
];

/// A source of verbatim `lines`.
fn lines(lines: &[&str]) -> Vec<u8> {
    lines
        .iter()
        .flat_map(|l| format!("{l}\n").into_bytes())
        .collect()
}

async fn put_part(app: &TestApp, id: &str, n: u32, bytes: Vec<u8>) -> (StatusCode, Value) {
    call(
        &app.private,
        put_raw(&format!("/blobs/{id}/parts/{n}"), bytes),
    )
    .await
}

async fn finish(app: &TestApp, id: &str) -> (StatusCode, Value) {
    call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/finish")),
    )
    .await
}

/// The original of `blob`, downloaded whole through the private port.
async fn download(app: &TestApp, blob: &Value) -> Vec<u8> {
    let id = id_of(blob);
    let filename = agrafe::blob::urls::encode_segment(blob["filename"].as_str().unwrap());
    let (status, headers, bytes) = fetch(
        &app.private,
        get_with(&format!("/blobs/{id}/original/{filename}"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["content-type"],
        blob["content_type"].as_str().unwrap()
    );
    // For checking the file with other tools: `AGRAFE_ROWS_OUT=/tmp/dir`.
    if let Ok(dir) = std::env::var("AGRAFE_ROWS_OUT") {
        std::fs::write(
            format!("{dir}/{}", blob["filename"].as_str().unwrap()),
            &bytes,
        )
        .unwrap();
    }
    bytes
}

/// Every entry of a ZIP package: name, compression method and bytes, read
/// back by an independent implementation, which checks each CRC.
fn unzip(bytes: &[u8]) -> Vec<(String, zip::CompressionMethod, Vec<u8>)> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("a readable ZIP");
    (0..archive.len())
        .map(|i| {
            let mut file = archive.by_index(i).unwrap();
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap();
            (file.name().unwrap().into_owned(), file.compression(), data)
        })
        .collect()
}

fn entry<'a>(entries: &'a [(String, zip::CompressionMethod, Vec<u8>)], name: &str) -> &'a [u8] {
    &entries
        .iter()
        .find(|(n, _, _)| n == name)
        .unwrap_or_else(|| panic!("no entry {name}"))
        .2
}

/// Build the blob and return it `ready`.
async fn build(app: &TestApp, id: &str) -> Value {
    assert!(run_job(app, JobKind::Rows).await);
    let built = get_blob(app, id).await;
    assert_eq!(built["state"], "ready", "{built}");
    built
}

#[tokio::test]
async fn csv_is_built_from_rows_in_parts() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, rows_blob("export été.csv", "text/csv")).await;
    let id = id_of(&created);
    assert_eq!(created["state"], "created");
    assert_eq!(created["skip_virus_scan"], true);
    assert_eq!(created["source_parts"], 0);
    assert!(created["size"].is_null());
    assert!(created.get("columns").is_none(), "not reported back");

    let (status, body) = put_part(&app, id, 1, lines(&SAMPLE[..2])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "created");
    assert_eq!(body["source_parts"], 1);
    assert_eq!(body["source_rows"], 2);
    assert!(body["size"].is_null(), "unknown until built");
    let (status, body) = put_part(&app, id, 2, lines(&SAMPLE[2..])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], 3);
    assert!(jobs_for(&app, id).await.is_empty());

    let (status, body) = finish(&app, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(jobs_for(&app, id).await, vec!["rows"]);

    let built = build(&app, id).await;
    assert!(built["archive_entries"].is_null());
    assert_eq!(jobs_for(&app, id).await, vec!["render"]);
    let bytes = download(&app, &built).await;
    assert_eq!(built["size"], bytes.len());
    assert_eq!(&bytes[..3], b"\xEF\xBB\xBF", "byte order mark");
    let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
    assert_eq!(
        text,
        concat!(
            "Name,Amount (€),Approved,Date,Created at\r\n",
            "Lovelace,1.10,true,2026-10-17,2026-10-17T08:44:26\r\n",
            "\"'=HYPERLINK(\"\"x\"\")\",-5,false,,\r\n",
            "\"a,b \"\"c\"\"\nd\",12345678901234567890,,1850-01-01,2026-10-17T12:00:00.5\r\n",
        )
    );

    // No preview; a CSV is text, so its thumbnail is a sample of its
    // first lines, like any text file's.
    while run_job(&app, JobKind::Render).await {}
    let rendered = get_blob(&app, id).await;
    assert_eq!(rendered["preview_state"], "unavailable");
    assert_eq!(rendered["thumbnail_state"], "ready");
}

#[tokio::test]
async fn xlsx_package() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, rows_blob("Export: 2026.xlsx", XLSX)).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, lines(&SAMPLE)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    let built = build(&app, id).await;
    let entries = unzip(&download(&app, &built).await);
    let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "[Content_Types].xml",
            "_rels/.rels",
            "xl/workbook.xml",
            "xl/_rels/workbook.xml.rels",
            "xl/styles.xml",
            "xl/worksheets/sheet1.xml",
        ]
    );
    assert!(
        entries
            .iter()
            .all(|(_, method, _)| *method == zip::CompressionMethod::Deflated)
    );
    let workbook = String::from_utf8(entry(&entries, "xl/workbook.xml").to_vec()).unwrap();
    assert!(
        workbook.contains(r#"<sheet name="Export_ 2026" sheetId="1" r:id="rId1"/>"#),
        "{workbook}"
    );
    let sheet = String::from_utf8(entry(&entries, "xl/worksheets/sheet1.xml").to_vec()).unwrap();
    assert!(sheet.starts_with("<?xml"), "{sheet}");
    assert!(sheet.ends_with("</sheetData></worksheet>"), "{sheet}");
    for want in [
        // The header, inline strings.
        r#"<row r="1"><c r="A1" t="inlineStr"><is><t xml:space="preserve">Name</t></is></c>"#,
        r#"<c r="E1" t="inlineStr"><is><t xml:space="preserve">Created at</t></is></c></row>"#,
        // Typed cells; the datetime's offset dropped.
        r#"<row r="2"><c r="A2" t="inlineStr"><is><t xml:space="preserve">Lovelace</t></is></c><c r="B2"><v>1.10</v></c><c r="C2" t="b"><v>1</v></c><c r="D2" s="1"><v>46312</v></c><c r="E2" s="2"><v>46312.3641898"#,
        // A formula is text, which Excel never evaluates; empty cells are left out.
        r#"<row r="3"><c r="A3" t="inlineStr"><is><t xml:space="preserve">=HYPERLINK("x")</t></is></c><c r="B3"><v>-5</v></c><c r="C3" t="b"><v>0</v></c></row>"#,
        // Line breaks kept; a date Excel cannot show as a serial is text.
        "<c r=\"A4\" t=\"inlineStr\"><is><t xml:space=\"preserve\">a,b \"c\"\nd</t></is></c><c r=\"B4\"><v>12345678901234567890</v></c><c r=\"D4\" t=\"inlineStr\"><is><t xml:space=\"preserve\">1850-01-01</t></is></c><c r=\"E4\" s=\"2\"><v>46312.5000057",
    ] {
        assert!(sheet.contains(want), "missing {want}\nin {sheet}");
    }
    let styles = String::from_utf8(entry(&entries, "xl/styles.xml").to_vec()).unwrap();
    assert!(styles.contains(r#"numFmtId="14""#) && styles.contains(r#"numFmtId="22""#));
}

#[tokio::test]
async fn ods_package() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, rows_blob("export.ods", ODS)).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, lines(&SAMPLE)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let built = build(&app, id).await;
    let bytes = download(&app, &built).await;
    // The format's detection: `mimetype` first, stored, no extra field,
    // so the media type sits at a fixed offset.
    assert_eq!(&bytes[30..38], b"mimetype");
    assert_eq!(&bytes[38..38 + ODS.len()], ODS.as_bytes());
    let entries = unzip(&bytes);
    let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, ["mimetype", "META-INF/manifest.xml", "content.xml"]);
    assert_eq!(entries[0].1, zip::CompressionMethod::Stored);
    assert_eq!(entries[2].1, zip::CompressionMethod::Deflated);
    let content = String::from_utf8(entry(&entries, "content.xml").to_vec()).unwrap();
    for want in [
        r#"<table:table table:name="export"><table:table-column table:number-columns-repeated="5"/>"#,
        r#"<table:table-row><table:table-cell office:value-type="string"><text:p>Name</text:p></table:table-cell>"#,
        r#"<table:table-cell office:value-type="float" office:value="1.10"><text:p>1.10</text:p></table:table-cell>"#,
        r#"<table:table-cell office:value-type="boolean" office:boolean-value="true"><text:p>true</text:p></table:table-cell>"#,
        r#"<table:table-cell office:value-type="date" office:date-value="2026-10-17"><text:p>2026-10-17</text:p></table:table-cell>"#,
        r#"<table:table-cell office:value-type="date" office:date-value="2026-10-17T08:44:26"><text:p>2026-10-17T08:44:26</text:p></table:table-cell>"#,
        r#"<table:table-cell office:value-type="string"><text:p>=HYPERLINK("x")</text:p></table:table-cell>"#,
        r#"<table:table-cell/><table:table-cell/></table:table-row>"#,
        r#"<table:table-cell office:value-type="string"><text:p>a,b "c"</text:p><text:p>d</text:p></table:table-cell>"#,
        r#"<table:table-cell office:value-type="date" office:date-value="1850-01-01">"#,
        r#"office:date-value="2026-10-17T12:00:00.5""#,
    ] {
        assert!(content.contains(want), "missing {want}\nin {content}");
    }
    assert!(
        content.ends_with(
            "</table:table></office:spreadsheet></office:body></office:document-content>"
        )
    );
}

/// A source with no lines produces a header alone, in every format.
#[tokio::test]
async fn empty_source_is_a_header() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    for (filename, content_type) in [("e.csv", "text/csv"), ("e.xlsx", XLSX), ("e.ods", ODS)] {
        let created = create(&app, rows_blob(filename, content_type)).await;
        let id = id_of(&created);
        let (status, body) = put_private(&app, id, vec![]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["source_rows"], 0);
        let built = build(&app, id).await;
        let bytes = download(&app, &built).await;
        if content_type == "text/csv" {
            assert_eq!(
                bytes,
                b"\xEF\xBB\xBFName,Amount (\xE2\x82\xAC),Approved,Date,Created at\r\n"
            );
        } else {
            let entries = unzip(&bytes);
            let sheet = entries.last().unwrap();
            let xml = String::from_utf8(sheet.2.clone()).unwrap();
            assert!(xml.contains("Name"), "{xml}");
            assert!(
                !xml.contains("r=\"2\"") && !xml.contains("<table:table-row><table:table-cell/>"),
                "{xml}"
            );
        }
    }
}

#[tokio::test]
async fn creation_checks_the_columns() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let post = |body: Value| call(&app.private, json_req(Method::POST, "/blobs", body));
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "missing",
            json!({"filename": "e.csv", "content_type": "text/csv", "input_type": "rows"}),
            "requires columns or sheets",
        ),
        (
            "empty",
            json!({"filename": "e.csv", "content_type": "text/csv", "input_type": "rows", "columns": []}),
            "at least one",
        ),
        (
            "unnamed",
            json!({"filename": "e.csv", "content_type": "text/csv", "input_type": "rows", "columns": [{"name": ""}]}),
            "empty name",
        ),
        (
            "unknown type",
            json!({"filename": "e.csv", "content_type": "text/csv", "input_type": "rows", "columns": [{"name": "a", "type": "time"}]}),
            "type",
        ),
        (
            "unknown key",
            json!({"filename": "e.csv", "content_type": "text/csv", "input_type": "rows", "columns": [{"name": "a", "width": 3}]}),
            "width",
        ),
        (
            "not for archives",
            json!({"filename": "e.zip", "content_type": "application/zip", "input_type": "archive", "columns": [{"name": "a"}]}),
            "rows input only",
        ),
        (
            "not for files",
            json!({"filename": "e.png", "content_type": "image/png", "input_type": "raw", "checksum": raw_blob()["checksum"], "size": 1, "columns": [{"name": "a"}]}),
            "rows input only",
        ),
        (
            "wrong output type",
            json!({"filename": "e.pdf", "content_type": "application/pdf", "input_type": "rows", "columns": [{"name": "a"}]}),
            "cannot be produced",
        ),
        (
            "control character in a name",
            json!({"filename": "e.csv", "content_type": "text/csv", "input_type": "rows", "columns": [{"name": "a\u{202E}b"}]}),
            "column 1 has a name with control characters",
        ),
        (
            "name longer than a cell",
            json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "columns": [{"name": "n".repeat(32_768)}]}),
            "column 1 has a name longer than the 32767 characters",
        ),
    ];
    for (name, body, says) in cases {
        let (status, res) = post(body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {res}");
        let message = res["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{name}: {res}");
    }
    let many: Vec<Value> = (0..=16_384)
        .map(|i| json!({"name": i.to_string()}))
        .collect();
    let (status, res) = post(
        json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "columns": many}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
    assert!(
        res["error"]["message"].as_str().unwrap().contains("16384"),
        "{res}"
    );
    let many: Vec<Value> = (0..=16_384)
        .map(|i| json!({"name": i.to_string()}))
        .collect();
    let (status, res) = post(json!({"filename": "e.csv", "content_type": "text/csv", "input_type": "rows", "columns": many})).await;
    assert_eq!(status, StatusCode::CREATED, "{res}");

    // The request body has a limit of its own, above axum's default.
    let wide: Vec<Value> = (0..16_384)
        .map(|i| json!({"name": format!("{i:0>600}")}))
        .collect();
    let (status, res) = post(
        json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "columns": wide}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{res}");
    let huge: Vec<Value> = (0..16_384)
        .map(|i| json!({"name": format!("{i:0>1100}")}))
        .collect();
    let (status, res) = post(
        json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "columns": huge}),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{res}");
}

/// Every bad line answers 422 naming it and the cell; the part is
/// discarded and the blob keeps the parts it had.
#[tokio::test]
async fn row_rejections() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, rows_blob("bad.xlsx", XLSX)).await;
    let id = id_of(&created);
    let (status, body) = put_part(&app, id, 1, lines(&SAMPLE[..1])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "not json",
            b"nope\n".to_vec(),
            "line 1: expected a JSON array",
        ),
        (
            "an object that is not a switch",
            b"{\"Name\": \"x\"}\n".to_vec(),
            "line 1: expected {\"sheet\": \"...\"} to switch sheets",
        ),
        (
            "too short",
            b"[\"x\", 1, true, null]\n".to_vec(),
            "expected 5 cells",
        ),
        (
            "too long",
            b"[\"x\", 1, true, null, null, 1]\n".to_vec(),
            "got 6",
        ),
        (
            "nested",
            b"[[\"x\"], 1, true, null, null]\n".to_vec(),
            "cell 1 (Name): must be a scalar",
        ),
        (
            "object cell",
            b"[{}, 1, true, null, null]\n".to_vec(),
            "cell 1 (Name)",
        ),
        (
            "huge number",
            b"[\"x\", 1e999, true, null, null]\n".to_vec(),
            "cell 2 (Amount (€)): is a number too large",
        ),
        (
            "bad date",
            b"[\"x\", 1, true, \"17/10/2026\", null]\n".to_vec(),
            "cell 4 (Date): must be a date as YYYY-MM-DD",
        ),
        (
            "date as number",
            b"[\"x\", 1, true, 20261017, null]\n".to_vec(),
            "cell 4 (Date)",
        ),
        (
            "bad datetime",
            b"[\"x\", 1, true, null, \"2026-10-17\"]\n".to_vec(),
            "cell 5 (Created at): must be a datetime",
        ),
        (
            "second line",
            b"[\"x\", 1, true, null, null]\n[\"x\", 1, \"yes\", \"nope\", null]\n".to_vec(),
            "line 2: cell 4 (Date)",
        ),
        (
            "no newline",
            b"[\"x\", 1, true, null, null]".to_vec(),
            "newline",
        ),
    ];
    for (name, bytes, says) in cases {
        let (status, body) = put_part(&app, id, 2, bytes).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{name}: {body}");
        let got = get_blob(&app, id).await;
        assert_eq!(got["state"], "created", "{name}");
        assert_eq!(got["source_parts"], 1, "{name}");
        assert_eq!(got["source_rows"], 1, "{name}");
    }
    // A boolean is a scalar like any other in an untyped column, and a
    // string that looks like a date stays text there.
    let (status, body) = put_part(
        &app,
        id,
        2,
        b"[true, \"n/a\", \"2026-10-17\", null, null]\n".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], 2);
}

/// A sheet cell holds 32 767 characters; a longer text is refused at
/// accept for a sheet format and taken for CSV.
#[tokio::test]
async fn sheet_cell_limit() {
    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.upload.max_size = bytesize::ByteSize::kib(256);
    })
    .await;
    let long = format!("[\"{}\", 1, true, null, null]\n", "x".repeat(32_768)).into_bytes();
    for (filename, content_type, limited) in [("c.xlsx", XLSX, true), ("c.csv", "text/csv", false)]
    {
        let created = create(&app, rows_blob(filename, content_type)).await;
        let (status, body) = put_part(&app, id_of(&created), 1, long.clone()).await;
        if limited {
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(
                message.contains("cell 1 (Name): text is longer than the 32767 characters"),
                "{body}"
            );
        } else {
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }
}

/// A sheet holds 1 048 576 rows, the header included; the source is
/// refused at the line that would cross it, whichever part it is in. CSV
/// has no such limit.
#[tokio::test]
async fn sheet_row_limit() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let one = json!(["x", 1, true, null, null]);
    for (filename, content_type, limited) in [("l.xlsx", XLSX, true), ("l.csv", "text/csv", false)]
    {
        let created = create(&app, rows_blob(filename, content_type)).await;
        let id = id_of(&created);
        let (status, _) = put_part(&app, id, 1, source(std::slice::from_ref(&one))).await;
        assert_eq!(status, StatusCode::OK);
        // As if 1 048 572 rows had landed: three more fit, a fourth does not.
        sqlx::query(
            "UPDATE blobs SET source_rows = 1048572, source_sheet_rows = 1048572 WHERE id = $1",
        )
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
        let (status, body) = put_part(&app, id, 2, source(&vec![one.clone(); 4])).await;
        if limited {
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.starts_with("line 4:"), "{body}");
            assert!(message.contains("1048576"), "{body}");
            assert_eq!(get_blob(&app, id).await["source_parts"], 1);
            let (status, body) = put_part(&app, id, 2, source(&vec![one.clone(); 3])).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["source_rows"], 1_048_575);
        } else {
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }
}

/// A workbook: the sheets declared in order, the source switching between
/// them with `{"sheet": "..."}` lines, in any part, and every sheet in
/// the package, a header alone for one no switch pointed at. A switch
/// naming the current sheet, the first included, changes nothing: a
/// backend announcing every sheet in a loop needs no special case, and
/// the build keeps reading the sheet instead of ending it.
#[tokio::test]
async fn xlsx_workbook() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, workbook_blob("expenses.xlsx", XLSX)).await;
    let id = id_of(&created);
    assert!(created.get("sheets").is_none(), "not reported back");
    // Rows before any switch belong to the first sheet, announced or not;
    // a switch may come in a later part than the rows before it.
    let (status, body) = put_part(
        &app,
        id,
        1,
        lines(&[r#"{"sheet": "Expenses"}"#, SAMPLE[0], SAMPLE[1]]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = put_part(
        &app,
        id,
        2,
        lines(&[
            SAMPLE[2],
            r#"{"sheet": "Reviews & notes"}"#,
            r#"["Lovelace", "Approved"]"#,
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], 4, "switch lines are not rows");
    let (status, body) = put_part(
        &app,
        id,
        3,
        lines(&[
            r#"{"sheet": "Reviews & notes"}"#,
            r#"["Hopper", "Rejected — late"]"#,
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], 5);
    let (status, body) = finish(&app, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let built = build(&app, id).await;
    let entries = unzip(&download(&app, &built).await);
    let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "[Content_Types].xml",
            "_rels/.rels",
            "xl/workbook.xml",
            "xl/_rels/workbook.xml.rels",
            "xl/styles.xml",
            "xl/worksheets/sheet1.xml",
            "xl/worksheets/sheet2.xml",
            "xl/worksheets/sheet3.xml",
        ]
    );
    let text = |name: &str| String::from_utf8(entry(&entries, name).to_vec()).unwrap();
    let workbook = text("xl/workbook.xml");
    assert!(
        workbook.contains(concat!(
            r#"<sheets><sheet name="Expenses" sheetId="1" r:id="rId1"/>"#,
            r#"<sheet name="Reviews &amp; notes" sheetId="2" r:id="rId2"/>"#,
            r#"<sheet name="Empty" sheetId="3" r:id="rId3"/></sheets>"#
        )),
        "{workbook}"
    );
    let rels = text("xl/_rels/workbook.xml.rels");
    assert!(
        rels.contains(r#"Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet3.xml""#),
        "{rels}"
    );
    assert!(
        rels.contains(r#"Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles""#),
        "{rels}"
    );
    let types = text("[Content_Types].xml");
    assert_eq!(types.matches("spreadsheetml.worksheet+xml").count(), 3);

    let first = text("xl/worksheets/sheet1.xml");
    assert_eq!(first.matches("<row ").count(), 4, "header and three rows");
    assert!(
        first.contains("Lovelace") && first.contains(r#"<row r="4">"#),
        "{first}"
    );
    let second = text("xl/worksheets/sheet2.xml");
    assert_eq!(second.matches("<row ").count(), 3, "header and two rows");
    assert!(
        second.contains(r#"<row r="1"><c r="A1" t="inlineStr"><is><t xml:space="preserve">Expense</t></is></c><c r="B1" t="inlineStr"><is><t xml:space="preserve">Reviews</t></is></c></row>"#),
        "{second}"
    );
    assert!(
        second.contains("Approved") && second.contains("Rejected — late"),
        "{second}"
    );
    let third = text("xl/worksheets/sheet3.xml");
    assert_eq!(third.matches("<row ").count(), 1, "a header alone");
    assert!(third.contains("Nothing"), "{third}");
}

#[tokio::test]
async fn ods_workbook() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, workbook_blob("expenses.ods", ODS)).await;
    let id = id_of(&created);
    // Straight to the last sheet: the ones skipped get a header alone.
    let (status, body) = put_private(
        &app,
        id,
        lines(&[SAMPLE[0], r#"{"sheet": "Empty"}"#, r#"["not so empty"]"#]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], 2);
    let built = build(&app, id).await;
    let entries = unzip(&download(&app, &built).await);
    let content = String::from_utf8(entry(&entries, "content.xml").to_vec()).unwrap();
    let tables: Vec<usize> = content
        .match_indices("<table:table ")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(tables.len(), 3, "{content}");
    let (first, rest) = content.split_at(tables[1]);
    let (second, third) = rest.split_at(tables[2] - tables[1]);
    assert!(
        first.contains(r#"<table:table table:name="Expenses"><table:table-column table:number-columns-repeated="5"/>"#),
        "{first}"
    );
    assert_eq!(first.matches("<table:table-row>").count(), 2);
    assert!(first.contains("Lovelace"), "{first}");
    assert!(
        second.contains(r#"<table:table table:name="Reviews &amp; notes"><table:table-column table:number-columns-repeated="2"/>"#),
        "{second}"
    );
    assert_eq!(second.matches("<table:table-row>").count(), 1, "{second}");
    assert!(
        third.starts_with(r#"<table:table table:name="Empty"><table:table-column table:number-columns-repeated="1"/>"#),
        "{third}"
    );
    assert_eq!(third.matches("<table:table-row>").count(), 2, "{third}");
    assert!(third.contains("not so empty"), "{third}");
    assert!(
        third.ends_with(
            "</table:table></office:spreadsheet></office:body></office:document-content>"
        ),
        "{third}"
    );
}

/// A switch must name the current sheet or a declared one after it; a wrong one
/// fails the part like any bad line, and the blob keeps what it had.
#[tokio::test]
async fn sheet_switch_rejections() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, workbook_blob("switch.xlsx", XLSX)).await;
    let id = id_of(&created);
    let (status, body) = put_part(
        &app,
        id,
        1,
        lines(&[
            SAMPLE[0],
            r#"{"sheet": "Reviews & notes"}"#,
            r#"["a", "b"]"#,
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cases: Vec<(&str, &str, &str)> = vec![
        (
            "undeclared",
            r#"{"sheet": "Résumés"}"#,
            "line 1: no sheet named \"Résumés\" is declared",
        ),
        (
            "case matters",
            r#"{"sheet": "empty"}"#,
            "no sheet named \"empty\"",
        ),
        (
            "backwards",
            r#"{"sheet": "Expenses"}"#,
            "line 1: sheet \"Expenses\" comes before the current sheet \"Reviews & notes\"",
        ),
        (
            "not a switch",
            r#"{"sheet": "Empty", "rows": 2}"#,
            "line 1: expected {\"sheet\": \"...\"} to switch sheets",
        ),
        ("not a name", r#"{"sheet": 3}"#, "to switch sheets"),
        (
            "rows against the current sheet",
            r#"["a", "b", "c"]"#,
            "expected 2 cells",
        ),
        (
            "rows against the new sheet",
            "{\"sheet\": \"Empty\"}\n[\"a\", \"b\"]",
            "line 2: expected 1 cells",
        ),
    ];
    for (name, line, says) in cases {
        let (status, body) = put_part(&app, id, 2, lines(&[line])).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{name}: {body}");
        let got = get_blob(&app, id).await;
        assert_eq!(got["source_parts"], 1, "{name}");
        assert_eq!(got["source_rows"], 2, "{name}");
    }
    // The position survives a refused part: still on the second sheet, and
    // naming it again is a no-op, not a switch.
    let (status, body) = put_part(
        &app,
        id,
        2,
        lines(&[r#"{"sheet": "Reviews & notes"}"#, r#"["c", "d"]"#]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], 3);

    // A blob declared with `columns` has one sheet, so no switch can name a
    // later one, whatever the format; naming the one sheet is allowed.
    for (filename, content_type) in [("one.xlsx", XLSX), ("one.csv", "text/csv")] {
        let created = create(&app, rows_blob(filename, content_type)).await;
        let (status, body) = put_part(
            &app,
            id_of(&created),
            1,
            lines(&[r#"{"sheet": "Reviews"}"#]),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("no sheet named \"Reviews\""), "{body}");
        let (status, body) =
            put_part(&app, id_of(&created), 1, lines(&[r#"{"sheet": "one"}"#])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["source_rows"], 0, "a switch is not a row");
    }
}

/// The row limit is per sheet: a switch starts the count over, while
/// naming the current sheet again does not.
#[tokio::test]
async fn sheet_row_limit_is_per_sheet() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, workbook_blob("per-sheet.xlsx", XLSX)).await;
    let id = id_of(&created);
    let one = json!(["x", 1, true, null, null]);
    let (status, _) = put_part(&app, id, 1, source(std::slice::from_ref(&one))).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query(
        "UPDATE blobs SET source_rows = 1048575, source_sheet_rows = 1048575 WHERE id = $1",
    )
    .bind(parse_id(id))
    .execute(app.state.pool())
    .await
    .unwrap();
    let mut same = lines(&[r#"{"sheet": "Expenses"}"#]);
    same.extend(source(std::slice::from_ref(&one)));
    let (status, body) = put_part(&app, id, 2, same).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sheet \"Expenses\" would exceed 1048576 rows"),
        "{body}"
    );
    let mut next = lines(&[r#"{"sheet": "Reviews & notes"}"#]);
    next.extend(source(&vec![json!(["a", "b"]); 4]));
    let (status, body) = put_part(&app, id, 2, next).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], 1_048_579);
}

#[tokio::test]
async fn creation_checks_the_sheets() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let post = |body: Value| call(&app.private, json_req(Method::POST, "/blobs", body));
    let cols = json!([{"name": "a"}]);
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "csv",
            json!({"filename": "e.csv", "content_type": "text/csv", "input_type": "rows", "sheets": [{"name": "s", "columns": cols}]}),
            "a CSV holds one sheet",
        ),
        (
            "both",
            json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "columns": cols, "sheets": [{"name": "s", "columns": cols}]}),
            "mutually exclusive",
        ),
        (
            "none",
            json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "sheets": []}),
            "at least one sheet",
        ),
        (
            "not for archives",
            json!({"filename": "e.zip", "content_type": "application/zip", "input_type": "archive", "sheets": [{"name": "s", "columns": cols}]}),
            "sheets apply to rows input only",
        ),
        (
            "too many",
            json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "sheets": (0..1001).map(|i| json!({"name": format!("s{i}"), "columns": cols})).collect::<Vec<_>>()}),
            "at most 1000 sheets",
        ),
        (
            "forbidden character",
            json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "sheets": [{"name": "a/b", "columns": cols}]}),
            "sheet 1 has a name that must not contain any of [ ] : * ? / \\",
        ),
        (
            "too long",
            json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "sheets": [{"name": "n".repeat(32), "columns": cols}]}),
            "31 characters",
        ),
        (
            "duplicate ignoring case",
            json!({"filename": "e.ods", "content_type": ODS, "input_type": "rows", "sheets": [{"name": "Reviews", "columns": cols}, {"name": "REVIEWS", "columns": cols}]}),
            "sheet 2 has the same name as sheet 1",
        ),
        (
            "unknown key",
            json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "sheets": [{"name": "s", "columns": cols, "hidden": true}]}),
            "hidden",
        ),
        (
            "a sheet's columns",
            json!({"filename": "e.xlsx", "content_type": XLSX, "input_type": "rows", "sheets": [{"name": "s", "columns": cols}, {"name": "t", "columns": [{"name": "a\u{202E}b"}]}]}),
            "sheet 2: column 1 has a name with control characters",
        ),
    ];
    for (name, body, says) in cases {
        let (status, res) = post(body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {res}");
        let message = res["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{name}: {res}");
    }
}

/// A build that cannot succeed, here a row stripped of its key material,
/// is dead after its first attempt; the blob stays `uploaded`.
#[tokio::test]
async fn permanent_rows_build_failures_die_at_once() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, rows_blob("broken.csv", "text/csv")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, lines(&SAMPLE)).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("UPDATE blobs SET wrapped_key = NULL WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Rows).await);
    let (attempts, dead, error): (i32, bool, String) = sqlx::query_as(
        "SELECT attempts, failed_at IS NOT NULL, last_error FROM jobs \
         WHERE kind = 'rows' AND blob_id = $1",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!((attempts, dead), (1, true), "{error}");
    assert!(error.contains("wrapped key"), "{error}");
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
}

/// A source part damaged in the store, whether its length fits no stream
/// or its header does not parse, fails the build for good at the first
/// attempt: another would read the same bytes. Nothing is written.
#[tokio::test]
async fn damaged_source_parts_fail_the_build() {
    let db = TestDatabase::new().await;
    use object_store::ObjectStoreExt;
    use object_store::path::Path as ObjectPath;

    let app = setup(db).await;
    type Damage = fn(&mut Vec<u8>);
    let damages: [(&str, Damage); 2] = [
        ("which no stream of this layout produces", |b| b.truncate(5)),
        ("malformed stream header", |b| b[0] ^= 1),
    ];
    for (want, damage) in damages {
        let created = create(&app, rows_blob("damaged.csv", "text/csv")).await;
        let id = id_of(&created);
        let (status, _) = put_private(&app, id, lines(&SAMPLE)).await;
        assert_eq!(status, StatusCode::OK);
        let key = ObjectPath::from(agrafe::blob::Blob::source_key(parse_id(id), 1));
        let mut stored = app
            .state
            .store()
            .get(&key)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec();
        damage(&mut stored);
        app.state.store().put(&key, stored.into()).await.unwrap();

        assert!(run_job(&app, JobKind::Rows).await);
        let (attempts, dead, error): (i32, bool, String) = sqlx::query_as(
            "SELECT attempts, failed_at IS NOT NULL, last_error FROM jobs \
             WHERE kind = 'rows' AND blob_id = $1",
        )
        .bind(parse_id(id))
        .fetch_one(app.state.pool())
        .await
        .unwrap();
        assert_eq!((attempts, dead), (1, true), "{error}");
        assert!(error.contains(want), "{error}");
        assert_eq!(get_blob(&app, id).await["state"], "uploaded");
        let original = format!("{id}/original");
        assert!(
            !stored_objects(&app)
                .await
                .iter()
                .any(|m| m.location.as_ref() == original),
            "{want}"
        );
    }
}

/// A source larger than one flush of the writer and one encryption
/// segment streams through whole: every row comes out, in order.
#[tokio::test]
async fn long_source_streams() {
    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.upload.max_size = bytesize::ByteSize::mib(4);
    })
    .await;
    let n = 20_000;
    let lines: Vec<Value> = (0..n)
        .map(|i| json!([format!("item {i}"), i, i % 2 == 0, "2026-01-01", null]))
        .collect();
    for (filename, content_type) in [("long.csv", "text/csv"), ("long.xlsx", XLSX)] {
        let created = create(&app, rows_blob(filename, content_type)).await;
        let id = id_of(&created);
        let (status, body) = put_private(&app, id, source(&lines)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["source_rows"], n);
        let built = build(&app, id).await;
        let bytes = download(&app, &built).await;
        let text = if content_type == "text/csv" {
            String::from_utf8(bytes).unwrap()
        } else {
            let entries = unzip(&bytes);
            String::from_utf8(entry(&entries, "xl/worksheets/sheet1.xml").to_vec()).unwrap()
        };
        assert!(text.contains("item 0"), "{filename}");
        assert!(text.contains(&format!("item {}", n - 1)), "{filename}");
        let count = text.matches("item ").count();
        assert_eq!(count, n, "{filename}");
    }
}
