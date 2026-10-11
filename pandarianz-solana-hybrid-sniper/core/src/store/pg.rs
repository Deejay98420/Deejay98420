//! PostgreSQL backend (tokio-postgres). Records are stored as key columns + JSONB documents.

use super::{Recovered, StoreCmd};
use crate::paper::{Order, Position};
use crate::stream::RawEvent;
use crate::types::Commitment;
use anyhow::Context;
use tokio_postgres::{Client, NoTls};

pub const MIGRATION_001: &str = include_str!("../../../migrations/001_init.sql");

pub struct PgStore {
    pub client: Client,
    pub profile_key: String,
}

fn status_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

impl PgStore {
    pub async fn connect(url: &str, profile_key: &str) -> anyhow::Result<Self> {
        let (client, conn) = tokio_postgres::connect(url, NoTls).await.context("connecting to PostgreSQL (DATABASE_URL)")?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::error!(error = %e, "postgres connection closed");
            }
        });
        Ok(Self { client, profile_key: profile_key.into() })
    }

    pub async fn migrate(&self) -> anyhow::Result<()> {
        // serialize concurrent migrators
        self.client.batch_execute("select pg_advisory_lock(727274)").await?;
        let r = self.client.batch_execute(MIGRATION_001).await;
        self.client.batch_execute("select pg_advisory_unlock(727274)").await?;
        r.context("applying migration 001")?;
        Ok(())
    }

    pub async fn apply(&self, cmd: StoreCmd) -> anyhow::Result<()> {
        if let StoreCmd::Batch(cmds) = cmd {
            self.client.batch_execute("BEGIN").await?;
            for c in cmds {
                if let Err(e) = Box::pin(self.apply(c)).await {
                    self.client.batch_execute("ROLLBACK").await.ok();
                    return Err(e.context("batch rolled back"));
                }
            }
            self.client.batch_execute("COMMIT").await?;
            return Ok(());
        }
        let c = &self.client;
        let pk = &self.profile_key;
        match cmd {
            StoreCmd::Run { run_id, data_mode, started_us, strategy_version, config } => {
                c.execute(
                    "insert into runs (run_id, profile_key, data_mode, started_us, strategy_version, config) values ($1,$2,$3,$4,$5,$6) on conflict do nothing",
                    &[&run_id, pk, &data_mode, &started_us, &strategy_version, &config],
                )
                .await?;
            }
            StoreCmd::RawEvent { run_id, ev } => {
                let doc = serde_json::to_value(&*ev)?;
                c.execute(
                    "insert into raw_events (profile_key, signature, run_id, data_mode, provider, slot, commitment, recv_us, doc) values ($1,$2,$3,$4,$5,$6,$7,$8,$9) on conflict do nothing",
                    &[pk, &ev.signature, &run_id, &ev.data_mode.as_str(), &ev.provider, &(ev.slot as i64), &status_str(&ev.commitment), &ev.recv_us, &doc],
                )
                .await?;
            }
            StoreCmd::Evaluation(e) => {
                let doc = serde_json::to_value(&*e)?;
                c.execute(
                    "insert into evaluations (eval_id, run_id, profile_key, data_mode, ts_us, mint, path, decision, strategy_version, score, reasons, doc) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) on conflict do nothing",
                    &[&e.eval_id, &e.run_id, pk, &e.data_mode, &e.ts_us, &e.mint, &e.path, &e.decision, &e.strategy_version, &e.score, &e.reasons, &doc],
                )
                .await?;
            }
            StoreCmd::Book(b) => {
                let doc = serde_json::to_value(&b)?;
                c.execute(
                    "insert into books (book_id, profile_key, scenario, start_balance, created_us, doc) values ($1,$2,$3,$4,$5,$6) on conflict do nothing",
                    &[&b.book_id, pk, &b.scenario, &(b.start_balance as i64), &b.created_us, &doc],
                )
                .await?;
            }
            StoreCmd::OrderSubmitted(o) => {
                let doc = serde_json::to_value(&*o)?;
                c.execute(
                    "insert into order_submissions (order_id, idempotency_key, profile_key, book_id, ts_us, doc) values ($1,$2,$3,$4,$5,$6)",
                    &[&o.order_id, &o.idempotency_key, pk, &o.book_id, &o.decision_us, &doc],
                )
                .await?;
            }
            StoreCmd::OrderCompleted(o) => {
                let doc = serde_json::to_value(&*o)?;
                c.execute(
                    "insert into orders (order_id, idempotency_key, profile_key, book_id, kind, status, mint, ts_us, strategy_version, doc) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
                    &[&o.order_id, &o.idempotency_key, pk, &o.book_id, &status_str(&o.kind), &status_str(&o.status), &o.mint.to_string(), &o.completed_us.unwrap_or(o.decision_us), &o.strategy_version, &doc],
                )
                .await?;
            }
            StoreCmd::Ledger(l) => {
                c.execute(
                    "insert into ledger (book_id, seq, ts_us, kind, amount, balance_after, ref_id) values ($1,$2,$3,$4,$5,$6,$7)",
                    &[&l.book_id, &(l.seq as i64), &l.ts_us, &status_str(&l.kind), &l.amount, &l.balance_after, &l.ref_id],
                )
                .await?;
            }
            StoreCmd::Position(p) => {
                let doc = serde_json::to_value(&*p)?;
                // version guard: never overwrite a newer snapshot with an older one
                c.execute(
                    "insert into positions (position_id, book_id, profile_key, mint, status, version, updated_us, doc) values ($1,$2,$3,$4,$5,$6,$7,$8)
                     on conflict (position_id) do update set status = excluded.status, version = excluded.version, updated_us = excluded.updated_us, doc = excluded.doc
                     where positions.version < excluded.version",
                    &[&p.position_id, &p.book_id, pk, &p.mint.to_string(), &status_str(&p.status), &(p.version as i64), &p.last_value_us.max(p.opened_us), &doc],
                )
                .await?;
            }
            StoreCmd::PositionEvent { position_id, ts_us, kind, doc } => {
                c.execute("insert into position_events (position_id, ts_us, kind, doc) values ($1,$2,$3,$4)", &[&position_id, &ts_us, &kind, &doc])
                    .await?;
            }
            StoreCmd::RouteCheck(r) => {
                let doc = serde_json::to_value(&r)?;
                c.execute(
                    "insert into route_checks (profile_key, ts_us, mint, side, source, ok, latency_us, error, doc) values ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
                    &[pk, &r.ts_us, &r.mint, &r.side, &r.source, &r.ok, &r.latency_us, &r.error, &doc],
                )
                .await?;
            }
            StoreCmd::Flush(tx) => {
                let _ = tx.send(());
            }
            StoreCmd::Batch(_) => unreachable!("handled above"),
        }
        Ok(())
    }

    pub async fn recover(&self) -> anyhow::Result<Recovered> {
        let c = &self.client;
        let pk = &self.profile_key;
        let mut r = Recovered::default();
        for row in c
            .query(
                "select b.book_id, b.start_balance, coalesce(sum(l.amount),0)::bigint, coalesce(max(l.seq),0)::bigint
                 from books b left join ledger l on l.book_id = b.book_id where b.profile_key = $1 group by b.book_id, b.start_balance",
                &[pk],
            )
            .await?
        {
            let id: String = row.get(0);
            let start: i64 = row.get(1);
            let sum: i64 = row.get(2);
            let seq: i64 = row.get(3);
            r.books.insert(id, (start as u64, sum, seq as u64));
        }
        for row in c.query("select doc from positions where profile_key = $1", &[pk]).await? {
            let v: serde_json::Value = row.get(0);
            r.positions.push(serde_json::from_value::<Position>(v)?);
        }
        for row in c
            .query(
                "select s.doc from order_submissions s left join orders o on o.order_id = s.order_id where s.profile_key = $1 and o.order_id is null",
                &[pk],
            )
            .await?
        {
            let v: serde_json::Value = row.get(0);
            r.dangling_orders.push(serde_json::from_value::<Order>(v)?);
        }
        for row in c.query("select idempotency_key from order_submissions where profile_key = $1", &[pk]).await? {
            r.idempotency_keys.push(row.get(0));
        }
        for row in c
            .query("select signature, commitment from raw_events where profile_key = $1 order by seq desc limit 200000", &[pk])
            .await?
        {
            let s: String = row.get(0);
            let cm: String = row.get(1);
            let cm = match cm.as_str() {
                "processed" => Commitment::Processed,
                "finalized" => Commitment::Finalized,
                _ => Commitment::Confirmed,
            };
            r.signatures.push((s, cm));
        }
        for row in c
            .query(
                "select book_id, coalesce(sum(((doc->'costs'->>'base_fee')::bigint + (doc->'costs'->>'priority_fee')::bigint)),0)::bigint
                 from orders where profile_key = $1 and kind = 'entry' and status = 'failed_slippage' group by book_id",
                &[pk],
            )
            .await?
        {
            let b: String = row.get(0);
            let v: i64 = row.get(1);
            r.failed_entry_costs.insert(b, v as u64);
        }
        Ok(r)
    }

    /// Journal events for state warm-up after restart (oldest first).
    pub async fn load_events_since(&self, since_us: i64) -> anyhow::Result<Vec<RawEvent>> {
        let rows = self
            .client
            .query("select doc from raw_events where profile_key = $1 and recv_us >= $2 order by recv_us, seq", &[&self.profile_key, &since_us])
            .await?;
        rows.into_iter().map(|r| Ok(serde_json::from_value(r.get::<_, serde_json::Value>(0))?)).collect()
    }

    /// Full journal of a profile (for replay).
    pub async fn load_journal(&self) -> anyhow::Result<Vec<RawEvent>> {
        self.load_events_since(i64::MIN).await
    }

    pub async fn ledger_count(&self) -> anyhow::Result<i64> {
        Ok(self.client.query_one("select count(*) from ledger l join books b on b.book_id=l.book_id where b.profile_key=$1", &[&self.profile_key]).await?.get(0))
    }
}
