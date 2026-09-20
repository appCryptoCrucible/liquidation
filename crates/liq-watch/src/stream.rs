//! Streaming consumer: SQLite + JSONL per decoded event.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;
use crate::join::EngineJoin;
use crate::types::DecodedLiquidation;

pub struct StreamSink {
    db: Connection,
    jsonl: std::fs::File,
}

impl StreamSink {
    pub fn open(sqlite: &Path, jsonl: &Path) -> Result<Self> {
        let db = Connection::open(sqlite)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS liquidations (
                block INTEGER NOT NULL,
                tx_index INTEGER NOT NULL,
                log_index INTEGER NOT NULL,
                family TEXT NOT NULL,
                instance TEXT NOT NULL,
                block_hash TEXT NOT NULL,
                tx_hash TEXT NOT NULL,
                payload TEXT NOT NULL,
                PRIMARY KEY (block, tx_index, log_index)
            );",
        )?;
        let jsonl = OpenOptions::new().create(true).append(true).open(jsonl)?;
        Ok(Self { db, jsonl })
    }

    pub fn persist<J: EngineJoin>(&mut self, ev: &DecodedLiquidation, join: &J) -> Result<()> {
        let payload =
            serde_json::to_string(ev).map_err(|e| crate::WatchError::Json(e.to_string()))?;
        let block = i64::try_from(ev.block).map_err(|_| crate::WatchError::TxIndexOverflow)?;
        let tx_index = i64::from(ev.tx_index);
        let log_index = i64::from(ev.log_index);
        self.db.execute(
            "INSERT OR REPLACE INTO liquidations
             (block, tx_index, log_index, family, instance, block_hash, tx_hash, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                block,
                tx_index,
                log_index,
                ev.family.as_str(),
                ev.instance.as_str(),
                format!("{:#x}", ev.block_hash),
                format!("{:#x}", ev.tx_hash),
                payload.as_str(),
            ],
        )?;
        self.jsonl.write_all(payload.as_bytes())?;
        self.jsonl.write_all(b"\n")?;
        self.jsonl.flush()?;
        join.observe(ev);
        Ok(())
    }
}
