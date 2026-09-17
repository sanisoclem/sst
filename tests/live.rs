use sst::complete;
use sst::config::{Auth, Credentials, Encryption};
use sst::db;
use sst::export;

fn credentials() -> Option<Credentials> {
    let host = std::env::var("SST_TEST_HOST").ok()?;
    Some(Credentials {
        host,
        port: std::env::var("SST_TEST_PORT").ok()?.parse().ok()?,
        database: Some("ShopDB".into()),
        auth: Auth::Sql {
            username: "sa".into(),
            password: std::env::var("SST_TEST_PASSWORD").ok()?,
        },
        encrypt: Encryption::Required,
        trust_certificate: true,
    })
}

#[tokio::test]
async fn queries_renders_completes_and_exports() {
    let Some(credentials) = credentials() else {
        eprintln!("skipping: SST_TEST_HOST not set");
        return;
    };
    let mut client = db::connect(&credentials, None).await.expect("connect");

    let databases = db::databases(&mut client).await.expect("databases");
    assert!(databases.contains(&"ShopDB".to_string()), "{databases:?}");

    let results = db::run_query(
        &mut client,
        "SELECT TOP 5 * FROM Sales.Orders ORDER BY OrderID",
        1000,
    )
    .await
    .expect("query");
    let orders = &results[0];
    assert_eq!(
        orders.columns,
        [
            "OrderID",
            "CustomerID",
            "OrderDate",
            "TotalAmount",
            "Status",
            "Notes",
            "Shipped"
        ]
    );
    assert_eq!(orders.rows.len(), 5);
    assert_eq!(orders.rows[0][0].as_deref(), Some("1"));
    assert!(
        orders.rows[0][2]
            .as_deref()
            .is_some_and(|d| d.starts_with("2026-")),
        "datetime2 should render readably, got {:?}",
        orders.rows[0][2]
    );

    let audit = &db::run_query(
        &mut client,
        "SELECT TOP 3 LogID, LoggedAt, Severity, TraceGuid, Payload FROM dbo.AuditLog ORDER BY LogID",
        1000,
    )
    .await
    .expect("audit query")[0];
    assert!(
        audit.rows[0][3].as_deref().is_some_and(|g| g.len() == 36),
        "guid"
    );
    assert!(
        audit.rows[0][4]
            .as_deref()
            .is_some_and(|p| p.starts_with("0x")),
        "varbinary"
    );

    let nulls = &db::run_query(&mut client, "SELECT NULL AS a, '' AS b", 1000)
        .await
        .expect("null query")[0];
    assert_eq!(nulls.rows[0][0], None, "NULL is None");
    assert_eq!(
        nulls.rows[0][1].as_deref(),
        Some(""),
        "empty string is not NULL"
    );

    let capped = &db::run_query(&mut client, "SELECT * FROM Sales.Orders", 10)
        .await
        .expect("capped query")[0];
    assert_eq!(capped.rows.len(), 10);
    assert!(capped.truncated, "over the cap should be flagged");

    let tables = db::tables(&mut client).await.expect("tables");
    let names: Vec<String> = tables.iter().map(|t| t.qualified()).collect();
    assert!(names.contains(&"Sales.Orders".to_string()), "{names:?}");
    assert!(names.contains(&"dbo.AuditLog".to_string()), "{names:?}");

    let buffer = "SELECT o.tot FROM Sales.Orders o";
    let found = complete::candidates(&tables, buffer, buffer, "SELECT o.tot".len());
    assert_eq!(
        found.first().map(|c| c.label.as_str()),
        Some("TotalAmount"),
        "alias-qualified column, got {found:?}"
    );
    assert_eq!(found[0].detail, "decimal");

    let from = "SELECT * FROM Sales.";
    let after_from = complete::candidates(&tables, from, from, from.len());
    let labels: Vec<&str> = after_from.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["Customers", "Orders"], "schema-qualified tables");

    {
        let directory = std::env::temp_dir().join("sqltui-stream-test");
        std::fs::create_dir_all(&directory).unwrap();

        let shown = &db::run_query(&mut client, "SELECT * FROM Sales.Orders", 1000)
            .await
            .expect("capped display")[0];
        assert_eq!(shown.rows.len(), 1000, "grid holds the cap");
        assert!(shown.truncated);

        let path = directory.join("all.csv");
        let mut writer = export::Writer::create(&path).unwrap();
        let written = db::stream_to_file(&mut client, "SELECT * FROM Sales.Orders", 0, &mut writer)
            .await
            .expect("stream");
        writer.finish(&path).unwrap();
        assert_eq!(
            written, 2000,
            "every row reaches the file, not just the cap"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            2001,
            "header plus 2000 rows"
        );

        let second = directory.join("second.xlsx");
        let mut writer = export::Writer::create(&second).unwrap();
        let written = db::stream_to_file(
            &mut client,
            "SELECT TOP 5 * FROM Sales.Customers; SELECT TOP 7 * FROM dbo.AuditLog",
            1,
            &mut writer,
        )
        .await
        .expect("second result set");
        writer.finish(&second).unwrap();
        assert_eq!(written, 7, "the chosen result set, not the first");

        let _ = std::fs::remove_dir_all(&directory);
    }

    let directory = std::env::temp_dir().join("sqltui-export-test");
    std::fs::create_dir_all(&directory).unwrap();

    let csv_path = directory.join("out.csv");
    let mut writer = export::Writer::create(&csv_path).unwrap();
    db::stream_to_file(
        &mut client,
        "SELECT TOP 5 * FROM Sales.Orders",
        0,
        &mut writer,
    )
    .await
    .unwrap();
    writer.finish(&csv_path).unwrap();
    let text = std::fs::read_to_string(&csv_path).unwrap();
    assert!(
        text.starts_with("OrderID,CustomerID,OrderDate"),
        "{}",
        &text[..40]
    );
    assert_eq!(text.lines().count(), 6, "header plus five rows");

    let xlsx_path = directory.join("out.xlsx");
    let mut writer = export::Writer::create(&xlsx_path).unwrap();
    db::stream_to_file(
        &mut client,
        "SELECT TOP 5 * FROM Sales.Orders",
        0,
        &mut writer,
    )
    .await
    .unwrap();
    writer.finish(&xlsx_path).unwrap();
    let bytes = std::fs::read(&xlsx_path).unwrap();
    assert!(bytes.starts_with(b"PK"), "xlsx is a zip");

    assert!(
        export::Writer::create(&directory.join("out.txt")).is_err(),
        "unknown extension"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn every_encryption_level_connects() {
    let Some(base) = credentials() else {
        eprintln!("skipping: SST_TEST_HOST not set");
        return;
    };

    for level in [
        Encryption::Required,
        Encryption::On,
        Encryption::LoginOnly,
        Encryption::Off,
    ] {
        let credentials = Credentials {
            encrypt: level,
            ..base.clone()
        };
        let mut client = db::connect(&credentials, None)
            .await
            .unwrap_or_else(|error| panic!("{} failed: {error}", level.label()));

        let rows = db::run_query(&mut client, "SELECT TOP 1 OrderID FROM Sales.Orders", 10)
            .await
            .unwrap_or_else(|error| panic!("{} query failed: {error}", level.label()));
        assert_eq!(
            rows[0].rows[0][0].as_deref(),
            Some("1"),
            "{}",
            level.label()
        );
    }
}
