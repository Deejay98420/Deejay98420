//! Instruction account layouts from the official IDLs (`fixtures/idl/*.ix-accounts.json`, with
//! provenance and sha256). Used READ-ONLY to find which accounts an observed instruction touched
//! (bonding-curve vault, user token account, fee recipients, ...). Nothing here builds instructions.

use crate::types::programs;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct File {
    address: String,
    instructions: Vec<IxDef>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct IxDef {
    pub name: String,
    pub discriminator_hex: String,
    pub accounts: Vec<AccountDef>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct AccountDef {
    pub name: String,
}

pub const PUMP_LAYOUTS: &str = include_str!("../../fixtures/idl/pump.ix-accounts.json");
pub const PUMP_AMM_LAYOUTS: &str = include_str!("../../fixtures/idl/pump_amm.ix-accounts.json");

type Table = HashMap<(String, [u8; 8]), IxDef>;

fn table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = HashMap::new();
        for src in [PUMP_LAYOUTS, PUMP_AMM_LAYOUTS] {
            let f: File = serde_json::from_str(src).expect("bundled IDL layout JSON");
            for ix in f.instructions {
                let h = ix.discriminator_hex.as_bytes();
                let d: Vec<u8> = h.chunks(2).map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).expect("hex discriminator")).collect();
                t.insert((f.address.clone(), d[..8].try_into().unwrap()), ix);
            }
        }
        t
    })
}

/// Layout of an instruction of `program` with discriminator `disc`, if it is one we track.
pub fn lookup(program: &str, disc: [u8; 8]) -> Option<&'static IxDef> {
    if program != programs::PUMP_FUN && program != programs::PUMP_SWAP {
        return None;
    }
    table().get(&(program.to_string(), disc))
}

impl IxDef {
    pub fn position(&self, account: &str) -> Option<usize> {
        self.accounts.iter().position(|a| a.name == account)
    }
    /// Account address bound to `name` in an observed instruction (None when the instruction carries
    /// fewer accounts, e.g. an older program version).
    pub fn get<'a>(&self, accounts: &'a [String], name: &str) -> Option<&'a str> {
        accounts.get(self.position(name)?).map(|s| s.as_str())
    }
}
