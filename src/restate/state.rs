//! Reading **virtual object state** across keys, via Restate's SQL surface.
//!
//! Restate exposes a `state` table through the same Datafusion endpoint as
//! `sys_invocation`, with one row per `(service, key, state key)` and the value in both
//! `value_utf8` and raw `value` columns. Predicates, `GROUP BY`, and `LIKE` *inside* a value
//! all evaluate server-side.
//!
//! That capability is why this module exists. The design used to assert that object state was
//! "addressable only by key", which was wrong, and the correction matters: an object can be the
//! single source of truth for the facts it owns, and a panel can read those facts across every
//! key without the object mirroring them into a second store that then has to be kept in step.
//!
//! Two properties of this surface to keep in mind at the call sites:
//!
//! - **Values are SDK-serialized**, which for the scalars the objects store means JSON. A
//!   `u64` arrives as `5`, a `bool` as `true`, a `String` as `"quoted"`. [`unquote`] handles the
//!   last of those so callers don't each strip quotes slightly differently.
//! - **It is an introspection endpoint**, so every read is HTTP to the admin API. Fine for a
//!   panel that repaints every few seconds; not fine on a per-signal hot path.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

use crate::config::RestateConfig;

/// One object's state as a flat map of state key → value, values already unquoted.
pub type ObjectState = BTreeMap<String, String>;

/// Every instance of a service, keyed by the object key.
pub type ServiceState = BTreeMap<String, ObjectState>;

pub struct StateReader {
    admin: String,
    client: reqwest::Client,
}

impl StateReader {
    pub fn new(cfg: &RestateConfig) -> Self {
        Self {
            admin: cfg.admin.clone(),
            client: reqwest::Client::new(),
        }
    }

    /// All state for one service, pivoted from Restate's row-per-key shape into a map per
    /// object key.
    ///
    /// The pivot happens here rather than in SQL because Datafusion has no `PIVOT`, and a
    /// hand-rolled `MAX(CASE WHEN key = …)` per column would hard-code the state keys into the
    /// query — so adding a key to an object would mean editing SQL somewhere else.
    pub async fn service_state(&self, service: &str) -> Result<ServiceState> {
        let rows = self
            .query(&format!(
                "SELECT service_key, key, value_utf8 FROM state \
                 WHERE service_name = '{}' ORDER BY service_key, key",
                escape(service)
            ))
            .await?;

        let mut out: ServiceState = BTreeMap::new();
        for row in rows {
            let Some(obj) = row.get("service_key").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(key) = row.get("key").and_then(|v| v.as_str()) else {
                continue;
            };
            // A non-UTF8 value (an embedding blob, say) has a NULL `value_utf8`. Recorded as
            // present-but-unreadable rather than skipped, so a caller can tell "this object has
            // no such key" from "this key holds bytes you asked for as text".
            let value = row
                .get("value_utf8")
                .and_then(|v| v.as_str())
                .map(unquote)
                .unwrap_or_else(|| "<binary>".to_string());
            out.entry(obj.to_string())
                .or_default()
                .insert(key.to_string(), value);
        }
        Ok(out)
    }

    /// Run a query and return its rows.
    async fn query(&self, sql: &str) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        let url = format!("{}/query", self.admin.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("accept", "application/json")
            .json(&serde_json::json!({ "query": sql }))
            .send()
            .await
            .context("querying Restate object state")?;
        if !resp.status().is_success() {
            let status = resp.status();
            bail!(
                "state query returned {status}: {}",
                resp.text().await.unwrap_or_default()
            );
        }
        let body: serde_json::Value = resp.json().await.context("parsing the state query")?;
        // `{"rows": [...]}` is what the endpoint returns; tolerate a bare array because this is
        // an introspection surface and its envelope is not a promised contract.
        let rows = body
            .get("rows")
            .and_then(|v| v.as_array())
            .or_else(|| body.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(rows
            .into_iter()
            .filter_map(|r| r.as_object().cloned())
            .collect())
    }
}

/// Strip the quotes JSON puts around a serialized string, leaving other scalars alone.
///
/// `ctx.set(k, "abc")` stores `"abc"` — quotes included — while `ctx.set(k, 5u64)` stores `5`.
/// Callers want `abc` and `5`, and doing this once here stops each of them from stripping
/// quotes in a slightly different way.
pub fn unquote(raw: &str) -> String {
    let t = raw.trim();
    match (t.strip_prefix('"'), t.strip_suffix('"')) {
        (Some(_), Some(_)) if t.len() >= 2 => t[1..t.len() - 1].to_string(),
        _ => t.to_string(),
    }
}

/// Escape a value for a single-quoted SQL literal.
///
/// Service and object keys reach this from config and from upstream identities
/// (`owner/repo`, `channel/ts`), so they are not trusted input to string interpolation.
fn escape(v: &str) -> String {
    v.replace('\'', "''")
}

/// Read a state value as a number.
pub fn as_i64(state: &ObjectState, key: &str) -> Option<i64> {
    state.get(key)?.trim().parse().ok()
}

/// Read a state value as a bool. Tolerant of `true`/`1`, because a value's JSON shape depends
/// on how the handler happened to type it.
pub fn as_bool(state: &ObjectState, key: &str) -> Option<bool> {
    match state.get(key)?.trim() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_lose_their_quotes_and_scalars_do_not_change() {
        assert_eq!(unquote("\"2025-03-13T14:41:37Z\""), "2025-03-13T14:41:37Z");
        assert_eq!(unquote("5"), "5");
        assert_eq!(unquote("true"), "true");
        // A lone quote is not a quoted string, and must not lose a character.
        assert_eq!(unquote("\""), "\"");
        assert_eq!(unquote(""), "");
        // Inner quotes survive; only the outermost pair is structural.
        assert_eq!(unquote("\"a\\\"b\""), "a\\\"b");
    }

    #[test]
    fn scalars_parse_and_refuse_what_they_should() {
        let mut s = ObjectState::new();
        s.insert("n".into(), "42".into());
        s.insert("t".into(), "true".into());
        s.insert("f".into(), "0".into());
        s.insert("junk".into(), "banana".into());

        assert_eq!(as_i64(&s, "n"), Some(42));
        assert_eq!(as_i64(&s, "junk"), None);
        assert_eq!(as_i64(&s, "absent"), None);
        assert_eq!(as_bool(&s, "t"), Some(true));
        assert_eq!(as_bool(&s, "f"), Some(false));
        assert!(as_bool(&s, "junk").is_none());
    }

    /// Object keys carry upstream identities and reach the query as SQL literals.
    #[test]
    fn quotes_in_a_key_cannot_break_out_of_the_literal() {
        assert_eq!(escape("o/r"), "o/r");
        assert_eq!(escape("it's"), "it''s");
        assert_eq!(
            escape("x' OR '1'='1"),
            "x'' OR ''1''=''1",
            "an injected predicate must stay inside the literal"
        );
    }
}
