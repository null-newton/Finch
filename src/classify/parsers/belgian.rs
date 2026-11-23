//! Belgian bank descriptors, Dutch / French / English (KBC, Belfius, BNP Paribas Fortis, ING, Argenta, Crelan, Keytrade, Beobank,
//! bpost bank...): debit-card / Bancontact / contactless payments, ATM withdrawals, SEPA transfers (OVERSCHRIJVING / VIREMENT),
//! standing orders, direct debits (DOMICILIERING / DOMICILIATION) with creditor id and mandate, loan instalments, fees, interest,
//! Payconiq. Structured communications (+++123/4567/89012+++) become the reference.
//! TO VERIFY WITH REAL EXPORTS: written from the commonly published layouts; each bank words its lines slightly differently.
use super::common::*;
use crate::{re, util::*};

const CARD: &str = r"(?i)^(?:BETALING\s+(?:MET\s+)?(?:(?:DE\s+)?DEBETKAART|BANCONTACT|MAESTRO|KAART|VISA(?:\s+DEBIT)?|MASTERCARD|APPLE\s+PAY|GOOGLE\s+PAY)|CONTACTLOZE\s+BETALING|BANCONTACT(?:\s+(?:AANKOOP|BETALING|ACHAT|PAIEMENT))?|AANKOOP\s+(?:BANCONTACT|MET\s+(?:DE\s+)?KAART)|PAIEMENT\s+(?:AVEC\s+LA\s+CARTE(?:\s+DE\s+DEBIT)?|PAR\s+CARTE|BANCONTACT|MAESTRO|CARTE|SANS\s+CONTACT)|ACHAT\s+(?:BANCONTACT|PAR\s+CARTE|CARTE)|CARD\s+PAYMENT|(?:DEBIT\s+)?CARD\s+TRANSACTION|PAYMENT\s+(?:WITH\s+)?(?:DEBIT\s+)?CARD)\b[\s:.-]*(.*)$";
const CARD_REFUND: &str = r"(?i)^(?:TERUGBETALING\s+(?:BANCONTACT|KAART|AANKOOP)|RETOUR\s+(?:BANCONTACT|AANKOOP)|REMBOURSEMENT\s+(?:ACHAT|BANCONTACT|CARTE)|CARD\s+REFUND|REFUND)\b[\s:.-]*(.*)$";
const ATM: &str = r"(?i)^(?:(?:GELDOPNEMING|GELDAFHALING)(?:\s+(?:AAN\s+(?:DE\s+)?)?(?:GELD)?(?:AUTOMAAT|BANCONTACT))?|OPNEMING\s+(?:CONTANT|CASH|AAN\s+(?:DE\s+)?AUTOMAAT|BANCONTACT)|CASH\s+WITHDRAWAL|ATM\s+WITHDRAWAL|RETRAIT\s+(?:D'?ESPECES|D'?ARGENT|BANCONTACT|CASH|AU\s+DISTRIBUTEUR))\b[\s:.-]*(.*)$";
const DD: &str = r"(?i)^(?:(?:EUROPESE\s+|SEPA\s+)?DOMICILIERING|INVORDERING\s+DOMICILIERING|DOMICILIATION(?:\s+(?:EUROPEENNE|SEPA))?|(?:SEPA\s+)?DIRECT\s+DEBIT)\b[\s:.-]*(.*)$";
const LOAN: &str = r"(?i)^(?:(?:AFLOSSING|TERUGBETALING|AFBETALING)\s+(?:HYPOTHECAIRE\s+)?(?:LENING|KREDIET|WOONKREDIET|AUTOLENING|PERSOONLIJKE\s+LENING)|(?:REMBOURSEMENT|ECHEANCE)\s+(?:DE\s+|DU\s+)?(?:PRET|CREDIT)(?:\s+(?:HYPOTHECAIRE|LOGEMENT|AUTO))?|HYPOTHECAIRE\s+LENING|WOONKREDIET|CREDIT\s+HYPOTHECAIRE|LOAN\s+REPAYMENT)\b\s*(.*)$";
const FEE: &str = r"(?i)^(?:BEHEERSKOSTEN|BEHEERSVERGOEDING|AANREKENING\s+(?:KOSTEN|BEHEERSKOSTEN)|KOSTEN\b|JAARLIJKSE\s+BIJDRAGE|BIJDRAGE\s+(?:KAART|REKENING|PAKKET)|DEBETRENTE|DEBETINTEREST|TARIFERING|PRIJS\s+(?:VAN\s+)?(?:UW\s+)?(?:REKENING|PAKKET|KAART)|FRAIS\b|COTISATION\b|TARIFICATION|INTERETS\s+DEBITEURS|REDEVANCE|CARD\s+FEE|ACCOUNT\s+FEE|MONTHLY\s+FEE)";
const INTEREST: &str = r"(?i)^(?:CREDITRENTE|CREDITINTEREST|BASISRENTE|GETROUWHEIDSPREMIE|RENTE\s+SPAARREKENING|INTERETS\s+CREDITEURS|INTERET\s+DE\s+BASE|PRIME\s+DE\s+FIDELITE|INTEREST\s+(?:PAYMENT|CREDIT))\b";
const TRANSFER: &str = r"(?i)^(?P<k>(?:(?:EUROPESE|INSTANT|SEPA)\s+)*OVERSCHRIJVING(?:\s+(?:SEPA|INSTANT|ONTVANGEN|EUROPEES|EUROPESE))*|STORTING|DOORLOPENDE\s+OPDRACHT|(?:VIREMENT|VIR)(?:\s+(?:EUROPEEN|EUROPEENNE|SEPA|INSTANTANE|INSTANTANEE|IMMEDIAT|RECU|EMIS))*|ORDRE\s+PERMANENT|(?:SEPA\s+|INSTANT\s+)?CREDIT\s+TRANSFER|(?:INSTANT\s+)?TRANSFER|INSTANT\s+PAYMENT|WERO)\b[\s:.-]*(?:(?P<dir>NAAR|VAN|VERS|EN\s+FAVEUR\s+DE|EN\s+VOTRE\s+FAVEUR\s+DE|IN\s+UW\s+VOORDEEL\s+VAN|TO|FROM)\b[\s:.-]*)?(?P<who>.*)$";
const PAYCONIQ: &str = r"(?i)^(?:BETALING\s+VIA\s+PAYCONIQ|PAIEMENT\s+(?:VIA\s+)?PAYCONIQ|PAYCONIQ(?:\s+BY\s+BANCONTACT)?)\b[\s:.-]*(.*)$";
/// Everything from these markers on is reference noise, not a name.
const NOISE: &str = r"(?i)\s*\b(?:MEDEDELING|COMMUNICATION|REFERENTIE|REFERENCE|REF|UITVOERINGSDATUM|VALUTADATUM|DATE\s+VALEUR|DATE\s+D'EXECUTION|BOEKINGSDATUM|DATUM|DATE|NUMMER|NUMERO|KAART|CARTE|CARD|BANKREFERENTIE|IBAN|BIC|ADRES|ADRESSE|MANDAAT|MANDAT|IDENTIFICATIE|ID\s+CREANCIER|SCHULDEISER|CREANCIER|WISSELKOERS|TAUX\s+DE\s+CHANGE|KOERS)\b.*$";
pub const STRUCT_COMM: &str = r"(?:\+\+\+|\*\*\*)\s*(\d{3})\s*/\s*(\d{4})\s*/\s*(\d{5})\s*(?:\+\+\+|\*\*\*)";
/// IBANs: spaced groups of digits (BE, FR...), a bank code in letters (NL, GB), or written without spaces. Never eats a following name.
pub const IBAN_ANY: &str = r"\b[A-Z]{2}\d{2}(?:\s?\d{4}){2,7}(?:\s?\d{1,4})?\b|\b[A-Z]{2}\d{2}\s?[A-Z]{4}(?:\s?\d{2,4}){2,6}\b|\b[A-Z]{2}\d{2}[A-Z0-9]{10,30}\b";
const CARD_MASK: &str = r"(?i)(?:\b(?:KAART|CARTE|CARD)\s*)?(?:\b(?:NUMMER|NUMERO|NR|N°)\s*:?\s*)?(?:\b\d{4}[\s-]?[0-9X*]{4}[\s-]?[X*]{4}[\s-]?\d{3,4}\b|\b[X*]{4,}\d{3,4}\b)";
const CREDITOR_ID: &str = r"\b[A-Z]{2}\d{2}ZZZ[A-Z0-9]{6,}\b";
const CREDITOR_NAME: &str = r"(?i)\b(?:SCHULDEISER|CREANCIER|CREDITOR|BEGUNSTIGDE|BENEFICIAIRE)\s*:\s*(.+?)(?=\s+(?:IDENTIFICATIE|ID|IDENT|REFERENTIE|REFERENCE|REF|MANDAAT|MANDAT|MEDEDELING|COMMUNICATION|NUMMER|NUMERO|DATUM|DATE)\b|\s*\||$)";
const MANDATE: &str = r"(?i)\b(?:REFERENTIE\s+MANDAAT|MANDAAT(?:\s*(?:NUMMER|REFERENTIE|NR))?|REF(?:ERENCE)?\s+(?:DU\s+)?MANDAT|MANDAT(?:\s*(?:NUMERO|N°|NR))?|MANDATE(?:\s+REF(?:ERENCE)?)?)\s*[:.]?\s*([A-Z0-9][A-Z0-9/_.-]{3,34})";
const OP_DATE: &str = r"(?i)\b(?:DATUM|DATE|OP|LE)\s*:?\s*(\d{2})[/.-](\d{2})[/.-](\d{2,4})\b";
const OWN_ACCOUNT_WORDS: &str = r"(?i)\b(?:SPAARREKENING|SPAARBOEKJE|ZICHTREKENING|TERMIJNREKENING|EIGEN\s+REKENING|COMPTE\s+D'?EPARGNE|COMPTE\s+A\s+VUE|PROPRE\s+COMPTE|SAVINGS\s+ACCOUNT)\b";

/// "IBAN QXKT": four letters from a hash of the normalised IBAN (merchant keys drop digits, and the number must not travel).
pub fn iban_tag(iban: &str) -> String { let h = sha1_hex(&iban.split_whitespace().collect::<String>().to_uppercase()); format!("IBAN {}", h.bytes().take(4).map(|b| (b'A' + b % 26) as char).collect::<String>()) }
pub fn structured_ref(text: &str) -> Option<String> { cap(re!(STRUCT_COMM), text).map(|c| format!("+++{}/{}/{}+++", g(&c, 1), g(&c, 2), g(&c, 3))) }
fn op_date(text: &str) -> Option<String> {
    let c = cap(re!(OP_DATE), text)?; let y = g(&c, 3); let y: i32 = if y.len() == 2 { 2000 + y.parse::<i32>().ok()? } else { y.parse().ok()? };
    let (dd, mm): (u32, u32) = (g(&c, 1).parse().ok()?, g(&c, 2).parse().ok()?); chrono::NaiveDate::from_ymd_opt(y, mm, dd).map(|x| x.to_string())
}
/// A name from the text after a keyword: IBAN / card masks / structured comms / times removed, cut at the first noise marker.
/// The counterparty named after a transfer keyword ("OVERSCHRIJVING NAAR X ..." -> "X"), or the whole text cleaned.
pub fn transfer_counterparty(text: &str) -> String { cap(re!(TRANSFER), text.trim()).map_or_else(|| clean_name(text), |c| clean_name(gn(&c, "who"))) }
pub fn clean_name(text: &str) -> String {
    let t = sub(re!(STRUCT_COMM), text, " "); let t = sub(re!(IBAN_ANY), &t, " "); let t = sub(re!(CARD_MASK), &t, " ");
    let t = sub(re!(NOISE), &t, ""); let t = sub(re!(r"(?i)\b\d{1,2}[:.h]\d{2}(?:[:.]\d{2})?(?:\s*(?:UUR|U|H)\b)?|\b\d{2}[/.-]\d{2}(?:[/.-]\d{2,4})?\b"), &t, " ");
    collapse(sub(re!(r"^[\s:.,-]+|[\s:.,-]+$"), &collapse(&t), "").as_ref())
}

pub fn parse(tx: &RawTx, h: &Household) -> Parsed {
    let segs = segments(&tx.description, false);
    let desc = collapse(&segs.join(" "));
    let head = collapse(&segs[0]);
    let amount = tx.amount;
    let mut r = parse_inner(tx, h, &segs, &desc, &head, amount);
    if r.reference.is_none() { r.reference = structured_ref(&desc); }
    r
}

fn parse_inner(tx: &RawTx, h: &Household, segs: &[String], desc: &str, head: &str, amount: f64) -> Parsed {
    if let Some(c) = cap(re!(LOAN), head) {
        let m1 = c.get(1).unwrap(); let refs: Vec<&str> = m1.as_str().split_whitespace().filter(|t| t.chars().any(char::is_numeric)).collect();
        let mut p = result("loan_payment", &collapse(&head[..m1.start()]).to_uppercase(), op_date(desc), None); p.reference = (!refs.is_empty()).then(|| refs.join(" ")); return p;
    }
    if let Some(c) = cap(re!(DD), desc) {
        let body = g(&c, 1);
        let name = cap(re!(CREDITOR_NAME), body).map(|n| clean_name(g(&n, 1))).filter(|n| !n.is_empty()).unwrap_or_else(|| clean_name(body));
        let mut p = with_cp(result("direct_debit", &name, None, None), (!name.is_empty()).then(|| name.clone()));
        p.creditor_id = cap(re!(CREDITOR_ID), desc).map(|m| g(&m, 0).into());
        p.mandate_ref = cap(re!(MANDATE), desc).map(|m| g(&m, 1).into()).filter(|m| Some(m) != p.creditor_id.as_ref());
        return p;
    }
    if let Some(c) = cap(re!(CARD_REFUND), desc) { let (rest, fx) = fx_split(g(&c, 1)); return result("card_refund", &clean_name(&rest), op_date(desc), fx); }
    if let Some(c) = cap(re!(ATM), desc) { let n = clean_name(g(&c, 1)); return result("atm", if n.is_empty() { head } else { &n }, op_date(desc), None); }
    if let Some(c) = cap(re!(CARD), desc) { let (rest, fx) = fx_split(g(&c, 1)); return result(if amount <= 0.0 { "card" } else { "card_refund" }, &clean_name(&rest), op_date(desc), fx); }
    if let Some(c) = cap(re!(PAYCONIQ), desc) {
        let who = clean_name(g(&c, 1));
        if looks_like_person(&who, &h.first_names) || family_in_lead(&who, &h.family) { return with_cp(result(if amount > 0.0 { "person_transfer_in" } else { "person_transfer_out" }, &who, None, None), Some(who.clone())); }
        return result(if amount <= 0.0 { "card" } else { "card_refund" }, &who, op_date(desc), None);
    }
    if is(re!(INTEREST), head) && amount > 0.0 { return result("interest", &collapse(head).to_uppercase(), None, None); }
    if is(re!(FEE), head) { return result("bank_fee", &clean_name(head).to_uppercase(), None, None); }
    if let Some(c) = cap(re!(TRANSFER), head) {
        let mut who = clean_name(gn(&c, "who"));
        if who.is_empty() { if let Some(n) = segs.iter().skip(1).map(|s| clean_name(s)).find(|s| !s.is_empty()) { who = n; } }
        // only an IBAN names the other side: a stable letters-only tag of it (never the number) so the payments still group
        if who.is_empty() { if let Some(m) = find(re!(IBAN_ANY), desc) { who = iban_tag(&desc[m.0..m.1]); } }
        let wero = gn(&c, "k").to_uppercase().contains("WERO");
        let ty = if is(re!(OWN_ACCOUNT_WORDS), desc) { "internal_transfer" } else { classify_transfer(&who, amount, h, wero, true, Some(&tx.bank)) };
        return with_cp(result(ty, &who, None, None), (!who.is_empty()).then(|| who.clone()));
    }
    result("other", desc, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(desc: &str, amount: f64) -> Parsed { parse(&RawTx { description: desc.into(), amount, booking_date: "2025-03-14".into(), bank: "KBC".into(), ..Default::default() }, &Household::default()) }
    #[test] fn cards() {
        let r = p("BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 1234 COLRUYT GENT 13/03 18.42 UUR", -45.1); assert_eq!(r.tx_type, "card"); assert_eq!(r.merchant_key, "COLRUYT GENT");
        let r = p("Paiement Bancontact DELHAIZE BRUXELLES DATE : 12/03/2025", -12.0); assert_eq!((r.tx_type.as_str(), r.merchant_raw.as_str(), r.op_date.as_deref()), ("card", "DELHAIZE BRUXELLES", Some("2025-03-12")));
        let r = p("CARD PAYMENT AMAZON EU 12,99 USD", -11.9); assert_eq!(r.fx_currency.as_deref(), Some("USD"));
    }
    #[test] fn transfers() {
        let r = p("EUROPESE OVERSCHRIJVING NAAR BE68 5390 0754 7034 PEETERS MARIE MEDEDELING : verjaardag", -50.0); assert_eq!((r.tx_type.as_str(), r.merchant_raw.as_str()), ("person_transfer_out", "PEETERS MARIE"));
        let r = p("OVERSCHRIJVING VAN ACME BV +++123/4567/89012+++", 2500.0); assert_eq!(r.tx_type, "transfer_in"); assert_eq!(r.merchant_raw, "ACME BV"); assert_eq!(r.reference.as_deref(), Some("+++123/4567/89012+++"));
        let r = p("OVERSCHRIJVING NAAR SPAARREKENING BE12 3456 7890 1234", -200.0); assert_eq!(r.tx_type, "internal_transfer");
        let a = p("OVERSCHRIJVING VAN BE68 5390 0754 7034", 50.0); let b = p("OVERSCHRIJVING VAN BE68539007547034", 60.0);
        assert!(a.merchant_key.starts_with("IBAN ") && a.merchant_key == b.merchant_key && !a.merchant_key.chars().any(|c| c.is_ascii_digit()), "{}", a.merchant_key);
        let r = p("VIREMENT EUROPEEN EN FAVEUR DE JAN DE SMET", -20.0); assert_eq!(r.tx_type, "person_transfer_out");
    }
    #[test] fn others() {
        let r = p("EUROPESE DOMICILIERING SCHULDEISER : PROXIMUS NV IDENTIFICATIE SCHULDEISER : BE12ZZZ0123456789 MANDAAT NUMMER : AB1234567", -60.0);
        assert_eq!((r.tx_type.as_str(), r.merchant_raw.as_str(), r.creditor_id.as_deref(), r.mandate_ref.as_deref()), ("direct_debit", "PROXIMUS NV", Some("BE12ZZZ0123456789"), Some("AB1234567")));
        let r = p("GELDOPNEMING AAN AUTOMAAT KBC LEUVEN", -100.0); assert_eq!((r.tx_type.as_str(), r.merchant_raw.as_str()), ("atm", "KBC LEUVEN"));
        assert_eq!(p("AFLOSSING HYPOTHECAIRE LENING 726-1234567-89", -950.0).tx_type, "loan_payment");
        assert_eq!(p("BEHEERSKOSTEN ZICHTREKENING", -2.5).tx_type, "bank_fee");
        assert_eq!(p("CREDITRENTE SPAARREKENING", 3.2).tx_type, "interest");
        assert_eq!(p("BETALING VIA PAYCONIQ FRITUUR DE MOLEN", -9.0).tx_type, "card");
    }
}
