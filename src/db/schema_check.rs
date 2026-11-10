//! Startup check that a database's tables can serve the reads and writes this
//! build makes.
//!
//! The schema is created with `CREATE TABLE IF NOT EXISTS` and May keeps no
//! migrations, so a database written by another build keeps its old tables.
//! The expected columns are read from the schema itself, applied to a private
//! in-memory database, so the check cannot drift from it. Only a difference
//! that can make a statement fail is reported: a missing column, another type
//! affinity, a `NOT NULL` the build may violate, or a required column the
//! build never fills. Declared-type spelling within one affinity, `NOT NULL`
//! on primary keys, defaults and extra optional columns change nothing.

use std::collections::{BTreeMap, BTreeSet};
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
    pk: i64,
    has_default: i64,
}

/// SQLite's column type affinity, from the declared type's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

impl Affinity {
    /// The rules of <https://sqlite.org/datatype3.html#determination_of_column_affinity>.
    fn of(declared: &str) -> Self {
        let upper = declared.to_ascii_uppercase();
        if upper.contains("INT") {
            Self::Integer
        } else if ["CHAR", "CLOB", "TEXT"].iter().any(|s| upper.contains(s)) {
            Self::Text
        } else if upper.contains("BLOB") || upper.is_empty() {
            Self::Blob
        } else if ["REAL", "FLOA", "DOUB"].iter().any(|s| upper.contains(s)) {
            Self::Real
        } else {
            Self::Numeric
        }
    }
}

impl fmt::Display for Affinity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
            Self::Real => "REAL",
            Self::Numeric => "NUMERIC",
        })
    }
}

/// What a column is declared as.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Declared {
    column_type: String,
    not_null: bool,
    primary_key: bool,
    has_default: bool,
}

impl Declared {
    fn affinity(&self) -> Affinity {
        Affinity::of(&self.column_type)
    }

    /// The declaration as the schema or table spells it, for messages.
    fn spelled(&self) -> String {
        let null = if self.not_null {
            "NOT NULL"
        } else {
            "nullable"
        };
        format!("{} {null}", self.column_type)
    }
}

/// One way a live table departs from the schema in a way that can fail a
/// statement.
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
    /// The column has another type affinity, so values are stored differently.
    Affinity {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
        /// Declaration the schema gives it.
        expected: String,
        /// Declaration the table has.
        found: String,
    },
    /// The column is `NOT NULL` in the table and nullable in the schema, which
    /// may write `NULL` to it.
    NowRequired {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
        /// Declaration the table has.
        found: String,
    },
    /// The table has a `NOT NULL` column without a default that the schema
    /// does not have, so inserts that omit it fail.
    UnfilledRequired {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
        /// Declaration the table has.
        found: String,
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
            Self::Affinity {
                table,
                column,
                expected,
                found,
            } => write!(
                f,
                "table `{table}` column `{column}` is {found}, which stores values differently from the schema's {expected}"
            ),
            Self::NowRequired {
                table,
                column,
                found,
            } => write!(
                f,
                "table `{table}` column `{column}` is {found}, but the schema lets it be NULL"
            ),
            Self::UnfilledRequired {
                table,
                column,
                found,
            } => write!(
                f,
                "table `{table}` has column `{column}` ({found}) with no default, which the schema does not have and never fills"
            ),
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
            "The database {} does not match the schema this build of May uses:",
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
        "SELECT m.name AS table_name, c.name AS column_name, c.type AS column_type, c.\"notnull\" AS not_null, c.pk AS pk, c.dflt_value IS NOT NULL AS has_default FROM sqlite_master AS m, pragma_table_info(m.name) AS c WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%'"
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
                    primary_key: row.pk != 0,
                    has_default: row.has_default != 0,
                },
            )
        })
        .collect())
}

/// Compare `live` with `expected`, for the tables `expected` has.
fn compare(expected: &Columns, live: &Columns) -> Vec<SchemaProblem> {
    let mut problems = Vec::new();
    for ((table, column), declared) in expected {
        let Some(found) = live.get(&(table.clone(), column.clone())) else {
            problems.push(SchemaProblem::Missing {
                table: table.clone(),
                column: column.clone(),
                expected: declared.spelled(),
            });
            continue;
        };
        if found.affinity() != declared.affinity() {
            problems.push(SchemaProblem::Affinity {
                table: table.clone(),
                column: column.clone(),
                expected: format!("{} ({})", declared.column_type, declared.affinity()),
                found: format!("{} ({})", found.column_type, found.affinity()),
            });
        } else if found.not_null && !found.primary_key && !declared.not_null {
            problems.push(SchemaProblem::NowRequired {
                table: table.clone(),
                column: column.clone(),
                found: found.spelled(),
            });
        }
    }
    let tables: BTreeSet<&str> = expected.keys().map(|(table, _)| table.as_str()).collect();
    for ((table, column), found) in live {
        if tables.contains(table.as_str())
            && !expected.contains_key(&(table.clone(), column.clone()))
            && found.not_null
            && !found.has_default
        {
            problems.push(SchemaProblem::UnfilledRequired {
                table: table.clone(),
                column: column.clone(),
                found: found.spelled(),
            });
        }
    }
    problems
}

impl ChatDb {
    /// Refuse a database whose tables cannot serve this build's statements.
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

    /// A database at `path` with `statements` applied after the schema.
    async fn altered(path: &Path, statements: &[&str]) {
        let db = ChatDb::open(path).await.expect("fresh database opens");
        for statement in statements {
            db.execute_for_test(statement)
                .await
                .expect("statement applies");
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
        altered(&path, &["ALTER TABLE request_logs DROP COLUMN keep_warm"]).await;

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
    async fn a_column_of_another_affinity_is_refused() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        altered(
            &path,
            &[
                "ALTER TABLE repo_usage DROP COLUMN owner_repo",
                "ALTER TABLE repo_usage ADD COLUMN owner_repo INTEGER",
            ],
        )
        .await;

        let problems = open_error(&path).await;
        assert_eq!(
            problems.problems,
            vec![SchemaProblem::Affinity {
                table: "repo_usage".into(),
                column: "owner_repo".into(),
                expected: "TEXT (TEXT)".into(),
                found: "INTEGER (INTEGER)".into(),
            }]
        );
    }

    #[tokio::test]
    async fn a_nullable_column_that_became_not_null_is_refused() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        altered(
            &path,
            &[
                "ALTER TABLE repo_usage DROP COLUMN owner_repo",
                "ALTER TABLE repo_usage ADD COLUMN owner_repo TEXT NOT NULL DEFAULT ''",
            ],
        )
        .await;

        let problems = open_error(&path).await;
        assert_eq!(
            problems.problems,
            vec![SchemaProblem::NowRequired {
                table: "repo_usage".into(),
                column: "owner_repo".into(),
                found: "TEXT NOT NULL".into(),
            }]
        );
    }

    #[tokio::test]
    async fn an_extra_required_column_without_default_is_refused() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        altered(
            &path,
            &["ALTER TABLE repo_usage ADD COLUMN legacy TEXT NOT NULL DEFAULT ''"],
        )
        .await;
        ChatDb::open(&path)
            .await
            .expect("a defaulted column passes");

        altered(
            &path,
            &[
                "DROP TABLE session_plans",
                "CREATE TABLE session_plans (session_id TEXT PRIMARY KEY, plan_state TEXT NOT NULL, updated_at TEXT NOT NULL, legacy TEXT NOT NULL)",
            ],
        )
        .await;
        let problems = open_error(&path).await;
        assert_eq!(
            problems.problems,
            vec![SchemaProblem::UnfilledRequired {
                table: "session_plans".into(),
                column: "legacy".into(),
                found: "TEXT NOT NULL".into(),
            }]
        );
    }

    #[tokio::test]
    async fn a_table_an_older_orm_created_passes() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        altered(
            &path,
            &[
                "DROP TABLE repo_usage",
                "CREATE TABLE repo_usage (worktree varchar(255) NOT NULL PRIMARY KEY, last_used_at bigint NOT NULL DEFAULT 0, use_count bigint NOT NULL, owner_repo varchar(255), legacy_note text, legacy_flag boolean NOT NULL DEFAULT 0)",
                "DROP TABLE session_yolo",
                "CREATE TABLE session_yolo (session_id varchar NOT NULL PRIMARY KEY, enabled boolean NOT NULL, updated_at varchar NOT NULL)",
                "CREATE TABLE unrelated_leftover (id integer NOT NULL, note text NOT NULL)",
            ],
        )
        .await;

        ChatDb::open(&path)
            .await
            .expect("differences that change nothing pass");
    }

    #[test]
    fn affinity_follows_the_documented_rules() {
        for (declared, affinity) in [
            ("bigint", Affinity::Integer),
            ("INT8", Affinity::Integer),
            ("varchar(255)", Affinity::Text),
            ("CLOB", Affinity::Text),
            ("text", Affinity::Text),
            ("BLOB", Affinity::Blob),
            ("", Affinity::Blob),
            ("float", Affinity::Real),
            ("DOUBLE PRECISION", Affinity::Real),
            ("boolean", Affinity::Numeric),
            ("DATETIME", Affinity::Numeric),
        ] {
            assert_eq!(Affinity::of(declared), affinity, "{declared}");
        }
    }
}
