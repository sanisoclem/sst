use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use futures_util::StreamExt as _;
use tiberius::{ColumnData, ColumnType, Config, EncryptionLevel, QueryItem, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::config::{Credentials, Encryption};
use crate::export::Writer;

pub type Client = tiberius::Client<Compat<TcpStream>>;

#[derive(Debug, Default, Clone)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub types: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub columns: Vec<Column>,
    pub is_view: bool,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: String,
}

impl Table {
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

pub async fn connect(credentials: &Credentials, database: Option<&str>) -> Result<Client> {
    let mut config = configure(credentials, database);

    let mut address = config.get_addr();
    for _ in 0..3 {
        let tcp = TcpStream::connect(&address)
            .await
            .with_context(|| format!("connecting to {address}"))?;
        tcp.set_nodelay(true)?;

        match Client::connect(config.clone(), tcp.compat_write()).await {
            Ok(client) => return Ok(client),
            Err(tiberius::error::Error::Routing { host, port }) => {
                address = format!("{host}:{port}");
                config.host(&host);
                config.port(port);
            }
            Err(error) => return Err(error.into()),
        }
    }
    bail!("the server kept redirecting the connection")
}

fn configure(credentials: &Credentials, database: Option<&str>) -> Config {
    let mut config = Config::new();
    config.host(&credentials.host);
    config.port(credentials.port);
    if let Some(database) = database.or(credentials.database.as_deref()) {
        config.database(database);
    }
    config.application_name("sst");
    config.authentication(credentials.auth_method());

    if credentials.trust_certificate {
        config.trust_cert();
    }
    config.encryption(match credentials.encrypt {
        Encryption::Required => EncryptionLevel::Required,
        Encryption::On => EncryptionLevel::On,
        Encryption::LoginOnly => EncryptionLevel::Off,
        Encryption::Off => EncryptionLevel::NotSupported,
    });
    config
}

pub async fn run_query(client: &mut Client, sql: &str, limit: usize) -> Result<Vec<ResultSet>> {
    let stream = client.simple_query(sql).await?;
    let batches = stream.into_results().await?;

    let mut results = Vec::new();
    for rows in batches {
        let Some(first) = rows.first() else {
            results.push(ResultSet::default());
            continue;
        };

        let columns: Vec<String> = first
            .columns()
            .iter()
            .map(|column| column.name().to_string())
            .collect();
        let types: Vec<String> = first
            .columns()
            .iter()
            .map(|column| sql_type_name(column.column_type()).to_string())
            .collect();

        let truncated = rows.len() > limit;
        let rendered = rows.iter().take(limit).map(render_row).collect();

        results.push(ResultSet {
            columns,
            types,
            rows: rendered,
            truncated,
        });
    }
    Ok(results)
}

pub async fn stream_to_file(
    client: &mut Client,
    sql: &str,
    wanted: usize,
    writer: &mut Writer,
) -> Result<u64> {
    let mut stream = client.simple_query(sql).await?;
    let mut current = 0;
    let mut written = 0;

    while let Some(item) = stream.next().await {
        match item? {
            QueryItem::Metadata(metadata) => {
                current = metadata.result_index();
                if current == wanted {
                    let columns = metadata
                        .columns()
                        .iter()
                        .map(|column| column.name().to_string())
                        .collect::<Vec<_>>();
                    writer.header(&columns)?;
                }
            }
            QueryItem::Row(row) => {
                if current == wanted {
                    writer.row(&render_row(&row))?;
                    written += 1;
                }
            }
        }
    }
    Ok(written)
}

pub async fn databases(client: &mut Client) -> Result<Vec<String>> {
    let rows = client
        .simple_query("SELECT name FROM sys.databases WHERE state = 0 ORDER BY name")
        .await?
        .into_first_result()
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get::<&str, _>(0).map(str::to_string))
        .collect())
}

pub async fn tables(client: &mut Client) -> Result<Vec<Table>> {
    const SQL: &str = "
        SELECT s.name AS schema_name, o.name AS object_name, o.type AS object_type,
               c.name AS column_name, t.name AS type_name
        FROM sys.objects o
        JOIN sys.schemas s ON s.schema_id = o.schema_id
        JOIN sys.columns c ON c.object_id = o.object_id
        JOIN sys.types t ON t.user_type_id = c.user_type_id
        WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0
        ORDER BY s.name, o.name, c.column_id";

    let rows = client.simple_query(SQL).await?.into_first_result().await?;
    Ok(gather_tables(&rows))
}

fn gather_tables(ordered_by_table: &[Row]) -> Vec<Table> {
    let mut tables: Vec<Table> = Vec::new();
    for row in ordered_by_table {
        let (Some(schema), Some(name), Some(kind), Some(column), Some(data_type)) = (
            row.get::<&str, _>(0),
            row.get::<&str, _>(1),
            row.get::<&str, _>(2),
            row.get::<&str, _>(3),
            row.get::<&str, _>(4),
        ) else {
            continue;
        };

        let matches = |table: &Table| table.schema == schema && table.name == name;
        if !tables.last().is_some_and(matches) {
            tables.push(Table {
                schema: schema.to_string(),
                name: name.to_string(),
                columns: Vec::new(),
                is_view: kind.trim() == "V",
            });
        }
        if let Some(table) = tables.last_mut() {
            table.columns.push(Column {
                name: column.to_string(),
                data_type: data_type.to_string(),
            });
        }
    }
    tables
}

fn sql_type_name(column_type: ColumnType) -> &'static str {
    use ColumnType::*;
    match column_type {
        Null => "null",
        Bit | Bitn => "bit",
        Int1 => "tinyint",
        Int2 => "smallint",
        Int4 | Intn => "int",
        Int8 => "bigint",
        Float4 | Float8 | Floatn => "float",
        Money | Money4 => "money",
        Decimaln => "decimal",
        Numericn => "numeric",
        Guid => "uniqueidentifier",
        Datetime | Datetimen => "datetime",
        Datetime4 => "smalldatetime",
        Datetime2 => "datetime2",
        Daten => "date",
        Timen => "time",
        DatetimeOffsetn => "datetimeoffset",
        BigVarBin => "varbinary",
        BigBinary => "binary",
        Image => "image",
        BigVarChar => "varchar",
        BigChar => "char",
        NVarchar => "nvarchar",
        NChar => "nchar",
        Text => "text",
        NText => "ntext",
        Xml => "xml",
        Udt => "udt",
        SSVariant => "sql_variant",
    }
}

fn render_row(row: &Row) -> Vec<Option<String>> {
    (0..row.len())
        .map(|index| render_cell(row, index))
        .collect()
}

fn render_cell(row: &Row, index: usize) -> Option<String> {
    let column_type = row.columns().get(index)?.column_type();

    match column_type {
        ColumnType::Datetime | ColumnType::Datetimen | ColumnType::Datetime2 => {
            return row
                .try_get::<NaiveDateTime, _>(index)
                .ok()
                .flatten()
                .map(|value| value.format("%Y-%m-%d %H:%M:%S%.f").to_string());
        }
        ColumnType::Datetime4 => {
            return row
                .try_get::<NaiveDateTime, _>(index)
                .ok()
                .flatten()
                .map(|value| value.format("%Y-%m-%d %H:%M").to_string());
        }
        ColumnType::Daten => {
            return row
                .try_get::<NaiveDate, _>(index)
                .ok()
                .flatten()
                .map(|value| value.to_string());
        }
        ColumnType::Timen => {
            return row
                .try_get::<NaiveTime, _>(index)
                .ok()
                .flatten()
                .map(|value| value.to_string());
        }
        ColumnType::DatetimeOffsetn => {
            return row
                .try_get::<DateTime<Utc>, _>(index)
                .ok()
                .flatten()
                .map(|value| value.to_rfc3339());
        }
        _ => {}
    }

    let (_, data) = row.cells().nth(index)?;
    render_non_temporal(data)
}

fn render_non_temporal(data: &ColumnData<'_>) -> Option<String> {
    match data {
        ColumnData::U8(value) => value.map(|v| v.to_string()),
        ColumnData::I16(value) => value.map(|v| v.to_string()),
        ColumnData::I32(value) => value.map(|v| v.to_string()),
        ColumnData::I64(value) => value.map(|v| v.to_string()),
        ColumnData::F32(value) => value.map(|v| v.to_string()),
        ColumnData::F64(value) => value.map(|v| v.to_string()),
        ColumnData::Bit(value) => value.map(|v| v.to_string()),
        ColumnData::String(value) => value.as_ref().map(|v| v.to_string()),
        ColumnData::Guid(value) => value.map(|v| v.to_string()),
        ColumnData::Numeric(value) => value.map(|v| v.to_string()),
        ColumnData::Xml(value) => value.as_ref().map(|v| v.to_string()),
        ColumnData::Binary(value) => value.as_ref().map(|bytes| {
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("0x{hex}")
        }),
        _ => None,
    }
}
