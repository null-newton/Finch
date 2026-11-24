//! A home that sets itself up (the desktop app's first start): the folders, a `config.toml`, a random database key kept in the OS
//! keychain, and the database with every migration. Existing files are never changed; a second call only opens what is there.
use crate::{config::{self, Config}, db, secrets};
use anyhow::{Context, Result};
use std::path::Path;

/// Creates what is missing under `root` and returns its configuration (database created and migrated).
pub fn ensure(root: &Path) -> Result<Config> {
    let cfg_path = root.join(config::CONFIG_NAME);
    if !cfg_path.exists() {
        for d in ["data", "memory"] { std::fs::create_dir_all(root.join(d)).with_context(|| format!("cannot create {}", root.join(d).display()))?; }
        let plain = if cfg!(feature = "sqlcipher") { "" } else { "# this build has no SQLCipher: the database is NOT encrypted (a release build encrypts it)\ninsecure_plaintext_db = true\n\n" };
        crate::memstore::atomic_write(&cfg_path, &format!("# Finch home, created on first start. Settings: see the README.\n{plain}[ui]\nopen_browser = false\n\n[update]\n# look for signed updates at start and every 6 hours (off: only the menu's \"Check for updates...\")\ncheck = false\n"))?;
    }
    let cfg = config::load_config(Some(&cfg_path))?;
    if !cfg.db_path.exists() {
        if cfg!(feature = "sqlcipher") && !cfg.insecure_plaintext_db && secrets::get_secret("db_key", false)?.is_none() {
            let key: String = (0..32).map(|_| format!("{:02x}", rand::random::<u8>())).collect(); secrets::store("db_key", &key).context("the database key could not be kept in the OS keychain")?;
        }
        db::connect(&cfg, db::Opts { create: true, ..Default::default() })?;
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    #[test] fn first_start_creates_everything_once() {
        let t = tempfile::tempdir().unwrap(); let root = t.path().join("Finch");
        let c = super::ensure(&root).unwrap(); assert!(c.db_path.is_file() && root.join("memory").is_dir() && !c.update_check);
        let n: i64 = crate::db::connect(&c, crate::db::Opts::default()).unwrap().query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0)).unwrap(); assert!(n > 10);
        std::fs::write(root.join("config.toml"), format!("{}\n# mine\n", std::fs::read_to_string(root.join("config.toml")).unwrap())).unwrap();
        super::ensure(&root).unwrap(); assert!(std::fs::read_to_string(root.join("config.toml")).unwrap().contains("# mine"), "an existing config is never rewritten");
    }
}
