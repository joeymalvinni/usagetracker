use rusqlite::{Connection, TransactionBehavior};

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial",
    sql: include_str!("../../migrations/0001_initial.sql"),
}];

// "USG2". This schema identity cleanly separates the disposable v2 database
// from every pre-registry schema without accumulating repair probes.
const APPLICATION_ID: i64 = 0x5553_4732;

const LEGACY_SCHEMA: &str = "
DROP TRIGGER IF EXISTS provider_daily_usage_summary_insert;
DROP TRIGGER IF EXISTS provider_daily_usage_summary_update;
DROP TRIGGER IF EXISTS provider_daily_usage_summary_delete;
DROP TABLE IF EXISTS pending_notifications;
DROP TABLE IF EXISTS notification_window_state;
DROP TABLE IF EXISTS usage_window_observations;
DROP TABLE IF EXISTS provider_daily_usage_summary;
DROP TABLE IF EXISTS provider_daily_usage;
DROP TABLE IF EXISTS raw_payloads;
DROP TABLE IF EXISTS usage_snapshots;
DROP TABLE IF EXISTS provider_health;
DROP TABLE IF EXISTS provider_backoff;
DROP TABLE IF EXISTS accounts;
DROP TABLE IF EXISTS usage_schema_migrations;
DROP TABLE IF EXISTS schema_migrations;
";

pub(super) fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let mut current = schema_version(conn)?;
    let latest = MIGRATIONS.last().map_or(0, |migration| migration.version);

    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let legacy_schema = application_id != APPLICATION_ID && has_legacy_schema(conn)?;
    if application_id != APPLICATION_ID && !legacy_schema && has_user_tables(conn)? {
        anyhow::bail!(
            "refusing to initialize a non-empty SQLite database that is not positively identified \
             as UsageTracker data (application_id={application_id}); choose an empty --db-path or \
             move the existing database"
        );
    }
    if legacy_schema {
        tracing::warn!(
            "resetting a positively identified legacy UsageTracker database; account names, hidden \
             state, removal state, and collection preferences will be discarded"
        );
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        let reset = conn.execute_batch(LEGACY_SCHEMA);
        conn.pragma_update(None, "foreign_keys", "ON")?;
        reset?;
        current = 0;
    }
    anyhow::ensure!(
        current <= latest,
        "database schema version {current} is newer than supported version {latest}"
    );

    // Pre-v1 databases used an ad-hoc set of schema probes and repairs. Data is
    // intentionally non-authoritative and reproducible, so reset that schema
    // once instead of carrying legacy repair branches through production code.
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current)
    {
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migration.sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, applied_at)
             VALUES (?1, ?2, CURRENT_TIMESTAMP)",
            (migration.version, migration.name),
        )?;
        transaction.pragma_update(None, "user_version", migration.version)?;
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.commit()?;
    }
    Ok(())
}

fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn has_legacy_schema(conn: &Connection) -> rusqlite::Result<bool> {
    // `accounts` and `usage_snapshots` are common names. Never use either one
    // alone as permission to delete data. The old migration table is an
    // explicit marker; otherwise require the distinctive pair of historical
    // UsageTracker table shapes.
    Ok((has_columns(
        conn,
        "usage_schema_migrations",
        &["version", "name", "applied_at"],
    )?) || (has_columns(
        conn,
        "accounts",
        &[
            "id",
            "provider_id",
            "external_account_id",
            "created_at",
            "updated_at",
        ],
    )? && has_columns(
        conn,
        "usage_snapshots",
        &[
            "id",
            "provider_id",
            "account_id",
            "collected_at",
            "normalized_json",
        ],
    )?))
}

fn has_user_tables(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM sqlite_master
           WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         )",
        [],
        |row| row.get(0),
    )
}

fn has_columns(conn: &Connection, table: &str, required: &[&str]) -> rusqlite::Result<bool> {
    let exists = conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
         )",
        [table],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        return Ok(false);
    }

    let quoted_table = table.replace('"', "\"\"");
    let mut statement = conn.prepare(&format!("PRAGMA table_info(\"{quoted_table}\")"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    Ok(required.iter().all(|column| columns.contains(*column)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_the_authoritative_schema_transactionally() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrate(&mut conn).unwrap();

        assert_eq!(schema_version(&conn).unwrap(), 1);
        assert_eq!(
            conn.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            APPLICATION_ID
        );
        let applied: (i64, String) = conn
            .query_row("SELECT version, name FROM schema_migrations", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(applied, (1, "initial".to_string()));
        assert!(conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'accounts')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap());
    }

    #[test]
    fn replaces_an_unversioned_legacy_schema_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts(
               id TEXT PRIMARY KEY,
               provider_id TEXT NOT NULL,
               external_account_id TEXT NOT NULL,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               obsolete TEXT
             );
             CREATE TABLE usage_snapshots(
               id TEXT PRIMARY KEY,
               provider_id TEXT NOT NULL,
               account_id TEXT NOT NULL,
               collected_at TEXT NOT NULL,
               normalized_json TEXT NOT NULL
             );
             INSERT INTO accounts VALUES
               ('old', 'codex', 'old', '2026-01-01', '2026-01-01', 'discard me');",
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        assert_eq!(schema_version(&conn).unwrap(), 1);
        let columns = conn
            .prepare("PRAGMA table_info(accounts)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.contains(&"provider_id".to_string()));
        assert!(!columns.contains(&"obsolete".to_string()));
    }

    #[test]
    fn refuses_an_unrelated_database_without_modifying_it() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts(id INTEGER PRIMARY KEY, secret TEXT NOT NULL);
             INSERT INTO accounts(secret) VALUES ('sentinel');
             CREATE TABLE unrelated(value TEXT NOT NULL);
             INSERT INTO unrelated VALUES ('preserve me');",
        )
        .unwrap();

        let error = migrate(&mut conn).unwrap_err().to_string();

        assert!(error.contains("not positively identified as UsageTracker"));
        assert_eq!(
            conn.query_row("SELECT secret FROM accounts", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "sentinel"
        );
        assert_eq!(
            conn.query_row("SELECT value FROM unrelated", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "preserve me"
        );
        assert_eq!(schema_version(&conn).unwrap(), 0);
    }
}
