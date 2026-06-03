use std::path::Path;

use rusqlite::{params, Connection, Result};

pub struct ThumbnailCache {
    conn: Connection,
}

const THUMBNAIL_CACHE_VARIANT: i64 = 2;

impl ThumbnailCache {
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        // wal_autocheckpoint=0: 起動時の自動チェックポイントを無効化して高速起動。
        // チェックポイントは on_exit() のバックグラウンドスレッドで実施。
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint=0;")?;

        // If the old single-column schema (path TEXT PRIMARY KEY, no thumb_max) is present,
        // drop it so we can recreate with the new composite key.
        let has_thumb_max: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('thumbnails') WHERE name = 'thumb_max'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        let has_variant: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('thumbnails') WHERE name = 'cache_variant'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_thumb_max || !has_variant {
            conn.execute_batch("DROP TABLE IF EXISTS thumbnails;")?;
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS thumbnails (
                 path      TEXT    NOT NULL,
                 mtime     INTEGER NOT NULL,
                 thumb_max INTEGER NOT NULL,
                 cache_variant INTEGER NOT NULL,
                 width     INTEGER NOT NULL,
                 height    INTEGER NOT NULL,
                 data      BLOB    NOT NULL,
                 PRIMARY KEY (path, thumb_max, cache_variant)
             );",
        )?;

        Ok(Self { conn })
    }

    /// Returns `(width, height, rgba_bytes)` when a cached entry matches path, mtime,
    /// and the thumbnail long-side limit.
    pub fn get(&self, path: &str, mtime: i64, thumb_max: u32) -> Option<(u32, u32, Vec<u8>)> {
        self.conn
            .query_row(
                "SELECT width, height, data FROM thumbnails
                  WHERE path = ?1 AND mtime = ?2 AND thumb_max = ?3 AND cache_variant = ?4",
                params![path, mtime, thumb_max, THUMBNAIL_CACHE_VARIANT],
                |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .ok()
    }

    /// WAL チェックポイントを実行してから接続を閉じる。on_exit() のバックグラウンドスレッド用。
    pub fn checkpoint_and_close(self) {
        self.conn.execute_batch("PRAGMA wal_checkpoint(FULL);").ok();
        // self が drop されることで接続が閉じる
    }

    pub fn put(
        &self,
        path: &str,
        mtime: i64,
        thumb_max: u32,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO thumbnails (path, mtime, thumb_max, cache_variant, width, height, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                path,
                mtime,
                thumb_max,
                THUMBNAIL_CACHE_VARIANT,
                width,
                height,
                data
            ],
        )?;
        Ok(())
    }

    pub fn remove_paths<I, P>(&mut self, paths: I) -> Result<()>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM thumbnails WHERE path = ?1")?;
            for path in paths {
                stmt.execute(params![path.as_ref().to_string_lossy()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ThumbnailCache;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "AssetView-cache-{}-{}.db",
            std::process::id(),
            stamp
        ))
    }

    #[test]
    fn cache_round_trips_entries() {
        let db_path = temp_db_path();
        let cache = ThumbnailCache::open(&db_path).unwrap();

        let path = "/tmp/example.png";
        let mtime = 1_725_000_000;
        let data = vec![1_u8, 2, 3, 4, 5, 6, 7, 8];

        cache.put(path, mtime, 300, 2, 1, &data).unwrap();
        assert_eq!(cache.get(path, mtime, 300), Some((2, 1, data.clone())));
        assert_eq!(cache.get(path, mtime + 1, 300), None); // mtime mismatch
        assert_eq!(cache.get(path, mtime, 150), None); // thumb_max mismatch

        let _ = std::fs::remove_file(db_path);
    }
}
