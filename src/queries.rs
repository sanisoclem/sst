use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const REMEMBERED: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub label: String,
    pub path: PathBuf,
    pub sql: String,
    pub saved: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Library {
    pub saved: Vec<Entry>,
    pub recent: Vec<Entry>,
}

impl Library {
    pub fn entries(&self) -> Vec<&Entry> {
        self.saved.iter().chain(self.recent.iter()).collect()
    }

    pub fn get(&self, index: usize) -> Option<&Entry> {
        self.entries().get(index).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.saved.is_empty() && self.recent.is_empty()
    }
}

pub fn server_root(host: &str, port: u16) -> Option<PathBuf> {
    Some(
        dirs::data_dir()?
            .join("sst")
            .join(slug(&format!("{host}_{port}"))),
    )
}

pub fn database_root(server: &Path, database: &str) -> PathBuf {
    server.join(slug(database))
}

pub fn load(root: Option<PathBuf>) -> Library {
    let Some(root) = root else {
        return Library::default();
    };
    Library {
        saved: read_directory(&root.join("saved"), true),
        recent: read_directory(&root.join("history"), false),
    }
}

pub fn save(root: &Path, name: &str, sql: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        bail!("give the query a name");
    }
    let directory = root.join("saved");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;

    let path = directory.join(format!("{}.sql", slug(name)));
    std::fs::write(&path, sql).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub fn remember(root: &Path, sql: &str) -> Result<()> {
    let sql = sql.trim();
    if sql.is_empty() {
        return Ok(());
    }
    let directory = root.join("history");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;

    if is_newest(&directory, sql) {
        return Ok(());
    }
    forget_copies(&directory, sql);
    std::fs::write(next_path(&directory), sql)?;
    prune(&directory);
    Ok(())
}

fn is_newest(directory: &Path, sql: &str) -> bool {
    files(directory)
        .first()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|newest| newest.trim() == sql)
}

fn forget_copies(directory: &Path, sql: &str) {
    for path in files(directory) {
        if std::fs::read_to_string(&path).is_ok_and(|contents| contents.trim() == sql) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn next_path(directory: &Path) -> PathBuf {
    let mut stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut path = directory.join(format!("{stamp:025}.sql"));
    while path.exists() {
        stamp += 1;
        path = directory.join(format!("{stamp:025}.sql"));
    }
    path
}

fn prune(directory: &Path) {
    for stale in files(directory).into_iter().skip(REMEMBERED) {
        let _ = std::fs::remove_file(stale);
    }
}

pub fn forget(path: &Path) -> Result<()> {
    std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
}

fn read_directory(directory: &Path, saved: bool) -> Vec<Entry> {
    files(directory)
        .into_iter()
        .filter_map(|path| {
            let sql = std::fs::read_to_string(&path).ok()?;
            let stem = path.file_stem()?.to_str()?.to_string();
            Some(Entry {
                label: match saved {
                    true => stem,
                    false => preview(&sql),
                },
                path,
                sql,
                saved,
            })
        })
        .collect()
}

fn files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    paths
}

fn preview(sql: &str) -> String {
    let flattened = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    match flattened.char_indices().nth(60) {
        Some((cut, _)) => format!("{}…", &flattened[..cut]),
        None => flattened,
    }
}

fn slug(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(
            |c| match c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                true => c,
                false => '_',
            },
        )
        .collect();
    match cleaned.trim_matches('_') {
        "" => "unnamed".to_string(),
        trimmed => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("sqltui-queries-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn keeps_a_host_and_database_usable_as_a_directory() {
        assert_eq!(slug("10.155.1.11_1433"), "10.155.1.11_1433");
        assert_eq!(slug("my server\\INSTANCE"), "my_server_INSTANCE");
        assert_eq!(slug("../../etc"), ".._.._etc", "no path traversal");
        assert_eq!(slug("///"), "unnamed");
    }

    #[test]
    fn saves_and_reloads_a_named_query() {
        let root = scratch("saved");
        save(&root, "daily orders", "SELECT 1").unwrap();

        let library = load(Some(root.clone()));
        assert_eq!(library.saved.len(), 1);
        assert_eq!(library.saved[0].label, "daily_orders");
        assert_eq!(library.saved[0].sql, "SELECT 1");
        assert!(library.saved[0].saved);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn history_keeps_only_the_most_recent() {
        let root = scratch("history");
        for n in 0..REMEMBERED + 5 {
            remember(&root, &format!("SELECT {n}")).unwrap();
        }
        let library = load(Some(root.clone()));
        assert_eq!(library.recent.len(), REMEMBERED);
        assert_eq!(
            library.recent[0].sql,
            format!("SELECT {}", REMEMBERED + 4),
            "newest first"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn running_the_same_query_again_does_not_fill_the_history() {
        let root = scratch("dedupe");
        remember(&root, "SELECT 1").unwrap();
        remember(&root, "SELECT 1").unwrap();
        remember(&root, "  SELECT 1  ").unwrap();
        assert_eq!(load(Some(root.clone())).recent.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn re_running_an_older_query_moves_it_back_to_the_top() {
        let root = scratch("promote");
        remember(&root, "SELECT 1").unwrap();
        remember(&root, "SELECT 2").unwrap();
        remember(&root, "SELECT 1").unwrap();

        let library = load(Some(root.clone()));
        assert_eq!(library.recent.len(), 2);
        assert_eq!(library.recent[0].sql, "SELECT 1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_query_is_not_remembered() {
        let root = scratch("empty");
        remember(&root, "   \n  ").unwrap();
        assert!(load(Some(root.clone())).recent.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn history_labels_show_the_query_rather_than_a_timestamp() {
        let root = scratch("labels");
        remember(&root, "SELECT *\n  FROM Sales.Orders\n  WHERE id = 1").unwrap();
        let library = load(Some(root.clone()));
        assert_eq!(
            library.recent[0].label,
            "SELECT * FROM Sales.Orders WHERE id = 1"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_long_query_preview_is_cut_not_wrapped() {
        let long = format!("SELECT {}", "x".repeat(200));
        let shown = preview(&long);
        assert!(shown.ends_with('…'));
        assert_eq!(shown.chars().count(), 61);
    }

    #[test]
    fn saved_and_recent_address_as_one_list() {
        let root = scratch("combined");
        save(&root, "one", "SELECT 1").unwrap();
        remember(&root, "SELECT 2").unwrap();

        let library = load(Some(root.clone()));
        assert_eq!(library.entries().len(), 2);
        assert!(library.get(0).unwrap().saved, "saved come first");
        assert!(!library.get(1).unwrap().saved);
        assert!(library.get(2).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
