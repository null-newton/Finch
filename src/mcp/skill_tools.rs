//! MCP tools of the coach's skills (E7): the savings calculator, the monthly review, a spike explained, the subscription audit,
//! the onboarding checklist and why a transaction has its category. Numbers come from code; refs and pseudonyms only.
use crate::{subs_extra::saving_of, util::*};
use chrono::NaiveDate;
use serde_json::{json, Value};

pub const MARKET_MAX_AGE_DAYS: i64 = 30;
const SAVINGS_DISC: &str = "Estimate from the prices you gave, not a quote: the provider's offer decides. General information, not financial advice.";

/// Staleness of a dated market figure (a price, a rate): older than 30 days is not presented as current.
pub fn age(label: &str, day: Option<&str>, today: NaiveDate) -> Result<Value, String> {
    let Some(s) = day else { return Ok(json!({"dated": false, "warning": format!("the date of the {label} was not given: state its source and date, and treat it as possibly outdated")})) };
    let x = d(s).ok_or(format!("{s:?} is not a date (YYYY-MM-DD)"))?; let n = (today - x).num_days(); let mut o = json!({"dated": true, "date": x.to_string(), "age_days": n});
    if n > MARKET_MAX_AGE_DAYS { o["warning"] = format!("the {label} is {n} days old (more than {MARKET_MAX_AGE_DAYS}): do not present it as current").into(); } else if n < 0 { o["warning"] = format!("the date of the {label} is in the future").into(); }
    Ok(o)
}
/// What switching a recurring service to a cheaper offer saves, net of the switching costs (a pure calculator over the given prices).
pub fn savings_estimate(a: &Value, today: NaiveDate) -> Result<Value, String> {
    let c = |k: &str| to_cents(a[k].as_f64().unwrap_or(0.0)); let (cur, alt, sw, months) = (c("current_monthly"), c("alternative_monthly"), c("switching_costs"), a["months"].as_i64().unwrap_or(12));
    let (m, net, be) = saving_of(cur, alt, sw, months); let mut notes: Vec<String> = vec![];
    let verdict = if m <= 0 { notes.push("the alternative is not cheaper than the current price".into()); "no_saving" } else if net <= 0 { notes.push(format!("the switching costs are paid back after {} month(s); the period asked is {months}", be.unwrap_or(0))); "costs_exceed_saving_over_the_period" } else { "saving" };
    if sw > 0 && be.is_some_and(|b| b > months) { notes.push("break-even is later than the horizon: a longer horizon would turn the result positive".into()); }
    let quote = age("alternative's price", a["quote_date"].as_str(), today)?; if let Some(w) = quote["warning"].as_str() { notes.push(w.into()); }
    notes.push("a promotional first-year price is not modelled: give the full price, or call the tool twice (promo months, then full price)".into());
    Ok(json!({"months": months, "current_monthly": money_str(cur), "alternative_monthly": money_str(alt), "switching_costs": money_str(sw), "monthly_saving": money_str(m), "yearly_saving": money_str(m * 12), "gross_saving": money_str(m * months),
        "net_saving": money_str(net), "break_even_months": be, "verdict": verdict, "quote": quote, "notes": notes, "computed_by": "finch", "disclaimer": SAVINGS_DISC}))
}
