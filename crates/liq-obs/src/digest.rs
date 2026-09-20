//! Taxonomy counts from shadow JSONL + watcher SQLite. Missing files fail; no guessed counts.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use rusqlite::Connection;
use serde::Deserialize;

use crate::error::{ObsError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestReport {
    pub sqlite_rows: u64,
    pub jsonl_rows: u64,
    pub by_outcome: BTreeMap<String, u64>,
    pub not_tracked: u64,
    pub health_wrong: u64,
    pub declined: u64,
    pub halt_lines: u64,
}

#[derive(Deserialize)]
struct OutcomeLine {
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

fn inc(map: &mut BTreeMap<String, u64>, k: &str) -> Result<()> {
    let e = map.entry(k.to_owned()).or_insert(0);
    *e = e.checked_add(1).ok_or_else(|| {
        tracing::error!("digest counter overflow");
        ObsError::Json("counter overflow".into())
    })?;
    Ok(())
}

pub fn digest_paths(sqlite: &Path, jsonl: &Path) -> Result<DigestReport> {
    if !sqlite.is_file() {
        tracing::error!(path = %sqlite.display(), "digest sqlite missing");
        return Err(ObsError::Io(format!("missing {}", sqlite.display())));
    }
    if !jsonl.is_file() {
        tracing::error!(path = %jsonl.display(), "digest jsonl missing");
        return Err(ObsError::Io(format!("missing {}", jsonl.display())));
    }
    let db = Connection::open(sqlite)?;
    let sqlite_rows: i64 = db.query_row("SELECT COUNT(*) FROM liquidations", [], |r| r.get(0))?;
    let sqlite_rows = u64::try_from(sqlite_rows).map_err(|_| {
        tracing::error!("sqlite COUNT negative");
        ObsError::Sqlite("negative count".into())
    })?;

    let file = File::open(jsonl)?;
    let reader = BufReader::new(file);
    let mut by_outcome = BTreeMap::new();
    let mut jsonl_rows = 0u64;
    let mut not_tracked = 0u64;
    let mut health_wrong = 0u64;
    let mut declined = 0u64;
    let mut halt_lines = 0u64;
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        jsonl_rows = jsonl_rows.checked_add(1).ok_or_else(|| {
            tracing::error!("jsonl row counter overflow");
            ObsError::Json("overflow".into())
        })?;
        let v: serde_json::Value = serde_json::from_str(&line)?;
        if v.get("halt").is_some() {
            halt_lines = halt_lines
                .checked_add(1)
                .ok_or(ObsError::Json("overflow".into()))?;
        }
        let name = v
            .get("outcome")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("name").and_then(|x| x.as_str()));
        if let Some(n) = name {
            inc(&mut by_outcome, n)?;
            match n {
                "NotTracked" => {
                    not_tracked = not_tracked
                        .checked_add(1)
                        .ok_or(ObsError::Json("overflow".into()))?;
                }
                "HealthWrong" => {
                    health_wrong = health_wrong
                        .checked_add(1)
                        .ok_or(ObsError::Json("overflow".into()))?;
                }
                "Declined" => {
                    declined = declined
                        .checked_add(1)
                        .ok_or(ObsError::Json("overflow".into()))?;
                }
                _ => {}
            }
        } else if let Ok(ol) = serde_json::from_value::<OutcomeLine>(v.clone()) {
            if let Some(n) = ol.outcome.or(ol.name) {
                inc(&mut by_outcome, &n)?;
            }
        }
    }
    Ok(DigestReport {
        sqlite_rows,
        jsonl_rows,
        by_outcome,
        not_tracked,
        health_wrong,
        declined,
        halt_lines,
    })
}

impl DigestReport {
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "sqlite_rows={}\njsonl_rows={}\nNotTracked={}\nHealthWrong={}\nDeclined={}\nhalts={}\nby_outcome={:?}\n",
            self.sqlite_rows,
            self.jsonl_rows,
            self.not_tracked,
            self.health_wrong,
            self.declined,
            self.halt_lines,
            self.by_outcome
        )
    }
}
