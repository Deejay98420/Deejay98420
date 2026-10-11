//! Re-verification of REAL captured mainnet transactions (`fixtures/real/*.json`, written by
//! `sniper capture`). The files are only READ: the test hashes every file before and after to prove
//! it. Whether a venue is mainnet-verified is decided by `sniper verify-fixtures` (report in
//! reports/verification/), not by this test: evidence mismatches on real data are reported, while
//! this test fails only on code defects (unparsable shapes, panics, modified files).

use sha2::{Digest, Sha256};
use sniper_core::decode::decode_raw;
use sniper_core::decode::pumpfun::PumpEvent;
use sniper_core::fixtures::{wallet, Curve, Pool};
use sniper_core::stream::ws::parse_tx_object;
use sniper_core::txbuild::{self, CurveAccounts, Emission};
use sniper_core::types::{Commitment, Pubkey};
use sniper_core::verify::{summarize, verify_tx};

fn fixture_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json") && !p.file_name().unwrap().to_string_lossy().starts_with("verification")).collect())
        .unwrap_or_default();
    files.sort();
    files
}

fn digest(files: &[std::path::PathBuf]) -> Vec<(String, String)> {
    files.iter().map(|f| (f.display().to_string(), format!("{:x}", Sha256::digest(std::fs::read(f).unwrap())))).collect()
}

/// Re-verify every mainnet fixture in `dir`; returns (files, events).
fn reverify(dir: &std::path::Path, network: &str) -> (usize, usize) {
    let files = fixture_files(dir);
    if files.is_empty() {
        eprintln!("REAL FIXTURES: 0 captured mainnet transactions in {} — mainnet decoder verification NOT yet performed (run `sniper capture`).", dir.display());
        return (0, 0);
    }
    let before = digest(&files);
    let mut results = Vec::new();
    let mut events = 0;
    for f in &files {
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap_or_else(|e| panic!("{}: not JSON: {e}", f.display()));
        if doc["provenance"]["network"] != network {
            continue;
        }
        let sig = doc["signature"].as_str().unwrap_or_else(|| panic!("{}: no signature", f.display()));
        let raw = parse_tx_object(&doc["rpc_result"], Some(sig), 0, "capture", Commitment::Confirmed, 0).unwrap_or_else(|| panic!("{}: unparsable transaction shape", f.display()));
        events += decode_raw(&raw).events.len();
        results.push(verify_tx(&doc["rpc_result"], sig));
    }
    assert_eq!(digest(&files), before, "fixture files must never be modified");
    let s = summarize(&results, 5);
    for (venue, r) in &s.venues {
        eprintln!(
            "{network} FIXTURES {venue}: {} — {} sigs, {} events, {} merged duplicates, checks {} pass / {} fail / {} inconclusive; failures {:?}",
            r.status, r.unique_signatures, r.events_decoded, r.duplicate_emissions_merged, r.checks_passed, r.checks_failed, r.checks_inconclusive, r.failures_by_check
        );
    }
    (results.len(), events)
}

#[test]
fn real_mainnet_fixtures_are_reverified_read_only() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/real");
    reverify(&dir, "mainnet-beta");
}

/// The harness itself, on realistic synthetic RPC shapes in a temp dir (never in fixtures/real).
#[test]
fn reverify_harness_self_test_on_realistic_synthetic_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Curve::new("SELF");
    let acc = CurveAccounts::of(&c);
    let PumpEvent::Trade(b) = c.buy(wallet("a"), 500_000_000, 0).unwrap() else { panic!() };
    let PumpEvent::Trade(s) = c.sell(wallet("a"), b.token_amount / 2, 0).unwrap() else { panic!() };
    let mint = Pubkey::synthetic("m");
    let mut pool = Pool::new("SELF", mint, 400_000_000_000_000, 70_000_000_000);
    let ps = pool.buy(wallet("p"), 1_000_000_000, 0);
    let docs = [
        ("s1", txbuild::pump_trade_tx(&acc, &b, 900_000_000_000_000, Emission::Both, serde_json::json!(0), "s1")),
        ("s2", txbuild::pump_trade_tx(&acc, &s, 900_000_000_000_000, Emission::Both, serde_json::json!(1), "s2")),
        ("s3", txbuild::pumpswap_swap_tx(&ps, &mint.to_string(), serde_json::json!(1), "s3")),
    ];
    for (sig, tx) in docs {
        let doc = serde_json::json!({"provenance": {"network": "synthetic-selftest"}, "signature": sig, "rpc_result": tx});
        std::fs::write(dir.path().join(format!("{sig}.json")), doc.to_string()).unwrap();
    }
    assert_eq!(reverify(dir.path(), "synthetic-selftest"), (3, 3));
    for f in fixture_files(dir.path()) {
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let v = verify_tx(&doc["rpc_result"], doc["signature"].as_str().unwrap());
        assert!(v.passed && v.checks.iter().filter(|c| c.hard).all(|c| c.ok), "{}: {:#?}", f.display(), v.checks);
    }
}
