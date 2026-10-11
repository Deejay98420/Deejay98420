//! PostgreSQL integration: migrations, persistence, immutability, idempotency, recovery.
//! Requires DATABASE_URL (the test prints SKIPPED otherwise; `scripts/sniper test` always sets it).

use sniper_core::config::EngineConfig;
use sniper_core::engine::{Engine, EngineOptions};
use sniper_core::paper::OrderStatus;
use sniper_core::store::{pg::PgStore, spawn_pg, StoreCmd};
use sniper_core::stream::StreamMsg;
use sniper_core::time::wall_us;
use sniper_core::types::DataMode;

fn url() -> Option<String> {
    let u = std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty());
    if u.is_none() {
        eprintln!("SKIPPED: DATABASE_URL not set");
    }
    u
}

async fn engine(url: &str, key: &str) -> (Engine, PgStore) {
    let pg = PgStore::connect(url, key).await.unwrap();
    pg.migrate().await.unwrap();
    let rec = pg.recover().await.unwrap();
    let warm = if rec.books.is_empty() { vec![] } else { pg.load_journal().await.unwrap() };
    let reader = PgStore::connect(url, key).await.unwrap();
    let store = spawn_pg(pg, 4096);
    let mut e = Engine::new(EngineConfig::default(), EngineOptions { data_mode: DataMode::Fixture, profile_key: key.into(), run_id: format!("t{}", wall_us()), jupiter: None, ingest_stats: None }, store, Some(rec));
    e.warmup(&warm);
    e.boot(serde_json::json!({"test": true})).await;
    (e, reader)
}

#[tokio::test]
async fn pg_persistence_immutability_and_restart_recovery() {
    let Some(url) = url() else { return };
    let key = format!("FIXTURE:pgtest{}", wall_us());
    let msgs = sniper_core::fixtures::hybrid_session();
    let cut = msgs.len() / 2;
    // run 1: half the stream
    let (mut e1, reader) = engine(&url, &key).await;
    for m in msgs[..cut].iter().cloned() {
        e1.handle_stream(m).await;
    }
    let balances = e1.ledger_for_test();
    let live1: usize = e1.books.iter().map(|b| b.positions.values().filter(|p| p.is_live()).count()).sum();
    e1.flush().await;
    let snap = e1.snapshot();
    assert_eq!(snap.store_errors, 0, "store write errors");
    drop(e1);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let c = &reader.client;
    let n_raw: i64 = c.query_one("select count(*) from raw_events where profile_key=$1", &[&key]).await.unwrap().get(0);
    assert!(n_raw > 100);
    // immutability: journal, ledger and orders reject UPDATE/DELETE
    for sql in [
        "update ledger set amount = amount + 1 where book_id like $1",
        "delete from raw_events where profile_key = $1",
        "update orders set status = 'filled' where profile_key = $1",
        "delete from evaluations where profile_key = $1",
    ] {
        let arg = if sql.contains("like") { format!("{key}%") } else { key.clone() };
        let r = c.execute(sql, &[&arg]).await;
        if let Err(e) = r {
            assert!(format!("{e:?}").contains("append-only"), "{sql}: {e:?}");
        } else {
            // zero rows touched is acceptable only if the table is empty for this profile
            let rows = r.unwrap();
            assert_eq!(rows, 0, "{sql} mutated rows");
        }
    }
    // idempotency: a second submission with the same key is ignored by the unique constraint
    let any_key: Option<String> = c.query_opt("select idempotency_key from order_submissions where profile_key=$1 limit 1", &[&key]).await.unwrap().map(|r| r.get(0));
    if let Some(k) = any_key {
        let r = c.execute("insert into order_submissions (order_id, idempotency_key, profile_key, book_id, ts_us, doc) values ('dup-x', $1, $2, 'b', 0, '{}')", &[&k, &key]).await;
        assert!(r.is_err(), "duplicate idempotency key must be rejected");
    }
    // run 2: recover and finish the stream (first half is re-delivered and must be deduplicated)
    let (mut e2, reader2) = engine(&url, &key).await;
    assert_eq!(e2.ledger_for_test(), balances, "balances recovered from PostgreSQL ledger");
    let live2: usize = e2.books.iter().map(|b| b.positions.values().filter(|p| p.is_live()).count()).sum();
    assert_eq!(live1, live2);
    for m in msgs.iter().cloned() {
        e2.handle_stream(m).await;
    }
    e2.handle_stream(StreamMsg::End { provider: "f".into(), recv_us: msgs.last().unwrap().recv_us() }).await;
    assert!(e2.health.engine_duplicates_dropped as usize >= cut / 2);
    for b in &e2.books {
        b.verify().unwrap();
    }
    let final_bal = e2.ledger_for_test();
    e2.flush().await;
    drop(e2);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let rec = reader2.recover().await.unwrap();
    for (id, bal) in final_bal {
        let (start, sum, _) = rec.books[&id];
        assert_eq!(start as i64 + sum, bal, "{id}: persisted ledger matches in-memory balance");
    }
    // every closed position reconciles against the persisted ledger
    let rows = reader2
        .client
        .query(
            "select p.position_id, (p.doc->>'realized_pnl')::bigint,
                    (select coalesce(sum(l.amount),0)::bigint from ledger l where l.ref_id = p.doc->>'entry_order_id'
                       or l.ref_id in (select o.order_id from orders o where o.doc->>'position_id' = p.position_id and o.kind='exit'))
             from positions p where p.profile_key=$1 and p.status='closed'",
            &[&key],
        )
        .await
        .unwrap();
    assert!(!rows.is_empty());
    for r in rows {
        let (id, pnl, led): (String, i64, i64) = (r.get(0), r.get(1), r.get(2));
        assert_eq!(pnl, led, "{id}");
    }
    let dup: i64 = reader2.client.query_one("select count(*) - count(distinct idempotency_key) from orders where profile_key=$1", &[&key]).await.unwrap().get(0);
    assert_eq!(dup, 0);
    let _ = OrderStatus::Filled;
    let _ = StoreCmd::Flush;
}
