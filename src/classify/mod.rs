//! Classification: descriptor parsing (`normalize`) into tx_enriched / tx_parse_meta, and the category pipeline (`rules`).
//! LLM / kNN merchant labels are read from the `merchants` table when present; writing them is a later phase.
pub mod parsers;
pub mod rules;

use anyhow::Result;
use parsers::{common::household, parse_tx, RawTx};
use rusqlite::{params, Connection};
use std::{collections::HashMap, path::Path};

/// Parse every stored descriptor with its bank's parser into tx_enriched (+ tx_parse_meta). Returns the number of rows.
pub fn normalize_all(con: &Connection, memory_dir: Option<&Path>) -> Result<usize> {
    let h = household(con, memory_dir);
    let accounts: HashMap<String, (Option<String>, Option<String>, String, Option<String>)> = con.prepare("SELECT a.uid, COALESCE(a.bank, s.aspsp_name), a.iban, a.cash_account_type, s.aspsp_country FROM accounts a LEFT JOIN sessions s ON s.session_id=a.session_id")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get::<_, Option<String>>(3)?.unwrap_or_default(), r.get(4)?))))?.collect::<rusqlite::Result<_>>()?;
    type R = (String, Option<String>, Option<f64>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let rows: Vec<R> = con.prepare("SELECT tx_key, description, amount, booking_date, account_uid, currency, counterparty, bank_tx_code, raw FROM transactions")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))?.collect::<rusqlite::Result<_>>()?;
    con.execute_batch("BEGIN")?;
    let res = (|| -> Result<()> {
        let mut e = con.prepare("INSERT OR REPLACE INTO tx_enriched VALUES (?,?,?,?,?,?,?)")?; let mut m = con.prepare("INSERT OR REPLACE INTO tx_parse_meta VALUES (?,?,?,?,?,?)")?;
        for (key, desc, amount, bdate, uid, cur, cp, code, raw) in &rows {
            let (bank, iban, ctype, country) = uid.as_ref().and_then(|u| accounts.get(u)).cloned().unwrap_or_default();
            let raw: Option<serde_json::Value> = raw.as_deref().and_then(|r| serde_json::from_str(r).ok());
            let code = code.clone().or_else(|| raw.as_ref().and_then(|r| r.get("coda_code")).and_then(|c| c.as_str()).map(String::from)).unwrap_or_default();
            let tx = RawTx { description: desc.clone().unwrap_or_default(), amount: amount.unwrap_or(0.0), booking_date: bdate.clone().unwrap_or_default(), currency: cur.clone().unwrap_or_else(|| "EUR".into()), counterparty: cp.clone().unwrap_or_default(), bank_tx_code: code, account_type: ctype, bank: bank.clone().unwrap_or_default(), raw };
            let p = parse_tx(&tx, &h, bank.as_deref(), iban.as_deref(), country.as_deref());
            e.execute(params![key, p.tx_type, p.op_date, p.merchant_raw, p.merchant_key, p.fx_amount, p.fx_currency])?;
            m.execute(params![key, p.parser, p.counterparty, p.mandate_ref, p.creditor_id, p.reference])?;
        }
        Ok(())
    })();
    match res { Ok(()) => { con.execute_batch("COMMIT")?; Ok(rows.len()) } Err(e) => { let _ = con.execute_batch("ROLLBACK"); Err(e) } }
}
