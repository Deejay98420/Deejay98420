//! Static proof that live execution is absent: no signing/broadcasting code, no key material,
//! no Solana SDK / signing crates, no transaction-building endpoints, GET-only API.

use std::path::{Path, PathBuf};

fn files(root: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if p.is_dir() {
            if ["target", "node_modules", ".next", ".git"].contains(&name.as_str()) {
                continue;
            }
            files(&p, exts, out);
        } else if exts.iter().any(|x| name.ends_with(x)) {
            out.push(p);
        }
    }
}

/// Forbidden capabilities (case-insensitive substrings). The patterns are assembled at runtime so
/// this file itself does not contain them.
fn forbidden() -> Vec<String> {
    [
        ["send", "Transaction"].concat(),
        ["send", "RawTransaction"].concat(),
        ["send", "Bundle"].concat(),
        ["sign", "Transaction"].concat(),
        ["sign", "_message"].concat(),
        ["Key", "pair"].concat(),
        ["secret", "_key"].concat(),
        ["private", "_key"].concat(),
        ["PRIVATE", "_KEY"].concat(),
        ["swap-", "instructions"].concat(),
        ["/swap", "?"].concat(),
        ["\"/swap", "\""].concat(),
        ["/ultra/v1/", "execute"].concat(),
        ["/ultra/v1/", "order"].concat(),
        ["ed25519", "_dalek"].concat(),
        ["solana", "_sdk"].concat(),
        ["solana-", "sdk"].concat(),
        ["solana-", "client"].concat(),
        ["@solana/", "web3"].concat(),
        ["mnemonic"].concat(),
        ["seed_", "phrase"].concat(),
    ]
    .into_iter()
    .collect()
}

#[test]
fn no_live_execution_capability_in_source() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let mut fs = Vec::new();
    files(&root.join("core/src"), &[".rs"], &mut fs);
    files(&root.join("core/Cargo.toml").parent().unwrap().join("Cargo.toml"), &[".toml"], &mut fs);
    fs.push(root.join("core/Cargo.toml"));
    files(&root.join("dashboard"), &[".ts", ".tsx", ".js", ".mjs", ".json"], &mut fs);
    files(&root.join("scripts"), &[".sh", ""], &mut fs);
    fs.push(root.join(".env.example"));
    fs.push(root.join("docker-compose.yml"));
    let pats = forbidden();
    let mut hits = Vec::new();
    for f in fs.iter().filter(|f| f.is_file()) {
        let name = f.to_string_lossy();
        if name.ends_with("package-lock.json") {
            continue;
        }
        let text = std::fs::read_to_string(f).unwrap_or_default().to_lowercase();
        for p in &pats {
            if text.contains(&p.to_lowercase()) {
                hits.push(format!("{} contains {:?}", f.display(), p));
            }
        }
    }
    assert!(hits.is_empty(), "live-execution capability found:\n{}", hits.join("\n"));
    assert!(fs.len() > 20, "scanner found too few files ({}), check paths", fs.len());
}

#[test]
fn no_signing_or_solana_sdk_crates_in_dependency_graph() {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock");
    let text = std::fs::read_to_string(lock).expect("Cargo.lock");
    for bad in ["name = \"solana-sdk\"", "name = \"solana-client\"", "name = \"solana-program\"", "name = \"ed25519-dalek\"", "name = \"solana-keypair\"", "name = \"solana-signer\""] {
        assert!(!text.contains(bad), "forbidden crate in Cargo.lock: {bad}");
    }
}

#[test]
fn env_example_has_placeholders_only() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let text = std::fs::read_to_string(root.join(".env.example")).unwrap();
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#') && l.contains('=')) {
        let (k, v) = line.split_once('=').unwrap();
        let v = v.trim();
        if k.contains("KEY") || k.contains("TOKEN") || k.contains("SECRET") {
            assert!(v.is_empty() || v.starts_with('<'), "{k} must be a placeholder");
        }
        assert!(!v.contains("api-key=") || v.contains("<"), "{k} embeds a real-looking key");
    }
}

#[test]
fn shipped_strategy_config_parses_and_every_key_takes_effect() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let text = std::fs::read_to_string(root.join("config/strategy.toml")).unwrap();
    let cfg: sniper_core::config::EngineConfig = toml::from_str(&text).unwrap();
    // round-trip: every key in the file must exist in the effective config (no silently ignored keys)
    let file: toml::Value = toml::from_str(&text).unwrap();
    let eff = toml::Value::try_from(&cfg).unwrap();
    fn walk(path: &str, f: &toml::Value, e: &toml::Value) {
        if let (toml::Value::Table(ft), toml::Value::Table(et)) = (f, e) {
            for (k, v) in ft {
                let p = format!("{path}.{k}");
                let ev = et.get(k).unwrap_or_else(|| panic!("config key {p} is not used by the engine"));
                walk(&p, v, ev);
            }
        }
    }
    walk("", &file, &eff);
    assert_eq!(cfg.fill_basis, "conservative");
    assert!(cfg.strategy.entry.require_mint_check);
}
