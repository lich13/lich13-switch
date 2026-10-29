//! Derived index only: historical records and their price snapshots stay unchanged.
use crate::storage::Result;
use rusqlite::Connection;

fn db<T>(result: rusqlite::Result<T>) -> Result<T> {
    result.map_err(|_| super::model::failure("待定价索引维护失败"))
}

pub(super) const SELECT: &str =
    "SELECT p.id,r.body FROM pending_prices p CROSS JOIN records r ON r.id=p.id WHERE p.id>?1 ORDER BY p.id LIMIT 128";

pub(super) fn ensure(connection: &Connection) -> Result<()> {
    // Installation and its first scan commit together. Older writers also maintain
    // this optional index through SQLite triggers, without a new record format.
    let tx = db(connection.unchecked_transaction())?;
    let installed: i64 = db(tx.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE (type='table' AND name='pending_prices') OR (type='trigger' AND name IN ('pending_prices_insert','pending_prices_update','pending_prices_delete'))",
        [], |row| row.get(0),
    ))?;
    if installed != 4 {
        db(tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS pending_prices(id TEXT PRIMARY KEY) WITHOUT ROWID;
            DROP TRIGGER IF EXISTS pending_prices_insert;
            DROP TRIGGER IF EXISTS pending_prices_update;
            DROP TRIGGER IF EXISTS pending_prices_delete;",
        ))?;
        for (name, event) in [("insert", "INSERT"), ("update", "UPDATE OF body,id")] {
            let remove_old = if name == "update" {
                "DELETE FROM pending_prices WHERE id=OLD.id;"
            } else {
                ""
            };
            db(tx.execute_batch(&format!("CREATE TRIGGER pending_prices_{name} AFTER {event} ON records BEGIN
                {remove_old}
                DELETE FROM pending_prices WHERE id=NEW.id;
                INSERT OR REPLACE INTO pending_prices(id) SELECT NEW.id WHERE EXISTS(SELECT 1 FROM json_each(NEW.body,'$.attempts') WHERE json_extract(value,'$.price') IS NULL);
                END;")))?;
        }
        db(tx.execute_batch("CREATE TRIGGER pending_prices_delete AFTER DELETE ON records BEGIN DELETE FROM pending_prices WHERE id=OLD.id; END;
            DELETE FROM pending_prices;
            INSERT INTO pending_prices(id) SELECT id FROM records WHERE EXISTS(SELECT 1 FROM json_each(body,'$.attempts') WHERE json_extract(value,'$.price') IS NULL);"))?;
    }
    db(tx.commit())
}
