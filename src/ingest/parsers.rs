//! Statement parsers: CSV (profile), OFX/QFX (SGML 1.x and XML 2.x) and CAMT.053 XML. Booked rows only, ISO dates, signed
//! amounts (negative = money out). Strict: an unreadable date or amount aborts the import with the line number.
use super::profiles::{Col, Profile};
use crate::{re, rs, util::*};
use anyhow::{bail, Result};
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct Row { pub date: String, pub amount: f64, pub currency: String, pub description: String, pub value_date: Option<String>, pub counterparty: String, pub reference: Option<String>, pub raw: Map<String, Value>, pub line: usize }
#[derive(Debug, Clone)]
pub struct Parsed { pub format: &'static str, pub rows: Vec<Row>, pub iban: Option<String>, pub currency: Option<String>, pub warnings: Vec<String> }

/// utf-8 (with or without BOM) first, then cp1252.
pub fn decode(data: &[u8], encoding: &str) -> Result<String> {
    if encoding != "auto" {
        let enc = encoding_rs::Encoding::for_label(encoding.replace("-sig", "").as_bytes()).ok_or_else(|| anyhow::anyhow!("cannot decode the file as {encoding}: unknown encoding"))?;
        let (t, _, bad) = enc.decode(data); if bad { bail!("cannot decode the file as {encoding}"); } return Ok(t.trim_start_matches('\u{feff}').into());
    }
    if let Some(r) = data.strip_prefix(b"\xef\xbb\xbf") { return Ok(String::from_utf8_lossy(r).into()); }
    if data.starts_with(b"\xff\xfe") || data.starts_with(b"\xfe\xff") { return Ok(encoding_rs::UTF_16LE.decode(data).0.into()); }
    Ok(match std::str::from_utf8(data) { Ok(s) => s.into(), Err(_) => encoding_rs::WINDOWS_1252.decode(data).0.into() })
}
/// Lowercase, accent-free, single-spaced: header names compare on this.
pub fn fold_h(s: &str) -> String { collapse(&strip_accents(s).replace('\u{feff}', "")).to_lowercase() }

pub fn parse_amount(text: &str, decimal: &str, thousands: &str) -> Result<f64> {
    let mut t = text.trim().to_string();
    if t.is_empty() { bail!("empty amount"); }
    let mut neg = false;
    if t.starts_with('(') && t.ends_with(')') { neg = true; t = t[1..t.len() - 1].into(); }
    t = sub(re!(r"(?i)[€$£\s\x{a0}\x{202f}]|EUR|USD|GBP"), &t, "").into();
    if let Some(x) = t.strip_suffix('-') { neg = true; t = x.into(); }
    if let Some(x) = t.strip_prefix('+') { t = x.into(); }
    if !thousands.is_empty() { t = t.replace(thousands, ""); }
    let dec = if decimal != "auto" { decimal } else if t.contains(',') && t.contains('.') { if t.rfind(',') > t.rfind('.') { "," } else { "." } } else if t.contains(',') { if is(re!(r",\d{1,2}$"), &t) { "," } else { "." } } else { "." };
    t = if dec == "," { t.replace('.', "").replace(',', ".") } else { t.replace(',', "") };
    if !is(re!(r"^-?\d+(\.\d+)?$"), &t) { bail!("not an amount: {text:?}"); }
    let v: f64 = t.parse()?; Ok(if neg { -v } else { v })
}
pub fn parse_date(text: &str, formats: &[String]) -> Result<String> {
    let t = text.trim();
    let mut c = vec![t.to_string()];
    if t.contains(' ') { c.push(t.split_whitespace().next().unwrap_or("").into()); }
    if t.contains('T') { c.push(t.split('T').next().unwrap_or("").into()); }
    for x in &c { for f in formats { if let Ok(dt) = NaiveDateTime::parse_from_str(x, f) { return Ok(dt.date().to_string()); } if let Ok(dd) = NaiveDate::parse_from_str(x, f) { return Ok(dd.to_string()); } } }
    bail!("unreadable date {text:?} (expected {})", formats.join(" or "))
}
fn delimiter(sample: &str, wanted: &str) -> u8 {
    if wanted != "auto" { return if wanted == "\\t" || wanted == "tab" { b'\t' } else { wanted.as_bytes()[0] }; }
    let best = [';', ',', '\t', '|'].into_iter().map(|d| (sample.matches(d).count(), d)).fold((0, ';'), |a, b| if b.0 > a.0 { b } else { a });
    if best.0 > 0 { best.1 as u8 } else { b';' }
}

pub fn parse_csv(data: &[u8], p: &Profile) -> Result<Parsed> {
    let text = decode(data, &p.encoding)?;
    let mut lines: Vec<&str> = text.lines().collect();
    let start = if !p.header_contains.is_empty() {
        let want: Vec<_> = p.header_contains.iter().map(|x| fold_h(x)).collect();
        lines.iter().position(|l| want.iter().all(|w| fold_h(l).contains(w.as_str()))).ok_or_else(|| anyhow::anyhow!("no header line containing {:?} found (wrong file or profile?)", p.header_contains))?
    } else { p.skip_rows };
    if p.skip_footer > 0 { lines.truncate(lines.len().saturating_sub(p.skip_footer)); }
    let body = lines.get(start..).unwrap_or(&[]);
    if body.is_empty() { bail!("the file has no data after the skipped lines"); }
    let joined = body.join("\n");
    let mut rd = csv::ReaderBuilder::new().has_headers(false).flexible(true).delimiter(delimiter(body[0], &p.delimiter)).from_reader(joined.as_bytes());
    let recs: Vec<Vec<String>> = rd.records().filter_map(|r| r.ok()).map(|r| r.iter().map(String::from).collect()).collect();
    let (header, rows_in, first): (Vec<String>, &[Vec<String>], usize) = if p.has_header { (recs.first().map(|h| h.iter().map(|x| fold_h(x)).collect()).unwrap_or_default(), recs.get(1..).unwrap_or(&[]), start + 2) } else { (vec![], &recs[..], start + 1) };
    let col = |spec: &str| -> Result<usize> {
        if header.is_empty() { bail!("column {spec:?} given by name but the profile says has_header = false"); }
        let n = fold_h(spec); header.iter().position(|h| *h == n).ok_or_else(|| anyhow::anyhow!("column {spec:?} not found; the file has: {}", header.iter().filter(|h| !h.is_empty()).cloned().collect::<Vec<_>>().join(", ")))
    };
    let mut cols: Vec<(String, Vec<usize>)> = vec![];
    for (k, c) in &p.columns { cols.push((k.clone(), match c { Col::Index(i) => vec![*i], Col::Name(n) => vec![col(n)?], Col::Many(v) => v.iter().map(|n| col(n)).collect::<Result<_>>()? })); }
    let ci = |k: &str| cols.iter().find(|c| c.0 == k).map(|c| c.1.clone());
    let fcol = p.filter_column.as_deref().map(col).transpose()?;
    let keep: Vec<String> = p.filter_keep.iter().map(|k| fold_h(k)).collect();
    let cell = |r: &[String], i: Option<usize>| i.and_then(|i| r.get(i)).map_or(String::new(), |c| c.trim().to_string());
    let one = |k: &str| ci(k).and_then(|v| v.first().copied());
    let (mut rows, mut bad) = (vec![], vec![]);
    for (n, r) in rows_in.iter().enumerate().map(|(i, r)| (i + first, r)) {
        if r.iter().all(|c| c.trim().is_empty()) { continue; }
        if fcol.is_some() && !keep.contains(&fold_h(&cell(r, fcol))) { continue; }
        let dtxt = cell(r, one("date")); if dtxt.is_empty() { continue; }
        let res = (|| -> Result<(String, Option<String>, f64)> {
            let d = parse_date(&dtxt, &p.date_format)?;
            let vd = { let v = cell(r, one("value_date")); if v.is_empty() { None } else { Some(parse_date(&v, &p.date_format)?) } };
            let mut amt = if one("amount").is_some() { parse_amount(&cell(r, one("amount")), &p.decimal, &p.thousands)? } else {
                let (de, cr) = (cell(r, one("debit")), cell(r, one("credit")));
                if de.is_empty() && cr.is_empty() { bail!("no debit nor credit amount"); }
                (if cr.is_empty() { 0.0 } else { parse_amount(&cr, &p.decimal, &p.thousands)?.abs() }) - (if de.is_empty() { 0.0 } else { parse_amount(&de, &p.decimal, &p.thousands)?.abs() })
            };
            let fee = cell(r, one("fee")); if !fee.is_empty() { amt -= parse_amount(&fee, &p.decimal, &p.thousands)?; }
            Ok((d, vd, amt))
        })();
        let (d, vd, mut amt) = match res { Ok(x) => x, Err(e) => { bad.push(format!("line {n}: {e}")); continue; } };
        if p.negate { amt = -amt; }
        let desc = dedup_keep_order(ci("description").unwrap_or_default().into_iter().map(|i| cell(r, Some(i))).filter(|x| !x.is_empty())).join(" | ");
        let cur = { let c = cell(r, one("currency")); if c.is_empty() { p.currency.clone() } else { c } };
        let raw: Map<String, Value> = if header.is_empty() { Map::from_iter([("row".to_string(), Value::from(r.clone()))]) } else { header.iter().zip(r).filter(|(h, _)| !h.is_empty()).map(|(h, c)| (h.clone(), Value::from(c.clone()))).collect() };
        let rf = cell(r, one("reference"));
        rows.push(Row { date: d, amount: (amt * 100.0).round() / 100.0, currency: cur.to_uppercase(), description: collapse_ws(&desc), value_date: vd, counterparty: cell(r, one("counterparty")), reference: (!rf.is_empty()).then_some(rf), raw, line: n });
    }
    if !bad.is_empty() { bail!("{} unreadable row(s), nothing imported: {}{}", bad.len(), bad[..bad.len().min(5)].join("; "), if bad.len() > 5 { format!("; ... {} more", bad.len() - 5) } else { String::new() }); }
    if rows.is_empty() { bail!("no transaction rows found (check the profile: header, skip_rows, filter)"); }
    Ok(Parsed { format: "csv", rows, iban: None, currency: None, warnings: vec![] })
}
/// Python `re.sub(r"\s+", " ", s)` (no strip).
fn collapse_ws(s: &str) -> String { rs!(r"\s+").replace_all(s, " ").into() }

pub fn html_unescape(s: &str) -> String {
    rs!(r"&(#x[0-9a-fA-F]+|#\d+|amp|lt|gt|quot|apos|nbsp);").replace_all(s, |c: &regex::Captures| match &c[1] { "amp" => "&".into(), "lt" => "<".into(), "gt" => ">".into(), "quot" => "\"".into(), "apos" => "'".into(), "nbsp" => "\u{a0}".into(),
        x if x.starts_with("#x") => u32::from_str_radix(&x[2..], 16).ok().and_then(char::from_u32).map_or(c[0].to_string(), String::from), x => x[1..].parse().ok().and_then(char::from_u32).map_or(c[0].to_string(), String::from) }).into()
}
fn ofx_date(v: &str) -> Result<String> { let c = cap(re!(r"^(\d{4})(\d{2})(\d{2})"), v.trim()).ok_or_else(|| anyhow::anyhow!("unreadable OFX date {v:?}"))?; Ok(format!("{}-{}-{}", g(&c, 1), g(&c, 2), g(&c, 3))) }

pub fn parse_ofx(data: &[u8]) -> Result<Parsed> {
    let text = decode(data, "auto")?;
    let iban = cap(re!(r"<ACCTID>\s*([^<\r\n]+)"), &text).map(|c| g(&c, 1).to_string());
    let cur = cap(re!(r"<CURDEF>\s*([A-Za-z]{3})"), &text).map(|c| g(&c, 1).to_uppercase());
    let blocks: Vec<String> = re!(r"(?si)<STMTTRN>(.*?)(?=</STMTTRN>|<STMTTRN>|</BANKTRANLIST>|</CCSTMTRS>|</STMTRS>|$)").captures_iter(&text).flatten().map(|c| g(&c, 1).to_string()).collect();
    if blocks.is_empty() { bail!("no <STMTTRN> transactions found: not an OFX bank/card statement"); }
    let (mut rows, mut bad) = (vec![], vec![]);
    for (n, b) in blocks.iter().enumerate().map(|(i, b)| (i + 1, b)) {
        let mut f = Map::new();
        for c in re!(r"<([A-Za-z0-9.]+)>([^<\r\n]*)").captures_iter(b).flatten() { f.entry(g(&c, 1).to_uppercase()).or_insert(Value::from(html_unescape(g(&c, 2).trim()))); }
        let gs = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let r = (|| -> Result<(String, f64)> { Ok((ofx_date(f.get("DTPOSTED").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("KeyError('DTPOSTED')"))?)?, parse_amount(f.get("TRNAMT").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("KeyError('TRNAMT')"))?, "auto", "")?)) })();
        let (dd, amt) = match r { Ok(x) => x, Err(e) => { bad.push(format!("transaction {n}: {e}")); continue; } };
        let desc = dedup_keep_order([gs("NAME"), gs("MEMO")].into_iter().filter(|x| !x.is_empty())).join(" | ");
        let vd = if gs("DTUSER").is_empty() { None } else { Some(ofx_date(&gs("DTUSER"))?) };
        let c = if !gs("CURRENCY").is_empty() { gs("CURRENCY") } else { cur.clone().unwrap_or_else(|| "EUR".into()) };
        rows.push(Row { date: dd, amount: (amt * 100.0).round() / 100.0, currency: c.to_uppercase(), description: collapse_ws(&desc), value_date: vd, counterparty: gs("NAME"), reference: (!gs("FITID").is_empty()).then(|| gs("FITID")), raw: f.clone(), line: n });
    }
    if !bad.is_empty() { bail!("{} unreadable transaction(s), nothing imported: {}", bad.len(), bad[..bad.len().min(5)].join("; ")); }
    let iban = iban.map(|i| i.replace(' ', "")).filter(|i| is(re!(r"^[A-Z]{2}\d{2}"), i));
    Ok(Parsed { format: "ofx", rows, iban, currency: cur, warnings: vec![] })
}

fn child<'a>(n: roxmltree::Node<'a, 'a>, path: &str) -> Option<roxmltree::Node<'a, 'a>> { path.split('/').try_fold(n, |n, p| n.children().find(|c| c.is_element() && c.tag_name().name() == p)) }
fn t(n: roxmltree::Node, path: &str) -> String { child(n, path).and_then(|x| x.text()).map_or(String::new(), |s| s.trim().to_string()) }

pub fn parse_camt053(data: &[u8]) -> Result<Parsed> {
    if data.contains(&0) && !(data.starts_with(b"\xff\xfe") || data.starts_with(b"\xfe\xff")) { bail!("unsupported XML encoding (UTF-16/32 without a byte-order mark)"); }
    let text = decode(data, "auto")?;
    if is(re!(r"(?i)<!\s*(DOCTYPE|ENTITY)"), &text) { bail!("refusing an XML file with a DOCTYPE/ENTITY declaration (not valid CAMT.053)"); }
    let doc = roxmltree::Document::parse(text.trim_start_matches('\u{feff}')).map_err(|e| anyhow::anyhow!("invalid XML: {e}"))?;
    let by = |name: &str| doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == name).collect::<Vec<_>>();
    let stmts = { let s = by("Stmt"); if s.is_empty() { by("Rpt") } else { s } };
    if stmts.is_empty() { bail!("no <Stmt> element: not a CAMT.053 bank statement"); }
    let (mut rows, mut warnings, mut ibans) = (vec![], vec![], Vec::<String>::new());
    for st in stmts {
        let iban = { let i = t(st, "Acct/Id/IBAN"); if i.is_empty() { t(st, "Acct/Id/Othr/Id") } else { i } };
        if !iban.is_empty() && !ibans.contains(&iban) { ibans.push(iban); }
        for ntry in st.children().filter(|c| c.is_element() && c.tag_name().name() == "Ntry") {
            let sts = { let s = t(ntry, "Sts/Cd"); if s.is_empty() { t(ntry, "Sts") } else { s } };
            if !sts.is_empty() && !["BOOK", "BOOKED"].contains(&sts.to_uppercase().as_str()) { warnings.push(format!("skipped a {sts} entry")); continue; }
            let amt_el = child(ntry, "Amt").ok_or_else(|| anyhow::anyhow!("unreadable CAMT entry (no Amt); nothing imported"))?;
            let amt_txt = amt_el.text().unwrap_or("");
            let amt = parse_amount(amt_txt, ".", "").map_err(|e| anyhow::anyhow!("unreadable CAMT entry ({e}); nothing imported"))?;
            let cur = amt_el.attribute("Ccy").unwrap_or("EUR");
            let mut dd = t(ntry, "BookgDt/Dt"); if dd.is_empty() { dd = truncate_chars(&t(ntry, "BookgDt/DtTm"), 10); } if dd.is_empty() { dd = t(ntry, "ValDt/Dt"); }
            if dd.is_empty() { bail!("unreadable CAMT entry (entry without booking date); nothing imported"); }
            let dd = parse_date(&dd, &["%Y-%m-%d".into()]).map_err(|e| anyhow::anyhow!("unreadable CAMT entry ({e}); nothing imported"))?;
            let mut debit = t(ntry, "CdtDbtInd").to_uppercase() == "DBIT";
            if t(ntry, "RvslInd").to_lowercase() == "true" { debit = !debit; }
            let amt = if debit { -amt.abs() } else { amt.abs() };
            let texts = |tag: &str| ntry.descendants().filter(|x| x.is_element() && x.tag_name().name() == tag).filter_map(|x| x.text().map(str::trim).filter(|s| !s.is_empty()).map(String::from)).collect::<Vec<_>>();
            let mut parts = texts("Ustrd");
            let r1: Vec<_> = texts("Ref").into_iter().filter(|x| !parts.contains(x)).take(1).collect(); parts.extend(r1);
            let add = t(ntry, "AddtlNtryInf"); if !add.is_empty() { parts.push(add); }
            let cpp = if debit { "NtryDtls/TxDtls/RltdPties/Cdtr/Nm" } else { "NtryDtls/TxDtls/RltdPties/Dbtr/Nm" };
            let cp = { let c = t(ntry, cpp); if c.is_empty() { t(ntry, &cpp.replace("/Nm", "/Pty/Nm")) } else { c } };
            let desc = dedup_keep_order((!cp.is_empty()).then(|| cp.clone()).into_iter().chain(parts)).join(" | ");
            let vd = t(ntry, "ValDt/Dt"); let rf = { let r = t(ntry, "AcctSvcrRef"); if r.is_empty() { t(ntry, "NtryRef") } else { r } };
            rows.push(Row { date: dd, amount: (amt * 100.0).round() / 100.0, currency: cur.to_uppercase(), description: collapse_ws(&desc), value_date: (!vd.is_empty()).then_some(vd), counterparty: cp, reference: (!rf.is_empty()).then_some(rf), raw: Map::from_iter([("status".into(), Value::from(sts)), ("amount".into(), Value::from(amt_txt))]), line: rows.len() + 1 });
        }
    }
    if ibans.len() > 1 { warnings.push(format!("the file holds statements of several accounts: {}", ibans.join(", "))); }
    if rows.is_empty() { bail!("no booked entries found in the CAMT.053 file"); }
    let iban = (ibans.len() == 1 && is(re!(r"^[A-Z]{2}\d{2}"), &ibans[0])).then(|| ibans[0].replace(' ', ""));
    Ok(Parsed { format: "camt053", rows, iban, currency: None, warnings })
}

// ---------------------------------------------------------------- CODA (Belgian bank statement file, 128-column records)
fn coda_date(s: &str) -> Option<String> { let (dd, mm, yy): (u32, u32, i32) = (s.get(0..2)?.parse().ok()?, s.get(2..4)?.parse().ok()?, s.get(4..6)?.parse().ok()?); NaiveDate::from_ymd_opt(2000 + yy, mm, dd).map(|x| x.to_string()) }
/// CODA 2.x: record 1 = account (IBAN, currency), 2.1 / 2.2 / 2.3 = one movement (amount, dates, communication, counterparty),
/// 8 = new balance. Only the movement itself (detail 0000) is kept: the details of a globalised movement would count it twice.
/// TO VERIFY WITH A REAL FILE: field positions follow the published CODA 2 layout.
pub fn parse_coda(data: &[u8]) -> Result<Parsed> {
    let text = decode(data, "auto")?;
    let (mut rows, mut iban, mut currency, mut warnings) = (Vec::<Row>::new(), None::<String>, None::<String>, vec![]);
    let f = |c: &[char], a: usize, b: usize| c.get(a..b.min(c.len())).map_or(String::new(), |x| x.iter().collect::<String>());
    let mut cur: Option<(Row, String, String, String)> = None; // (row, communication, structured ref, name)
    let flush = |cur: &mut Option<(Row, String, String, String)>, rows: &mut Vec<Row>| if let Some((mut r, comm, sref, name)) = cur.take() {
        let comm = collapse(&comm);
        r.description = dedup_keep_order([collapse(&name), comm.clone(), sref.clone()].into_iter().filter(|x| !x.is_empty())).join(" | ");
        if r.description.is_empty() { r.description = r.raw.get("coda_code").and_then(Value::as_str).unwrap_or("").to_string(); }
        r.counterparty = collapse(&name); r.reference = (!sref.is_empty()).then_some(sref); rows.push(r);
    };
    for (n, line) in text.lines().enumerate() {
        let c: Vec<char> = line.trim_end_matches('\r').chars().collect();
        if c.is_empty() { continue; }
        match (c[0], c.get(1).copied().unwrap_or(' ')) {
            ('1', st) => { let raw = f(&c, 5, 42); iban = Some(match st { '2' => f(&c, 5, 21), '3' => f(&c, 5, 39), _ => f(&c, 5, 17) }.trim().replace(' ', "")).filter(|i| !i.is_empty()); let cy = f(&c, 39, 42); currency = cy.chars().all(|x| x.is_ascii_uppercase()).then_some(cy).filter(|x| x.len() == 3); let _ = raw; }
            ('2', '1') => {
                flush(&mut cur, &mut rows);
                if f(&c, 6, 10) != "0000" { continue; }
                let digits = f(&c, 32, 47); let v: f64 = digits.trim().parse::<f64>().map_err(|_| anyhow::anyhow!("line {}: unreadable CODA amount {digits:?}", n + 1))? / 1000.0;
                let amt = if f(&c, 31, 32) == "1" { -v } else { v };
                let date = coda_date(&f(&c, 115, 121)).or_else(|| coda_date(&f(&c, 47, 53))).ok_or_else(|| anyhow::anyhow!("line {}: unreadable CODA date", n + 1))?;
                let (ctype, comm) = (f(&c, 61, 62), f(&c, 62, 115));
                let sref = if ctype == "1" && comm.starts_with("101") { let d = &comm[3..15.min(comm.len())]; if d.len() == 12 && d.chars().all(|x| x.is_ascii_digit()) { format!("+++{}/{}/{}+++", &d[..3], &d[3..7], &d[7..]) } else { String::new() } } else { String::new() };
                let row = Row { date, amount: (amt * 100.0).round() / 100.0, currency: currency.clone().unwrap_or_else(|| "EUR".into()), description: String::new(), value_date: coda_date(&f(&c, 47, 53)), counterparty: String::new(), reference: None,
                    raw: Map::from_iter([("coda_code".into(), Value::from(f(&c, 53, 61))), ("bank_ref".into(), Value::from(f(&c, 10, 31).trim()))]), line: n + 1 };
                cur = Some((row, if ctype == "1" { String::new() } else { comm }, sref, String::new()));
            }
            ('2', '2') => if let Some(x) = cur.as_mut() { if x.2.is_empty() { x.1.push_str(&f(&c, 10, 63)); } },
            ('2', '3') => if let Some(x) = cur.as_mut() { x.3 = f(&c, 47, 82); if x.2.is_empty() { x.1.push_str(&f(&c, 82, 125)); } let acc = f(&c, 10, 47); if !acc.trim().is_empty() { x.0.raw.insert("counterparty_account".into(), Value::from(acc.trim())); } },
            ('8', _) | ('9', _) => flush(&mut cur, &mut rows),
            _ => {}
        }
    }
    flush(&mut cur, &mut rows);
    if rows.is_empty() { bail!("no movements found in the CODA file"); }
    if iban.as_deref().is_some_and(|i| !is(re!(r"^[A-Z]{2}\d{2}"), i)) { warnings.push("the CODA account number is not an IBAN: the account check is skipped".into()); iban = None; }
    Ok(Parsed { format: "coda", rows, iban, currency, warnings })
}

pub fn detect_format(name: &str, data: &[u8]) -> Result<&'static str> {
    let low = name.to_lowercase();
    if low.ends_with(".cod") || low.ends_with(".coda") || (data.starts_with(b"0000") && data.iter().position(|b| *b == b'\n').is_some_and(|i| (128..=129).contains(&i))) { return Ok("coda"); } let head = &data[..data.len().min(4096)]; let up = String::from_utf8_lossy(head).to_uppercase();
    let lead = String::from_utf8_lossy(head); let lead = lead.trim_start_matches(['\u{feff}', ' ', '\r', '\n', '\t']);
    if low.ends_with(".ofx") || low.ends_with(".qfx") || String::from_utf8_lossy(&data[..data.len().min(512)]).contains("OFXHEADER") || up.contains("<OFX>") { return Ok("ofx"); }
    if low.ends_with(".xml") || lead.starts_with("<?xml") || String::from_utf8_lossy(&data[..data.len().min(8192)]).contains("BkToCstmrStmt") { return Ok("camt053"); }
    if low.ends_with(".pdf") || data.starts_with(b"%PDF") { bail!("PDF statements are not supported; export CSV/OFX/CAMT.053 from your bank instead"); }
    Ok("csv")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn amounts() {
        for (s, v) in [("1 234,56", 1234.56), ("-12.30", -12.3), ("(5,00)", -5.0), ("1,234", 1234.0), ("12,5", 12.5), ("3.50-", -3.5), ("€ 7", 7.0), ("1.234,5", 1234.5)] { assert_eq!(parse_amount(s, "auto", "").unwrap(), v, "{s}"); }
        assert!(parse_amount("abc", "auto", "").is_err());
    }
    #[test] fn dates() { assert_eq!(parse_date("03/02/25", &["%d/%m/%y".into()]).unwrap(), "2025-02-03"); assert_eq!(parse_date("2025-02-03 10:11:12", &["%Y-%m-%d %H:%M:%S".into()]).unwrap(), "2025-02-03"); assert_eq!(parse_date("2025-02-03T10:11", &["%Y-%m-%d".into()]).unwrap(), "2025-02-03"); }
    #[test] fn ofx() {
        let d = b"OFXHEADER:100\n<OFX><CURDEF>EUR<ACCTID>FR7612345\n<STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20250102<TRNAMT>-12.50<FITID>A1<NAME>SHOP &amp; CO<MEMO>card\n</STMTTRN></OFX>";
        let p = parse_ofx(d).unwrap(); assert_eq!(p.rows[0].description, "SHOP & CO | card"); assert_eq!(p.rows[0].amount, -12.5); assert_eq!(p.iban.as_deref(), Some("FR7612345"));
    }
    #[test] fn camt() {
        let x = br#"<?xml version="1.0"?><Document xmlns="urn:iso:std:iso:20022:tech:xsd:camt.053.001.02"><BkToCstmrStmt><Stmt><Acct><Id><IBAN>FR7630001007941234567890185</IBAN></Id></Acct>
          <Ntry><Amt Ccy="EUR">10.00</Amt><CdtDbtInd>DBIT</CdtDbtInd><Sts>BOOK</Sts><BookgDt><Dt>2025-03-01</Dt></BookgDt><NtryDtls><TxDtls><RltdPties><Cdtr><Nm>ACME</Nm></Cdtr></RltdPties><RmtInf><Ustrd>INV 1</Ustrd></RmtInf></TxDtls></NtryDtls></Ntry>
          <Ntry><Amt Ccy="EUR">5.00</Amt><CdtDbtInd>CRDT</CdtDbtInd><Sts>PDNG</Sts><BookgDt><Dt>2025-03-02</Dt></BookgDt></Ntry></Stmt></BkToCstmrStmt></Document>"#;
        let p = parse_camt053(x).unwrap(); assert_eq!(p.rows.len(), 1); assert_eq!(p.rows[0].amount, -10.0); assert_eq!(p.rows[0].description, "ACME | INV 1"); assert_eq!(p.warnings, vec!["skipped a PDNG entry"]);
        assert!(parse_camt053(b"<?xml version='1.0'?><!DOCTYPE x [<!ENTITY a 'b'>]><x/>").is_err());
    }
    fn rec(parts: &[(usize, &str)]) -> String { let mut c = vec![' '; 128]; for (p, s) in parts { for (i, ch) in s.chars().enumerate() { c[p - 1 + i] = ch; } } c.into_iter().collect() }
    #[test] fn coda() {
        let lines = [rec(&[(1, "0000014032530005")]), rec(&[(1, "12001"), (6, "BE68539007547034"), (40, "EUR")]),
            rec(&[(1, "21"), (3, "0001"), (7, "0000"), (32, "1"), (33, "000000000012340"), (48, "130325"), (54, "00150000"), (62, "1"), (63, "101123456789012"), (116, "140325")]),
            rec(&[(1, "23"), (3, "0001"), (7, "0000"), (11, "BE12345678901234"), (48, "PROXIMUS NV")]),
            rec(&[(1, "21"), (3, "0002"), (7, "0000"), (32, "0"), (33, "000000002500000"), (48, "150325"), (62, "0"), (63, "LOON MAART"), (116, "150325")]),
            rec(&[(1, "8")]), rec(&[(1, "9")])].join("\n");
        assert_eq!(detect_format("x.txt", lines.as_bytes()).unwrap(), "coda");
        let p = parse_coda(lines.as_bytes()).unwrap();
        assert_eq!(p.iban.as_deref(), Some("BE68539007547034")); assert_eq!(p.rows.len(), 2);
        assert_eq!((p.rows[0].date.as_str(), p.rows[0].amount, p.rows[0].description.as_str()), ("2025-03-14", -12.34, "PROXIMUS NV | +++123/4567/89012+++"));
        assert_eq!((p.rows[1].amount, p.rows[1].description.as_str()), (2500.0, "LOON MAART"));
    }
}
