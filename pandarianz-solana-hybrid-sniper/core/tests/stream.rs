//! Stream layer: dedup, commitment upgrades, bounded queue backpressure, reconnection, idle timeout.

use futures_util::{SinkExt, StreamExt};
use sniper_core::stream::backoff::Backoff;
use sniper_core::stream::dedup::SignatureDedup;
use sniper_core::stream::{ws, Admit, ConnState, Ingestor, RawEvent, StreamMsg};
use sniper_core::types::{Commitment, DataMode};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

fn ev(sig: &str, c: Commitment) -> StreamMsg {
    StreamMsg::Event(RawEvent { provider: "t".into(), data_mode: DataMode::Fixture, slot: 1, signature: sig.into(), commitment: c, err: None, logs: vec![], inner_ix: vec![], block_time: None, recv_us: 0, backfill: false, fetch_us: None })
}

#[test]
fn dedup_window_duplicates_upgrades_and_eviction() {
    let mut d = SignatureDedup::new(2);
    assert_eq!(d.admit("a", Commitment::Processed), Admit::New);
    assert_eq!(d.admit("a", Commitment::Processed), Admit::Duplicate);
    assert_eq!(d.admit("a", Commitment::Confirmed), Admit::Upgrade);
    assert_eq!(d.admit("a", Commitment::Processed), Admit::Duplicate);
    assert_eq!(d.admit("b", Commitment::Confirmed), Admit::New);
    assert_eq!(d.admit("c", Commitment::Confirmed), Admit::New); // evicts "a"
    assert_eq!(d.len(), 2);
    assert_eq!(d.admit("a", Commitment::Confirmed), Admit::New);
}

#[tokio::test]
async fn ingestor_drops_duplicates_from_redundant_providers() {
    let (ing, mut rx) = Ingestor::new(16, 100);
    for _ in 0..3 {
        ing.push(ev("same", Commitment::Confirmed)).await.unwrap();
    }
    ing.push(ev("other", Commitment::Confirmed)).await.unwrap();
    let s = ing.snapshot();
    assert_eq!(s.duplicates, 2);
    assert_eq!(s.forwarded, 2);
    rx.recv().await.unwrap();
    rx.recv().await.unwrap();
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn bounded_queue_applies_backpressure_without_dropping() {
    let (ing, mut rx) = Ingestor::new(2, 10_000);
    let producer = {
        let ing = ing.clone();
        tokio::spawn(async move {
            for i in 0..50 {
                ing.push(ev(&format!("s{i}"), Commitment::Confirmed)).await.unwrap();
            }
        })
    };
    let mut got = Vec::new();
    while got.len() < 50 {
        tokio::time::sleep(Duration::from_millis(1)).await; // slow consumer
        if let Some(StreamMsg::Event(e)) = rx.recv().await {
            got.push(e.signature);
        }
    }
    producer.await.unwrap();
    let s = ing.snapshot();
    assert_eq!(got.len(), 50, "no message lost");
    assert_eq!(got, (0..50).map(|i| format!("s{i}")).collect::<Vec<_>>(), "order preserved");
    assert!(s.backpressure_waits > 0, "producer had to wait: {s:?}");
    assert!(s.queue_high_water <= 2);
}

#[test]
fn backoff_grows_caps_and_exhausts() {
    let mut b = Backoff::new(100, 1000, Some(5));
    let d: Vec<u64> = std::iter::from_fn(|| b.next_delay()).map(|d| d.as_millis() as u64).collect();
    assert_eq!(d.len(), 5);
    assert!(d[0] >= 100 && d[0] <= 125);
    assert!(d.iter().all(|x| *x <= 1000));
    assert_eq!(*d.last().unwrap(), 1000);
    b.reset();
    assert!(b.next_delay().is_some());
}

/// Local WebSocket server: each connection acks the subscription, sends one logsNotification, then
/// closes (connection 1) or goes silent (connection 2, exercising the idle timeout).
async fn flaky_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let h = tokio::spawn(async move {
        for conn in 0..3u32 {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            // wait for the subscribe request(s)
            let first = ws.next().await.unwrap().unwrap();
            assert!(first.to_text().unwrap().contains("logsSubscribe"));
            ws.send(Message::Text(r#"{"jsonrpc":"2.0","result":11,"id":1}"#.into())).await.unwrap();
            let note = serde_json::json!({"jsonrpc":"2.0","method":"logsNotification","params":{"result":{"context":{"slot":100+conn},"value":{"signature":format!("sig-{conn}"),"err":null,"logs":[]}},"subscription":11}});
            ws.send(Message::Text(note.to_string())).await.unwrap();
            match conn {
                0 => {
                    ws.close(None).await.ok();
                }
                1 => {
                    tokio::time::sleep(Duration::from_millis(600)).await; // silent → client idle timeout
                }
                _ => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });
    (format!("ws://{addr}"), h)
}

#[tokio::test]
async fn ws_source_reconnects_and_resubscribes() {
    let (url, _srv) = flaky_server().await;
    let (ing, mut rx) = Ingestor::new(64, 1000);
    let cfg = ws::WsConfig {
        provider: "local".into(),
        url,
        kind: ws::SubscriptionKind::Logs { mentions: vec!["6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".into()] },
        commitment: Commitment::Confirmed,
        idle_timeout: Duration::from_millis(250),
        ping_interval: Duration::from_secs(10),
        backoff: Backoff::new(20, 100, Some(10)),
        tip: false,
        slot_updates: false,
    };
    let task = tokio::spawn(ws::run(cfg, ing.clone()));
    let mut sigs = Vec::new();
    let mut states = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while sigs.len() < 3 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Some(StreamMsg::Event(e))) => {
                assert_eq!(e.data_mode, DataMode::Live);
                sigs.push(e.signature)
            }
            Ok(Some(StreamMsg::Connection { state, .. })) => states.push(state),
            _ => {}
        }
    }
    task.abort();
    assert_eq!(sigs, vec!["sig-0", "sig-1", "sig-2"]);
    assert!(states.iter().filter(|s| **s == ConnState::Reconnecting).count() >= 3, "{states:?}");
    assert!(states.contains(&ConnState::Disconnected));
    assert!(states.contains(&ConnState::Subscribed));
}

#[tokio::test]
async fn ws_source_reports_exhaustion() {
    // nothing listens on this port
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", l.local_addr().unwrap());
    drop(l);
    let (ing, mut rx) = Ingestor::new(64, 10);
    let cfg = ws::WsConfig {
        provider: "dead".into(),
        url,
        kind: ws::SubscriptionKind::Logs { mentions: vec!["x".into()] },
        commitment: Commitment::Confirmed,
        idle_timeout: Duration::from_millis(100),
        ping_interval: Duration::from_secs(10),
        backoff: Backoff::new(5, 10, Some(2)),
        tip: false,
        slot_updates: false,
    };
    ws::run(cfg, ing).await;
    let mut last = None;
    while let Ok(m) = rx.try_recv() {
        if let StreamMsg::Connection { state, .. } = m {
            last = Some(state);
        }
    }
    assert_eq!(last, Some(ConnState::Exhausted));
}

#[test]
fn journaled_dead_slot_marker_replays_as_rollback() {
    let m = RawEvent::dead_slot_marker(77, 5, DataMode::Fixture);
    assert!(m.is_slot_marker());
    // markers never decode into venue events
    assert!(sniper_core::decode::decode_raw(&m).events.is_empty());
    match m.into_stream_msg() {
        StreamMsg::Slot { slot, status, recv_us } => {
            assert_eq!((slot, recv_us), (77, 5));
            assert_eq!(status, sniper_core::stream::SlotStatus::Dead);
        }
        other => panic!("{other:?}"),
    }
}
