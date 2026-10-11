//! V1.2.2 regressions reconstructing the structures reported by the forensic audit of 15 real
//! mainnet captures (amounts and recipient accounts taken from the audit). These are SYNTHETIC
//! reconstructions of the observed shapes — they prove the verifier logic, not mainnet
//! verification; the real files are re-verified by `sniper verify-fixtures` on the user's machine.

use serde_json::json;
use sniper_core::decode::pumpfun::{PumpEvent, PumpTrade};
use sniper_core::fixtures::{wallet, Curve};
use sniper_core::txbuild::{self, CurveAccounts, Emission, SynthTx, TradeOpts};
use sniper_core::types::Pubkey;
use sniper_core::verify::{summarize, verify_tx, Outcome, TxVerification};

const WRAPPER: &str = "WrapRouter1111111111111111111111111111111111";

fn check<'a>(v: &'a TxVerification, name: &str) -> &'a sniper_core::verify::Check {
    v.checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name} missing: {:#?}", v.checks))
}

/// A buy (or sell) whose decoded fee / creator fee are set to `fee` / `cfee` (0 = keep the
/// SDK-consistent values, so formula checks also pass), built with `opts`.
fn trade_tx(label: &str, is_buy: bool, fee: u64, cfee: u64, opts: TradeOpts, how: Emission) -> (serde_json::Value, PumpTrade) {
    let mut c = Curve::new(label);
    let acc = CurveAccounts::of(&c);
    let user = wallet(&format!("{label}-user"));
    let PumpEvent::Trade(mut t) = c.buy(user, 2_000_000_000, 0).unwrap() else { panic!() };
    if !is_buy {
        let PumpEvent::Trade(s) = c.sell(user, t.token_amount, 0).unwrap() else { panic!() };
        t = s;
    }
    if fee > 0 {
        t.fee = Some(fee);
    }
    if cfee > 0 {
        t.creator_fee = Some(cfee);
    }
    let mut tx = SynthTx::new(&user.to_string(), json!(0));
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &user.to_string(), &acc.mint, Some(if is_buy { 0 } else { t.token_amount as i128 }));
    if let Some((bb, _)) = &opts.buyback {
        tx.lamports(bb, 0);
    }
    txbuild::add_pump_trade_opts(&mut tx, &acc, &t, &PumpEvent::Trade(t.clone()), how, &opts);
    (tx.build(&format!("{label}-sig"), 1), t)
}

fn bb(addr: &str, share: u64) -> Option<(String, u64)> {
    Some((addr.to_string(), share))
}

/// Audit table: decoded fee = fee-recipient share + official buyback recipient share.
#[test]
fn split_protocol_fees_from_the_audit_reconcile_exactly_without_assuming_50_50() {
    let rows: [(&str, u64, u64, u64, &str); 5] = [
        ("3R1MvPxv", 6_567_902, 3_283_951, 3_283_951, "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
        ("3ccp1Kv", 9_383, 4_692, 4_691, "5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD"),
        ("3jcsU1N", 469_136, 234_568, 234_568, "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
        ("3y72Fb3", 375_309, 187_655, 187_654, "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
        ("4Ciusi8", 36_843, 18_422, 18_421, "GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL"),
    ];
    for (label, fee, first, second, recipient) in rows {
        assert_eq!(first + second, fee, "{label}: audit amounts are exact integers");
        let (tx, _) = trade_tx(label, true, fee, 1_000, TradeOpts { buyback: bb(recipient, second), ..Default::default() }, Emission::Both);
        let v = verify_tx(&tx, &format!("{label}-sig"));
        let c = check(&v, "trade.protocol_fee_observed");
        assert_eq!(c.outcome, Outcome::Pass, "{label}: {}", c.detail);
        assert!(c.detail.contains("fee_recipient") && c.detail.contains(&first.to_string()) && c.detail.contains(&second.to_string()), "{}", c.detail);
        assert!(c.detail.contains("official Global.buybackFeeRecipients") && c.detail.contains("parsed System transfers inside ix 0 buy"), "{}", c.detail);
        assert!(c.instruction.as_deref().is_some_and(|s| s.starts_with("ix 0 buy")), "{:?}", c.instruction);
        assert_eq!(v.fee_hypotheses, vec!["2_recipient(s):parsed_transfers"]);
    }
}

#[test]
fn second_recipient_must_be_official_and_bound_to_the_instruction() {
    // same split, but the second account is not in Global.buybackFeeRecipients → not a fee recipient
    let (tx, _) = trade_tx("NOTOFF", true, 9_383, 1_000, TradeOpts { buyback: bb(&Pubkey::synthetic("random").to_string(), 4_691), ..Default::default() }, Emission::Both);
    let c = check(&verify_tx(&tx, "NOTOFF-sig"), "trade.protocol_fee_observed").clone();
    assert_eq!(c.outcome, Outcome::Fail, "{}", c.detail);
    assert!(c.detail.contains("4692"), "{}", c.detail);
}

/// `5guPtRHy…`: sell through a wrapper, fee shares visible only as balance deltas → INCONCLUSIVE.
#[test]
fn balance_only_split_through_a_wrapper_stays_inconclusive() {
    let opts = TradeOpts { buyback: bb("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW", 419_336), wrapper: Some(WRAPPER.into()), ..Default::default() };
    let (tx, _) = trade_tx("5guPtRHy", false, 838_673, 1_000, opts, Emission::Both);
    let v = verify_tx(&tx, "5guPtRHy-sig");
    let c = check(&v, "trade.protocol_fee_observed");
    assert_eq!(c.outcome, Outcome::Inconclusive, "{}", c.detail);
    assert!(c.detail.contains("419337") && c.detail.contains("419336") && c.detail.contains("equals decoded fee 838673"), "{}", c.detail);
    assert!(c.detail.contains("also named by ix 0"), "{}", c.detail);
    // the instruction itself is matched through the wrapper's CPI
    assert_eq!(check(&v, "trade.instruction").outcome, Outcome::Pass);
    // the same sell as a top-level Pump instruction: accounts named only by it → attributable
    let opts = TradeOpts { buyback: bb("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW", 419_336), ..Default::default() };
    let (tx, _) = trade_tx("TOPSELL", false, 838_673, 1_000, opts, Emission::Both);
    let c = check(&verify_tx(&tx, "TOPSELL-sig"), "trade.protocol_fee_observed").clone();
    assert_eq!(c.outcome, Outcome::Pass, "{}", c.detail);
    assert!(c.detail.contains("balance deltas; accounts named only by ix 0 sell"), "{}", c.detail);
}

/// `3R1MvPxv…`: creator vault gets 650,240 + 2,074,075; the decoded creator fee is 2,074,075.
#[test]
fn creator_fee_compared_with_its_own_transfer_and_extra_vault_funding_tracked_separately() {
    let mut c = Curve::new("3R1MvPxv");
    let acc = CurveAccounts::of(&c);
    let user = wallet("3R1M-user");
    let PumpEvent::Trade(mut t) = c.buy(user, 2_000_000_000, 0).unwrap() else { panic!() };
    t.creator_fee = Some(2_074_075);
    let opts = TradeOpts { creator_extra: Some(650_240), buyback: bb("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD", t.fee.unwrap() / 2), ..Default::default() };
    let mut tx = SynthTx::new(&user.to_string(), json!(0));
    tx.new_account(&acc.creator_vault);
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &user.to_string(), &acc.mint, Some(0));
    txbuild::add_pump_trade_opts(&mut tx, &acc, &t, &PumpEvent::Trade(t.clone()), Emission::Both, &opts);
    let tx = tx.build("3R1M-sig", 1);
    let m = sniper_core::txmodel::TxModel::parse(&tx, "3R1M-sig").unwrap();
    assert_eq!(m.lamport_delta(&acc.creator_vault), Some(2_724_315), "vault gain = 650,240 + 2,074,075 from zero");
    let v = verify_tx(&tx, "3R1M-sig");
    let ch = check(&v, "trade.creator_fee_observed");
    assert_eq!(ch.outcome, Outcome::Pass, "{}", ch.detail);
    assert!(ch.detail.contains("creator-fee transfer 2074075") && ch.detail.contains("650240"), "{}", ch.detail);
    assert!(v.notes.iter().any(|n| n.contains("650240") && n.contains("not counted as fee")), "{:?}", v.notes);
    // if no transfer equals the decoded creator fee it is a FAIL, never absorbed as "extra funding"
    let mut wrong = t.clone();
    wrong.creator_fee = Some(2_000_000);
    let mut tx = SynthTx::new(&user.to_string(), json!(0));
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &user.to_string(), &acc.mint, Some(0));
    txbuild::add_pump_trade_opts(&mut tx, &acc, &t, &PumpEvent::Trade(wrong), Emission::Both, &opts);
    assert_eq!(check(&verify_tx(&tx.build("w", 1), "w"), "trade.creator_fee_observed").outcome, Outcome::Fail);
}

/// `2VbUyJh…`, `3As2Jg5…`, `5E744U1…`: Pump.fun trade as a CPI inside another program.
#[test]
fn nested_cpi_trades_are_matched_through_stack_provenance() {
    for how in [Emission::Both, Emission::CpiOnly, Emission::LogOnly] {
        let (tx, t) = trade_tx("NEST", true, 9_383, 1_000, TradeOpts { wrapper: Some(WRAPPER.into()), ..Default::default() }, how);
        let v = verify_tx(&tx, "NEST-sig");
        let c = check(&v, "trade.instruction");
        assert_eq!(c.outcome, Outcome::Pass, "{how:?}: {}", c.detail);
        assert_eq!(c.instruction.as_deref().map(|s| s.starts_with("ix 0.0 buy")), Some(true), "{how:?}: {:?}", c.instruction);
        assert_eq!(check(&v, "trade.token_transfer").outcome, Outcome::Pass, "{how:?}");
        assert!(check(&v, "trade.token_transfer").detail.contains(&t.token_amount.to_string()));
    }
}

/// `5cchEYq…`: top-level Pump.fun instruction with an unrecognized discriminator.
#[test]
fn unrecognized_instruction_layout_is_inconclusive_never_verified() {
    let opts = TradeOpts { disc: Some([0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3]), ..Default::default() };
    let (tx, _) = trade_tx("UNK", true, 9_383, 1_000, opts, Emission::Both);
    let v = verify_tx(&tx, "UNK-sig");
    let c = check(&v, "trade.instruction");
    assert_eq!(c.outcome, Outcome::Inconclusive);
    assert!(c.detail.contains("unrecognized instruction discriminator deadbeef00010203"), "{}", c.detail);
    let (_, _, n) = v.event_outcomes(0);
    assert!(n > 0, "an unsupported instruction leaves the event not fully verified");
    let s = summarize(&[v], 1);
    assert_eq!(s.venues["pump_bonding_curve"].events_fully_verified, 0);
}

/// `5guPtRHy…`, `66uGS3a…`, `vP1QQ6j…`: sells that close the user's token account.
#[test]
fn sells_closing_the_token_account_reconcile_from_parsed_transfer_and_close() {
    let (tx, t) = trade_tx("CLOSE", false, 9_383, 1_000, TradeOpts { close_user_ata: true, ..Default::default() }, Emission::Both);
    let user = wallet("CLOSE-user").to_string();
    let ata = txbuild::user_ata(&user, &Curve::new("CLOSE").mint.to_string());
    assert!(!tx["meta"]["postTokenBalances"].as_array().unwrap().iter().any(|b| b["owner"] == json!(user)), "closed account absent from post balances");
    let m = sniper_core::txmodel::TxModel::parse(&tx, "CLOSE-sig").unwrap();
    assert_eq!(m.closed_accounts.len(), 1);
    assert_eq!((m.closed_accounts[0].account.as_str(), m.closed_accounts[0].balance_at_close), (ata.as_str(), 0));
    let v = verify_tx(&tx, "CLOSE-sig");
    for name in ["trade.user_token_delta", "token.mint_conservation", "trade.token_transfer"] {
        let c = check(&v, name);
        assert_eq!(c.outcome, Outcome::Pass, "{name}: {}", c.detail);
    }
    assert!(check(&v, "trade.user_token_delta").detail.contains(&format!("-{}", t.token_amount)));
    // closing an account that still held tokens cannot be reconciled → INCONCLUSIVE, not a pass
    let mut c = Curve::new("HALF");
    let acc = CurveAccounts::of(&c);
    let u = wallet("HALF-user");
    let PumpEvent::Trade(b) = c.buy(u, 2_000_000_000, 0).unwrap() else { panic!() };
    let PumpEvent::Trade(s) = c.sell(u, b.token_amount / 2, 0).unwrap() else { panic!() };
    let mut tx = SynthTx::new(&u.to_string(), json!(0));
    tx.token(&acc.vault, &acc.mint, &acc.curve, Some(900_000_000_000_000), Some(900_000_000_000_000));
    txbuild::ensure_user_ata(&mut tx, &u.to_string(), &acc.mint, Some(b.token_amount as i128));
    txbuild::add_pump_trade_opts(&mut tx, &acc, &s, &PumpEvent::Trade(s.clone()), Emission::Both, &TradeOpts { close_user_ata: true, ..Default::default() });
    let v = verify_tx(&tx.build("half", 1), "half");
    assert_eq!(check(&v, "trade.user_token_delta").outcome, Outcome::Inconclusive, "{}", check(&v, "trade.user_token_delta").detail);
    assert_eq!(check(&v, "token.mint_conservation").outcome, Outcome::Inconclusive);
}

#[test]
fn venue_rule_unchanged_five_full_passes_with_buys_and_sells_and_no_failures() {
    let mut results = Vec::new();
    for i in 0..3 {
        let (_, t) = trade_tx(&format!("B{i}"), true, 0, 0, TradeOpts::default(), Emission::Both);
        let share = t.fee.unwrap() / 2; // any split: only the total is checked
        let (tx, _) = trade_tx(&format!("B{i}"), true, 0, 0, TradeOpts { buyback: bb("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD", share), ..Default::default() }, Emission::Both);
        results.push(verify_tx(&tx, &format!("B{i}-sig")));
    }
    let (tx, _) = trade_tx("S0", false, 0, 0, TradeOpts { close_user_ata: true, ..Default::default() }, Emission::Both);
    results.push(verify_tx(&tx, "S0-sig"));
    let s = summarize(&results, 5);
    assert_eq!(s.venues["pump_bonding_curve"].events_fully_verified, 4, "{:#?}", s.venues);
    assert_eq!(s.venues["pump_bonding_curve"].status, "insufficient_evidence");
    let (tx, _) = trade_tx("S1", false, 0, 0, TradeOpts::default(), Emission::Both);
    results.push(verify_tx(&tx, "S1-sig"));
    let s = summarize(&results, 5);
    assert_eq!(s.venues["pump_bonding_curve"].status, "mainnet_verified", "synthetic only — real status comes from fixtures/real");
    assert_eq!(s.venues["pumpswap"].status, "insufficient_evidence");
    assert_eq!(s.venues["raydium_amm_v4"].status, "disabled");
}

#[test]
fn reverification_archives_the_previous_report_and_shows_before_after_per_check() {
    use sha2::{Digest, Sha256};
    let fx = tempfile::tempdir().unwrap();
    let rep = tempfile::tempdir().unwrap();
    let (tx, _) = trade_tx("3ccp1Kv", true, 0, 0, TradeOpts { buyback: bb("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD", 4_691), ..Default::default() }, Emission::Both);
    // shape of a captured file (the CLI only reads it); labelled mainnet so it is included
    let doc = json!({"provenance": {"network": "mainnet-beta", "note": "synthetic reconstruction for a CLI test"}, "signature": "3ccp1Kv-sig", "rpc_result": tx});
    let f = fx.path().join("3ccp1Kv-sig.json");
    std::fs::write(&f, doc.to_string()).unwrap();
    let hash = || format!("{:x}", Sha256::digest(std::fs::read(&f).unwrap()));
    let h0 = hash();
    // a previous (V1.2.1-style) report in which this check failed
    std::fs::write(rep.path().join("verification.json"), json!({"venues": {"pump_bonding_curve": {"status": "MISMATCH", "checks_passed": 10, "checks_failed": 1, "checks_inconclusive": 0, "events_fully_verified": 0}}}).to_string()).unwrap();
    std::fs::write(
        rep.path().join("verification-detail.json"),
        json!([{"signature": "3ccp1Kv-sig", "checks": [{"event_index": 0, "name": "trade.protocol_fee_observed", "outcome": "fail", "ok": false, "hard": true, "detail": "fee recipient: observed 4692 vs decoded fee 9383"}]}]).to_string(),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sniper"))
        .args(["verify-fixtures", "--dir", fx.path().to_str().unwrap(), "--report-dir", rep.path().to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("BEFORE → AFTER"), "{stdout}");
    assert!(stdout.contains("hard checks pass/fail/inconclusive 10/1/0 →"), "{stdout}");
    assert!(stdout.lines().any(|l| l.contains("trade.protocol_fee_observed") && l.contains("FAIL → PASS")), "{stdout}");
    assert_eq!(hash(), h0, "fixture file untouched");
    let hist: Vec<_> = std::fs::read_dir(rep.path().join("history")).unwrap().flatten().collect();
    assert_eq!(hist.len(), 1);
    assert!(hist[0].path().join("verification-detail.json").exists(), "previous report preserved");
    assert!(std::fs::read_dir(rep.path()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("comparison-")));
}
