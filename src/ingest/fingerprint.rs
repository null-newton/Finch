//! Content fingerprint shared by file imports and the API sync: booking date + amount + normalised description.
use crate::{re, rs, util::*};
use anyhow::Result;
use rusqlite::Connection;

pub type Key = (String, String, String);

pub fn norm_desc(t: Option<&str>) -> String { collapse(&rs!(r"[^A-Z0-9]+").replace_all(&strip_accents(t.unwrap_or("")).to_uppercase(), " ")) }
pub fn content_key(booking_date: &str, amount: f64, desc: Option<&str>) -> Key { (booking_date.into(), amount2(amount), norm_desc(desc)) }
/// Stable tx_key of the n-th (1-based) identical row of an account, independent of file name/order.
pub fn import_tx_key(uid: &str, k: &Key, occ: usize) -> String { format!("{uid}:imp:{}", &sha256_hex([uid, k.0.as_str(), k.1.as_str(), k.2.as_str(), occ.to_string().as_str()].join("|").as_bytes())[..24]) }
/// Fortuneo-style entry_reference that is only "<date>T00:00:00-<position>": not an identity.
pub fn is_position_ref(r: Option<&str>) -> bool { r.is_some_and(|r| is(re!(r"^\d{4}-\d{2}-\d{2}T00:00:00-\d+$"), r)) }
pub fn content_tx_key(uid: &str, cur: Option<&str>, k: &Key, occ: usize) -> String { format!("{uid}:cfp:{}", &sha256_hex([k.0.as_str(), k.1.as_str(), k.2.as_str(), cur.unwrap_or("").to_uppercase().as_str(), occ.to_string().as_str()].join("|").as_bytes())[..24]) }

fn move_gold(con: &Connection, old: &str, new: &str) -> Result<()> {
    if crate::db::has_table(con, "gold_labels")? { con.execute("UPDATE OR IGNORE gold_labels SET tx_key=? WHERE tx_key=?", [new, old])?; con.execute("DELETE FROM gold_labels WHERE tx_key=?", [old])?; }
    Ok(())
}
/// Point every table that refers to a transaction at its new key.
pub fn rekey_tables(con: &Connection, old: &str, new: &str) -> Result<()> {
    for t in ["transactions", "tx_enriched", "tx_overrides", "import_rows", "tx_parse_meta"] { con.execute(&format!("UPDATE {t} SET tx_key=? WHERE tx_key=?"), [new, old])?; }
    move_gold(con, old, new)?;
    con.execute("UPDATE tx_splits SET tx_key=? WHERE tx_key=?", [new, old])?;
    for t in ["transfer_links", "transfer_rejections"] { for c in ["out_tx_key", "in_tx_key"] { con.execute(&format!("UPDATE {t} SET {c}=? WHERE {c}=?"), [new, old])?; } }
    Ok(())
}
/// Row `old` is the same transaction as the kept row `new`: carry the user's data over (the kept row wins) and drop `old`.
pub fn replace_tx_key(con: &Connection, old: &str, new: &str, reason: &str) -> Result<()> {
    con.execute("INSERT OR REPLACE INTO tx_key_remap VALUES (?,?,?,datetime('now'))", [old, new, reason])?;
    con.execute("UPDATE tx_splits SET tx_key=? WHERE tx_key=?", [new, old])?;
    con.execute("UPDATE OR IGNORE tx_overrides SET tx_key=? WHERE tx_key=?", [new, old])?;
    if crate::db::has_table(con, "tx_person")? { con.execute("UPDATE OR IGNORE tx_person SET tx_key=? WHERE tx_key=?", [new, old])?; con.execute("DELETE FROM tx_person WHERE tx_key=?", [old])?; }
    move_gold(con, old, new)?;
    for c in ["out_tx_key", "in_tx_key"] { con.execute(&format!("UPDATE transfer_links SET {c}=? WHERE {c}=?"), [new, old])?; con.execute(&format!("UPDATE OR IGNORE transfer_rejections SET {c}=? WHERE {c}=?"), [new, old])?; }
    con.execute("DELETE FROM transfer_rejections WHERE out_tx_key=? OR in_tx_key=?", [old, old])?;
    for t in ["tx_overrides", "tx_enriched", "tx_parse_meta", "import_rows", "transactions"] { con.execute(&format!("DELETE FROM {t} WHERE tx_key=?"), [old])?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn keys() {
        assert_eq!(norm_desc(Some("Café  du-Coin!")), "CAFE DU COIN");
        assert_eq!(content_key("2025-01-02", -3.5, Some("x")).1, "-3.50");
        assert!(is_position_ref(Some("2025-12-11T00:00:00-3"))); assert!(!is_position_ref(Some("ABC")));
        let k = content_key("2025-01-02", -3.5, Some("x")); assert_eq!(import_tx_key("a", &k, 1).len(), "a:imp:".len() + 24);
    }
}
