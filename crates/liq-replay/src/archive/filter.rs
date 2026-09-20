//! C3 `receipts_log_filter` address list (D15). Generated, never hand-listed.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use alloy_primitives::Address;

use super::extract::ExtractError;

/// Committed C3 unique address count (STATE.md C3 / H1).
pub const C3_EXPECTED: usize = 13_074;

/// Load unique addresses from `ops/reth/reth.toml`.
pub fn load_c3_addresses(root: &Path) -> Result<Vec<Address>, ExtractError> {
    let path = root.join("ops/reth/reth.toml");
    let text = fs::read_to_string(&path).map_err(|e| ExtractError::Io(e.to_string()))?;
    parse_receipts_log_filter(&text)
}

/// Parse `[prune.segments.receipts_log_filter]` keys. Fail closed on a
/// non-comment line that is not a quoted address.
pub fn parse_receipts_log_filter(text: &str) -> Result<Vec<Address>, ExtractError> {
    let start = text
        .find("[prune.segments.receipts_log_filter]")
        .ok_or_else(|| ExtractError::Filter("missing receipts_log_filter table".into()))?;
    let rest = text
        .get(start..)
        .ok_or_else(|| ExtractError::Filter("filter table slice".into()))?;
    let mut set = BTreeSet::new();
    for (i, line) in rest.lines().enumerate() {
        if i == 0 {
            continue;
        }
        let t = line.trim();
        if t.starts_with('[') {
            break;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some(q0) = t.find('"') else {
            return Err(ExtractError::Filter(format!(
                "non-address filter line: {t}"
            )));
        };
        let after = t
            .get(q0.saturating_add(1)..)
            .ok_or_else(|| ExtractError::Filter("quote slice".into()))?;
        let Some(q1) = after.find('"') else {
            return Err(ExtractError::Filter(format!("unterminated address: {t}")));
        };
        let raw = after
            .get(..q1)
            .ok_or_else(|| ExtractError::Filter("address slice".into()))?;
        let addr = Address::from_str(raw)
            .map_err(|_| ExtractError::Filter(format!("bad address {raw}")))?;
        set.insert(addr);
    }
    if set.is_empty() {
        return Err(ExtractError::Filter("empty receipts_log_filter".into()));
    }
    Ok(set.into_iter().collect())
}
