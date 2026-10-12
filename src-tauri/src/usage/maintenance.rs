//! SQLite housekeeping changes storage only, never usage records or price snapshots.
//! Incremental vacuum follows CC Switch's approach: drain every pragma result row.
use crate::storage::Result;
use rusqlite::{Connection, Error, ErrorCode};
use std::time::{Duration, Instant};

const WAL_LIMIT: i64 = 8 * 1024 * 1024;
const STORAGE_TARGET: i64 = 200 * 1024 * 1024;

pub(super) fn configure(connection: &Connection) -> Result<()> {
    // Effective immediately for a new database. An existing database needs the
    // one-time transactional VACUUM in the background maintenance task.
    connection
        .execute_batch(
            "PRAGMA auto_vacuum=INCREMENTAL;
             PRAGMA journal_size_limit=8388608;
             PRAGMA wal_autocheckpoint=1000;",
        )
        .map_err(|_| super::model::failure("用量存储维护设置失败"))
}

fn busy(error: &Error) -> bool {
    matches!(error, Error::SqliteFailure(code, _) if matches!(code.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

pub(super) fn reclaim(
    connection: &Connection,
    migration_ready: bool,
    repack: bool,
) -> Result<bool> {
    // A live read snapshot may pin WAL pages. Never wait for it or delete WAL;
    // leave SQLite in charge and retry during the next maintenance cycle.
    connection
        .busy_timeout(Duration::ZERO)
        .map_err(|_| super::model::failure("用量存储维护设置失败"))?;
    let result = (|| -> rusqlite::Result<()> {
        let mode: i64 = connection.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
        if migration_ready && (mode == 0 || repack) {
            if repack {
                super::query::compact_indexes(connection)?;
            }
            // VACUUM is atomic, including disk-full or interruption. It preserves
            // every record and index, and runs off the gateway forwarding path.
            connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;")?;
        }
        let started = Instant::now();
        let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let vacuum = if pages.saturating_mul(page_size) > STORAGE_TARGET - WAL_LIMIT {
            "PRAGMA incremental_vacuum(16384)"
        } else {
            "PRAGMA incremental_vacuum(4096)"
        };
        let mut statement = connection.prepare(vacuum)?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {
            if started.elapsed() >= Duration::from_millis(250) {
                break;
            }
        }
        drop(rows);
        drop(statement);
        let (_, frames, copied): (i64, i64, i64) =
            connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if frames > 0 && frames == copied && frames.saturating_mul(page_size) >= WAL_LIMIT {
            // A new reader may start after PASSIVE. Zero busy timeout makes that
            // race harmless: SQLite leaves its live WAL intact instead of waiting.
            let _: (i64, i64, i64) =
                connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        }
        Ok(())
    })();
    let restored = connection.busy_timeout(Duration::from_secs(3));
    if restored.is_err() {
        return Err(super::model::failure("用量存储维护设置失败"));
    }
    match result {
        Ok(()) => Ok(migration_ready),
        Err(error) if busy(&error) => Ok(false),
        Err(_) => Err(super::model::failure(
            "用量存储空间整理失败，记录已保留；请检查可用磁盘空间",
        )),
    }
}
