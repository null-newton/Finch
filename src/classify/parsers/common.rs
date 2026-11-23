//! Shared building blocks of the per-bank descriptor parsers: input/output structures, merchant keys, holder / household
//! detection, and the person-vs-company heuristics.
use crate::{re, rs, util::*};
use chrono::NaiveDate;
use rusqlite::Connection;
use std::{collections::{BTreeMap, HashSet}, path::Path, sync::LazyLock};

pub type Toks = HashSet<String>;
fn set(s: &str) -> Toks { s.split_whitespace().map(String::from).collect() }
pub static PROCESSORS: LazyLock<Toks> = LazyLock::new(|| set("MOL SUMUP SQ NYX SUNDAY PAYPAL ZTL STRIPE IZ PAYPLUG LYF"));
pub static TITLES: LazyLock<Toks> = LazyLock::new(|| set("M MR MME MLLE OU MONSIEUR MADAME DHR MEVR MEVROUW HEER MIJNHEER EN ET MRS MISS MS DR"));
pub static PREFIX_WORDS: LazyLock<Toks> = LazyLock::new(|| set("SEPA INST INSTANT INSTANTANE INSTANTANEE EUROPESE EUROPEEN EUROPEENNE OVERSCHRIJVING VIREMENT VIR STORTING ONTVANGEN RECU DOORLOPENDE OPDRACHT ORDRE PERMANENT SCT WERO"));
/// Surname particles (Van Damme, De Smet, Vander Elst, Le Clercq): part of a Belgian person's name, never a connector.
pub static NAME_PARTICLES: LazyLock<Toks> = LazyLock::new(|| set("VAN VANDE VANDEN VANDER DE DER DEN DU LE LA T TEN TER VON DI"));
pub static PRODUCT_WORDS: LazyLock<Toks> = LazyLock::new(|| set("VISA CARTE CARD KAART DEBETKAART KREDIETKAART COMPTE REKENING ZICHTREKENING SPAARREKENING TERMIJNREKENING MASTERCARD GOLD PLATINUM
COURANT EPARGNE SPAREN SPAAR ZICHT VUE DEPOT CREDIT KREDIET PRET LENING PROFESSIONNEL PROFESSIONEEL JOINT GEZAMENLIJK PERSONAL CURRENT SAVINGS SAVING POCKET BUSINESS PLUS EUR USD GBP"));
pub static GENERIC_BANK_WORDS: LazyLock<Toks> = LazyLock::new(|| set("BANQUE BANK BANCA BANQ CREDIT EPARGNE BELGIUM BELGIQUE BELGIE GROUP GROUPE PARIBAS NV SA BV THE LA LE DE DES DU ET EN VAN"));
pub static CONNECTORS: LazyLock<Toks> = LazyLock::new(|| set("DE DU DES LA LE LES ET AU AUX SUR EN VAN DER DEN HET TE TER OP VOOR BIJ OF AND THE D L"));
pub static P2P_PREFIXES: LazyLock<Toks> = LazyLock::new(|| set("PAYCONIQ WERO TIKKIE"));
pub static LEGAL_FORMS: LazyLock<Toks> = LazyLock::new(|| set("BV NV VZW ASBL SRL SA SC CV BVBA SPRL CVBA SCRL COMMV SCS VOF SNC GCV SCOMM EBVBA SPRLU IVZW AISBL GMBH LTD INC LLC AG PLC SAS SARL"));
pub static FIRST_NAMES: LazyLock<Toks> = LazyLock::new(|| set(include_str!("first_names.txt")));
/// Organisation words (Dutch, French, English, as written by Belgian banks): any of them means "not a person".
pub static ORG_WORDS: LazyLock<Toks> = LazyLock::new(|| set("BV NV VZW ASBL SRL SA SC CV BVBA SPRL CVBA SCRL COOP ASSOCIATIE VERENIGING ASSOCIATION SYNDIC SYNDICUS MEDE EIGENDOM COPROPRIETE
BANK BANQUE KREDIET CREDIT VERZEKERING VERZEKERINGEN ASSURANCE ASSURANCES MUTUALITEIT ZIEKENFONDS MUTUALITE MUTUALITES MUTUELLE SCHOOL ECOLE COLLEGE ATHENEE LYCEUM UNIVERSITEIT UNIVERSITE HOGESCHOOL
CLUB GEMEENTE STAD VILLE COMMUNE PROVINCIE PROVINCE FOD SPF FINANCIEN FINANCES BELASTINGDIENST VLAAMSE WAALSE BRUSSELS REGIE OVERHEID SERVICE SERVICES DIENST DIENSTEN GROEP GROUP GROUPE
BEDRIJF ENTREPRISE SOCIETE COMPAGNIE AGENTSCHAP AGENCE KANTOOR CABINET BUREAU KLINIEK CLINIQUE ZIEKENHUIS HOPITAL CENTRUM CENTRE CENTER HUIS MAISON GARAGE BAKKERIJ BOULANGERIE APOTHEEK PHARMACIE
RESTAURANT HOTEL TRAITEUR SLAGERIJ BOUCHERIE STUDIO BELGIUM BELGIQUE BELGIE EUROPE PAYMENTS PAYMENT LTD LIMITED INC GMBH TELECOM ENERGIE ENERGY ENGIE LUMINUS FLUVIUS ORES SIBELGA PROXIMUS TELENET ORANGE
BASE SCARLET MOBILE VOO WATERGROEP VIVAQUA SWDE FARYS AQUAFIN DELHAIZE COLRUYT CARREFOUR ALDI LIDL MATCH SPAR OKAY BIOPLANET KRUIDVAT HEMA ACTION BRICO GAMMA HUBO TORFS DECATHLON MEDIAMARKT VANDENBORRE
COOLBLUE BOL AMAZON PAYPAL REVOLUT NMBS SNCB DELIJN STIB TEC BPOST POST POSTE RIZIV INAMI RSZ ONSS RVA ONEM KINDERBIJSLAG FAMIFED GROEIPAKKET FONDATION STICHTING FEDERATIE FEDERATION UNIE UNION
SOFTWARE HOLDING LEASING VERHUUR LOCATION ELECTRICITE ELEKTRICITEIT WATER EAU GAS GAZ INTERNET TELEFOON TELEPHONE BELASTING BELASTINGEN IMPOTS TAKS TAXE TAXES BOETE AMENDE HUUR LOYER
LABO LABORATORIUM LABORATOIRE MEDISCH MEDICAL MEDICALE KINDEROPVANG CRECHE KINDERDAGVERBLIJF DIERENARTS VETERINAIRE INSTITUUT INSTITUT ACADEMIE CONSERVATORIUM"));

pub const FX_RE: &str = r"\s+(\d+,\d{2})\s+([A-Z]{3})\b.*$";
/// A title in front of a person's name (Dutch / French / English as Belgian banks print them).
pub const PERSON_RE: &str = r"(?i)^(DHR EN MEVR|MR EN MEVR|M ET MME|MR ET MME|MEVROUW|MIJNHEER|DE HEER|MONSIEUR|MADAME|MEVR\.?|DHR\.?|MLLE|MME|MR|M\.?)\s";

/// What a parser gets: the stored transaction plus the account it belongs to.
#[derive(Debug, Clone, Default)]
pub struct RawTx { pub description: String, pub amount: f64, pub booking_date: String, pub currency: String, pub counterparty: String, pub bank_tx_code: String, pub account_type: String, pub bank: String, pub raw: Option<serde_json::Value> }

/// Who the account holders are (holders >= 2 tokens identify OWN-account transfers; family flags a probable relative).
#[derive(Debug, Clone, Default)]
pub struct Household { pub holders: Vec<Toks>, pub family: Toks, pub first_names: Toks, pub own_banks: BTreeMap<String, String> }

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed { pub tx_type: String, pub op_date: Option<String>, pub merchant_raw: String, pub merchant_key: String, pub fx_amount: Option<f64>, pub fx_currency: Option<String>, pub counterparty: Option<String>, pub mandate_ref: Option<String>, pub creditor_id: Option<String>, pub reference: Option<String>, pub parser: String }

pub fn result(tx_type: &str, merchant_raw: &str, op_date: Option<String>, fx: Option<(f64, String)>) -> Parsed {
    Parsed { tx_type: tx_type.into(), op_date, merchant_key: merchant_key(merchant_raw), merchant_raw: merchant_raw.into(), fx_amount: fx.as_ref().map(|f| f.0), fx_currency: fx.map(|f| f.1), ..Default::default() }
}
pub fn with_cp(mut p: Parsed, cp: Option<String>) -> Parsed { p.counterparty = cp; p }

/// Descriptor fields: the remittance lines joined by ' | ' (CE, CIC, Revolut), or also padded with 2+ spaces (Fortuneo).
pub fn segments(desc: &str, padded: bool) -> Vec<String> {
    let mut out = vec![];
    for part in rs!(r"\s+\|\s+").split(desc.trim()) { let part = part.trim(); if padded { out.extend(rs!(r"\s{2,}").split(part).map(str::trim).filter(|p| !p.is_empty()).map(String::from)); } else if !part.is_empty() { out.push(part.into()); } }
    if out.is_empty() { vec![String::new()] } else { out }
}
/// 'dd/mm' of a card payment -> ISO date: the latest such date not after the booking date.
pub fn ddmm_date(ddmm: &str, booking: &str, sep: char) -> Option<String> {
    let mut it = ddmm.split(sep); let (dd, mm): (u32, u32) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    let y: i32 = booking.get(..4)?.parse().ok()?; let bm: u32 = booking.get(5..7)?.parse().ok()?;
    let y = y - (mm > bm) as i32; NaiveDate::from_ymd_opt(y, mm, dd)?; Some(format!("{y}-{mm:02}-{dd:02}"))
}
pub fn fx_split(rest: &str) -> (String, Option<(f64, String)>) {
    match cap(re!(FX_RE), rest) { Some(c) => (rest[..c.get(0).unwrap().start()].into(), Some((g(&c, 1).replace(',', ".").parse().unwrap_or(0.0), g(&c, 2).into()))), None => (rest.into(), None) }
}

pub fn merchant_key(raw: &str) -> String {
    let mut out: Vec<String> = vec![];
    for tok in raw.to_uppercase().split_whitespace() {
        let mut tok = tok.to_string();
        if let Some((l, r)) = tok.clone().split_once('*') { let l = if PROCESSORS.contains(l) { "" } else { l }; tok = format!("{l}{}", if !r.is_empty() && !r.chars().any(|c| c.is_numeric()) { format!(" {r}") } else { String::new() }); }
        let t = rs!(r"\d+").replace_all(&tok, ""); let t = rs!(r"[^\w' ]+").replace_all(&t, " "); let t = t.trim_matches([' ', '\'']);
        out.extend(t.split_whitespace().filter(|x| x.chars().count() >= 2).map(String::from));
    }
    out.join(" ")
}

pub fn name_tokens(name: Option<&str>) -> Toks {
    let n = sub(re!(PERSON_RE), &name.unwrap_or("").to_uppercase(), "").to_string();
    rs!(r"\W+").split(&strip_accents(&n)).filter(|t| t.chars().count() > 2 && !TITLES.contains(*t)).map(String::from).collect()
}
/// Does an account name look like a holder (person) rather than a product label?
pub fn holder_name(name: Option<&str>, cash_type: Option<&str>) -> bool {
    let up = name.unwrap_or("").to_uppercase(); let up = up.trim();
    if up.is_empty() || cash_type.unwrap_or("").eq_ignore_ascii_case("CARD") { return false; }
    let toks: Vec<&str> = rs!(r"\W+").split(up).collect();
    if toks.iter().any(|t| PRODUCT_WORDS.contains(*t)) { return false; }
    is(re!(PERSON_RE), &format!("{up} ")) || (2..=4).contains(&toks.iter().filter(|t| !t.is_empty()).count())
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct MemberYaml { #[serde(default)] pub id: String, #[serde(default)] pub name: Option<String>, #[serde(default)] pub full_name: Option<String>, #[serde(default)] pub role: Option<String>, #[serde(default)] pub aliases: Option<Vec<serde_yaml::Value>> }
/// household.yaml as a mapping; anything unreadable or malformed is an empty household (a bad hand edit never stops normalize).
pub fn household_yaml(memory_dir: Option<&Path>) -> serde_yaml::Mapping {
    memory_dir.and_then(|d| std::fs::read_to_string(d.join("household.yaml")).ok()).and_then(|t| serde_yaml::from_str::<serde_yaml::Value>(&t).ok()).and_then(|v| v.as_mapping().cloned()).unwrap_or_default()
}
pub fn members_yaml(memory_dir: Option<&Path>) -> Vec<MemberYaml> {
    household_yaml(memory_dir).get("members").and_then(|m| m.as_sequence()).map(|s| s.iter().filter_map(|m| serde_yaml::from_value(m.clone()).ok()).collect()).unwrap_or_default()
}
pub fn load_members(memory_dir: Option<&Path>) -> Vec<String> { members_yaml(memory_dir).into_iter().flat_map(|m| [m.name, m.full_name]).flatten().filter(|s| !s.is_empty()).collect() }
pub fn load_member_aliases(memory_dir: Option<&Path>) -> Vec<String> { members_yaml(memory_dir).into_iter().flat_map(|m| m.aliases.unwrap_or_default()).filter_map(|a| match a { serde_yaml::Value::String(s) => Some(s), serde_yaml::Value::Number(n) => Some(n.to_string()), _ => None }).filter(|s| !s.is_empty()).collect() }

pub fn account_holder_tokens(name: Option<&str>, owner: Option<&str>, cash_type: Option<&str>) -> Toks {
    if let Some(o) = owner.filter(|o| !o.trim().eq_ignore_ascii_case("joint")) {
        let mut t = name_tokens(Some(o));
        if t.len() < 2 && holder_name(name, cash_type) { let nt = name_tokens(name); if !t.is_empty() && t.is_subset(&nt) { t = nt; } }
        return t;
    }
    if holder_name(name, cash_type) { name_tokens(name) } else { Toks::new() }
}
pub fn candidate_holders(con: &Connection, members: &[String]) -> Vec<Toks> {
    let mut out: Vec<Toks> = vec![];
    let mut add = |t: Toks| if !t.is_empty() && !out.contains(&t) { out.push(t); };
    if let Ok(mut st) = con.prepare("SELECT name, owner, cash_account_type FROM accounts") {
        let rows: Vec<(Option<String>, Option<String>, Option<String>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|i| i.flatten().collect()).unwrap_or_default();
        for (n, o, c) in rows { add(account_holder_tokens(n.as_deref(), o.as_deref(), c.as_deref())); }
    }
    for m in members { add(name_tokens(Some(m))); }
    out
}
pub fn bank_token(bank: Option<&str>) -> Option<String> { rs!(r"\W+").split(&strip_accents(bank.unwrap_or("")).to_uppercase()).find(|t| t.chars().count() >= 3 && !GENERIC_BANK_WORDS.contains(*t)).map(String::from) }

pub fn household(con: &Connection, memory_dir: Option<&Path>) -> Household {
    let members = load_members(memory_dir);
    let cands = candidate_holders(con, &[members.clone(), load_member_aliases(memory_dir)].concat());
    let mut first = Toks::new();
    let rows: Vec<(Option<String>, Option<String>)> = con.prepare("SELECT name, owner FROM accounts").and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|i| i.flatten().collect())).unwrap_or_default();
    for (n, o) in rows { for x in [n, o].into_iter().flatten() {
        let s = strip_accents(&sub(re!(PERSON_RE), &x.to_uppercase(), ""));
        let toks: Vec<&str> = rs!(r"\W+").split(&s).filter(|t| t.chars().count() > 2).collect();
        if !toks.is_empty() && holder_name(Some(&x), None) { first.insert(toks[0].into()); }
    } }
    for m in &members { if let Some(t) = rs!(r"\W+").split(&strip_accents(&m.to_uppercase())).find(|t| !t.is_empty()) { first.insert(t.into()); } }
    let banks: Vec<String> = con.prepare("SELECT DISTINCT bank FROM accounts WHERE bank IS NOT NULL").and_then(|mut s| s.query_map([], |r| r.get(0)).map(|i| i.flatten().collect())).unwrap_or_default();
    let own_banks = banks.into_iter().filter_map(|b| bank_token(Some(&b)).map(|t| (t, b))).collect();
    Household { holders: cands.iter().filter(|h| h.len() >= 2).cloned().collect(), family: cands.iter().flatten().cloned().collect(), first_names: first, own_banks }
}

// ---------------------------------------------------------------- transfer counterparties
const TAGS: &str = "MOTIF|MTF|OBJET|REF|REFERENCE|RI|DE|LIB|LIBELLE|EREF|RUM|ICS|ID|INFO|ORDRE|DATE|BIC|IBAN|NOTPROVIDED|NOM|BEN|ORD";
const COUNTERPARTY_TAGS: [&str; 5] = ["DE", "A", "BEN", "ORD", "NOM"];

/// For text starting with SEPA field tags ('/DE ACME /MOTIF x'): the counterparty ('' if none). None when not tag-structured.
pub fn counterparty_from_tags(t: &str) -> Option<String> {
    if !t.starts_with('/') { return None; }
    let r = re!(r"(?i)/(MOTIF|MTF|OBJET|REF|REFERENCE|RI|DE|LIB|LIBELLE|EREF|RUM|ICS|ID|INFO|ORDRE|DATE|BIC|IBAN|NOTPROVIDED|NOM|BEN|ORD|A)\b:?\s*");
    let caps: Vec<_> = r.captures_iter(t).flatten().map(|c| (g(&c, 1).to_uppercase(), c.get(0).unwrap().start(), c.get(0).unwrap().end())).collect();
    for (i, (tag, _, end)) in caps.iter().enumerate() { if COUNTERPARTY_TAGS.contains(&tag.as_str()) { let stop = caps.get(i + 1).map_or(t.len(), |n| n.1); return Some(t[*end..stop].trim().into()); } }
    let _ = TAGS; Some(String::new())
}
/// Counterparty text without any reason: tag-structured, slashed or bare 'MOTIF ...' tails dropped.
pub fn strip_reason(t: &str) -> String {
    if let Some(x) = counterparty_from_tags(t) { return x; }
    let a = sub(re!(r"(?i)\s*/(?:MOTIF|MTF|OBJET|REF|REFERENCE|RI|DE|LIB|LIBELLE|EREF|RUM|ICS|ID|INFO|ORDRE|DATE|BIC|IBAN|NOTPROVIDED|NOM|BEN|ORD)\b:?.*$"), t, "").to_string();
    sub(re!(r"(?i)\s+(?:(?:MOTIF|MTF|OBJET|LIBELLE)\b|(?:LIB|REF|REFERENCE|RI|EREF|INFO)\b\s*:).*$"), &a, "").trim().into()
}
pub fn strip_vir_prefix(text: &str) -> String {
    let t = sub1(re!(r"(?i)^VIR(?:EMENT)?\b(?:\s+(?:SEPA|INST(?:ANTANE)?|WERO|RECUS?|EMISE?|PERMANENT|OCCASIONNEL|SCT|SDD)\b)*\s*"), text.trim(), "").to_string();
    strip_reason(&t)
}
pub fn lead_tokens(head: &str) -> Vec<String> {
    let mut t: Vec<String> = rs!(r"\W+").split(&strip_accents(head).to_uppercase()).filter(|x| !x.is_empty()).map(String::from).collect();
    while t.first().is_some_and(|x| TITLES.contains(x) || PREFIX_WORDS.contains(x)) { t.remove(0); }
    t
}
/// The counterparty IS one of the holders: all of that holder's tokens among the leading words.
pub fn is_own_name(head: &str, holders: &[Toks]) -> bool {
    let lead = lead_tokens(head);
    holders.iter().any(|h| h.len() >= 2 && { let l: Toks = lead.iter().take(h.len() + 1).cloned().collect(); h.is_subset(&l) })
}
pub fn has_reference_token(t: &[String]) -> bool { t.iter().any(|x| x.chars().any(|c| c.is_numeric()) && x.chars().any(|c| c.is_ascii_uppercase())) }
pub fn family_in_lead(head: &str, family: &Toks) -> bool { let lead: Vec<String> = lead_tokens(head).into_iter().take(4).collect(); lead.iter().any(|t| family.contains(t)) && !has_reference_token(&lead) }

/// Conservative guess that a counterparty is a natural person (person names must never reach an LLM).
pub fn looks_like_person(text: &str, first_names: &Toks) -> bool {
    let t = strip_accents(text).to_uppercase(); let t = t.trim();
    if t.is_empty() || t.chars().any(|c| c.is_numeric()) { return false; }
    if is(re!(PERSON_RE), &format!("{t} ")) || is(re!(r"(?i)^(DR|DOCTEUR|DOTT|DOTTORE|MAITRE|ME|PR|PROF|PROFESSEUR)\b\.?\s"), &format!("{t} ")) { return true; }
    let all: Vec<&str> = rs!(r"[^A-Z'-]+").split(t).filter(|x| !x.is_empty() && !PREFIX_WORDS.contains(*x)).collect();
    // surname particles are dropped before counting; a name with one still needs a known given name or an initial (below)
    let toks: Vec<&str> = all.iter().copied().filter(|x| !NAME_PARTICLES.contains(*x)).collect();
    if !(2..=4).contains(&toks.len()) || toks.iter().any(|x| ORG_WORDS.contains(*x) || CONNECTORS.contains(*x)) { return false; }
    toks.iter().any(|x| x.chars().count() == 1) || toks.iter().any(|x| { let f = x.replace('-', " "); let f = f.split_whitespace().next().unwrap_or(""); FIRST_NAMES.contains(f) || first_names.contains(f) })
}
pub fn names_other_own_bank(head: &str, h: &Household, own_bank: Option<&str>) -> bool {
    let lead = lead_tokens(head);
    let Some(l0) = lead.first() else { return false };
    if !h.own_banks.contains_key(l0) || Some(l0.clone()) == bank_token(own_bank) { return false; }
    lead.len() == 1 || lead[1..].iter().any(|t| h.family.contains(t) || h.first_names.contains(t))
}
pub fn lead_is_p2p(head: &str) -> bool { lead_tokens(head).first().is_some_and(|t| P2P_PREFIXES.contains(t)) }

/// tx_type of a transfer from its counterparty text (prefix already stripped).
pub fn classify_transfer(head: &str, amount: f64, h: &Household, wero: bool, smart: bool, own_bank: Option<&str>) -> &'static str {
    if is_own_name(head, &h.holders) || is(re!(r"(?i)Virement avec|Livret"), head) || (smart && names_other_own_bank(head, h, own_bank)) { return "internal_transfer"; }
    if wero { return if amount > 0.0 { "wero_in" } else { "wero_out" }; }
    if is(re!(PERSON_RE), &format!("{head} ")) || family_in_lead(head, &h.family) || (smart && (looks_like_person(head, &h.first_names) || lead_is_p2p(head))) { return if amount > 0.0 { "person_transfer_in" } else { "person_transfer_out" }; }
    if amount > 0.0 { "transfer_in" } else { "transfer_out" }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn keys() { assert_eq!(merchant_key("SUMUP*CAFE 12 PARIS"), "CAFE PARIS"); assert_eq!(merchant_key("AMAZON*MK12 MKTP"), "AMAZON MKTP"); assert_eq!(merchant_key("CB  Carrefour 75"), "CB CARREFOUR"); }
    #[test] fn own_name() { let h: Toks = ["VAN", "DAMME", "JAN"].map(String::from).into(); assert!(is_own_name("VAN DAMME JAN", &[h])); }
    #[test] fn person() {
        let e = Toks::new();
        for p in ["MARIE PEETERS", "JAN DE SMET", "Lotte Van den Broeck", "DHR WOUTERS", "PAUL M", "Mevr. Claes"] { assert!(looks_like_person(p, &e), "{p}"); }
        for o in ["ACME BV", "CAFE DE LA GARE", "VZW DE KLIMOP", "BAKKERIJ JANSSENS", "STAD GENT"] { assert!(!looks_like_person(o, &e), "{o}"); }
    }
    #[test] fn dates() { assert_eq!(ddmm_date("28/12", "2025-01-03", '/').as_deref(), Some("2024-12-28")); assert_eq!(ddmm_date("31/02", "2025-03-03", '/'), None); }
}
