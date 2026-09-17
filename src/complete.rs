use crate::db::Table;

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "VALUES",
    "SET",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS NULL",
    "IS NOT NULL",
    "LIKE",
    "IN",
    "BETWEEN",
    "AS",
    "ON",
    "DISTINCT",
    "TOP",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "WITH",
    "UNION",
    "UNION ALL",
    "EXISTS",
    "OFFSET",
    "FETCH NEXT",
    "DESC",
    "ASC",
];

const BEFORE_A_TABLE: &[&str] = &["FROM", "JOIN", "INTO", "UPDATE", "TABLE"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Word {
    pub prefix: String,
    pub qualifier: Option<String>,
    pub start_byte: usize,
}

pub fn word_at(line: &str, cursor: usize) -> Word {
    let upto = &line[..cursor.min(line.len())];
    let start = upto
        .rfind(|c: char| !is_ident(c))
        .map(|index| index + upto[index..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(0);
    let prefix = upto[start..].to_string();

    let before = upto[..start].trim_end();
    let qualifier = before.strip_suffix('.').map(|head| {
        let head = head.trim_end();
        let at = head
            .rfind(|c: char| !is_ident(c) && c != '.')
            .map(|index| index + head[index..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0);
        unquote(&head[at..])
    });

    Word {
        prefix,
        qualifier,
        start_byte: start,
    }
}

pub fn candidates(tables: &[Table], buffer: &str, line: &str, cursor: usize) -> Vec<Candidate> {
    let word = word_at(line, cursor);
    let referenced = referenced(buffer, tables);

    let pool = match &word.qualifier {
        Some(qualifier) => qualified(tables, &referenced, qualifier),
        None => unqualified(tables, &referenced, line, &word),
    };

    rank(pool, &word.prefix)
}

fn qualified(tables: &[Table], referenced: &[Reference], qualifier: &str) -> Vec<Candidate> {
    let same = |a: &str| a.eq_ignore_ascii_case(qualifier);

    let by_alias = referenced
        .iter()
        .find(|reference| reference.alias.as_deref().is_some_and(same));
    if let Some(reference) = by_alias {
        return columns_of(&tables[reference.table]);
    }

    if let Some(table) = tables.iter().find(|table| same(&table.name)) {
        return columns_of(table);
    }

    tables
        .iter()
        .filter(|table| same(&table.schema))
        .map(|table| Candidate {
            text: table.name.clone(),
            label: table.name.clone(),
            detail: kind(table).to_string(),
        })
        .collect()
}

fn unqualified(
    tables: &[Table],
    referenced: &[Reference],
    line: &str,
    word: &Word,
) -> Vec<Candidate> {
    let mut pool = Vec::new();

    if wants_table(&line[..word.start_byte]) {
        pool.extend(table_candidates(tables));
        return pool;
    }

    for reference in referenced {
        pool.extend(columns_of(&tables[reference.table]));
    }
    pool.extend(table_candidates(tables));
    pool.extend(KEYWORDS.iter().map(|keyword| Candidate {
        text: keyword.to_string(),
        label: keyword.to_string(),
        detail: "keyword".into(),
    }));
    pool
}

fn table_candidates(tables: &[Table]) -> Vec<Candidate> {
    tables
        .iter()
        .map(|table| Candidate {
            text: table.qualified(),
            label: table.qualified(),
            detail: kind(table).to_string(),
        })
        .collect()
}

fn columns_of(table: &Table) -> Vec<Candidate> {
    table
        .columns
        .iter()
        .map(|column| Candidate {
            text: column.name.clone(),
            label: column.name.clone(),
            detail: column.data_type.clone(),
        })
        .collect()
}

fn kind(table: &Table) -> &'static str {
    match table.is_view {
        true => "view",
        false => "table",
    }
}

fn wants_table(before: &str) -> bool {
    last_word(before).is_some_and(|word| {
        BEFORE_A_TABLE
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(&word))
    })
}

fn last_word(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let start = trimmed
        .rfind(|c: char| !is_ident(c))
        .map(|index| index + 1)
        .unwrap_or(0);
    let word = &trimmed[start..];
    (!word.is_empty()).then(|| word.to_string())
}

pub struct Reference {
    alias: Option<String>,
    table: usize,
}

fn referenced(buffer: &str, tables: &[Table]) -> Vec<Reference> {
    let tokens: Vec<String> = tokenize(buffer);
    let mut found = Vec::new();

    for (position, token) in tokens.iter().enumerate() {
        if !BEFORE_A_TABLE.iter().any(|k| k.eq_ignore_ascii_case(token)) {
            continue;
        }
        let Some(name) = tokens.get(position + 1) else {
            continue;
        };
        let Some(index) = resolve(tables, name) else {
            continue;
        };

        let alias = match tokens.get(position + 2).map(String::as_str) {
            Some(next) if next.eq_ignore_ascii_case("AS") => tokens.get(position + 3).cloned(),
            Some(next) if is_alias(next) => Some(next.to_string()),
            _ => None,
        };
        found.push(Reference {
            alias,
            table: index,
        });
    }
    found
}

fn resolve(tables: &[Table], name: &str) -> Option<usize> {
    let name = unquote(name);
    tables.iter().position(|table| {
        table.qualified().eq_ignore_ascii_case(&name) || table.name.eq_ignore_ascii_case(&name)
    })
}

fn is_alias(token: &str) -> bool {
    !token.is_empty()
        && token.chars().all(is_ident)
        && !KEYWORDS
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(token))
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if is_ident(character) || character == '.' || character == '[' || character == ']' {
            current.push(character);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn unquote(name: &str) -> String {
    name.split('.')
        .map(|part| part.trim_matches(['[', ']']))
        .collect::<Vec<_>>()
        .join(".")
}

fn is_ident(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '@' | '#' | '$')
}

fn rank(pool: Vec<Candidate>, prefix: &str) -> Vec<Candidate> {
    let needle = prefix.to_ascii_lowercase();
    let mut scored: Vec<(u8, Candidate)> = pool
        .into_iter()
        .filter_map(|candidate| {
            let haystack = candidate.label.to_ascii_lowercase();
            if needle.is_empty() {
                return Some((0, candidate));
            }
            if haystack.starts_with(&needle) {
                return Some((0, candidate));
            }
            if haystack
                .rsplit('.')
                .next()
                .is_some_and(|tail| tail.starts_with(&needle))
            {
                return Some((1, candidate));
            }
            haystack.contains(&needle).then_some((2, candidate))
        })
        .collect();

    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.label.cmp(&b.1.label)));
    scored.dedup_by(|a, b| a.1.label == b.1.label && a.1.detail == b.1.detail);
    scored
        .into_iter()
        .map(|(_, candidate)| candidate)
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;

    fn schema() -> Vec<Table> {
        vec![
            table(
                "Sales",
                "Orders",
                &[
                    ("OrderID", "int"),
                    ("TotalAmount", "decimal"),
                    ("Status", "varchar"),
                ],
            ),
            table(
                "Sales",
                "Customers",
                &[("CustomerID", "int"), ("CustomerName", "nvarchar")],
            ),
            table(
                "dbo",
                "AuditLog",
                &[("LogID", "bigint"), ("Message", "nvarchar")],
            ),
        ]
    }

    fn table(schema: &str, name: &str, columns: &[(&str, &str)]) -> Table {
        Table {
            schema: schema.into(),
            name: name.into(),
            is_view: false,
            columns: columns
                .iter()
                .map(|(name, data_type)| Column {
                    name: (*name).into(),
                    data_type: (*data_type).into(),
                })
                .collect(),
        }
    }

    fn labels(marked: &str) -> Vec<String> {
        let cursor = marked
            .find('‸')
            .expect("test buffer needs a ‸ cursor marker");
        let buffer = marked.replace('‸', "");
        let line_start = buffer[..cursor].rfind('\n').map(|at| at + 1).unwrap_or(0);
        let line_end = buffer[cursor..]
            .find('\n')
            .map(|at| at + cursor)
            .unwrap_or(buffer.len());

        candidates(
            &schema(),
            &buffer,
            &buffer[line_start..line_end],
            cursor - line_start,
        )
        .into_iter()
        .map(|candidate| candidate.label)
        .collect()
    }

    #[test]
    fn reads_the_word_under_the_cursor() {
        let word = word_at("SELECT o.tot", 12);
        assert_eq!(word.prefix, "tot");
        assert_eq!(word.qualifier.as_deref(), Some("o"));
        assert_eq!(word.start_byte, 9);

        let bare = word_at("SELECT Tot", 10);
        assert_eq!(bare.prefix, "Tot");
        assert_eq!(bare.qualifier, None);
    }

    #[test]
    fn an_alias_resolves_to_its_own_table() {
        let found = labels("SELECT o.tot‸ FROM Sales.Orders o");
        assert_eq!(found, ["TotalAmount"]);
    }

    #[test]
    fn an_alias_bound_on_an_earlier_line_still_resolves() {
        let found = labels("SELECT *\nFROM Sales.Orders o\nWHERE o.stat‸");
        assert_eq!(found, ["Status"]);
    }

    #[test]
    fn as_introduces_an_alias_too() {
        assert_eq!(
            labels("SELECT c.CustomerN‸ FROM Sales.Customers AS c"),
            ["CustomerName"]
        );
    }

    #[test]
    fn a_schema_qualifier_offers_its_tables() {
        assert_eq!(labels("SELECT * FROM Sales.‸"), ["Customers", "Orders"]);
        assert_eq!(labels("SELECT * FROM dbo.‸"), ["AuditLog"]);
    }

    #[test]
    fn a_table_name_qualifier_offers_its_columns() {
        assert_eq!(
            labels("SELECT Orders.Tot‸ FROM Sales.Orders"),
            ["TotalAmount"]
        );
    }

    #[test]
    fn after_from_it_offers_tables_not_columns_or_keywords() {
        let found = labels("SELECT * FROM Sal‸");
        assert_eq!(found, ["Sales.Customers", "Sales.Orders"]);
    }

    #[test]
    fn join_is_a_table_position() {
        assert_eq!(
            labels("SELECT * FROM Sales.Orders o JOIN Cust‸"),
            ["Sales.Customers"]
        );
    }

    #[test]
    fn a_qualified_table_inserts_only_the_part_after_the_dot() {
        assert_eq!(
            labels("SELECT * FROM Sales.Orders o JOIN Sales.Cust‸"),
            ["Customers"]
        );
    }

    #[test]
    fn columns_of_referenced_tables_come_before_anything_else() {
        let found = labels("SELECT Tot‸ FROM Sales.Orders");
        assert_eq!(found.first().map(String::as_str), Some("TotalAmount"));
    }

    #[test]
    fn brackets_around_identifiers_are_ignored() {
        assert_eq!(labels("SELECT o.Stat‸ FROM [Sales].[Orders] o"), ["Status"]);
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(
            labels("SELECT o.TOTALAM‸ FROM Sales.Orders o"),
            ["TotalAmount"]
        );
        assert_eq!(labels("select * from sales.‸"), ["Customers", "Orders"]);
    }

    #[test]
    fn a_qualified_table_matches_on_the_part_after_the_dot() {
        let found = labels("SELECT * FROM Audit‸");
        assert_eq!(found, ["dbo.AuditLog"]);
    }

    #[test]
    fn an_unknown_qualifier_offers_nothing_rather_than_everything() {
        assert!(labels("SELECT zz.foo‸ FROM Sales.Orders o").is_empty());
    }

    #[test]
    fn keywords_are_offered_but_never_ahead_of_schema() {
        let found = labels("SELECT * FROM Sales.Orders o WHERE Stat‸");
        assert_eq!(found.first().map(String::as_str), Some("Status"));
    }

    #[test]
    fn an_empty_prefix_after_a_dot_lists_the_whole_table() {
        assert_eq!(
            labels("SELECT o.‸ FROM Sales.Orders o"),
            ["OrderID", "Status", "TotalAmount"]
        );
    }
}
