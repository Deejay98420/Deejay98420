//! Offline fixture source: JSONL of `StreamMsg`. Every event is forced to `DataMode::Fixture`
//! (or `Replay` for recorded journals) so fixture data can never masquerade as live.

use super::{Ingestor, StreamMsg};
use crate::types::DataMode;
use anyhow::Context;
use std::path::Path;

pub fn load_jsonl(path: &Path, mode: DataMode) -> anyhow::Result<Vec<StreamMsg>> {
    assert!(mode != DataMode::Live, "fixture loader cannot produce LIVE data");
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut msg: StreamMsg =
            serde_json::from_str(line).with_context(|| format!("{}:{}: invalid fixture line", path.display(), i + 1))?;
        if let StreamMsg::Event(e) = &mut msg {
            e.data_mode = mode;
        }
        out.push(msg);
    }
    Ok(out)
}

/// Feed fixture messages through the ingestor (dedup + backpressure apply exactly as for live).
pub async fn replay(ingestor: &Ingestor, msgs: Vec<StreamMsg>, provider: &str) -> anyhow::Result<()> {
    let last = msgs.last().map(|m| m.recv_us()).unwrap_or(0);
    for m in msgs {
        ingestor.push(m).await.map_err(|_| anyhow::anyhow!("engine queue closed"))?;
    }
    ingestor
        .push(StreamMsg::End { provider: provider.into(), recv_us: last })
        .await
        .map_err(|_| anyhow::anyhow!("engine queue closed"))?;
    Ok(())
}
