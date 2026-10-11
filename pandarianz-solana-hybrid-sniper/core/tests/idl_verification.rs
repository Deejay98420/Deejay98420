//! Decoder verification against the OFFICIAL program IDLs (`fixtures/idl/*.idl-subset.json`, extracted
//! from `@pump-fun/pump-sdk@4.0.0` and `@pump-fun/pump-swap-sdk@2.1.0`, provenance inside the files).
//!
//! A generic Borsh encoder driven only by the IDL type descriptions builds event payloads; the
//! production decoders must recover every field the engine uses. This does not depend on our own
//! encoders, so a layout drift in either direction fails here.

use serde_json::Value;
use sha2::{Digest, Sha256};
use sniper_core::decode::anchor::event_discriminator;
use sniper_core::decode::pumpfun::{self, PumpEvent};
use sniper_core::decode::pumpswap::{self, PumpSwapEvent};
use sniper_core::types::Pubkey;
use std::collections::HashMap;

fn idl(name: &str) -> Value {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/idl").join(name);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn type_def<'a>(idl: &'a Value, name: &str) -> &'a Value {
    idl["types"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("type {name}"))
}

/// Deterministic value per field name.
fn num(field: &str, bits: u32) -> u128 {
    let h = Sha256::digest(field.as_bytes());
    let v = u128::from_le_bytes(h[..16].try_into().unwrap());
    if bits >= 128 { v >> 1 } else { v % (1u128 << (bits - 1)) }
}

#[derive(Default)]
struct Enc {
    buf: Vec<u8>,
    vals: HashMap<String, Value>,
}

impl Enc {
    fn field(&mut self, idl: &Value, name: &str, ty: &Value) {
        match ty {
            Value::String(t) => match t.as_str() {
                "pubkey" => {
                    let p = Pubkey::synthetic(name);
                    self.buf.extend(p.0);
                    self.vals.insert(name.into(), Value::String(p.to_string()));
                }
                "bool" => {
                    let b = num(name, 8) % 2 == 1;
                    self.buf.push(b as u8);
                    self.vals.insert(name.into(), Value::Bool(b));
                }
                "u8" => {
                    let v = num(name, 8) as u8;
                    self.buf.push(v);
                    self.vals.insert(name.into(), v.into());
                }
                "u16" => {
                    let v = num(name, 16) as u16;
                    self.buf.extend(v.to_le_bytes());
                    self.vals.insert(name.into(), v.into());
                }
                "u32" => {
                    let v = num(name, 32) as u32;
                    self.buf.extend(v.to_le_bytes());
                    self.vals.insert(name.into(), v.into());
                }
                "u64" => {
                    let v = num(name, 63) as u64;
                    self.buf.extend(v.to_le_bytes());
                    self.vals.insert(name.into(), v.into());
                }
                "i64" => {
                    let v = num(name, 62) as i64;
                    self.buf.extend(v.to_le_bytes());
                    self.vals.insert(name.into(), v.into());
                }
                "i128" => {
                    let v = -(num(name, 90) as i128);
                    self.buf.extend(v.to_le_bytes());
                    self.vals.insert(name.into(), Value::String(v.to_string()));
                }
                "u128" => {
                    let v = num(name, 100);
                    self.buf.extend(v.to_le_bytes());
                    self.vals.insert(name.into(), Value::String(v.to_string()));
                }
                "string" => {
                    let s = format!("ix_{}", num(name, 16));
                    self.buf.extend((s.len() as u32).to_le_bytes());
                    self.buf.extend(s.as_bytes());
                    self.vals.insert(name.into(), Value::String(s));
                }
                other => panic!("unsupported IDL type {other}"),
            },
            Value::Object(o) if o.contains_key("vec") => {
                // two elements
                self.buf.extend(2u32.to_le_bytes());
                for i in 0..2 {
                    self.field(idl, &format!("{name}[{i}]"), &o["vec"]);
                }
            }
            Value::Object(o) if o.contains_key("defined") => {
                let tn = o["defined"]["name"].as_str().unwrap();
                for f in type_def(idl, tn)["type"]["fields"].as_array().unwrap() {
                    self.field(idl, &format!("{name}.{}", f["name"].as_str().unwrap()), &f["type"]);
                }
            }
            other => panic!("unsupported IDL type {other}"),
        }
    }

    fn event(idl: &Value, name: &str) -> (Enc, [u8; 8]) {
        let ev = idl["events"].as_array().unwrap().iter().find(|e| e["name"] == name).unwrap();
        let disc: Vec<u8> = ev["discriminator"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u8).collect();
        let mut e = Enc::default();
        for f in type_def(idl, name)["type"]["fields"].as_array().unwrap() {
            e.field(idl, f["name"].as_str().unwrap(), &f["type"]);
        }
        (e, disc.try_into().unwrap())
    }
    fn u64(&self, k: &str) -> u64 {
        self.vals[k].as_u64().unwrap()
    }
    fn i64(&self, k: &str) -> i64 {
        self.vals[k].as_i64().unwrap()
    }
    fn pk(&self, k: &str) -> Pubkey {
        Pubkey::from_b58(self.vals[k].as_str().unwrap()).unwrap()
    }
    fn b(&self, k: &str) -> bool {
        self.vals[k].as_bool().unwrap()
    }
}

#[test]
fn idl_files_have_provenance() {
    for f in ["pump.idl-subset.json", "pump_amm.idl-subset.json"] {
        let v = idl(f);
        assert!(v["provenance"]["npm_package"].as_str().unwrap().starts_with("@pump-fun/"));
        assert_eq!(v["provenance"]["sha256"].as_str().unwrap().len(), 64);
    }
    assert_eq!(idl("pump.idl-subset.json")["address"], sniper_core::types::programs::PUMP_FUN);
    assert_eq!(idl("pump_amm.idl-subset.json")["address"], sniper_core::types::programs::PUMP_SWAP);
}

#[test]
fn event_and_account_discriminators_match_official_idl() {
    for (file, names) in [
        ("pump.idl-subset.json", vec!["CreateEvent", "TradeEvent", "CompleteEvent", "CompletePumpAmmMigrationEvent"]),
        ("pump_amm.idl-subset.json", vec!["CreatePoolEvent", "BuyEvent", "SellEvent", "DepositEvent", "WithdrawEvent"]),
    ] {
        let v = idl(file);
        for n in names {
            let (_, disc) = Enc::event(&v, n);
            assert_eq!(disc, event_discriminator(n), "{file}:{n}");
        }
    }
    let acct = |file: &str, name: &str| -> [u8; 8] {
        let v = idl(file);
        let a = v["accounts"].as_array().unwrap().iter().find(|a| a["name"] == name).unwrap().clone();
        a["discriminator"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as u8).collect::<Vec<_>>().try_into().unwrap()
    };
    assert_eq!(acct("pump_amm.idl-subset.json", "Pool"), pumpswap::POOL_DISCRIMINATOR);
    assert_eq!(acct("pump.idl-subset.json", "BondingCurve"), pumpfun::BONDING_CURVE_DISCRIMINATOR);
}

#[test]
fn pump_trade_event_full_layout_decodes() {
    let v = idl("pump.idl-subset.json");
    let (e, disc) = Enc::event(&v, "TradeEvent");
    let Some(PumpEvent::Trade(t)) = pumpfun::decode(disc, &e.buf).unwrap() else { panic!() };
    assert_eq!(t.mint, e.pk("mint"));
    assert_eq!(t.sol_amount, e.u64("sol_amount"));
    assert_eq!(t.token_amount, e.u64("token_amount"));
    assert_eq!(t.is_buy, e.b("is_buy"));
    assert_eq!(t.user, e.pk("user"));
    assert_eq!(t.timestamp, e.i64("timestamp"));
    assert_eq!(t.virtual_sol_reserves, e.u64("virtual_sol_reserves"));
    assert_eq!(t.virtual_token_reserves, e.u64("virtual_token_reserves"));
    assert_eq!(t.real_sol_reserves, Some(e.u64("real_sol_reserves")));
    assert_eq!(t.real_token_reserves, Some(e.u64("real_token_reserves")));
    assert_eq!(t.fee_basis_points, Some(e.u64("fee_basis_points")));
    assert_eq!(t.creator, Some(e.pk("creator")));
    assert_eq!(t.creator_fee_basis_points, Some(e.u64("creator_fee_basis_points")));
    let tail = t.tail.as_ref().expect("full tail decoded");
    assert_eq!(tail.ix_name, e.vals["ix_name"].as_str().unwrap());
    assert_eq!(tail.shareholders.len(), 2);
    assert_eq!(tail.shareholders[1].0, e.pk("shareholders[1].address"));
    assert_eq!(tail.quote_mint, e.pk("quote_mint"));
    assert_eq!(tail.quote_amount, e.u64("quote_amount"));
    assert_eq!(tail.real_quote_reserves, e.u64("real_quote_reserves"));
    assert_eq!(tail.creator_fee_unclaimed, e.u64("creator_fee_unclaimed"));
    // a synthetic quote mint is not SOL → never traded as SOL
    assert!(!t.is_sol_quoted());
}

#[test]
fn pump_create_complete_migration_full_layout_decode() {
    let v = idl("pump.idl-subset.json");
    let (e, disc) = Enc::event(&v, "CreateEvent");
    let Some(PumpEvent::Create(c)) = pumpfun::decode(disc, &e.buf).unwrap() else { panic!() };
    assert_eq!(c.name, e.vals["name"].as_str().unwrap());
    assert_eq!(c.mint, e.pk("mint"));
    assert_eq!(c.creator, Some(e.pk("creator")));
    assert_eq!(c.token_total_supply, Some(e.u64("token_total_supply")));
    let tail = c.tail.expect("tail");
    assert_eq!(tail.quote_mint, e.pk("quote_mint"));
    assert_eq!(tail.creator_fee_bps, e.u64("creator_fee_bps"));
    assert_eq!(tail.depth, e.vals["depth"].as_u64().unwrap() as u8);

    let (e, disc) = Enc::event(&v, "CompleteEvent");
    let Some(PumpEvent::Complete(c)) = pumpfun::decode(disc, &e.buf).unwrap() else { panic!() };
    assert_eq!((c.mint, c.timestamp, c.quote_mint), (e.pk("mint"), e.i64("timestamp"), Some(e.pk("quote_mint"))));

    let (e, disc) = Enc::event(&v, "CompletePumpAmmMigrationEvent");
    let Some(PumpEvent::Migration(m)) = pumpfun::decode(disc, &e.buf).unwrap() else { panic!() };
    assert_eq!((m.mint, m.pool, m.mint_amount, m.sol_amount), (e.pk("mint"), e.pk("pool"), e.u64("mint_amount"), e.u64("sol_amount")));
}

#[test]
fn pumpswap_buy_sell_full_layout_decode() {
    let v = idl("pump_amm.idl-subset.json");
    for (name, is_buy) in [("BuyEvent", true), ("SellEvent", false)] {
        let (e, disc) = Enc::event(&v, name);
        let Some(PumpSwapEvent::Swap(s)) = pumpswap::decode(disc, &e.buf).unwrap() else { panic!() };
        assert_eq!(s.is_buy, is_buy);
        let (base_k, user_k, pool_q_k) = if is_buy {
            ("base_amount_out", "user_quote_amount_in", "quote_amount_in_with_lp_fee")
        } else {
            ("base_amount_in", "user_quote_amount_out", "quote_amount_out_without_lp_fee")
        };
        assert_eq!(s.base_amount, e.u64(base_k), "{name}");
        assert_eq!(s.quote_amount_user, e.u64(user_k), "{name}");
        assert_eq!(s.quote_amount_pool, e.u64(pool_q_k), "{name}");
        assert_eq!(s.pool_base_token_reserves, e.u64("pool_base_token_reserves"));
        assert_eq!(s.pool_quote_token_reserves, e.u64("pool_quote_token_reserves"));
        assert_eq!(s.lp_fee_bps, e.u64("lp_fee_basis_points"));
        assert_eq!(s.protocol_fee_bps, e.u64("protocol_fee_basis_points"));
        assert_eq!(s.lp_fee, e.u64("lp_fee"));
        assert_eq!(s.pool, e.pk("pool"));
        assert_eq!(s.user, e.pk("user"));
        assert_eq!(s.coin_creator, Some(e.pk("coin_creator")));
        assert_eq!(s.coin_creator_fee_bps, Some(e.u64("coin_creator_fee_basis_points")));
        let vq: i128 = e.vals["virtual_quote_reserves"].as_str().unwrap().parse().unwrap();
        assert_eq!(s.virtual_quote_reserves, Some(vq), "{name}: signed virtual reserves at the IDL offset");
    }
}

#[test]
fn pumpswap_pool_create_and_liquidity_decode() {
    let v = idl("pump_amm.idl-subset.json");
    let (e, disc) = Enc::event(&v, "CreatePoolEvent");
    let Some(PumpSwapEvent::CreatePool(c)) = pumpswap::decode(disc, &e.buf).unwrap() else { panic!() };
    assert_eq!((c.base_mint, c.quote_mint, c.pool), (e.pk("base_mint"), e.pk("quote_mint"), e.pk("pool")));
    assert_eq!((c.pool_base_amount, c.pool_quote_amount), (e.u64("pool_base_amount"), e.u64("pool_quote_amount")));
    assert_eq!(c.coin_creator, Some(e.pk("coin_creator")));
    assert_eq!(c.creator_fee_bps, Some(e.u64("creator_fee_bps")));
    for (name, dep) in [("DepositEvent", true), ("WithdrawEvent", false)] {
        let (e, disc) = Enc::event(&v, name);
        let Some(PumpSwapEvent::Liquidity(l)) = pumpswap::decode(disc, &e.buf).unwrap() else { panic!() };
        assert_eq!(l.is_deposit, dep);
        let (b, q) = if dep { ("base_amount_in", "quote_amount_in") } else { ("base_amount_out", "quote_amount_out") };
        assert_eq!((l.base_amount, l.quote_amount), (e.u64(b), e.u64(q)), "{name}");
        assert_eq!(l.pool_quote_token_reserves, e.u64("pool_quote_token_reserves"));
        assert_eq!((l.pool, l.user), (e.pk("pool"), e.pk("user")));
    }
}

#[test]
fn pool_and_bonding_curve_accounts_decode_from_idl_layout() {
    let v = idl("pump_amm.idl-subset.json");
    let mut e = Enc::default();
    for f in type_def(&v, "Pool")["type"]["fields"].as_array().unwrap() {
        e.field(&v, f["name"].as_str().unwrap(), &f["type"]);
    }
    let mut data = pumpswap::POOL_DISCRIMINATOR.to_vec();
    data.extend(&e.buf);
    let p = pumpswap::decode_pool_account(&data).unwrap();
    assert_eq!((p.base_mint, p.quote_mint, p.lp_mint), (e.pk("base_mint"), e.pk("quote_mint"), e.pk("lp_mint")));
    assert_eq!(p.coin_creator, Some(e.pk("coin_creator")));
    let vq: i128 = e.vals["virtual_quote_reserves"].as_str().unwrap().parse().unwrap();
    assert_eq!(p.virtual_quote_reserves, Some(vq));

    let v = idl("pump.idl-subset.json");
    let mut e = Enc::default();
    for f in type_def(&v, "BondingCurve")["type"]["fields"].as_array().unwrap() {
        e.field(&v, f["name"].as_str().unwrap(), &f["type"]);
    }
    let mut data = pumpfun::BONDING_CURVE_DISCRIMINATOR.to_vec();
    data.extend(&e.buf);
    let b = pumpfun::decode_bonding_curve_account(&data).unwrap();
    assert_eq!(b.virtual_token_reserves, e.u64("virtual_token_reserves"));
    assert_eq!(b.virtual_quote_reserves, e.u64("virtual_quote_reserves"));
    assert_eq!(b.real_quote_reserves, e.u64("real_quote_reserves"));
    assert_eq!(b.complete, e.b("complete"));
    assert_eq!(b.creator, e.pk("creator"));
    assert_eq!(b.quote_mint, Some(e.pk("quote_mint")));
}

#[test]
fn synthetic_fixture_encoders_emit_the_current_idl_length() {
    // our fixture encoders must produce exactly the IDL layout length for full events
    let v = idl("pump.idl-subset.json");
    let mut c = sniper_core::fixtures::Curve::new("L");
    let ev = c.buy(sniper_core::fixtures::wallet("u"), 1_000_000, 0).unwrap();
    let ours = pumpfun::encode(&ev);
    let (mut e, _) = Enc::event(&v, "TradeEvent");
    // generic encoder writes 2 shareholders and a different ix_name length; normalize
    let PumpEvent::Trade(t) = &ev else { panic!() };
    let tail = t.tail.as_ref().unwrap();
    let generic_len = e.buf.len() - 2 * 34 - e.vals["ix_name"].as_str().unwrap().len() + tail.ix_name.len();
    e.buf.clear();
    assert_eq!(ours.len() - 8, generic_len);
    let v = idl("pump_amm.idl-subset.json");
    let mut p = sniper_core::fixtures::Pool::new("L", Pubkey::synthetic("m"), 1_000_000_000, 1_000_000_000);
    for (name, ev) in [("BuyEvent", p.buy(sniper_core::fixtures::wallet("b"), 1_000_000, 0)), ("SellEvent", p.sell(sniper_core::fixtures::wallet("s"), 1_000, 0))] {
        let ours = pumpswap::encode(&ev);
        let (e, _) = Enc::event(&v, name);
        let ix_adj = if name == "BuyEvent" { e.vals["ix_name"].as_str().unwrap().len() as isize - 3 } else { 0 };
        assert_eq!((ours.len() - 8) as isize, e.buf.len() as isize - ix_adj, "{name}");
    }
}
