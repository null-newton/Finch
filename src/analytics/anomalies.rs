//! Unusual spending: deterministic robust statistics, every finding with its evidence and a stable `anm_` id.
//! * category_spike: a closed month far above the category's own history (>= `anomaly_min_history_months` earlier covered months):
//!   z = (x - median) / max(1.4826 MAD, 10 % of the median, 10 EUR) >= `anomaly_z`, excess >= `anomaly_min_excess`, x >= ratio x median.
//! * duplicate_charge: same merchant, same amount, same account within `anomaly_duplicate_days` (not inside one recurring series).
//! * new_merchant: never seen before, first booking within `anomaly_new_merchant_days`, a payment >= `anomaly_new_merchant_min`.
//! * large_transaction: >= max(Q3 + k IQR, 3 x median, `anomaly_large_tx_min`) for its category (>= 8 payments of history).
use super::{recurring::Recurring, *};
use std::collections::HashSet;

const SEVERITIES: [&str; 3] = ["high", "medium", "low"];
fn id(parts: &[&str]) -> String { format!("anm_{}", &sha1_hex(&parts.join("|"))[..10]) }
fn eur(c: i64) -> String { format!("{} EUR", money_str(c)) }
fn median_fl(v: &[i64]) -> f64 { let mut v = v.to_vec(); v.sort(); let n = v.len(); if n % 2 == 1 { v[n / 2] as f64 } else { (v[n / 2 - 1] + v[n / 2]) as f64 / 2.0 } }
/// Python `statistics.quantiles(v, n=4, method="inclusive")`.
fn quartiles(v: &[i64]) -> (f64, f64) { let mut s = v.to_vec(); s.sort(); let m = s.len() as i64 - 1; let q = |i: i64| { let (j, dl) = ((i * m) / 4, (i * m) % 4); (s[j as usize] as f64 * (4 - dl) as f64 + s[(j as usize + 1).min(s.len() - 1)] as f64 * dl as f64) / 4.0 }; (q(1), q(3)) }

#[allow(clippy::too_many_arguments)]
fn anomaly(id: String, ty: &str, sev: &str, subject: &str, period: &str, amount: i64, baseline: Option<i64>, score: Option<f64>, message: String, evidence: Vec<String>, accounts: Vec<String>) -> Value {
    json!({"id": id, "type": ty, "severity": sev, "subject": subject, "period": period, "amount": money_str(amount), "baseline": money_opt(baseline), "score": score, "message": message, "evidence": evidence, "accounts": accounts, "dismissed": false})
}

fn spikes(ds: &Dataset, scope: &Scope) -> Vec<Value> {
    let s = &ds.settings; let mut out = vec![];
    let mut by: BTreeMap<String, Vec<&Tx>> = BTreeMap::new();
    for t in ds.txs_in(scope).into_iter().filter(|t| is_spending(&t.category) && !t.is_saved() && !t.is_one_off()) { by.entry(t.category.clone()).or_default().push(t); }
    for (cat, txs) in by {
        let carriers = ds.carriers_split(&cat, scope).0; let months = ds.coverage.common_months(&carriers);
        if months.len() < s.anomaly_min_history_months + 1 { continue; }
        let mut per: BTreeMap<&String, i64> = months.iter().map(|m| (m, 0)).collect();
        for t in &txs { if let Some(v) = per.get_mut(&t.month()) { *v -= t.amount_c; } }
        for m in last_n(&months, s.anomaly_months) {
            let h: Vec<i64> = months.iter().filter(|x| **x < m).map(|x| per[x]).collect(); let hist = &h[h.len().saturating_sub(24)..];
            if hist.len() < s.anomaly_min_history_months { continue; }
            let med = median_c(hist); let mad = median_fl(&hist.iter().map(|v| (v - med).abs()).collect::<Vec<_>>());
            let scale = (1.4826 * mad).max(0.10 * med.abs() as f64).max(1000.0); let x = per[&m]; let z = (x - med) as f64 / scale; let excess = x - med;
            if z < s.anomaly_z || (excess as f64) < s.anomaly_min_excess * 100.0 || (x as f64) < s.anomaly_min_ratio * med as f64 { continue; }
            let (sev, zr) = if med == 0 { if (excess as f64) < 4.0 * s.anomaly_min_excess * 100.0 { continue; } (if excess >= 50000 { "medium" } else { "low" }, None) }
                else { (if z >= 8.0 || excess >= 50000 { "high" } else if z >= 5.0 || excess >= 20000 { "medium" } else { "low" }, Some((z * 100.0).round() / 100.0)) };
            let mut big: Vec<&&Tx> = txs.iter().filter(|t| t.month() == m).collect(); big.sort_by(|a, b| (a.amount_c, &a.key).cmp(&(b.amount_c, &b.key)));
            let text = format!("Spending in {cat} in {m} is {}, versus a typical {} per month {}", eur(x), eur(med), if zr.is_some() { format!("(robust z-score {z:.1}, {} earlier months).", hist.len()) } else { format!("(usually nothing in this category; {} earlier months).", hist.len()) });
            let mut acc: Vec<String> = carriers.iter().map(|a| ds.label(a)).collect(); acc.sort(); acc.dedup();
            out.push(anomaly(id(&["category_spike", &cat, &m]), "category_spike", sev, &cat, &m, x, Some(med), zr, text, big.iter().take(5).map(|t| t.key.clone()).collect(), acc));
        }
    }
    out
}
fn duplicates(ds: &Dataset, scope: &Scope, whole: &[Tx], rec: &Recurring) -> Vec<Value> {
    let s = &ds.settings; let since = ds.today - chrono::Duration::days(s.anomaly_lookback_days as i64);
    let in_series: HashMap<&str, &str> = rec.series.iter().flat_map(|x| x.occ.iter().map(move |o| (o.0.as_str(), x.id.as_str()))).collect();
    let allowed: HashSet<String> = ds.uids_in(scope).into_iter().collect();
    let mut groups: BTreeMap<(String, String, i64), Vec<&Tx>> = BTreeMap::new();
    for t in whole.iter().filter(|t| t.amount_c < 0 && allowed.contains(&t.account) && t.source != "split" && is_spending(&t.category) && (-t.amount_c) as f64 >= s.anomaly_duplicate_min_amount * 100.0) {
        groups.entry((t.account.clone(), if t.entity.is_empty() { t.mkey.clone() } else { t.entity.clone() }, t.amount_c)).or_default().push(t);
    }
    let mut out = vec![];
    for ((acc, ent, amt), mut txs) in groups {
        txs.sort_by(|a, b| (a.date, &a.key).cmp(&(b.date, &b.key)));
        let mut i = 0;
        while i < txs.len() {
            let mut cl = vec![txs[i]]; let mut j = i + 1;
            while j < txs.len() && (txs[j].date - cl.last().unwrap().date).num_days() <= s.anomaly_duplicate_days as i64 { cl.push(txs[j]); j += 1; }
            i = if cl.len() > 1 { j } else { i + 1 };
            if cl.len() < 2 || cl.last().unwrap().date < since { continue; }
            let ids: HashSet<Option<&&str>> = cl.iter().map(|t| in_series.get(t.key.as_str())).collect();
            if !ids.contains(&None) && ids.len() == 1 { continue; }
            let extra = (cl.len() as i64 - 1) * -amt; let sev = if extra >= 10000 { "high" } else if extra < 2000 { "low" } else { "medium" };
            let (f, l) = (cl[0].date, cl.last().unwrap().date);
            out.push(anomaly(id(&["duplicate_charge", &acc, &cl[0].key]), "duplicate_charge", sev, &ent, &l.to_string(), extra, Some(-amt), None, format!("{} payments of {} to {ent} within {} day(s) ({f} to {l}).", cl.len(), eur(-amt), (l - f).num_days()), cl.iter().map(|t| t.key.clone()).collect(), vec![ds.label(&acc)]));
        }
    }
    out
}
fn new_merchants(ds: &Dataset, scope: &Scope, whole: &[Tx]) -> Vec<Value> {
    let s = &ds.settings; let since = ds.today - chrono::Duration::days(s.anomaly_new_merchant_days as i64);
    let allowed: HashSet<String> = ds.uids_in(scope).into_iter().collect(); let key = |t: &Tx| if t.entity.is_empty() { t.mkey.clone() } else { t.entity.clone() };
    let mut first: HashMap<String, (NaiveDate, String)> = HashMap::new();
    for t in whole { let k = key(t); if first.get(&k).is_none_or(|f| t.date < f.0) { first.insert(k, (t.date, t.key.clone())); } }
    let mut big: BTreeMap<String, Vec<&Tx>> = BTreeMap::new();
    for t in whole { let k = key(t); if first[&k].0 >= since && t.date >= since && t.amount_c < 0 && allowed.contains(&t.account) && is_spending(&t.category) && (-t.amount_c) as f64 >= s.anomaly_new_merchant_min * 100.0 && !t.is_saved() && !t.is_one_off() { big.entry(k).or_default().push(t); } }
    let min = (s.anomaly_new_merchant_min * 100.0) as i64;
    big.into_iter().map(|(k, mut txs)| {
        let top = txs.iter().map(|t| -t.amount_c).max().unwrap(); txs.sort_by(|a, b| (a.amount_c, &a.key).cmp(&(b.amount_c, &b.key)));
        let mut acc: Vec<String> = txs.iter().map(|t| ds.label(&t.account)).collect(); acc.sort(); acc.dedup(); let (fd, fk) = &first[&k];
        anomaly(id(&["new_merchant", &k, fk]), "new_merchant", if top >= 3 * min { "high" } else { "medium" }, &k, &fd.to_string(), top, Some(min), None, format!("New merchant {k} (first seen {fd}) with a payment of {}.", eur(top)), txs.iter().map(|t| t.key.clone()).collect(), acc)
    }).collect()
}
fn large(ds: &Dataset, scope: &Scope, whole: &[Tx], rec: &Recurring) -> Vec<Value> {
    let s = &ds.settings; let since = ds.today - chrono::Duration::days(s.anomaly_lookback_days as i64);
    let in_series: HashSet<&str> = rec.series.iter().flat_map(|x| x.occ.iter().map(|o| o.0.as_str())).collect();
    let allowed: HashSet<String> = ds.uids_in(scope).into_iter().collect();
    let mut by: BTreeMap<&str, Vec<&Tx>> = BTreeMap::new();
    for t in whole.iter().filter(|t| t.amount_c < 0 && allowed.contains(&t.account) && is_spending(&t.category) && !t.is_saved() && !t.is_one_off()) { by.entry(&t.category).or_default().push(t); }
    let mut out = vec![];
    for (cat, txs) in by.into_iter().filter(|(_, v)| v.len() >= 8) {
        let amts: Vec<i64> = txs.iter().map(|t| -t.amount_c).collect(); let (q1, q3) = quartiles(&amts); let med = median_c(&amts);
        let thr = (q3 + s.anomaly_large_tx_iqr_factor * (q3 - q1)).max(3.0 * med as f64).max(s.anomaly_large_tx_min * 100.0);
        for t in txs.iter().filter(|t| t.date >= since && (-t.amount_c) as f64 >= thr && !in_series.contains(t.key.as_str())) {
            let ratio = (-t.amount_c) as f64 / thr; let sev = if ratio >= 3.0 { "high" } else if ratio >= 1.5 { "medium" } else { "low" };
            out.push(anomaly(id(&["large_transaction", &t.key]), "large_transaction", sev, cat, &t.date.to_string(), -t.amount_c, Some(med), Some((ratio * 100.0).round() / 100.0),
                format!("A payment of {} to {} on {} is far above the usual {cat} payments (median {}, threshold {}).", eur(-t.amount_c), t.entity, t.date, eur(med), eur(thr as i64)), vec![t.key.clone()], vec![ds.label(&t.account)]));
        }
    }
    out
}

pub fn detect(ds: &Dataset, scope: &Scope, rec: &Recurring, dismissed: &HashSet<String>, include_dismissed: bool) -> Value {
    let whole = ds.whole();
    let new = new_merchants(ds, scope, &whole);
    let seen: HashSet<String> = new.iter().flat_map(|a| a["evidence"].as_array().unwrap().iter().filter_map(|x| x.as_str().map(String::from))).collect();
    let lg: Vec<Value> = large(ds, scope, &whole, rec).into_iter().filter(|a| !seen.contains(a["evidence"][0].as_str().unwrap_or(""))).collect();
    let mut found: Vec<Value> = [spikes(ds, scope), duplicates(ds, scope, &whole, rec), new, lg].concat();
    for a in &mut found { let d = dismissed.contains(a["id"].as_str().unwrap()); a["dismissed"] = d.into(); }
    let n_dis = found.iter().filter(|a| a["dismissed"] == true).count();
    if !include_dismissed { found.retain(|a| a["dismissed"] == false); }
    let sv = |a: &Value| SEVERITIES.iter().position(|s| a["severity"] == *s).unwrap_or(3);
    found.sort_by(|a, b| sv(a).cmp(&sv(b)).then(b["period"].as_str().cmp(&a["period"].as_str())).then(a["id"].as_str().cmp(&b["id"].as_str())));
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for a in &found { *counts.entry(a["type"].as_str().unwrap().into()).or_default() += 1; *counts.entry(format!("severity_{}", a["severity"].as_str().unwrap())).or_default() += 1; }
    let mut notes = vec![format!("category spikes: months covered by every account carrying the category, >= {} earlier months required", ds.settings.anomaly_min_history_months)]; notes.extend(ds.base_notes());
    let ev: BTreeSet<String> = found.iter().flat_map(|a| a["evidence"].as_array().unwrap().iter().filter_map(|x| x.as_str().map(String::from))).collect();
    json!({"as_of": ds.today.to_string(), "anomalies": found, "counts": counts, "dismissed": n_dis, "scope": scope.describe(), "coverage": ds.coverage.info(&ds.uids_in(scope), &[], "per-detector windows and minimum history", notes), "evidence": ev})
}
pub fn dismissed_ids(con: &rusqlite::Connection) -> Result<HashSet<String>> { Ok(con.prepare("SELECT id FROM anomalies WHERE dismissed_at IS NOT NULL")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?) }
/// Store the current anomalies; rows keep `first_seen` and any dismissal; undismissed ones that are gone are deleted.
pub fn refresh(con: &rusqlite::Connection, ds: &Dataset, rec: &Recurring) -> Result<Value> {
    let stamp = now_iso();
    let old: HashMap<String, (String, Option<String>)> = con.prepare("SELECT id, first_seen, dismissed_at FROM anomalies")?.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?;
    let dis: HashSet<String> = old.iter().filter(|o| o.1 .1.is_some()).map(|o| o.0.clone()).collect();
    let res = detect(ds, &Scope::default(), rec, &dis, true); let list = res["anomalies"].as_array().unwrap(); let mut new_ids = vec![];
    con.execute_batch("BEGIN")?;
    let r = (|| -> Result<()> {
        for a in list {
            let aid = a["id"].as_str().unwrap(); let mut a = a.clone(); a["first_seen"] = old.get(aid).map_or(stamp.clone(), |o| o.0.clone()).into();
            let payload = serde_json::to_string(&super::recurring::sorted(&a))?; let amt = parse_money(a["amount"].as_str().unwrap_or("0"));
            if !old.contains_key(aid) { new_ids.push(aid.to_string()); con.execute("INSERT INTO anomalies(id, type, severity, subject, period, amount_c, payload, first_seen, last_seen) VALUES (?,?,?,?,?,?,?,?,?)", rusqlite::params![aid, a["type"].as_str(), a["severity"].as_str(), a["subject"].as_str(), a["period"].as_str(), amt, payload, a["first_seen"].as_str(), stamp])?; }
            else { con.execute("UPDATE anomalies SET type=?, severity=?, subject=?, period=?, amount_c=?, payload=?, last_seen=? WHERE id=? AND payload<>?", rusqlite::params![a["type"].as_str(), a["severity"].as_str(), a["subject"].as_str(), a["period"].as_str(), amt, payload, stamp, aid, payload])?; }
        }
        let keep: HashSet<&str> = list.iter().filter_map(|a| a["id"].as_str()).collect();
        for (gid, (_, dm)) in &old { if !keep.contains(gid.as_str()) && dm.is_none() { con.execute("DELETE FROM anomalies WHERE id=?", [gid])?; } }
        Ok(())
    })();
    match r { Ok(()) => { con.execute_batch("COMMIT")?; Ok(json!({"found": list.len(), "new": new_ids.len(), "open": list.iter().filter(|a| a["dismissed"] == false).count(), "dismissed": res["dismissed"], "new_ids": new_ids})) } Err(e) => { let _ = con.execute_batch("ROLLBACK"); Err(e) } }
}
fn parse_money(s: &str) -> i64 { to_cents(s.parse().unwrap_or(0.0)) }
pub fn dismiss(con: &rusqlite::Connection, aid: &str, note: Option<&str>) -> Result<bool> { Ok(con.execute("UPDATE anomalies SET dismissed_at=?, dismiss_note=? WHERE id=?", rusqlite::params![now_iso(), note, aid])? > 0) }

#[cfg(test)]
mod tests { use super::*; #[test] fn q() { assert_eq!(quartiles(&[1, 2, 3, 4, 5, 6, 7, 8]), (2.75, 6.25)); } }
