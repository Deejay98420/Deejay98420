//! Regression checks on REAL captured mainnet transactions (`fixtures/real/*.json`, produced by
//! `sniper capture`). With no captures present the test reports that explicitly and passes.

use sniper_core::decode::pumpfun::PumpEvent;
use sniper_core::decode::pumpswap::PumpSwapEvent;
use sniper_core::decode::{decode_raw, VenueEvent};
use sniper_core::stream::ws::parse_tx_object;
use sniper_core::types::Commitment;

#[test]
fn real_mainnet_fixtures_decode_and_satisfy_invariants() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/real");
    check_dir(&dir, "mainnet-beta");
}

/// The harness itself is exercised on synthetic RPC-shaped transactions in a temp dir (never in fixtures/real).
#[test]
fn invariant_harness_self_test_on_synthetic_rpc_shapes() {
    use sniper_core::decode::{anchor, build, pumpswap};
    use sniper_core::fixtures::{wallet, Curve, Pool};
    let dir = tempfile::tempdir().unwrap();
    let mut c = Curve::new("SELF");
    let evs = vec![c.create("S", "S", 0), c.buy(wallet("a"), 500_000_000, 0).unwrap(), c.buy(wallet("b"), 200_000_000, 0).unwrap()];
    let sell = c.sell(wallet("a"), 1_000_000_000_000, 0).unwrap();
    let mut pool = Pool::new("SELF", sniper_core::types::Pubkey::synthetic("m"), 400_000_000_000_000, 70_000_000_000);
    let ps = pool.buy(wallet("p"), 1_000_000_000, 0);
    let mut data = anchor::EVENT_IX_TAG.to_vec();
    data.extend(pumpswap::encode(&ps));
    let docs = [
        ("s1", build::pump_logs(&evs), vec![]),
        ("s2", build::pump_logs(&[sell]), vec![]),
        ("s3", build::pumpswap_inner(&[ps]).0, vec![serde_json::json!({"programId": sniper_core::types::programs::PUMP_SWAP, "data": bs58::encode(&data).into_string()})]),
    ];
    for (sig, logs, inner) in docs {
        let doc = serde_json::json!({
            "provenance": {"network": "synthetic-selftest"},
            "signature": sig,
            "rpc_result": {"slot": 1, "blockTime": 1, "meta": {"err": null, "logMessages": logs, "innerInstructions": [{"index": 0, "instructions": inner}]}, "transaction": {"signatures": [sig]}},
        });
        std::fs::write(dir.path().join(format!("{sig}.json")), doc.to_string()).unwrap();
    }
    let (files, events, checked) = check_dir(dir.path(), "synthetic-selftest");
    assert_eq!((files, events, checked), (3, 5, 4));
}

fn check_dir(dir: &std::path::Path, network: &str) -> (usize, usize, usize) {
    let mut files: Vec<_> = std::fs::read_dir(&dir).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect()).unwrap_or_default();
    files.sort();
    if files.is_empty() {
        eprintln!("REAL FIXTURES: 0 captured mainnet transactions in fixtures/real — mainnet decoder verification NOT yet performed (run `sniper capture`).");
        return (0, 0, 0);
    }
    let (mut events, mut checked) = (0, 0);
    for f in &files {
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        assert_eq!(doc["provenance"]["network"], network, "{}", f.display());
        let sig = doc["signature"].as_str().unwrap();
        let raw = parse_tx_object(&doc["rpc_result"], Some(sig), 0, "capture", Commitment::Confirmed, 0).expect("parsable transaction");
        let out = decode_raw(&raw);
        assert!(out.warnings.is_empty(), "{}: {:?}", f.display(), out.warnings);
        if network == "mainnet-beta" {
            // independent transaction evidence (token/SOL balance deltas, fee arithmetic)
            let v = sniper_core::verify::verify_tx(&doc["rpc_result"], sig);
            let failed: Vec<_> = v.checks.iter().filter(|c| c.hard && !c.ok).map(|c| format!("{}: {}", c.name, c.detail)).collect();
            assert!(failed.is_empty(), "{}: decoder disagrees with transaction evidence: {failed:?}", f.display());
            assert!(doc["provenance"]["slot"].as_u64().is_some() || doc["rpc_result"]["slot"].as_u64().is_some(), "slot provenance");
        }
        for d in &out.events {
            events += 1;
            match &d.event {
                VenueEvent::Pump(PumpEvent::Trade(t)) if t.is_sol_quoted() => {
                    // constant product on virtual reserves: reconstruct the pre-trade state from the
                    // post-trade reserves and amounts; k must not decrease by more than rounding
                    let (vs, vt) = (t.virtual_sol_reserves as i128, t.virtual_token_reserves as i128);
                    let (ds, dt) = (t.sol_amount as i128, t.token_amount as i128);
                    let (pre_s, pre_t) = if t.is_buy { (vs - ds, vt + dt) } else { (vs + ds, vt - dt) };
                    let (k_pre, k_post) = (pre_s * pre_t, vs * vt);
                    assert!(k_post * 1_000_000 >= k_pre * 999_999, "{}: curve invariant k_post {k_post} < k_pre {k_pre}", f.display());
                    checked += 1;
                }
                VenueEvent::PumpSwap(PumpSwapEvent::Swap(s)) if s.is_buy => {
                    // SDK: base_out = floor(base * (net-1) / (quote_eff + net - 1)), net = quote_in (excl. fees)
                    let q_eff = (s.pool_quote_token_reserves as i128 + s.virtual_quote_reserves.unwrap_or(0)) as u128;
                    let net = (s.quote_amount_pool - s.lp_fee) as u128;
                    let expected = s.pool_base_token_reserves as u128 * (net - 1) / (q_eff + net - 1);
                    let diff = (expected as i128 - s.base_amount as i128).abs();
                    assert!(diff <= 1 + expected as i128 / 10_000, "{}: pumpswap buy out {} vs model {expected}", f.display(), s.base_amount);
                    checked += 1;
                }
                _ => {}
            }
        }
    }
    eprintln!("{network} FIXTURES: {} files, {events} venue events decoded, {checked} invariant checks passed", files.len());
    (files.len(), events, checked)
}
