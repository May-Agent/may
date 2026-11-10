//! Startup check that a database's tables have the columns this build writes.
//!
//! The schema is created with `CREATE TABLE IF NOT EXISTS` and May keeps no
//! migrations, so a database written by another build keeps its old tables
//! and every write that names a missing column fails. The expected columns
//! are read from the schema itself, applied to a private in-memory database,
//! so the check cannot drift from it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use skyzen_services::Db as SkyDb;

use super::{ChatDb, ChatDbError, query};

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct ColumnRow {
    table_name: String,
    column_name: String,
    column_type: String,
    not_null: i64,
}

/// What a column is declared as.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Declared {
    column_type: String,
    not_null: bool,
}

impl fmt::Display for Declared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let null = if self.not_null {
            "NOT NULL"
        } else {
            "nullable"
        };
        write!(f, "{} {null}", self.column_type)
    }
}

/// One way a live table departs from the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaProblem {
    /// The schema has the column and the table does not.
    Missing {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
        /// Declaration the schema gives it.
        expected: String,
    },
    /// The column exists with another declared type or nullability.
    Differs {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
        /// Declaration the schema gives it.
        expected: String,
        /// Declaration the table has.
        found: String,
    },
    /// The table has a column the schema does not.
    Unexpected {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
}

impl fmt::Display for SchemaProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing {
                table,
                column,
                expected,
            } => write!(f, "table `{table}` lacks column `{column}` ({expected})"),
            Self::Differs {
                table,
                column,
                expected,
                found,
            } => write!(
                f,
                "table `{table}` column `{column}` is {found}, the schema expects {expected}"
            ),
            Self::Unexpected { table, column } => {
                write!(
                    f,
                    "table `{table}` has column `{column}`, which the schema does not"
                )
            }
        }
    }
}

/// Every problem found in one database, for the error that refuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaProblems {
    /// Database file that was checked.
    pub path: String,
    /// What differs, in table and column order.
    pub problems: Vec<SchemaProblem>,
}

impl fmt::Display for SchemaProblems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "The database {} does not match the schema this build of May uses. \
             May keeps no migrations: move the file away so a fresh one is created.",
            self.path
        )?;
        for problem in &self.problems {
            write!(f, "\n- {problem}")?;
        }
        Ok(())
    }
}

type Columns = BTreeMap<(String, String), Declared>;

async fn columns(db: &SkyDb) -> Result<Columns, ChatDbError> {
    let rows = query!(
        db,
        "SELECT m.name AS table_name, c.name AS column_name, c.type AS column_type, c.\"notnull\" AS not_null FROM sqlite_master AS m, pragma_table_info(m.name) AS c WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%'"
    )
    .fetch_all::<ColumnRow>()
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                (row.table_name, row.column_name),
                Declared {
                    column_type: row.column_type,
                    not_null: row.not_null != 0,
                },
            )
        })
        .collect())
}

/// Compare `live` with `expected`, for the tables `expected` has.
fn compare(expected: &Columns, live: &Columns) -> Vec<SchemaProblem> {
    let mut problems = Vec::new();
    for ((table, column), declared) in expected {
        match live.get(&(table.clone(), column.clone())) {
            None => problems.push(SchemaProblem::Missing {
                table: table.clone(),
                column: column.clone(),
                expected: declared.to_string(),
            }),
            Some(found) if found != declared => problems.push(SchemaProblem::Differs {
                table: table.clone(),
                column: column.clone(),
                expected: declared.to_string(),
                found: found.to_string(),
            }),
            Some(_) => {}
        }
    }
    for (table, column) in live.keys() {
        let known_table = expected.keys().any(|(name, _)| name == table);
        if known_table && !expected.contains_key(&(table.clone(), column.clone())) {
            problems.push(SchemaProblem::Unexpected {
                table: table.clone(),
                column: column.clone(),
            });
        }
    }
    problems
}

impl ChatDb {
    /// Refuse a database whose tables differ from what the schema creates.
    ///
    /// Runs after the schema was applied, so a table that was missing is
    /// already there and only a table left over from another build can fail.
    pub(super) async fn check_schema(db: &SkyDb, db_path: &Path) -> Result<(), ChatDbError> {
        let reference = SkyDb::connect_sqlite_memory().await?;
        Self::apply_schema(&reference).await?;
        let problems = compare(&columns(&reference).await?, &columns(db).await?);
        if problems.is_empty() {
            return Ok(());
        }
        Err(ChatDbError::SchemaMismatch(SchemaProblems {
            path: db_path.display().to_string(),
            problems,
        }))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    async fn open_error(path: &Path) -> SchemaProblems {
        match ChatDb::open(path).await {
            Err(ChatDbError::SchemaMismatch(problems)) => problems,
            other => panic!("the database should be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fresh_database_passes_and_reopens() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        ChatDb::open(&path).await.expect("fresh database opens");
        ChatDb::open(&path).await.expect("same database reopens");
    }

    #[tokio::test]
    async fn a_database_missing_a_column_is_refused_naming_it() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("fresh database opens");
        db.execute_for_test("ALTER TABLE request_logs DROP COLUMN keep_warm")
            .await
            .expect("column drops");
        drop(db);

        let problems = open_error(&path).await;
        assert_eq!(
            problems.problems,
            vec![SchemaProblem::Missing {
                table: "request_logs".into(),
                column: "keep_warm".into(),
                expected: "BOOLEAN NOT NULL".into(),
            }]
        );
        let text = problems.to_string();
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(
            text.contains("`request_logs` lacks column `keep_warm`"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_column_of_another_type_is_refused() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("fresh database opens");
        db.execute_for_test("ALTER TABLE request_logs DROP COLUMN keep_warm")
            .await
            .expect("column drops");
        db.execute_for_test("ALTER TABLE request_logs ADD COLUMN keep_warm TEXT")
            .await
            .expect("column returns with another type");
        drop(db);

        let problems = open_error(&path).await;
        assert_eq!(
            problems.problems,
            vec![SchemaProblem::Differs {
                table: "request_logs".into(),
                column: "keep_warm".into(),
                expected: "BOOLEAN NOT NULL".into(),
                found: "TEXT nullable".into(),
            }]
        );
    }

    #[tokio::test]
    async fn a_column_the_schema_lacks_is_reported() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("fresh database opens");
        db.execute_for_test("ALTER TABLE request_logs ADD COLUMN leftover TEXT")
            .await
            .expect("column added");
        drop(db);

        let problems = open_error(&path).await;
        assert_eq!(
            problems.problems,
            vec![SchemaProblem::Unexpected {
                table: "request_logs".into(),
                column: "leftover".into(),
            }]
        );
    }
}
