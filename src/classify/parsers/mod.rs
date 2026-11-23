//! Per-bank descriptor parsers. Every parser returns the same [`Parsed`] fields; the registry maps an account's bank to a
//! parser. Belgian banks (and unknown ones) use the Belgian NL / FR / EN parser; Revolut has its own and falls back to it.
//! To add a bank: write `parsers/<bank>.rs` with `parse`, add it to [`parser_name_for`] / [`parse_tx`], add fixtures.
pub mod belgian;
pub mod common;
pub mod revolut;

use crate::{re, util::*};
pub use common::{Household, Parsed, RawTx};

/// The parser for an account's bank. Belgian banks are recognised by name (KBC / CBC, Belfius, BNP Paribas Fortis / Hello bank!,
/// ING, Argenta, Crelan, AXA, Keytrade, Beobank, bpost bank, Triodos, vdk, Nagelmackers, Europabank...) or a BE IBAN.
pub fn parser_name_for(bank: Option<&str>, iban: Option<&str>, country: Option<&str>) -> &'static str {
    let b = strip_accents(&bank.unwrap_or("").to_lowercase());
    if is(re!(r"\brevolut\b"), &b) { return "revolut"; }
    let _ = (iban, country); "belgian"
}
/// Which Belgian bank (for the record in tx_parse_meta.parser); "belgian" when unknown.
pub fn bank_label(bank: Option<&str>) -> &'static str {
    let b = strip_accents(&bank.unwrap_or("").to_lowercase());
    for (p, n) in [(r"\b(kbc|cbc)\b", "kbc"), (r"\bbelfius\b", "belfius"), (r"\bbnp\b|\bfortis\b|hello ?bank", "bnp_fortis"), (r"\bing\b", "ing"), (r"\bargenta\b", "argenta"), (r"\bcrelan\b|\baxa bank\b", "crelan"),
        (r"\bkeytrade\b", "keytrade"), (r"\bbeobank\b", "beobank"), (r"\bbpost\b", "bpost"), (r"\btriodos\b", "triodos"), (r"\bvdk\b", "vdk"), (r"\bnagelmackers\b", "nagelmackers"), (r"\beuropabank\b", "europabank")] {
        if fancy_regex::Regex::new(p).map(|r| is(&r, &b)).unwrap_or(false) { return n; }
    }
    "belgian"
}
/// Parse with the bank's parser (Revolut falls back to the Belgian one for lines it does not know).
pub fn parse_tx(tx: &RawTx, h: &Household, bank: Option<&str>, iban: Option<&str>, country: Option<&str>) -> Parsed {
    let name = parser_name_for(bank, iban, country);
    let mut r = if name == "revolut" { revolut::parse(tx, h) } else { belgian::parse(tx, h) };
    r.parser = if name == "revolut" { "revolut".into() } else { format!("belgian:{}", bank_label(bank)) }; r
}
