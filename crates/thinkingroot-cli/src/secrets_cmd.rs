//! `root secrets {set,list,unset}` — manage the local
//! `~/.config/thinkingroot/secrets.toml` store (mode 0600) that Root
//! Functions read via `ctx.env`. Pure local file ops; no engine or
//! cloud round-trip.

use std::io::Read;

use anyhow::{Context, Result};
use thinkingroot_cloud_auth::secrets;

pub fn set(name: &str, value: Option<&str>) -> Result<()> {
    let val = match value {
        Some(v) => v.to_string(),
        None => {
            // Read from stdin so secrets can be piped without landing in
            // shell history.
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("read secret value from stdin")?;
            s.trim_end_matches(['\n', '\r']).to_string()
        }
    };
    secrets::set(name, &val).map_err(|e| anyhow::anyhow!("set secret: {e}"))?;
    println!("✓ secret '{name}' set");
    Ok(())
}

pub fn list() -> Result<()> {
    let names = secrets::list_names().map_err(|e| anyhow::anyhow!("list secrets: {e}"))?;
    if names.is_empty() {
        println!("No secrets set (~/.config/thinkingroot/secrets.toml is empty or absent).");
    } else {
        for n in names {
            println!("{n}");
        }
    }
    Ok(())
}

pub fn unset(name: &str) -> Result<()> {
    let existed = secrets::unset(name).map_err(|e| anyhow::anyhow!("unset secret: {e}"))?;
    if existed {
        println!("✓ secret '{name}' removed");
    } else {
        println!("(no secret named '{name}')");
    }
    Ok(())
}
