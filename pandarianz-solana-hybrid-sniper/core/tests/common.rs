#![allow(dead_code)]
use sniper_core::config::EngineConfig;
use sniper_core::engine::{Engine, EngineOptions};
use sniper_core::store::{spawn_mem, MemData};
use sniper_core::stream::StreamMsg;
use sniper_core::types::DataMode;
use std::sync::{Arc, Mutex};

pub fn engine_with(cfg: EngineConfig, data: Arc<Mutex<MemData>>, profile: &str) -> Engine {
    let key = format!("FIXTURE:{profile}");
    let rec = data.lock().unwrap().recover(&key);
    let store = spawn_mem(data, key.clone(), 4096);
    let mut e = Engine::new(cfg, EngineOptions { data_mode: DataMode::Fixture, profile_key: key, run_id: "test".into(), jupiter: None, ingest_stats: None }, store, Some(rec));
    e.exit_on_end = true;
    e
}

pub async fn feed(e: &mut Engine, msgs: Vec<StreamMsg>) {
    for m in msgs {
        e.handle_stream(m).await;
    }
}
