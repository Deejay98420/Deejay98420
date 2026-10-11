//! V1.2.4: offline replay of all 35 REAL mainnet transactions (15 Pump.fun in fixtures/mainnet/v121,
//! 20 PumpSwap in fixtures/mainnet/v123), checked against the archive manifest before and after.
//! Every one of the 24 former PumpSwap failures and 9 inconclusives is re-asserted individually,
//! and in-memory mutations of real transactions prove contradictions FAIL and gaps stay INCONCLUSIVE.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sniper_core::verify::{summarize, verify_tx, Outcome, TxVerification};
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/mainnet")
}

/// All 35 transactions (signature, rpc_result), every file checked against the V1.2.3 archive manifest.
fn load() -> Vec<(String, Value)> {
    let manifest: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(root().join("v123/EVIDENCE_MANIFEST.json")).unwrap()).unwrap();
    let mut out = Vec::new();
    for e in manifest {
        let name = e["path"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
        if !e["path"].as_str().unwrap().starts_with("fixtures/real/") || name.starts_with("verification") {
            continue;
        }
        let path = [root().join("v123").join(&name), root().join("v121").join(&name)].into_iter().find(|p| p.exists()).unwrap_or_else(|| panic!("{name} missing"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), e["sha256"].as_str().unwrap(), "{name}: bytes differ from the evidence manifest");
        let doc: Value = serde_json::from_slice(&bytes).unwrap();
        out.push((doc["signature"].as_str().unwrap().to_string(), doc["rpc_result"].clone()));
    }
    assert_eq!(out.len(), 35);
    out
}

fn tx(prefix: &str) -> (String, Value) {
    load().into_iter().find(|(s, _)| s.starts_with(prefix)).unwrap_or_else(|| panic!("{prefix}"))
}

fn checks<'a>(v: &'a TxVerification, ev: usize, name: &str) -> &'a sniper_core::verify::Check {
    v.checks.iter().find(|c| c.event_index == ev && c.name == name).unwrap_or_else(|| panic!("{} ev{ev} {name} missing", v.signature))
}

#[test]
fn all_35_real_transactions_replay_with_exact_venue_outcomes() {
    let results: Vec<TxVerification> = load().iter().map(|(s, t)| verify_tx(t, s)).collect();
    let s = summarize(&results, 5);
    let p = &s.venues["pump_bonding_curve"];
    assert_eq!((p.checks_passed, p.checks_failed, p.checks_inconclusive), (164, 0, 5), "Pump.fun must not regress");
    assert_eq!((p.events_fully_verified, p.fully_verified_buys, p.fully_verified_sells), (10, 7, 3));
    assert_eq!(p.status, "mainnet_verified");
    let a = &s.venues["pumpswap"];
    assert_eq!(a.unique_signatures, 20);
    assert_eq!((a.checks_passed, a.checks_failed, a.checks_inconclusive), (252, 0, 0), "{:#?}", a.failures_by_check);
    assert_eq!((a.events_fully_verified, a.fully_verified_buys, a.fully_verified_sells), (21, 10, 11));
    assert_eq!(a.status, "mainnet_verified");
    assert_eq!(s.venues["raydium_amm_v4"].status, "disabled");
    // every hard PASS on a PumpSwap swap names its instruction
    for r in results.iter().filter(|r| r.items.iter().any(|i| i.starts_with("pumpswap/"))) {
        for c in r.checks.iter().filter(|c| c.hard && c.outcome == Outcome::Pass) {
            assert!(c.instruction.is_some(), "{} {}", r.signature, c.name);
        }
    }
}

/// The 24 V1.2.3 failures and 9 inconclusives, one by one: (signature prefix, event, check, evidence fragments).
#[test]
fn every_former_failure_and_inconclusive_is_resolved_with_instruction_evidence() {
    let fee_v1 = ["official buyback wallet", "event buyback_fee", "[parsed transfers inside ix"];
    let cases: Vec<(&str, usize, &str, &[&str])> = vec![
        // 15 swap.protocol_fee_observed failures (v1: fee recipient ATA + official buyback ATA)
        ("2JUcyCfT", 0, "swap.protocol_fee_observed", &["79387", "39694", "39693"]),
        ("3BNQKP7V", 0, "swap.protocol_fee_observed", &["135611", "67806", "67805"]),
        ("3t4gQBTQ", 0, "swap.protocol_fee_observed", &["184149", "92075", "92074"]),
        ("3zV4Shbf", 0, "swap.protocol_fee_observed", &["3675243230", "1837621615"]),
        ("3zV4Shbf", 1, "swap.protocol_fee_observed", &["3512941790", "1756470895"]),
        ("455hevwa", 0, "swap.protocol_fee_observed", &["149000", "74500"]),
        ("4JmdVUUU", 0, "swap.protocol_fee_observed", &["532178", "266089"]),
        ("4VeQPGDw", 0, "swap.protocol_fee_observed", &["decoded protocol fee 70", ": 35"]),
        ("4biAZZLY", 0, "swap.protocol_fee_observed", &["decoded protocol fee 134", ": 67"]),
        ("4hC1P9zK", 0, "swap.protocol_fee_observed", &["decoded protocol fee 96", ": 48"]),
        ("57ChT6wL", 0, "swap.protocol_fee_observed", &["180541", "90271", "90270"]),
        ("57M2ZAsY", 0, "swap.protocol_fee_observed", &["decoded protocol fee 580", ": 290"]),
        ("5QP9C5kQ", 0, "swap.protocol_fee_observed", &["decoded protocol fee 593", ": 297", ": 296"]),
        ("7kaeJtWt", 0, "swap.protocol_fee_observed", &["2424284", "1212142"]),
        ("AeLpMcby", 0, "swap.protocol_fee_observed", &["2790932", "1395466"]),
        // 6 swap.pool_quote_delta failures (v2: protocol + creator fees stay in the vault, buyback slice leaves)
        ("2BcdZqag", 0, "swap.pool_quote_delta", &["-2525314016", "buyback slice 11835620", "fees stay in the vault"]),
        ("37F13Y3Y", 0, "swap.pool_quote_delta", &["98975507", "buyback slice 24493"]),
        ("3zGx4mqH", 0, "swap.pool_quote_delta", &["-58557231", "buyback slice 14791"]),
        ("5GCsNP5U", 0, "swap.pool_quote_delta", &["9897551", "buyback slice 2449"]),
        ("5UirE2yX", 0, "swap.pool_quote_delta", &["-215942", "buyback slice 54"]),
        ("Wo4pHWDz", 0, "swap.pool_quote_delta", &["-65537553", "buyback slice 16554"]),
        // 3 token.mint_conservation failures (wrapped SOL is not mint/burn)
        ("2JUcyCfT", 0, "token.mint_conservation", &["wrapped SOL", "token Δ = lamport Δ − rent Δ"]),
        ("3zV4Shbf", 0, "token.mint_conservation", &["wrapped SOL"]),
        ("3zV4Shbf", 1, "token.mint_conservation", &["wrapped SOL"]),
        // 6 swap.fee_transfers inconclusives → v2 buyback slice to an official ATA + vault identity
        ("2BcdZqag", 0, "swap.protocol_fee_observed", &["buyback slice 11835620", "retained in the pool vault"]),
        ("37F13Y3Y", 0, "swap.protocol_fee_observed", &["buyback slice 24493", "retained in the pool vault"]),
        ("3zGx4mqH", 0, "swap.protocol_fee_observed", &["buyback slice 14791"]),
        ("5GCsNP5U", 0, "swap.protocol_fee_observed", &["buyback slice 2449"]),
        ("5UirE2yX", 0, "swap.protocol_fee_observed", &["buyback slice 54"]),
        ("Wo4pHWDz", 0, "swap.protocol_fee_observed", &["buyback slice 16554"]),
        // 3 swap.user_base_delta inconclusives → parsed net inside the swap instruction
        ("2JUcyCfT", 0, "swap.user_base_delta", &["net inside ix 7 sell", "-174060"]),
        ("3zV4Shbf", 0, "swap.user_base_delta", &["net inside ix 8 sell", "-9713036781"]),
        ("3zV4Shbf", 1, "swap.user_base_delta", &["net inside ix 14 buy", "9287742585"]),
    ];
    assert_eq!(cases.len(), 24 + 9);
    for (prefix, ev, name, frags) in cases {
        let (sig, t) = tx(prefix);
        let v = verify_tx(&t, &sig);
        let c = checks(&v, ev, name);
        assert_eq!(c.outcome, Outcome::Pass, "{prefix} ev{ev} {name}: {}", c.detail);
        for f in frags {
            assert!(c.detail.contains(f), "{prefix} ev{ev} {name}: missing {f:?} in {}", c.detail);
        }
        if name == "swap.protocol_fee_observed" && !c.detail.contains("retained in the pool vault") {
            for f in fee_v1 {
                assert!(c.detail.contains(f), "{prefix}: {f:?} in {}", c.detail);
            }
        }
    }
}

fn inner_mut(t: &mut Value) -> impl Iterator<Item = &mut Value> {
    t["meta"]["innerInstructions"].as_array_mut().unwrap().iter_mut().flat_map(|g| g["instructions"].as_array_mut().unwrap().iter_mut())
}

#[test]
fn a_contradicting_buyback_transfer_fails() {
    let (sig, mut t) = tx("4biAZZLY");
    for ix in inner_mut(&mut t) {
        if ix["parsed"]["info"]["destination"].as_str().is_some_and(|d| d.starts_with("CA7v8g")) {
            ix["parsed"]["info"]["tokenAmount"]["amount"] = json!("66");
        }
    }
    let v = verify_tx(&t, &sig);
    assert_eq!(checks(&v, 0, "swap.protocol_fee_observed").outcome, Outcome::Fail, "{}", checks(&v, 0, "swap.protocol_fee_observed").detail);
}

#[test]
fn a_buyback_account_not_owned_by_an_official_wallet_is_not_a_fee_recipient() {
    let (sig, mut t) = tx("4biAZZLY");
    for field in ["preTokenBalances", "postTokenBalances"] {
        for b in t["meta"][field].as_array_mut().unwrap() {
            if b["owner"] == "EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL" {
                b["owner"] = json!("Unknown1111111111111111111111111111111111111");
            }
        }
    }
    let v = verify_tx(&t, &sig);
    let c = checks(&v, 0, "swap.protocol_fee_observed");
    assert_eq!(c.outcome, Outcome::Fail, "{}", c.detail);
    assert!(c.detail.contains("decoded protocol fee 134 vs parsed 67"), "{}", c.detail);
}

#[test]
fn a_v2_vault_movement_that_contradicts_the_event_fails() {
    let (sig, mut t) = tx("2BcdZqag");
    for ix in inner_mut(&mut t) {
        if ix["parsed"]["info"]["tokenAmount"]["amount"] == "2513478396" {
            ix["parsed"]["info"]["tokenAmount"]["amount"] = json!("2513478397");
        }
    }
    let v = verify_tx(&t, &sig);
    assert_eq!(checks(&v, 0, "swap.pool_quote_delta").outcome, Outcome::Fail);
    assert!(!v.passed);
}

#[test]
fn wrapped_sol_that_disagrees_with_lamports_is_not_a_pass() {
    let (sig, mut t) = tx("2JUcyCfT");
    let keys: Vec<String> = t["transaction"]["message"]["accountKeys"].as_array().unwrap().iter().map(|k| k["pubkey"].as_str().unwrap().to_string()).collect();
    let wsol: Vec<String> = t["meta"]["postTokenBalances"].as_array().unwrap().iter().chain(t["meta"]["preTokenBalances"].as_array().unwrap()).filter(|b| b["mint"] == "So11111111111111111111111111111111111111112").map(|b| keys[b["accountIndex"].as_u64().unwrap() as usize].clone()).collect();
    let i = keys.iter().position(|k| *k == wsol[0]).unwrap();
    let post = t["meta"]["postBalances"][i].as_i64().unwrap();
    t["meta"]["postBalances"][i] = json!(post + 1000);
    let v = verify_tx(&t, &sig);
    assert_ne!(checks(&v, 0, "token.mint_conservation").outcome, Outcome::Pass, "{}", checks(&v, 0, "token.mint_conservation").detail);
}

#[test]
fn missing_inner_instruction_records_never_produce_a_pass_or_a_failure_for_swaps() {
    for prefix in ["4biAZZLY", "2BcdZqag", "3zV4Shbf"] {
        let (sig, mut t) = tx(prefix);
        t["meta"]["innerInstructions"] = Value::Null;
        let v = verify_tx(&t, &sig);
        for name in ["swap.protocol_fee_observed", "swap.pool_quote_delta", "swap.base_transfer"] {
            for c in v.checks.iter().filter(|c| c.name == name) {
                assert_eq!(c.outcome, Outcome::Inconclusive, "{prefix} {name}: {}", c.detail);
            }
        }
    }
}

#[test]
fn two_swaps_sharing_a_fee_account_are_reconciled_per_instruction() {
    let (sig, t) = tx("3zV4Shbf");
    let v = verify_tx(&t, &sig);
    let a = checks(&v, 0, "swap.protocol_fee_observed");
    let b = checks(&v, 1, "swap.protocol_fee_observed");
    assert!(a.detail.contains("inside ix 8 sell") && b.detail.contains("inside ix 14 buy"), "{} | {}", a.detail, b.detail);
    assert!(a.detail.contains("CzspTp") && b.detail.contains("CzspTp"), "same fee account, separate instruction-level amounts");
}
