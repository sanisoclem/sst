use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rust_xlsxwriter::{Format, Workbook};

const XLSX_ROW_MAX: u64 = 1_048_576;

pub enum Writer {
    Csv(Box<csv::Writer<File>>),
    Xlsx {
        workbook: Box<Workbook>,
        next_row: u32,
    },
}

impl Writer {
    pub fn create(path: &Path) -> Result<Self> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some(extension) if extension.eq_ignore_ascii_case("csv") => {
                let file =
                    File::create(path).with_context(|| format!("creating {}", path.display()))?;
                Ok(Writer::Csv(Box::new(csv::Writer::from_writer(file))))
            }
            Some(extension) if extension.eq_ignore_ascii_case("xlsx") => {
                let mut workbook = Workbook::new();
                workbook.add_worksheet_with_constant_memory();
                Ok(Writer::Xlsx {
                    workbook: Box::new(workbook),
                    next_row: 0,
                })
            }
            Some(other) => bail!("unknown format {other:?} — use .csv or .xlsx"),
            None => bail!("give the file a .csv or .xlsx extension"),
        }
    }

    pub fn header(&mut self, columns: &[String]) -> Result<()> {
        match self {
            Writer::Csv(writer) => writer.write_record(columns)?,
            Writer::Xlsx { workbook, next_row } => {
                let heading = Format::new().set_bold();
                let sheet = workbook.worksheet_from_index(0)?;
                for (index, name) in columns.iter().enumerate() {
                    let column = index as u16;
                    sheet.write_string_with_format(0, column, name, &heading)?;
                    sheet.set_column_width(column, guess_width(name))?;
                }
                sheet.set_freeze_panes(1, 0)?;
                *next_row = 1;
            }
        }
        Ok(())
    }

    pub fn row(&mut self, cells: &[Option<String>]) -> Result<()> {
        match self {
            Writer::Csv(writer) => {
                writer.write_record(cells.iter().map(|cell| cell.as_deref().unwrap_or("")))?
            }
            Writer::Xlsx { workbook, next_row } => {
                if u64::from(*next_row) >= XLSX_ROW_MAX {
                    bail!("this result is past Excel's {XLSX_ROW_MAX} row limit — export .csv");
                }
                let sheet = workbook.worksheet_from_index(0)?;
                for (index, cell) in cells.iter().enumerate() {
                    let Some(value) = cell else { continue };
                    let column = index as u16;
                    match value.parse::<f64>() {
                        Ok(number) if is_numeric(value) => {
                            sheet.write_number(*next_row, column, number)?
                        }
                        _ => sheet.write_string(*next_row, column, value)?,
                    };
                }
                *next_row += 1;
            }
        }
        Ok(())
    }

    pub fn finish(self, path: &Path) -> Result<()> {
        match self {
            Writer::Csv(mut writer) => writer.flush()?,
            Writer::Xlsx { mut workbook, .. } => workbook
                .save(path)
                .with_context(|| format!("writing {}", path.display()))?,
        }
        Ok(())
    }
}

pub fn resolve(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

pub fn is_read_only(sql: &str) -> bool {
    !sql.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(writes)
}

fn writes(word: &str) -> bool {
    const WRITES: &[&str] = &[
        "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "DROP", "ALTER", "CREATE", "EXEC",
        "EXECUTE", "GRANT", "REVOKE", "BACKUP", "RESTORE",
    ];
    let stored_procedure = word
        .get(..3)
        .is_some_and(|head| head.eq_ignore_ascii_case("sp_"));
    stored_procedure || WRITES.iter().any(|write| word.eq_ignore_ascii_case(write))
}

fn guess_width(heading: &str) -> f64 {
    (heading.chars().count() as f64 + 4.0).min(60.0)
}

fn is_numeric(value: &str) -> bool {
    !value.starts_with('0') || value.len() == 1 || value.starts_with("0.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_statements_that_would_run_twice() {
        assert!(is_read_only("SELECT * FROM Sales.Orders WHERE id = 1"));
        assert!(is_read_only("WITH x AS (SELECT 1 AS n) SELECT * FROM x"));

        assert!(!is_read_only("DELETE FROM Sales.Orders"));
        assert!(!is_read_only("insert into t values (1)"));
        assert!(!is_read_only("SELECT 1; UPDATE t SET a = 2"));
        assert!(!is_read_only("EXEC dbo.DoSomething"));
        assert!(!is_read_only("EXEC sp_who2"));
    }

    #[test]
    fn an_identifier_containing_a_keyword_is_not_the_keyword() {
        assert!(is_read_only("SELECT update_date, deleted_at FROM t"));
        assert!(is_read_only("SELECT * FROM UpdateLog"));
        assert!(!is_read_only("UPDATE t SET update_date = 1"));
    }

    #[test]
    fn leading_zeroes_stay_text() {
        assert!(!is_numeric("00123"));
        assert!(is_numeric("0"));
        assert!(is_numeric("0.5"));
        assert!(is_numeric("123"));
    }
}
