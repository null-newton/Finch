//! Revolut (via Enable Banking): the bank's transaction code (CARD_PAYMENT, TRANSFER, TOPUP, EXCHANGE, FEE, ATM, REFUND...) and the
//! counterparty name are more reliable than the free-text description. Person-to-person lines are never sent to an LLM.
use super::common::*;
use crate::{re, util::*};

const CURRENCIES: &str = "EUR|USD|GBP|CHF|PLN|SEK|NOK|DKK|CZK|HUF|RON|BGN|JPY|CAD|AUD|NZD|TRY|AED|ISK";
const DIRECTION: &str = r"(?i)^(To|From)\s+(.+)$";
const WITHDRAWAL: &str = r"(?i)^(?:Junior account withdrawal|Withdrawal by)\b\s*\(?([^)]*)\)?";
const PERSON_LINE: &str = r"(?i)^(?:Payment from|Sent from(?: Revolut)?|Received from|Transfer (?:to|from)|Money (?:from|to)|Request from)\b\s*(.*)$";

fn pocket(s: &str) -> bool { fancy_regex::Regex::new(&format!(r"^(?:To|From)\s+(?:{CURRENCIES})\b")).map(|r| is(&r, s)).unwrap_or(false) }
fn fx(tx: &RawTx) -> Option<(f64, String)> {
    let inst = tx.raw.as_ref()?.get("exchange_rate")?.get("instructed_amount")?;
    let cur = inst.get("currency")?.as_str()?; if cur == tx.currency { return None; }
    let a = inst.get("amount")?; Some((a.as_f64().or_else(|| a.as_str()?.parse().ok())?, cur.into()))
}

pub fn parse(tx: &RawTx, h: &Household) -> Parsed {
    let code = tx.bank_tx_code.to_uppercase(); let segs = segments(&tx.description, false); let last = segs.last().unwrap().clone(); let amount = tx.amount;
    let all = collapse(&segs.join(" "));
    let pm = segs.iter().find_map(|s| cap(re!(PERSON_LINE), s).map(|c| g(&c, 1).to_string()));
    if let Some(pm) = pm.filter(|_| !["CARD_PAYMENT", "TOPUP", "ATM", "FEE", "CHARGE", "EXCHANGE"].contains(&code.as_str())) {
        let mut who = collapse(if tx.counterparty.is_empty() { &pm } else { &tx.counterparty });
        if who.is_empty() { who = collapse(segs.iter().find(|s| !is(re!(PERSON_LINE), s)).map_or("", |s| s.as_str())); }
        let kind = if !who.is_empty() && is_own_name(&who, &h.holders) { "internal_transfer" } else if amount > 0.0 { "person_transfer_in" } else { "person_transfer_out" };
        return with_cp(result(kind, if who.is_empty() { &last } else { &who }, None, None), (!who.is_empty()).then(|| who.clone()));
    }
    if code == "CARD_PAYMENT" || (code.is_empty() && !is(re!(DIRECTION), &last) && !pocket(&last)) { return result(if amount <= 0.0 { "card" } else { "card_refund" }, &all, None, fx(tx)); }
    match code.as_str() {
        "REFUND" | "CARD_REFUND" => return result("card_refund", &all, None, fx(tx)),
        "ATM" => return result("atm", &all, None, None),
        "FEE" | "CHARGE" => return result("bank_fee", &all, None, None),
        _ => {}
    }
    if code == "EXCHANGE" || is(re!(r"(?i)^Exchanged to\b"), &last) { return result("fx_exchange", &all, None, None); }
    if code == "TOPUP" || is(re!(r"(?i)\bTop-Up\b"), &last) { return with_cp(result("topup", &collapse(&segs[0]), None, None), (!tx.counterparty.is_empty()).then(|| tx.counterparty.clone())); }
    if code == "TRANSFER" || is(re!(DIRECTION), &last) || is(re!(WITHDRAWAL), &last) {
        if tx.account_type.eq_ignore_ascii_case("SVGS") || pocket(&last) { return result("savings_internal", &collapse(&sub(re!(r"(?i)\b[0-9a-f]{8}-[0-9a-f-]{27}\b"), &last, "")), None, None); }
        let mut who = tx.counterparty.clone();
        if who.is_empty() { if let Some(c) = cap(re!(DIRECTION), &last) { who = g(&c, 2).into(); } }
        if who.is_empty() { if let Some(c) = cap(re!(WITHDRAWAL), &last) { who = g(&c, 1).into(); } }
        let who = collapse(&who);
        let ty = if !who.is_empty() && is_own_name(&who, &h.holders) { "internal_transfer" } else if amount > 0.0 { "person_transfer_in" } else { "person_transfer_out" };
        return with_cp(result(ty, if who.is_empty() { &last } else { &who }, None, None), (!who.is_empty()).then(|| who.clone()));
    }
    super::belgian::parse(tx, h)
}
