//! Shadow-run recorder: [`Submitter`] → daily JSONL (D06). No ClickHouse.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use liq_types::{IntendedSubmission, SubmitReceipt, Submitter, Venue};
use parking_lot::Mutex;
use serde::Serialize;

use crate::error::{ObsError, Result};

#[derive(Serialize)]
struct ShadowRow<'a> {
    plan: String,
    bid: String,
    venue: ShadowVenue<'a>,
    deadline: u64,
    trace: u64,
    intended_unix_ns: String,
}

#[derive(Serialize)]
#[serde(tag = "kind")]
enum ShadowVenue<'a> {
    MevShare { relay: &'a str },
    BuilderBundle { endpoint: &'a str, builder: u16 },
}

fn venue(v: Venue) -> ShadowVenue<'static> {
    match v {
        Venue::MevShare { relay } => ShadowVenue::MevShare { relay },
        Venue::BuilderBundle { endpoint, builder } => ShadowVenue::BuilderBundle {
            endpoint,
            builder: builder.0,
        },
    }
}

struct JsonlWriter {
    dir: PathBuf,
    day: u64,
    file: File,
}

impl JsonlWriter {
    fn open(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            tracing::error!(path = %dir.display(), "shadow JSONL directory missing");
            return Err(ObsError::ShadowDir(dir.display().to_string()));
        }
        let day = unix_day()?;
        let file = open_day(dir, day)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            day,
            file,
        })
    }

    fn write_row(&mut self, row: &ShadowRow<'_>) -> Result<()> {
        let day = unix_day()?;
        if day != self.day {
            self.file = open_day(&self.dir, day)?;
            self.day = day;
        }
        serde_json::to_writer(&mut self.file, row)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }
}

fn unix_day() -> Result<u64> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            tracing::error!("system clock before unix epoch");
            ObsError::Clock
        })?
        .as_secs();
    secs.checked_div(86_400).ok_or(ObsError::Clock)
}

fn unix_ns() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            tracing::error!("system clock before unix epoch");
            ObsError::Clock
        })?
        .as_nanos())
}

fn open_day(dir: &Path, day: u64) -> Result<File> {
    let path = dir.join(format!("shadow-{day}.jsonl"));
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

/// Pipeline submitter that writes [`IntendedSubmission`] + intended timestamp.
pub struct ShadowRecorder {
    sink: Mutex<JsonlWriter>,
}

impl ShadowRecorder {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            sink: Mutex::new(JsonlWriter::open(dir.as_ref())?),
        })
    }
}

impl Submitter for ShadowRecorder {
    type Error = ObsError;

    fn submit(&self, submission: &IntendedSubmission) -> Result<SubmitReceipt> {
        let intended_unix_ns = unix_ns()?;
        let row = ShadowRow {
            plan: format!("{:#x}", submission.plan),
            bid: submission.bid.to_string(),
            venue: venue(submission.venue),
            deadline: submission.deadline,
            trace: submission.trace.raw(),
            intended_unix_ns: intended_unix_ns.to_string(),
        };
        self.sink.lock().write_row(&row)?;
        Ok(SubmitReceipt::Shadow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, U256};
    use liq_types::{BuilderId, TraceId, Venue};

    #[test]
    fn writes_intended_timestamp() {
        let dir = std::env::temp_dir().join(format!(
            "liq-obs-shadow-{}-{}",
            std::process::id(),
            unix_ns().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let rec = ShadowRecorder::open(&dir).unwrap();
        let sub = IntendedSubmission {
            plan: Bytes::from_static(&[0xab, 0xcd]),
            bid: U256::from(7u64),
            venue: Venue::BuilderBundle {
                endpoint: "https://unused.example/relay",
                builder: BuilderId(1),
            },
            deadline: 26_018_680,
            trace: TraceId::from_raw(42),
        };
        assert_eq!(rec.submit(&sub).unwrap(), SubmitReceipt::Shadow);
        let day = unix_day().unwrap();
        let path = dir.join(format!("shadow-{day}.jsonl"));
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(v["trace"], 42);
        assert_eq!(v["bid"], "7");
        assert!(v["intended_unix_ns"].as_str().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
