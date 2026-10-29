//! The token usage dashboard (Settings → Token usage): every model request
//! from the log, summed by day, model, purpose and chat, with the cost from
//! the prices the user set per model. Nothing here is estimated: the token
//! counts are the provider's own, and a model without a price shows its
//! tokens and no cost.

use crate::db::{Db, Price, UsageEntry};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

const DAY_MS: i64 = 86_400_000;

/// Cost of one request at `price` (per 1M tokens); `None` without a price.
pub fn cost(e: &UsageEntry, price: Option<&Price>) -> Option<f64> {
    let p = price?;
    let fresh = e.input.saturating_sub(e.cached) as f64;
    Some((fresh * p.input + e.cached as f64 * p.cached + e.output as f64 * p.output) / 1e6)
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Sum {
    pub requests: u64,
    pub input: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost: f64,
    /// Requests whose model has no price (their cost is not in `cost`).
    pub unpriced: u64,
}

impl Sum {
    fn add(&mut self, e: &UsageEntry, c: Option<f64>) {
        self.requests += 1;
        self.input += e.input;
        self.cached += e.cached;
        self.cache_write += e.cache_write;
        self.output += e.output;
        match c {
            Some(c) => self.cost += c,
            None => self.unpriced += 1,
        }
    }

    fn tokens(&self) -> u64 {
        self.input + self.output
    }
}

/// `YYYY-MM-DD` of a UTC day number (days since 1970-01-01).
pub fn day_label(day: i64) -> String {
    // Howard Hinnant's civil-from-days.
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Groups sorted by cost, then tokens, biggest first.
fn ranked(map: HashMap<String, Sum>) -> Vec<(String, Sum)> {
    let mut v: Vec<_> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cost.total_cmp(&a.1.cost).then(b.1.tokens().cmp(&a.1.tokens())).then(a.0.cmp(&b.0)));
    v
}

/// The dashboard for the last `days` UTC days (today included), or all time.
pub fn dashboard(db: &Db, days: Option<i64>, now_ms: i64) -> Value {
    let today = now_ms.div_euclid(DAY_MS);
    let from_day = days.map(|d| today - (d.max(1) - 1));
    let entries = db.usage_log(from_day.map_or(0, |d| d * DAY_MS)).unwrap_or_default();
    let prices = db.prices().unwrap_or_default();

    let mut total = Sum::default();
    let mut by_day: BTreeMap<i64, Sum> = BTreeMap::new();
    let (mut by_model, mut by_purpose, mut by_chat): (HashMap<String, Sum>, HashMap<String, Sum>, HashMap<String, Sum>) =
        Default::default();
    for e in &entries {
        let c = cost(e, prices.get(&e.model));
        total.add(e, c);
        by_day.entry(e.ts.div_euclid(DAY_MS)).or_default().add(e, c);
        by_model.entry(e.model.clone()).or_default().add(e, c);
        by_purpose.entry(e.purpose.clone()).or_default().add(e, c);
        by_chat.entry(e.session_id.clone()).or_default().add(e, c);
    }
    // Every day of the range, also the quiet ones (a chart with gaps lies).
    let first = from_day.or_else(|| by_day.keys().next().copied()).unwrap_or(today);
    let daily: Vec<Value> = (first.max(today - 365)..=today)
        .map(|d| {
            let s = by_day.get(&d).cloned().unwrap_or_default();
            json!({"day": day_label(d), "sum": s})
        })
        .collect();
    let chats: Vec<Value> = ranked(by_chat)
        .into_iter()
        .take(15)
        .map(|(id, s)| {
            let title = if id.is_empty() {
                "Outside a chat (planning, summaries)".to_string()
            } else {
                db.get_session(&id).ok().flatten().map(|x| x.title).unwrap_or_else(|| "Deleted chat".into())
            };
            json!({"id": id, "title": title, "sum": s})
        })
        .collect();
    let group = |m: HashMap<String, Sum>| -> Vec<Value> {
        ranked(m).into_iter().map(|(k, s)| json!({"name": k, "sum": s})).collect()
    };
    let titles: HashMap<String, String> = entries
        .iter()
        .rev()
        .take(100)
        .filter_map(|e| {
            let t = db.get_session(&e.session_id).ok().flatten()?.title;
            Some((e.session_id.clone(), t))
        })
        .collect();
    let recent: Vec<Value> = entries
        .iter()
        .rev()
        .take(100)
        .map(|e| {
            json!({
                "ts": e.ts,
                "chat": titles.get(&e.session_id).cloned().unwrap_or_default(),
                "model": e.model,
                "purpose": e.purpose,
                "input": e.input,
                "cached": e.cached,
                "output": e.output,
                "cost": cost(e, prices.get(&e.model)),
            })
        })
        .collect();
    // Models to price: every one ever used, and every one with a price.
    let mut models: Vec<String> = db.usage_log(0).unwrap_or_default().into_iter().map(|e| e.model).collect();
    models.extend(prices.keys().cloned());
    models.sort();
    models.dedup();
    let price_rows: Vec<Value> =
        models.iter().map(|m| json!({"model": m, "price": prices.get(m)})).collect();
    // Requests counted before the per-request log existed (no detail).
    let older = db.usage_totals(None, days).map(|t| t.requests.saturating_sub(total.requests)).unwrap_or(0);
    json!({
        "days": days,
        "total": total,
        "daily": daily,
        "models": group(by_model),
        "purposes": group(by_purpose),
        "chats": chats,
        "recent": recent,
        "prices": price_rows,
        "older_requests": older,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Usage;

    #[test]
    fn day_labels_are_calendar_dates() {
        assert_eq!(day_label(0), "1970-01-01");
        assert_eq!(day_label(20_756), "2026-10-30");
        assert_eq!(day_label(11_016), "2000-02-29");
    }

    #[test]
    fn the_dashboard_adds_up_by_model_purpose_and_chat_with_costs() {
        let db = Db::open_in_memory().unwrap();
        let a = db.create_session("Bakery site", None).unwrap();
        let u = |input, cached, output| Usage { input, cached, cache_write: 0, output };
        db.add_usage_entry(&a.id, "gpt-6-sol", "chat", &u(1_000_000, 400_000, 100_000)).unwrap();
        db.add_usage_entry(&a.id, "gpt-6-sol", "final check", &u(200_000, 0, 10_000)).unwrap();
        db.add_usage_entry("", "gpt-6-luna", "spec", &u(5_000, 0, 500)).unwrap();
        db.set_price("gpt-6-sol", Price { input: 2.0, cached: 0.5, output: 8.0 }).unwrap();

        let d = dashboard(&db, Some(7), crate::db::now_ms_for_tests());
        assert_eq!(d["total"]["requests"], 3);
        assert_eq!(d["total"]["input"], 1_205_000);
        assert_eq!(d["total"]["cached"], 400_000);
        // sol: 600K fresh × $2 + 400K cached × $0.5 + 100K out × $8 = 1.2 + 0.2 + 0.8;
        // then 200K × $2 + 10K × $8 = 0.4 + 0.08. luna has no price.
        let total = d["total"]["cost"].as_f64().unwrap();
        assert!((total - 2.68).abs() < 1e-9, "{total}");
        assert_eq!(d["total"]["unpriced"], 1, "luna's request has no price");
        assert_eq!(d["models"][0]["name"], "gpt-6-sol", "the costliest model first");
        let purposes: Vec<&str> = d["purposes"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert_eq!(purposes, ["chat", "final check", "spec"]);
        assert_eq!(d["chats"][0]["title"], "Bakery site");
        assert_eq!(d["chats"][1]["title"], "Outside a chat (planning, summaries)");
        assert_eq!(d["daily"].as_array().unwrap().len(), 7, "every day of the range, quiet ones too");
        assert_eq!(d["recent"][0]["purpose"], "spec", "newest request first");
        assert_eq!(d["older_requests"], 0);
        let priced: Vec<&str> = d["prices"].as_array().unwrap().iter().map(|p| p["model"].as_str().unwrap()).collect();
        assert_eq!(priced, ["gpt-6-luna", "gpt-6-sol"], "every model used can be priced");
    }
}
