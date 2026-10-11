mod common;
use sniper_core::config::EngineConfig;
use sniper_core::store::MemData;
use sniper_core::types::sol;
use std::sync::{Arc, Mutex};

#[tokio::test]
#[ignore]
async fn inspect_fixture_trades() {
    let data = Arc::new(Mutex::new(MemData::default()));
    let mut e = common::engine_with(EngineConfig::default(), data.clone(), "inspect");
    e.boot(serde_json::json!({})).await;
    let mut msgs = sniper_core::fixtures::hybrid_session();
    msgs.push(sniper_core::stream::StreamMsg::End { provider: "f".into(), recv_us: msgs.last().unwrap().recv_us() });
    common::feed(&mut e, msgs).await;
    let s = e.snapshot();
    for b in &s.books {
        println!("== {} balance {:.5} equity {:.5}", b.scenario, b.balance_sol, b.equity_sol);
        for p in b.open_positions.iter() {
            println!("  OPEN {} {:?} {:?} tokens {} cost {:.5} last {:?} reason {:?} unresolved {:?}", p.symbol.clone().unwrap_or_default(), p.path, p.status, p.tokens, sol(p.cost_basis as i64), p.last_value.map(|v| sol(v as i64)), p.exit_reason, p.unresolved_reason);
        }
    }
    for p in &s.trades {
        println!("{} {} {:?} cost {:.5} proceeds {:.5} pnl {:+.5} hold {:.0}s reason {:?} fails {}", p.scenario, p.symbol.clone().unwrap_or_default(), p.path, sol(p.cost_basis as i64), sol(p.proceeds as i64), sol(p.realized_pnl.unwrap()), p.hold_s(0), p.exit_reason.as_ref().map(|r| r.code()), p.exit_attempts);
    }
    let d = data.lock().unwrap();
    for o in d.orders.iter().filter(|o| o.scenario == "base") {
        println!("ORDER {:?} {:?} {} amt {} min_out {} fill {:?} err {:?}", o.kind, o.status, &o.mint.to_string()[..6], o.amount, o.min_out, o.fill.as_ref().map(|f| (f.out_amount, f.latency_ms)), o.error);
    }
}
