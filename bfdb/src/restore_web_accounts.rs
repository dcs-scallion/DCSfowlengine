//! One-shot: copy web account trees from a sled backup into a live DB.
//! Usage: restore_web_accounts.exe <from_db_dir> <to_db_dir>

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let from = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: restore_web_accounts <from_db_dir> <to_db_dir>"))?;
    let to = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: restore_web_accounts <from_db_dir> <to_db_dir>"))?;

    println!("from: {from}");
    println!("to:   {to}");

    let old = sled::open(&from)?;
    let new = sled::open(&to)?;

    for name in [
        "web_accounts",
        "web_accounts_by_ucid",
        "web_claim_codes",
        "web_account_preferences",
    ] {
        let src = old.open_tree(name)?;
        let dst = new.open_tree(name)?;
        dst.clear()?;
        let mut n = 0u64;
        for r in src.iter() {
            let (k, v) = r?;
            dst.insert(k, v)?;
            n += 1;
        }
        println!("{name}: copied {n} entries");
    }

    new.flush()?;
    println!("flush ok");
    Ok(())
}
