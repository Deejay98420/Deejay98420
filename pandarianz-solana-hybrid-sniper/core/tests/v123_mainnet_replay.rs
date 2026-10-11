//! V1.2.3: offline replay of the 15 REAL mainnet transactions (`fixtures/mainnet/v121`, verbatim
//! copies checked against the archive's SHA-256 manifest before and after), plus in-memory
//! mutations of real transactions for contradictions, contamination and evidence gaps. The files
//! are never written.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sniper_core::verify::{summarize, verify_tx, Outcome, TxVerification};
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/mainnet/v121")
}

/// (signature, rpc_result) for every transaction, after checking each file against the manifest.
fn load() -> Vec<(String, Value)> {
    let manifest: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(dir().join("EVIDENCE_MANIFEST.json")).unwrap()).unwrap();
    let mut out = Vec::new();
    for e in manifest {
        let name = e["file"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
        if name.starts_with("verification") {
            continue;
        }
        let bytes = std::fs::read(dir().join(&name)).unwrap_or_else(|_| panic!("{name} missing"));
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), e["sha256"].as_str().unwrap(), "{name}: bytes differ from the evidence manifest");
        let doc: Value = serde_json::from_slice(&bytes).unwrap();
        out.push((doc["signature"].as_str().unwrap().to_string(), doc["rpc_result"].clone()));
    }
    assert_eq!(out.len(), 15);
    out
}

fn tx(prefix: &str) -> (String, Value) {
    load().into_iter().find(|(s, _)| s.starts_with(prefix)).unwrap()
}

fn check<'a>(v: &'a TxVerification, name: &str) -> &'a sniper_core::verify::Check {
    v.checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name} missing: {:#?}", v.checks))
}

#[test]
fn all_15_real_transactions_replay_offline_with_exact_outcomes() {
    let before: Vec<Vec<u8>> = std::fs::read_dir(dir()).unwrap().flatten().map(|e| std::fs::read(e.path()).unwrap()).collect();
    let results: Vec<TxVerification> = load().iter().map(|(s, t)| verify_tx(t, s)).collect();
    let s = summarize(&results, 5);
    let p = &s.venues["pump_bonding_curve"];
    assert_eq!(p.unique_signatures, 15);
    assert_eq!(p.checks_failed, 0, "{:#?}", s.failures);
    assert_eq!((p.events_fully_verified, p.fully_verified_buys, p.fully_verified_sells), (10, 7, 3), "{p:#?}");
    assert_eq!(p.fully_verified_token_quoted, 3, "token-quoted trades verified but never counted for the venue");
    let inc: Vec<(&str, u64)> = p.inconclusive_by_check.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    assert_eq!(inc, vec![("trade.creator_fee_observed", 1), ("trade.curve_net_balance", 1), ("trade.protocol_fee_observed", 2), ("trade.sol_amount", 1)]);
    assert_eq!(p.reconciled_via_ancestor.values().sum::<u64>(), 5, "all 5 inconclusives reconcile exactly but only through a wrapper");
    assert_eq!(p.status, "mainnet_verified");
    assert_eq!(p.attribution_mode, "strict");
    assert_eq!(s.venues["pumpswap"].status, "insufficient_evidence");
    assert_eq!(s.venues["pumpswap"].unique_signatures, 0);
    assert_eq!(s.venues["raydium_amm_v4"].status, "disabled");
    // every hard PASS on a trade names its emitting instruction; every non-PASS states a reason
    for r in &results {
        for c in r.checks.iter().filter(|c| c.hard) {
            if r.items.get(c.event_index).is_some_and(|i| i.ends_with("TradeEvent")) && c.outcome == Outcome::Pass {
                assert!(c.instruction.is_some(), "{} {}: no instruction provenance", r.signature, c.name);
            }
            if c.outcome != Outcome::Pass {
                assert!(c.detail.len() > 20, "{} {}", r.signature, c.name);
            }
        }
    }
    let after: Vec<Vec<u8>> = std::fs::read_dir(dir()).unwrap().flatten().map(|e| std::fs::read(e.path()).unwrap()).collect();
    assert_eq!(before, after, "fixture bytes unchanged");
}

/// 3As2Jg52: Pump.fun buy_exact_quote_in_v3 inside a routed swap (with CPMM and Raydium legs).
#[test]
fn gross_v3_payment_is_distinguished_from_base_sol_and_net_curve_balance() {
    let (sig, t) = tx("3As2Jg52");
    let v = verify_tx(&t, &sig);
    let s = check(&v, "trade.sol_amount");
    assert_eq!(s.outcome, Outcome::Pass, "{}", s.detail);
    for n in ["parsed payment 6286873 (ix 3.2)", "base 6209257", "protocol fee 58988", "creator fee 18628", "inside ix 3.0 buy_exact_quote_in_v3"] {
        assert!(s.detail.contains(n), "{n}: {}", s.detail);
    }
    // transaction-wide curve balance: a separate consistency check, attributable only via the wrapper
    let n = check(&v, "trade.curve_net_balance");
    assert_eq!(n.outcome, Outcome::Inconclusive);
    assert!(n.detail.contains("balance Δ 6257379 reconciles exactly with 6257379") && n.detail.contains("calling instruction(s) ix 3 "), "{}", n.detail);
    assert!(!n.detail.contains("ix 3.5") && !n.detail.contains("ix 3.8"), "CPMM / Raydium legs never name the curve: {}", n.detail);
    let f = check(&v, "trade.protocol_fee_observed");
    assert_eq!(f.outcome, Outcome::Inconclusive);
    assert!(f.detail.contains("official buyback recipient 5eHhjP") && f.detail.contains("event buyback_fee 29494 = 5000 bps of fee 58988"), "{}", f.detail);
    assert_eq!(check(&v, "trade.creator_fee_observed").outcome, Outcome::Pass);
    assert!(v.reconciled_via_ancestor.contains(&"trade.curve_net_balance".to_string()));
}

fn inner_mut(t: &mut Value, outer: u64) -> &mut Vec<Value> {
    t["meta"]["innerInstructions"].as_array_mut().unwrap().iter_mut().find(|g| g["index"] == outer).unwrap()["instructions"].as_array_mut().unwrap()
}

#[test]
fn an_explicitly_attributable_contradiction_still_fails() {
    let (sig, mut t) = tx("3As2Jg52");
    for ix in inner_mut(&mut t, 3).iter_mut() {
        if ix["parsed"]["type"] == "transfer" && ix["parsed"]["info"]["lamports"] == 6_286_873 {
            ix["parsed"]["info"]["lamports"] = json!(6_286_872);
        }
    }
    let v = verify_tx(&t, &sig);
    let s = check(&v, "trade.sol_amount");
    assert_eq!(s.outcome, Outcome::Fail, "{}", s.detail);
    assert!(s.detail.contains("parsed payment 6286872") && s.detail.contains("≠"), "{}", s.detail);
    assert!(!v.passed);
}

#[test]
fn unrelated_instructions_naming_the_curve_make_balance_evidence_inconclusive_not_pass() {
    let (sig, mut t) = tx("3As2Jg52");
    let curve = "49V8c4zjwuhvhNRuH87Sa32LpWrztgtQA8zW6PUZHLZd";
    t["transaction"]["message"]["instructions"].as_array_mut().unwrap().push(json!({"programId": "Contam1nate111111111111111111111111111111111", "accounts": [curve], "data": "1", "stackHeight": null}));
    let v = verify_tx(&t, &sig);
    // the parsed in-instruction payment is unaffected …
    assert_eq!(check(&v, "trade.sol_amount").outcome, Outcome::Pass);
    // … but the transaction-wide curve balance is no longer attributable
    let n = check(&v, "trade.curve_net_balance");
    assert_eq!(n.outcome, Outcome::Inconclusive);
    assert!(n.detail.contains("unrelated instruction(s)"), "{}", n.detail);
}

#[test]
fn missing_inner_instruction_records_are_evidence_gaps_never_failures() {
    for prefix in ["3As2Jg52", "5guPtRHy", "3ccp1Kv1"] {
        let (sig, mut t) = tx(prefix);
        t["meta"]["innerInstructions"] = Value::Null;
        let v = verify_tx(&t, &sig);
        assert!(!v.checks.iter().any(|c| c.hard && c.outcome == Outcome::Fail), "{prefix}: {:#?}", v.checks.iter().filter(|c| c.outcome == Outcome::Fail).collect::<Vec<_>>());
        assert!(v.checks.iter().any(|c| c.hard && c.outcome == Outcome::Inconclusive), "{prefix}");
    }
}

#[test]
fn odd_lamport_fee_split_and_extra_vault_funding_on_real_transactions() {
    let (sig, t) = tx("3ccp1Kv1");
    let f = verify_tx(&t, &sig);
    let c = check(&f, "trade.protocol_fee_observed");
    assert_eq!(c.outcome, Outcome::Pass, "{}", c.detail);
    assert!(c.detail.contains("decoded fee 9383") && c.detail.contains(": 4692") && c.detail.contains(": 4691"), "{}", c.detail);
    let (sig, t) = tx("3R1MvPxv");
    let v = verify_tx(&t, &sig);
    assert_eq!(check(&v, "trade.creator_fee_observed").outcome, Outcome::Pass);
    assert!(v.notes.iter().any(|n| n.contains("650240") && n.contains("not counted as fee")), "{:?}", v.notes);
    // a buyback account that is not an official Global.buybackFeeRecipients wallet is not a fee recipient
    let mut t2 = t.clone();
    let s = serde_json::to_string(&t2).unwrap().replace("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD", "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeE");
    t2 = serde_json::from_str(&s).unwrap();
    assert_eq!(check(&verify_tx(&t2, &sig), "trade.protocol_fee_observed").outcome, Outcome::Fail);
}

#[test]
fn token_quoted_v3_trades_reconcile_every_leg_from_parsed_transfers() {
    for prefix in ["2VbUyJhX", "5E744U16", "5cchEYq7"] {
        let (sig, t) = tx(prefix);
        let v = verify_tx(&t, &sig);
        let q = check(&v, "trade.quote_amount");
        assert_eq!(q.outcome, Outcome::Pass, "{prefix}: {}", q.detail);
        assert!(q.detail.contains("buyback slice") && q.detail.contains("paid directly"), "{}", q.detail);
        assert_eq!(check(&v, "trade.protocol_fee_observed").outcome, Outcome::Pass, "{prefix}: {}", check(&v, "trade.protocol_fee_observed").detail);
        assert!(v.token_quoted[0], "{prefix}: quoted in a non-SOL token (never traded)");
    }
}

/// Strict (default) vs --accept-ancestor-scoped, through the CLI, on a scratch copy.
#[test]
fn cli_reports_strict_and_ancestor_scoped_modes_without_touching_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(dir()).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == "json") && !e.file_name().to_string_lossy().starts_with("EVIDENCE") {
            std::fs::copy(e.path(), tmp.path().join(e.file_name())).unwrap();
        }
    }
    let run = |extra: &[&str]| {
        let rep = tempfile::tempdir().unwrap();
        let mut args = vec!["verify-fixtures", "--dir", tmp.path().to_str().unwrap(), "--report-dir", rep.path().to_str().unwrap()];
        args.extend_from_slice(extra);
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_sniper")).args(&args).output().unwrap();
        assert!(o.status.success());
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    let strict = run(&[]);
    assert!(strict.contains("hard checks 164 passed / 0 failed / 5 inconclusive"), "{strict}");
    assert!(strict.contains("10 trade events passed every hard check (7 buys, 3 sells)"), "{strict}");
    assert!(strict.contains("pump_bonding_curve=YES, pumpswap=no"));
    let acc = run(&["--accept-ancestor-scoped"]);
    assert!(acc.contains("hard checks 169 passed / 0 failed / 0 inconclusive"), "{acc}");
    assert!(acc.contains("12 trade events passed every hard check (8 buys, 4 sells)"), "{acc}");
    let _ = load(); // fixture bytes still match the manifest
}
