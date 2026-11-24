//! Every disclaimer and legal label of finch in one place (Belgian sources): the web (`/meta/disclaimers`), the CLI, the MCP tools and
//! the analytics that name a legal consequence import their wording from here, so a legal text is changed once.
use serde_json::{json, Value};

pub const AI_LABEL: &str = "AI-generated content: it can contain mistakes. Check the figures against your accounts.";
pub const AI_LABEL_SHORT: &str = "AI-generated";
pub const CONTRACT: &str = "General summary of consumer-law rules, not legal advice: the contract's own terms and your situation decide. Verify with your contract and the official source (FPS Economy, your provider or insurer) before sending anything.";
pub const CONTRACT_VERIFY: &str = "Verify with your contract and the official source before acting: this is a general summary, not legal advice.";
pub const GENERAL_ADVICE: &str = "This is general information, not financial advice.";
pub const LOAN: &str = "Estimate from the figures you gave, not an offer: your bank or a broker gives the binding numbers. General information, not financial advice; no product is recommended.";
pub const TAX: &str = "General information, not tax advice: rules, rates and ceilings change every year and depend on your situation (and, for some items, your region). Verify on the official site of the FPS Finance (FOD Financiën / SPF Finances: MyMinfin, Tax-on-web) or with an adviser before declaring anything. Nothing is filed by finch.";
pub const TAX_SHORT: &str = "General information, not tax advice.";

/// The labels by key, as the web app reads them (English only for now: `lang` says which language was served).
pub fn texts(lang: &str) -> Value {
    let code = match lang.get(..2).unwrap_or("en").to_lowercase().as_str() { "fr" => "fr", "it" => "it", _ => "en" };
    json!({"lang": code, "texts": {"ai_label": AI_LABEL, "ai_label_short": AI_LABEL_SHORT, "contract": CONTRACT, "contract_verify": CONTRACT_VERIFY, "general_advice": GENERAL_ADVICE, "loan": LOAN, "tax": TAX, "tax_short": TAX_SHORT}})
}
