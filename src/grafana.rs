//! Grafana as an evidence source — the numbers behind the alert, not a picture of them.
//!
//! A Slack alert carries almost none of its own evidence. `[FIRING:1] TenantStorageHigh`
//! names a rule and a tenant; the *saturation curve* that made it fire lives in Grafana.
//! [`crate::browser`] already reads that page in the operator's signed-in Chrome, which
//! works on anything SSO can reach — but what it comes back with is a description of a
//! rendered chart, and a number read off a picture cannot be checked.
//!
//! This tier asks Grafana instead. It matters here specifically because these alerts are
//! Grafana's *own* unified-alerting notifications, so the Slack message links the **rule
//! UID** — and from that one key the whole chain is available over HTTP:
//!
//! | | |
//! |---|---|
//! | `GET /api/v1/provisioning/alert-rules/{uid}` | the rule: its queries, its threshold, its `for` |
//! | `GET /api/dashboards/uid/{uid}` | panel definitions, and the queries behind them |
//! | `POST /api/ds/query` | those queries executed over a window — **actual series** |
//!
//! So the model reasons over numbers it can cite and you can re-run, which is the same
//! contract [`crate::prdiff`] and [`crate::persona`] hold themselves to. The check is
//! [`verify`]: a sentence stating a figure that isn't reproducible from the series is
//! marked, not printed as fact.
//!
//! # Read-only, structurally
//!
//! The token is a **Viewer** service account. Silencing an alert, editing a dashboard and
//! saving a panel are not things a Viewer token can do, so read-only here is a property of
//! the credential rather than of an allowlist we remembered to get right — which is
//! strictly stronger than the browser tier's three layers.
//!
//! # What it does not do
//!
//! It does not poll. Grafana raises alerts *into Slack*, and Slack is already ingested, so
//! a Grafana watcher would create a second subject for a firing this system already has —
//! the notification-dedup rule in AGENTS.md, applied one source earlier. Grafana is asked
//! a question when an alert arrives and is silent otherwise.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::debug;

use crate::config::Grafana as GrafanaCfg;

/// How many series one read may reason over. A tenant-labelled query can return hundreds;
/// past a couple of dozen the prompt is mostly labels and the conclusion is mush.
const MAX_SERIES: usize = 24;

/// Points kept per series after downsampling. Enough to show a shape and a breach, few
/// enough that twenty series still fit a prompt.
const MAX_POINTS: usize = 60;

// ---- link parsing ------------------------------------------------------------

/// What a Grafana alert's own links give away.
///
/// A Grafana Slack notification carries three links, and which one you follow decides
/// whether this tier works at all:
///
/// - `/alerting/grafana/{uid}/view` — the **rule**. The most useful of the three, because
///   the rule holds the query and the threshold.
/// - `/d/{uid}/{slug}` — the **dashboard**, usually with `from`/`to` and `var-*` set.
/// - `/alerting/silence/new` — a form for silencing the alert.
///
/// That last one is why this is a parser and not a `contains("grafana")`. Across 25
/// consecutive real alerts every single message carried all three, and the pre-existing
/// "first matching URL wins" would have picked the rule view every time by luck of
/// ordering — one Slack template change from handing a browser agent a silence form.
/// Here the silence link is refused by name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Links {
    pub rule_uid: Option<String>,
    pub dashboard_uid: Option<String>,
    pub panel_id: Option<String>,
    /// Grafana's own range expressions, kept verbatim (`now-6h`, or epoch millis).
    pub from: Option<String>,
    pub to: Option<String>,
    /// `var-environment=env-abc123` → `environment: env-abc123`. This is what says *which
    /// tenant*, and 146 of 164 real alerts carry one.
    pub vars: BTreeMap<String, String>,
    /// The dashboard link, for handing to the browser tier if this one comes up short.
    pub dashboard_url: Option<String>,
}

impl Links {
    /// Is there enough here to ask Grafana anything?
    pub fn actionable(&self) -> bool {
        self.rule_uid.is_some() || self.dashboard_uid.is_some()
    }
}

/// A link that must never be opened or followed, by any tier.
fn is_silence(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("/alerting/silence") || lower.contains("/alerting/silences")
}

/// Pull the query string of a URL into pairs, percent-decoding `%XX` and `+`.
fn query_pairs(url: &str) -> Vec<(String, String)> {
    let Some(q) = url.split_once('?').map(|(_, q)| q) else {
        return Vec::new();
    };
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    // Not an escape after all — a literal `%` in a label value.
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path segment after `marker`, if the path contains it.
fn segment_after(url: &str, marker: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let rest = path.split_once(marker)?.1;
    let seg = rest.split('/').next().unwrap_or("");
    let seg = seg.trim();
    (!seg.is_empty()).then(|| seg.to_string())
}

/// Read every Grafana link in one alert into a single [`Links`].
///
/// Deliberately a fold over all of them rather than a pick of one: the rule UID and the
/// dashboard's time range live in *different* links of the same message, and taking only
/// the first match throws away whichever half came second.
pub fn parse_links<'a>(urls: impl IntoIterator<Item = &'a str>, host_hint: &str) -> Links {
    let mut out = Links::default();
    let hint = host_hint.trim().to_ascii_lowercase();
    for url in urls {
        // Also unescaped at ingest, but every alert stored before that fix still has
        // `&amp;` between its parameters, and those are the alerts on the board now.
        let url = &crate::signal::unescape_html(url);
        let lower = url.to_ascii_lowercase();
        let ours = if hint.is_empty() {
            lower.contains("grafana")
        } else {
            lower.contains(&hint) || lower.contains("grafana")
        };
        if !ours || is_silence(url) {
            continue;
        }
        if let Some(uid) = segment_after(url, "/alerting/grafana/") {
            out.rule_uid.get_or_insert(uid);
        }
        if let Some(uid) = segment_after(url, "/d/") {
            let is_first = out.dashboard_uid.is_none();
            out.dashboard_uid.get_or_insert(uid);
            if is_first {
                out.dashboard_url = Some(url.to_string());
            }
        }
        for (k, v) in query_pairs(url) {
            match k.as_str() {
                "from" => {
                    out.from.get_or_insert(v);
                }
                "to" => {
                    out.to.get_or_insert(v);
                }
                "viewPanel" | "panelId" => {
                    out.panel_id.get_or_insert(v);
                }
                _ if k.starts_with("var-") && !v.is_empty() => {
                    out.vars.insert(k["var-".len()..].to_string(), v);
                }
                _ => {}
            }
        }
    }
    out
}

// ---- the wire types ----------------------------------------------------------

/// One query as Grafana stores it inside a rule or a panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub ref_id: String,
    pub datasource_uid: Option<String>,
    /// The query text — PromQL `expr`, LogQL, or SQL, depending on the datasource.
    pub expr: Option<String>,
    /// The raw model, passed back to `/api/ds/query` untouched. Reconstructing it would
    /// mean knowing every datasource's schema; echoing it means we support all of them.
    pub model: Value,
    /// True for `__expr__` / `-100` nodes: reduce, math and threshold stages. They are the
    /// rule's *logic*, not a series source, and sending them to `/api/ds/query` fails.
    pub is_expression: bool,
}

impl Query {
    fn from_rule_datum(datum: &Value) -> Option<Self> {
        let model = datum.get("model").cloned().unwrap_or_else(|| json!({}));
        let ref_id = datum
            .get("refId")
            .and_then(|v| v.as_str())
            .or_else(|| model.get("refId").and_then(|v| v.as_str()))
            .unwrap_or("A")
            .to_string();
        let ds_uid = datum
            .get("datasourceUid")
            .and_then(|v| v.as_str())
            .or_else(|| {
                model
                    .get("datasource")
                    .and_then(|d| d.get("uid"))
                    .and_then(|v| v.as_str())
            })
            .map(str::to_string);
        let ds_type = model
            .get("datasource")
            .and_then(|d| d.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let is_expression = ds_uid.as_deref() == Some("__expr__")
            || ds_uid.as_deref() == Some("-100")
            || ds_type == "__expr__";
        Some(Self {
            ref_id,
            datasource_uid: ds_uid,
            expr: model
                .get("expr")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            model,
            is_expression,
        })
    }
}

/// An alert rule, reduced to what a conclusion needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub uid: String,
    pub title: String,
    pub queries: Vec<Query>,
    /// The `refId` whose result decides firing.
    pub condition: Option<String>,
    /// Thresholds lifted out of the rule's expression stages — the number the series had
    /// to cross. Without it a conclusion can describe a curve but not say *why it fired*.
    pub thresholds: Vec<f64>,
    /// How long the condition must hold before firing (`5m`).
    pub pending_for: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    /// `__dashboardUid__` / `__panelId__`, which is how a rule points at its own graph.
    pub dashboard_uid: Option<String>,
    pub panel_id: Option<String>,
}

impl Rule {
    fn parse(v: &Value) -> Result<Self> {
        let uid = v
            .get("uid")
            .and_then(|x| x.as_str())
            .context("alert rule has no uid")?
            .to_string();
        let data = v.get("data").and_then(|d| d.as_array());
        let queries: Vec<Query> = data
            .map(|arr| arr.iter().filter_map(Query::from_rule_datum).collect())
            .unwrap_or_default();
        let thresholds = data.map(|arr| thresholds_in(arr)).unwrap_or_default();
        let map = |key: &str| -> BTreeMap<String, String> {
            v.get(key)
                .and_then(|x| x.as_object())
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        };
        let annotations = map("annotations");
        Ok(Self {
            uid,
            title: v
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("(untitled rule)")
                .to_string(),
            queries,
            condition: v
                .get("condition")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            thresholds,
            pending_for: v.get("for").and_then(|x| x.as_str()).map(str::to_string),
            labels: map("labels"),
            dashboard_uid: annotations.get("__dashboardUid__").cloned(),
            panel_id: annotations.get("__panelId__").cloned(),
            annotations,
        })
    }

    /// The queries worth executing: real datasource reads, expression stages dropped.
    pub fn series_queries(&self) -> Vec<&Query> {
        self.queries.iter().filter(|q| !q.is_expression).collect()
    }
}

/// Every `evaluator.params` number in a rule's expression stages, deduplicated in the
/// order Grafana lists them. `gt 0.9` and `lt 0.1` both land here; which direction the
/// comparison ran is in the rule text the model also gets.
fn thresholds_in(data: &[Value]) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for datum in data {
        let Some(conditions) = datum
            .get("model")
            .and_then(|m| m.get("conditions"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for c in conditions {
            let params = c
                .get("evaluator")
                .and_then(|e| e.get("params"))
                .and_then(|p| p.as_array());
            for p in params.into_iter().flatten() {
                if let Some(n) = p.as_f64() {
                    if !out.iter().any(|x| (*x - n).abs() < f64::EPSILON) {
                        out.push(n);
                    }
                }
            }
        }
    }
    out
}

/// One returned time series, summarized. The points are kept for the shape; the stats are
/// what a conclusion is allowed to cite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Series {
    pub ref_id: String,
    /// Prometheus-style labels, e.g. `{environment="env-abc", pod="api-0"}`.
    pub labels: BTreeMap<String, String>,
    pub points: Vec<(i64, f64)>,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub last: f64,
    pub first: f64,
}

impl Series {
    fn summarize(
        ref_id: String,
        labels: BTreeMap<String, String>,
        points: Vec<(i64, f64)>,
    ) -> Self {
        let vals: Vec<f64> = points
            .iter()
            .map(|(_, v)| *v)
            .filter(|v| v.is_finite())
            .collect();
        let n = vals.len().max(1) as f64;
        Self {
            ref_id,
            labels,
            min: vals.iter().copied().fold(f64::INFINITY, f64::min),
            max: vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            mean: vals.iter().sum::<f64>() / n,
            first: vals.first().copied().unwrap_or(f64::NAN),
            last: vals.last().copied().unwrap_or(f64::NAN),
            points,
        }
    }

    /// `{env="x", pod="y"}`, or `(no labels)` for a single unlabelled series.
    pub fn label_str(&self) -> String {
        if self.labels.is_empty() {
            return "(no labels)".into();
        }
        let inner: Vec<String> = self
            .labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect();
        format!("{{{}}}", inner.join(", "))
    }
}

/// Everything one read gathered, before a model sees any of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub rule: Option<Rule>,
    pub series: Vec<Series>,
    /// The window actually queried, as epoch millis.
    pub from_ms: i64,
    pub to_ms: i64,
    /// Series dropped by [`MAX_SERIES`]. Reported rather than silently truncated: a
    /// conclusion drawn from 24 of 300 tenants is a different claim from one drawn from all.
    pub series_omitted: usize,
    /// Why this read could not answer, if it could not.
    pub shortfall: Option<String>,
}

impl Evidence {
    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }
}

// ---- the client --------------------------------------------------------------

pub struct GrafanaClient {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl GrafanaClient {
    pub fn new(base_url: &str, token: String, timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .context("building Grafana HTTP client")?,
            base: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let url = self.url(path);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        Self::json(resp, &url).await
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let url = self.url(path);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        Self::json(resp, &url).await
    }

    /// Status classification is load-bearing: [`crate::restate::workflows`] retries a
    /// transient read and gives up on a terminal one, and it decides which by reading
    /// these strings. `401`/`403` name the fix, because "the Viewer token cannot see this
    /// datasource" is a different job from "the token expired".
    async fn json(resp: reqwest::Response, url: &str) -> Result<Value> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let hint = match status.as_u16() {
                401 => " — the Grafana token is missing, wrong, or revoked",
                403 => {
                    " — the token lacks permission for this (a Viewer service account \
                        still needs access to the folder or datasource)"
                }
                404 => " — no such rule, dashboard, or datasource",
                429 => " — Grafana is rate limiting",
                _ => "",
            };
            bail!(
                "Grafana {} on {url}{hint}: {}",
                status.as_u16(),
                body.chars().take(300).collect::<String>()
            );
        }
        resp.json()
            .await
            .with_context(|| format!("decoding the Grafana response from {url}"))
    }

    /// One alert rule, by the UID the Slack link carries.
    pub async fn rule(&self, uid: &str) -> Result<Rule> {
        let v = self
            .get_json(&format!("/api/v1/provisioning/alert-rules/{uid}"))
            .await?;
        Rule::parse(&v).with_context(|| format!("parsing alert rule {uid}"))
    }

    /// The queries behind a dashboard's panels — all of them, or just one panel's.
    ///
    /// The fallback for an alert whose rule UID we could not read: a dashboard's panels
    /// are asking the same questions the alert is, so their queries are the next best
    /// source of the same numbers.
    pub async fn panel_queries(&self, uid: &str, panel_id: Option<&str>) -> Result<Vec<Query>> {
        let v = self.get_json(&format!("/api/dashboards/uid/{uid}")).await?;
        let panels = v
            .get("dashboard")
            .and_then(|d| d.get("panels"))
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for panel in flatten_panels(&panels) {
            if let Some(want) = panel_id {
                let id = panel.get("id").map(value_to_string).unwrap_or_default();
                if id != want {
                    continue;
                }
            }
            let ds = panel.get("datasource").cloned();
            for target in panel
                .get("targets")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default()
            {
                let mut model = target.clone();
                // A panel target inherits the panel's datasource when it doesn't name one.
                if model.get("datasource").is_none() {
                    if let (Some(obj), Some(ds)) = (model.as_object_mut(), ds.clone()) {
                        obj.insert("datasource".into(), ds);
                    }
                }
                if let Some(q) = Query::from_rule_datum(&json!({ "model": model })) {
                    if !q.is_expression {
                        out.push(q);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Execute queries over a window and return the series.
    ///
    /// `from`/`to` are Grafana range expressions — `now-6h` and epoch-millis strings both
    /// work, which is what lets the alert link's own range be passed straight through.
    pub async fn query(&self, queries: &[&Query], from: &str, to: &str) -> Result<Vec<Series>> {
        if queries.is_empty() {
            return Ok(Vec::new());
        }
        let payload = json!({
            "from": from,
            "to": to,
            "queries": queries.iter().map(|q| {
                let mut model = q.model.clone();
                if let Some(obj) = model.as_object_mut() {
                    obj.insert("refId".into(), json!(q.ref_id));
                    if let Some(uid) = &q.datasource_uid {
                        obj.entry("datasource").or_insert_with(|| json!({ "uid": uid }));
                    }
                    // Bound the work Grafana does per query. Without it a 30-day range on a
                    // 15s-resolution series is 170k points per tenant, which is a slow query
                    // and then a discarded one.
                    obj.entry("maxDataPoints").or_insert(json!(MAX_POINTS * 4));
                    obj.entry("intervalMs").or_insert(json!(60_000));
                }
                model
            }).collect::<Vec<_>>(),
        });
        let v = self.post_json("/api/ds/query", payload).await?;
        Ok(frames_to_series(&v))
    }
}

/// Grafana nests panels inside `row` panels, so a flat read of `dashboard.panels` misses
/// every panel in a collapsed row — which is where the interesting ones tend to be filed.
fn flatten_panels(panels: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for p in panels {
        if let Some(inner) = p.get("panels").and_then(|x| x.as_array()) {
            out.extend(flatten_panels(inner));
        }
        if p.get("targets").is_some() {
            out.push(p.clone());
        }
    }
    out
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Turn `/api/ds/query`'s frame format into series.
///
/// The response is `results.{refId}.frames[].{schema.fields[], data.values[][]}`: values
/// is column-major, and by convention field 0 is time and the last numeric field is the
/// value. Labels live on the value field's `labels`, which is what distinguishes one
/// tenant's line from another's.
fn frames_to_series(v: &Value) -> Vec<Series> {
    let mut out = Vec::new();
    let Some(results) = v.get("results").and_then(|r| r.as_object()) else {
        return out;
    };
    for (ref_id, result) in results {
        let frames = result
            .get("frames")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default();
        for frame in frames {
            let fields = frame
                .get("schema")
                .and_then(|s| s.get("fields"))
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default();
            let columns = frame
                .get("data")
                .and_then(|d| d.get("values"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if fields.len() < 2 || columns.len() < 2 {
                continue;
            }
            let value_idx = fields.len() - 1;
            let labels = fields
                .get(value_idx)
                .and_then(|f| f.get("labels"))
                .and_then(|l| l.as_object())
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let times = columns[0].as_array().cloned().unwrap_or_default();
            let values = columns[value_idx].as_array().cloned().unwrap_or_default();
            let mut points: Vec<(i64, f64)> = times
                .iter()
                .zip(values.iter())
                .filter_map(|(t, val)| Some((t.as_i64()?, val.as_f64()?)))
                .filter(|(_, v)| v.is_finite())
                .collect();
            if points.is_empty() {
                continue;
            }
            downsample(&mut points, MAX_POINTS);
            out.push(Series::summarize(ref_id.clone(), labels, points));
        }
    }
    out
}

/// Keep the first, the last, and an even spread between — and never drop the extremes,
/// because the peak is usually the reason anyone is looking.
fn downsample(points: &mut Vec<(i64, f64)>, max: usize) {
    if points.len() <= max || max < 3 {
        return;
    }
    let hi = points
        .iter()
        .enumerate()
        .max_by(|a, b| a.1 .1.total_cmp(&b.1 .1))
        .map(|(i, _)| i);
    let lo = points
        .iter()
        .enumerate()
        .min_by(|a, b| a.1 .1.total_cmp(&b.1 .1))
        .map(|(i, _)| i);
    let last = points.len() - 1;
    let step = (points.len() - 1) as f64 / (max - 1) as f64;
    let mut keep: Vec<usize> = (0..max)
        .map(|i| (i as f64 * step).round() as usize)
        .collect();
    keep.extend([0, last]);
    keep.extend(hi);
    keep.extend(lo);
    keep.sort_unstable();
    keep.dedup();
    *points = keep
        .into_iter()
        .filter_map(|i| points.get(i).copied())
        .collect();
}

// ---- rendering and verification ---------------------------------------------

/// Format a number the way the prompt and the verifier both must: three significant
/// figures, no exponent, no trailing zeros. One function so that a figure the model copies
/// out of the prompt is byte-identical to what [`verify`] looks for — two formatters would
/// make correct conclusions unverifiable, which is worse than not checking at all.
pub fn num(v: f64) -> String {
    if !v.is_finite() {
        return "n/a".into();
    }
    if v == 0.0 {
        return "0".into();
    }
    let mag = v.abs();
    let decimals = if mag >= 100.0 {
        0
    } else if mag >= 10.0 {
        1
    } else if mag >= 1.0 {
        2
    } else {
        // Small values are the norm for ratios and error rates, where the significant
        // digits are all after the point.
        (2 - mag.log10().floor() as i32).clamp(2, 6) as usize
    };
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    };
    s
}

/// The evidence as the model sees it: the rule, the window, and every series' shape and
/// extremes. Deliberately numbers rather than a rendered chart — the whole reason this
/// tier exists is that a figure quoted from here can be checked against Grafana again.
pub fn render_for_prompt(ev: &Evidence) -> String {
    let mut out = String::new();
    if let Some(rule) = &ev.rule {
        out.push_str(&format!("Alert rule: {}\n", rule.title));
        if let Some(f) = &rule.pending_for {
            out.push_str(&format!("Must hold for: {f}\n"));
        }
        if !rule.thresholds.is_empty() {
            let t: Vec<String> = rule.thresholds.iter().map(|v| num(*v)).collect();
            out.push_str(&format!("Threshold(s): {}\n", t.join(", ")));
        }
        for q in rule.series_queries() {
            if let Some(expr) = &q.expr {
                out.push_str(&format!("Query {}: {expr}\n", q.ref_id));
            }
        }
        if !rule.annotations.is_empty() {
            for (k, v) in &rule.annotations {
                if !k.starts_with("__") {
                    out.push_str(&format!("Annotation {k}: {v}\n"));
                }
            }
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "Window queried: {} to {} (epoch ms)\n\n",
        ev.from_ms, ev.to_ms
    ));
    if ev.series.is_empty() {
        out.push_str("No series were returned for this window.\n");
        return out;
    }
    out.push_str(&format!("{} series:\n", ev.series.len()));
    for (i, s) in ev.series.iter().enumerate() {
        out.push_str(&format!(
            "\n[s{}] {} {}\n  min {}  max {}  mean {}  first {}  last {}\n",
            i + 1,
            s.ref_id,
            s.label_str(),
            num(s.min),
            num(s.max),
            num(s.mean),
            num(s.first),
            num(s.last),
        ));
        let sparse: Vec<String> = s
            .points
            .iter()
            .map(|(t, v)| format!("{t}={}", num(*v)))
            .collect();
        out.push_str(&format!("  points: {}\n", sparse.join(" ")));
    }
    if ev.series_omitted > 0 {
        out.push_str(&format!(
            "\n{} further series were not included — say so if the answer depends on them.\n",
            ev.series_omitted
        ));
    }
    out
}

/// A line of the model's conclusion, and whether its figures are real.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checked {
    pub text: String,
    /// Figures in this line that do not appear anywhere in the evidence.
    pub unverified: Vec<String>,
}

/// Check every figure the model stated against the numbers it was given.
///
/// This is the same bargain [`crate::prdiff`] and [`crate::persona`] make. A model handed
/// twenty series will produce a confident paragraph either way; the difference between
/// evidence and a plausible paragraph about evidence is whether the figures in it can be
/// found again. So a line is kept either way — deleting a possibly-correct conclusion is
/// its own failure — but a figure that isn't reproducible is named, and the caller marks
/// the line rather than presenting it as read.
///
/// Percentages and small integers are exempt: a stated `40%` is usually arithmetic over
/// two figures that *are* present, and `2` is usually a count of something. Flagging those
/// would train the reader to ignore the marks, which costs more than it catches.
pub fn verify(conclusion: &str, ev: &Evidence) -> Vec<Checked> {
    let mut allowed: Vec<String> = Vec::new();
    for s in &ev.series {
        for v in [s.min, s.max, s.mean, s.first, s.last] {
            allowed.push(num(v));
        }
        for (_, v) in &s.points {
            allowed.push(num(*v));
        }
    }
    if let Some(rule) = &ev.rule {
        allowed.extend(rule.thresholds.iter().map(|v| num(*v)));
        // Numbers already written in the rule — a `for: 5m`, a `> 0.9` inside the
        // expression — are things the model is quoting, not deriving.
        allowed.extend(numbers_in(&rule.title));
        if let Some(f) = &rule.pending_for {
            allowed.extend(numbers_in(f));
        }
        for q in &rule.queries {
            if let Some(e) = &q.expr {
                allowed.extend(numbers_in(e));
            }
        }
        for v in rule.annotations.values() {
            allowed.extend(numbers_in(v));
        }
        for v in rule.labels.values() {
            allowed.extend(numbers_in(v));
        }
    }
    allowed.push(ev.from_ms.to_string());
    allowed.push(ev.to_ms.to_string());
    allowed.push(ev.series.len().to_string());

    conclusion
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let unverified: Vec<String> = numbers_in(line)
                .into_iter()
                .filter(|n| !exempt(n, line) && !matches_any(n, &allowed))
                .collect();
            Checked {
                text: line.trim_end().to_string(),
                unverified,
            }
        })
        .collect()
}

/// Numeric literals in a string, sign and decimals kept, thousands separators removed.
fn numbers_in(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == ',') {
                i += 1;
            }
            if i < bytes.len()
                && bytes[i] == '.'
                && bytes.get(i + 1).is_some_and(|c| c.is_ascii_digit())
            {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let lit: String = bytes[start..i].iter().filter(|c| **c != ',').collect();
            if !lit.is_empty() {
                out.push(lit);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Small integers and percentages are not claims about the data.
fn exempt(n: &str, line: &str) -> bool {
    if let Ok(v) = n.parse::<f64>() {
        if v.fract() == 0.0 && v.abs() <= 24.0 {
            return true;
        }
    }
    // `40%`, `40 %`, or `40 percent` — arithmetic over figures that are present.
    let idx = line.find(n).map(|i| i + n.len()).unwrap_or(0);
    let tail = line[idx.min(line.len())..].trim_start();
    tail.starts_with('%') || tail.starts_with("percent")
}

/// Does the stated figure match an allowed one? Exact string first — the model is asked to
/// copy — then a numeric compare with a 1% tolerance, so a figure rounded one digit
/// differently is not reported as fabricated.
fn matches_any(stated: &str, allowed: &[String]) -> bool {
    if allowed.iter().any(|a| a == stated) {
        return true;
    }
    let Ok(want) = stated.parse::<f64>() else {
        return false;
    };
    allowed
        .iter()
        .filter_map(|a| a.parse::<f64>().ok())
        .any(|a| {
            if a == 0.0 && want == 0.0 {
                return true;
            }
            let scale = a.abs().max(want.abs());
            (a - want).abs() <= scale * 0.01
        })
}

/// Fold the checked lines back into Markdown, marking what could not be reproduced.
pub fn render_conclusion(checked: &[Checked]) -> String {
    checked
        .iter()
        .map(|c| {
            if c.unverified.is_empty() {
                c.text.clone()
            } else {
                format!(
                    "{}  _[unverified: {} not in the series]_",
                    c.text,
                    c.unverified.join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- the read ---------------------------------------------------------------

/// The brief. Numbers are in the prompt, so the instruction is to *use* them rather than
/// to describe the picture — and to say what it cannot tell, because "the series does not
/// show why" is a useful answer and an invented cause is not.
pub fn brief(alert: &str, evidence: &str) -> String {
    format!(
        "You are given the actual time series behind one alert. Draw a conclusion.\n\n\
         The alert, as it arrived in Slack:\n{alert}\n\n\
         The evidence, read from Grafana just now:\n{evidence}\n\n\
         Answer as short Markdown, one claim per line, using these headings:\n\
         - **State**: is this firing, recovering, or resolved *according to the series*?\n\
         - **Scope**: which environment, tenant, or pod the affected series identify. Name \
         them from the labels.\n\
         - **Shape**: what the curve does — step, ramp, spike, sawtooth, flat-at-ceiling — \
         and when it changed. Quote the figures.\n\
         - **Threshold**: how far past the threshold it went, and whether it is still past it.\n\
         - **Notable**: any series that behaves differently from the others.\n\
         - **Cannot tell**: what these series do not show.\n\n\
         Rules:\n\
         - Quote figures **exactly** as they appear above. Do not round them, rescale them, \
         or convert units. Every figure you state is checked against the series and marked \
         if it cannot be found.\n\
         - Do not infer a cause that the series does not show. A deploy, a config change, or \
         a customer action is a hypothesis — label it as one.\n\
         - If the series are flat and unremarkable, say so. A resolved alert with a boring \
         curve is a complete answer.\n\
         - Output only the report."
    )
}

/// Reads Grafana for one alert and draws a conclusion from the numbers.
pub struct Reader {
    cfg: GrafanaCfg,
    client: Option<GrafanaClient>,
    reasoner: std::sync::Arc<dyn crate::reasoner::Reasoner>,
}

/// What one read produced.
pub struct Outcome {
    /// The conclusion, with unreproducible figures marked.
    pub conclusion: String,
    pub evidence: Evidence,
    /// True when Grafana could not supply enough to answer, and the browser tier should
    /// be given the same alert.
    pub insufficient: bool,
}

impl Reader {
    pub fn new(
        cfg: GrafanaCfg,
        token: Option<String>,
        reasoner: std::sync::Arc<dyn crate::reasoner::Reasoner>,
    ) -> Self {
        let timeout =
            crate::config::parse_duration(&cfg.timeout).unwrap_or(Duration::from_secs(60));
        let client = match (cfg.enabled, token) {
            (true, Some(t)) if !t.trim().is_empty() && !cfg.base_url.trim().is_empty() => {
                GrafanaClient::new(&cfg.base_url, t, timeout).ok()
            }
            _ => None,
        };
        Self {
            cfg,
            client,
            reasoner,
        }
    }

    /// Configured, credentialled, and pointed at a host.
    pub fn ready(&self) -> bool {
        self.client.is_some()
    }

    pub fn host_hint(&self) -> &str {
        &self.cfg.base_url
    }

    /// Gather the numbers. Split from [`Self::read`] so the gathering is testable without
    /// a model, and so a shortfall is a value rather than an error — "Grafana had nothing
    /// for this window" is a normal outcome that should hand over to the browser, not a
    /// failure that gets retried four times.
    pub async fn gather(&self, links: &Links) -> Result<Evidence> {
        let client = self.client.as_ref().context(
            "Grafana is not configured ([grafana].enabled, base_url, and the \
                      `grafana` secret)",
        )?;
        let (from, to) = self.window(links);
        let mut ev = Evidence {
            rule: None,
            series: Vec::new(),
            from_ms: 0,
            to_ms: 0,
            series_omitted: 0,
            shortfall: None,
        };

        // The rule first: it is the only source of the threshold, and its own annotations
        // point at the dashboard even when the Slack message's link did not.
        if let Some(uid) = &links.rule_uid {
            match client.rule(uid).await {
                Ok(rule) => ev.rule = Some(rule),
                // A rule we cannot read is not fatal — the dashboard path may still answer.
                // Provisioning API access is a common gap on a Viewer token.
                Err(e) => {
                    debug!("grafana: rule {uid} unreadable: {e:#}");
                    ev.shortfall = Some(format!("the alert rule could not be read: {e}"));
                }
            }
        }

        let mut queries: Vec<Query> = ev
            .rule
            .as_ref()
            .map(|r| r.series_queries().into_iter().cloned().collect())
            .unwrap_or_default();

        // Fall back to the dashboard's panels, preferring the panel the rule names over
        // the whole dashboard — an alert is about one graph, not thirty.
        if queries.is_empty() {
            let dash = links
                .dashboard_uid
                .clone()
                .or_else(|| ev.rule.as_ref().and_then(|r| r.dashboard_uid.clone()));
            let panel = links
                .panel_id
                .clone()
                .or_else(|| ev.rule.as_ref().and_then(|r| r.panel_id.clone()));
            if let Some(uid) = dash {
                match client.panel_queries(&uid, panel.as_deref()).await {
                    Ok(q) if !q.is_empty() => queries = q,
                    Ok(_) => {
                        // A named panel that has no queries is worth one more try over the
                        // whole dashboard before giving up on the API path.
                        if panel.is_some() {
                            queries = client.panel_queries(&uid, None).await.unwrap_or_default();
                        }
                    }
                    Err(e) => {
                        debug!("grafana: dashboard {uid} unreadable: {e:#}");
                        ev.shortfall = Some(format!("the dashboard could not be read: {e}"));
                    }
                }
            }
        }

        if queries.is_empty() {
            ev.shortfall = Some(ev.shortfall.unwrap_or_else(|| {
                "the alert links neither a readable rule nor a dashboard with queries".into()
            }));
            return Ok(ev);
        }

        let refs: Vec<&Query> = queries.iter().collect();
        let mut series = client.query(&refs, &from, &to).await?;
        // Biggest excursion first, so what survives the cap is what the alert is about
        // rather than whichever tenant sorted first.
        series.sort_by(|a, b| b.max.abs().total_cmp(&a.max.abs()));
        if series.len() > MAX_SERIES {
            ev.series_omitted = series.len() - MAX_SERIES;
            series.truncate(MAX_SERIES);
        }
        if series.is_empty() {
            ev.shortfall = Some("the queries returned no points for this window".into());
        }
        let (from_ms, to_ms) = series
            .iter()
            .flat_map(|s| s.points.iter().map(|(t, _)| *t))
            .fold((i64::MAX, i64::MIN), |(lo, hi), t| (lo.min(t), hi.max(t)));
        ev.from_ms = if from_ms == i64::MAX { 0 } else { from_ms };
        ev.to_ms = if to_ms == i64::MIN { 0 } else { to_ms };
        ev.series = series;
        Ok(ev)
    }

    /// The window to query.
    ///
    /// The alert's own link wins when it has one — 157 of 164 real alerts carry `from`/`to`,
    /// and that range is the one whoever wrote the rule thought was the right context.
    /// Otherwise a configured lookback, which is what Grafana range expressions are for:
    /// `now-6h` needs no clock here and no clock skew with Grafana's.
    fn window(&self, links: &Links) -> (String, String) {
        let from = links
            .from
            .clone()
            .unwrap_or_else(|| format!("now-{}", self.cfg.lookback.trim()));
        let to = links.to.clone().unwrap_or_else(|| "now".to_string());
        (from, to)
    }

    /// Gather, conclude, and check the conclusion against what was gathered.
    pub async fn read(&self, alert: &str, links: &Links) -> Result<Outcome> {
        let evidence = self.gather(links).await?;
        if evidence.is_empty() {
            let why = evidence
                .shortfall
                .clone()
                .unwrap_or_else(|| "Grafana returned no series".into());
            return Ok(Outcome {
                conclusion: format!("Grafana had nothing to show for this alert: {why}"),
                evidence,
                insufficient: true,
            });
        }
        let rendered = render_for_prompt(&evidence);
        let answer = self
            .reasoner
            .summarize(&brief(alert, &rendered))
            .await
            .context("drawing a conclusion from the Grafana series")?;
        let checked = verify(&answer, &evidence);
        if checked.is_empty() {
            bail!("the model returned no conclusion for the Grafana series");
        }
        Ok(Outcome {
            conclusion: render_conclusion(&checked),
            evidence,
            insufficient: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URLs of one real `TenantPodRescheduled` notification, in the order they appear.
    /// Every Grafana alert observed carried all three of these.
    fn real_alert_urls() -> Vec<&'static str> {
        vec![
            "https://restateprod.grafana.net/alerting/grafana/aeb9f2c1x/view",
            "https://restateprod.grafana.net/alerting/silence/new?alertmanager=grafana&matcher=alertname%3DTenantPodRescheduled",
            "https://restateprod.grafana.net/d/abc123def/tenant-overview?from=1754300000000&to=1754310000000&var-environment=env-9xk2&viewPanel=7",
        ]
    }

    #[test]
    fn the_rule_the_dashboard_and_the_window_all_survive_one_alert() {
        let links = parse_links(real_alert_urls(), "restateprod.grafana.net");
        assert_eq!(links.rule_uid.as_deref(), Some("aeb9f2c1x"));
        assert_eq!(links.dashboard_uid.as_deref(), Some("abc123def"));
        assert_eq!(links.panel_id.as_deref(), Some("7"));
        assert_eq!(links.from.as_deref(), Some("1754300000000"));
        assert_eq!(
            links.vars.get("environment").map(String::as_str),
            Some("env-9xk2")
        );
        assert!(links.actionable());
    }

    /// The reason this is a parser. The old "first URL containing grafana" would have taken
    /// the rule view by luck of ordering; a template that listed silence first would have
    /// handed a browser agent a silence form.
    #[test]
    fn the_silence_link_is_refused_however_it_is_ordered() {
        let mut urls = real_alert_urls();
        urls.reverse();
        let links = parse_links(urls.clone(), "restateprod.grafana.net");
        assert_eq!(links.rule_uid.as_deref(), Some("aeb9f2c1x"));
        assert!(links
            .dashboard_url
            .as_deref()
            .is_some_and(|u| !u.contains("silence")));
        for u in urls {
            if u.contains("silence") {
                assert!(is_silence(u));
            }
        }
    }

    #[test]
    fn a_silence_only_alert_is_not_actionable() {
        let links = parse_links(
            vec!["https://restateprod.grafana.net/alerting/silence/new?x=1"],
            "restateprod.grafana.net",
        );
        assert!(!links.actionable());
        assert!(links.dashboard_url.is_none());
    }

    /// The bug the corpus test found. Slack escapes `&` in message text, so this is the
    /// shape a real dashboard link is *stored* in — and read literally, every parameter
    /// after the first is named `amp;<something>`. Before this was handled the parser
    /// recovered a time range from 11 of 164 alerts; after, from 157.
    ///
    /// It was never only a Grafana problem: the browser tier navigates to this URL, and
    /// Grafana ignoring `amp;to` means the page opens on the dashboard's default window
    /// rather than the one the alert was about.
    #[test]
    fn html_escaped_ampersands_do_not_eat_the_window_and_the_tenant() {
        let stored = "https://restateprod.grafana.net/d/cloud-region/restate-cloud-region-overview\
                      ?orgId=1&amp;var-region=inltao53ge1b4pab3zo23si077&amp;from=now-6h&amp;to=now\
                      &amp;viewPanel=2";
        let links = parse_links(vec![stored], "restateprod.grafana.net");
        assert_eq!(links.dashboard_uid.as_deref(), Some("cloud-region"));
        assert_eq!(links.from.as_deref(), Some("now-6h"));
        assert_eq!(links.to.as_deref(), Some("now"));
        assert_eq!(links.panel_id.as_deref(), Some("2"));
        assert_eq!(
            links.vars.get("region").map(String::as_str),
            Some("inltao53ge1b4pab3zo23si077")
        );
        // And the URL handed onward has no `&amp;` left in it to confuse Grafana.
        assert!(!links.dashboard_url.as_deref().unwrap().contains("&amp;"));
    }

    #[test]
    fn percent_encoded_template_vars_are_decoded() {
        let links = parse_links(
            vec!["https://g.grafana.net/d/u1/s?var-env=env%2Da+b&var-pod=api%2D0"],
            "",
        );
        assert_eq!(links.vars.get("env").map(String::as_str), Some("env-a b"));
        assert_eq!(links.vars.get("pod").map(String::as_str), Some("api-0"));
    }

    /// A literal `%` in a label must not be eaten as a broken escape.
    #[test]
    fn a_trailing_percent_is_kept() {
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("a%zzb"), "a%zzb");
    }

    fn rule_json() -> Value {
        json!({
            "uid": "aeb9f2c1x",
            "title": "TenantStorageHigh",
            "condition": "C",
            "for": "5m",
            "labels": { "severity": "warning" },
            "annotations": {
                "summary": "Tenant storage above 90%",
                "__dashboardUid__": "abc123def",
                "__panelId__": "7"
            },
            "data": [
                {
                    "refId": "A",
                    "datasourceUid": "prom-uid",
                    "model": {
                        "expr": "max by (environment) (tenant_storage_ratio)",
                        "datasource": { "type": "prometheus", "uid": "prom-uid" }
                    }
                },
                {
                    "refId": "B",
                    "datasourceUid": "__expr__",
                    "model": {
                        "type": "reduce",
                        "datasource": { "type": "__expr__", "uid": "__expr__" }
                    }
                },
                {
                    "refId": "C",
                    "datasourceUid": "__expr__",
                    "model": {
                        "type": "threshold",
                        "datasource": { "type": "__expr__", "uid": "__expr__" },
                        "conditions": [
                            { "evaluator": { "type": "gt", "params": [0.9] } }
                        ]
                    }
                }
            ]
        })
    }

    #[test]
    fn a_rule_yields_its_query_its_threshold_and_its_graph() {
        let rule = Rule::parse(&rule_json()).unwrap();
        assert_eq!(rule.title, "TenantStorageHigh");
        assert_eq!(rule.thresholds, vec![0.9]);
        assert_eq!(rule.pending_for.as_deref(), Some("5m"));
        assert_eq!(rule.dashboard_uid.as_deref(), Some("abc123def"));
        assert_eq!(rule.panel_id.as_deref(), Some("7"));
        // The reduce and threshold stages are the rule's logic, not series to fetch.
        let q = rule.series_queries();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].ref_id, "A");
        assert!(q[0]
            .expr
            .as_deref()
            .unwrap()
            .contains("tenant_storage_ratio"));
    }

    /// Sending an expression node to `/api/ds/query` is an error, so the split has to hold
    /// for every way Grafana spells one.
    #[test]
    fn every_spelling_of_an_expression_node_is_excluded() {
        for ds in ["__expr__", "-100"] {
            let q = Query::from_rule_datum(&json!({
                "refId": "X",
                "datasourceUid": ds,
                "model": { "type": "math" }
            }))
            .unwrap();
            assert!(q.is_expression, "{ds} should be an expression");
        }
        let by_type = Query::from_rule_datum(&json!({
            "refId": "X",
            "model": { "datasource": { "type": "__expr__" } }
        }))
        .unwrap();
        assert!(by_type.is_expression);
    }

    fn frames_json() -> Value {
        json!({
            "results": {
                "A": {
                    "frames": [
                        {
                            "schema": {
                                "fields": [
                                    { "name": "Time", "type": "time" },
                                    { "name": "Value", "type": "number",
                                      "labels": { "environment": "env-9xk2" } }
                                ]
                            },
                            "data": { "values": [
                                [1754300000000i64, 1754300060000i64, 1754300120000i64],
                                [0.71, 0.88, 0.94]
                            ]}
                        }
                    ]
                }
            }
        })
    }

    #[test]
    fn frames_become_series_with_labels_and_extremes() {
        let series = frames_to_series(&frames_json());
        assert_eq!(series.len(), 1);
        let s = &series[0];
        assert_eq!(s.ref_id, "A");
        assert_eq!(s.label_str(), "{environment=\"env-9xk2\"}");
        assert_eq!(s.points.len(), 3);
        assert_eq!(num(s.min), "0.71");
        assert_eq!(num(s.max), "0.94");
        assert_eq!(num(s.last), "0.94");
    }

    /// A frame with a null in it must lose the point, not the series — Prometheus returns
    /// nulls for gaps and a staircase with one gap is still the evidence.
    #[test]
    fn null_points_are_dropped_and_the_series_survives() {
        let mut v = frames_json();
        v["results"]["A"]["frames"][0]["data"]["values"][1] = json!([0.71, null, 0.94]);
        let series = frames_to_series(&v);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].points.len(), 2);
    }

    #[test]
    fn a_frame_with_no_points_is_not_a_series() {
        let mut v = frames_json();
        v["results"]["A"]["frames"][0]["data"]["values"] = json!([[], []]);
        assert!(frames_to_series(&v).is_empty());
    }

    /// Panels nested in a collapsed row are exactly the ones an alert tends to point at.
    #[test]
    fn queries_inside_a_collapsed_row_are_found() {
        let panels = vec![json!({
            "type": "row",
            "collapsed": true,
            "panels": [
                { "id": 7, "targets": [ { "expr": "up" } ],
                  "datasource": { "uid": "prom-uid", "type": "prometheus" } }
            ]
        })];
        let flat = flatten_panels(&panels);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0]["id"], 7);
    }

    #[test]
    fn downsampling_keeps_the_ends_and_the_peak() {
        let mut points: Vec<(i64, f64)> = (0..500).map(|i| (i as i64, 1.0)).collect();
        points[321] = (321, 99.0);
        downsample(&mut points, 20);
        assert!(points.len() <= 24, "got {}", points.len());
        assert_eq!(points.first().unwrap().0, 0);
        assert_eq!(points.last().unwrap().0, 499);
        assert!(
            points.iter().any(|(t, _)| *t == 321),
            "the peak is the reason anyone is looking"
        );
    }

    #[test]
    fn small_series_are_left_alone() {
        let mut points: Vec<(i64, f64)> = (0..5).map(|i| (i as i64, i as f64)).collect();
        let before = points.clone();
        downsample(&mut points, 60);
        assert_eq!(points, before);
    }

    #[test]
    fn numbers_render_at_a_readable_precision() {
        assert_eq!(num(0.94), "0.94");
        // Three significant figures, which for a ratio means five decimals.
        assert_eq!(num(0.0012345), "0.00123");
        assert_eq!(num(1234.6), "1235");
        assert_eq!(num(12.34), "12.3");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(f64::NAN), "n/a");
    }

    fn evidence() -> Evidence {
        Evidence {
            rule: Some(Rule::parse(&rule_json()).unwrap()),
            series: frames_to_series(&frames_json()),
            from_ms: 1754300000000,
            to_ms: 1754300120000,
            series_omitted: 0,
            shortfall: None,
        }
    }

    #[test]
    fn a_figure_from_the_series_verifies() {
        let checked = verify("Storage reached 0.94 against a 0.9 threshold.", &evidence());
        assert_eq!(checked.len(), 1);
        assert!(
            checked[0].unverified.is_empty(),
            "0.94 is the max and 0.9 is the threshold: {:?}",
            checked[0].unverified
        );
    }

    /// The failure this whole tier exists to catch: a confident figure that is not in the
    /// data. `2.4` appears nowhere in the series.
    #[test]
    fn an_invented_figure_is_named() {
        let checked = verify("Storage climbed to 2.4 TB across the window.", &evidence());
        assert_eq!(checked[0].unverified, vec!["2.4".to_string()]);
        let rendered = render_conclusion(&checked);
        assert!(rendered.contains("unverified"));
        assert!(
            rendered.contains("Storage climbed"),
            "the line is marked, not deleted"
        );
    }

    #[test]
    fn a_figure_rounded_one_digit_differently_is_not_called_a_fabrication() {
        // 0.94 rendered as 0.940, and 0.88 as 0.879 — both within tolerance.
        let checked = verify("Peaked at 0.940 after sitting at 0.879.", &evidence());
        assert!(
            checked[0].unverified.is_empty(),
            "{:?}",
            checked[0].unverified
        );
    }

    #[test]
    fn percentages_and_counts_are_not_flagged() {
        let checked = verify(
            "Up 33% on the window, across 3 pods, within the 5m pending period.",
            &evidence(),
        );
        assert!(
            checked[0].unverified.is_empty(),
            "{:?}",
            checked[0].unverified
        );
    }

    /// A label value the model quoted back is not a numeric claim about the series.
    #[test]
    fn a_figure_quoted_from_the_rule_verifies() {
        let checked = verify(
            "Rule `TenantStorageHigh` fires above 0.9 held for 5m.",
            &evidence(),
        );
        assert!(
            checked[0].unverified.is_empty(),
            "{:?}",
            checked[0].unverified
        );
    }

    #[test]
    fn each_line_is_checked_on_its_own() {
        let checked = verify(
            "- **State**: firing, last value 0.94.\n- **Shape**: ramped from 999.9.",
            &evidence(),
        );
        assert_eq!(checked.len(), 2);
        assert!(checked[0].unverified.is_empty());
        assert_eq!(checked[1].unverified, vec!["999.9".to_string()]);
    }

    #[test]
    fn the_prompt_carries_the_numbers_and_the_threshold() {
        let rendered = render_for_prompt(&evidence());
        assert!(rendered.contains("TenantStorageHigh"));
        assert!(rendered.contains("Threshold(s): 0.9"));
        assert!(rendered.contains("tenant_storage_ratio"));
        assert!(rendered.contains("environment=\"env-9xk2\""));
        assert!(rendered.contains("max 0.94"));
        // Internal annotations are plumbing, not context.
        assert!(!rendered.contains("__dashboardUid__"));
    }

    /// Truncation has to be visible: a conclusion drawn from 24 of 300 tenants is a
    /// different claim from one drawn from all of them.
    #[test]
    fn omitted_series_are_declared_in_the_prompt() {
        let mut ev = evidence();
        ev.series_omitted = 276;
        assert!(render_for_prompt(&ev).contains("276 further series"));
    }

    #[test]
    fn an_empty_window_says_so_rather_than_looking_like_zero() {
        let mut ev = evidence();
        ev.series.clear();
        assert!(render_for_prompt(&ev).contains("No series were returned"));
        assert!(ev.is_empty());
    }

    #[test]
    fn the_brief_demands_exact_figures_and_forbids_inventing_a_cause() {
        let b = brief("[FIRING:1] TenantStorageHigh", "…");
        assert!(b.contains("exactly"));
        assert!(b.contains("checked against the series"));
        assert!(b.contains("hypothesis"));
        assert!(b.contains("Cannot tell"));
    }

    #[test]
    fn an_unconfigured_reader_is_not_ready_and_refuses_to_gather() {
        let cfg = GrafanaCfg::default();
        let r = Reader::new(
            cfg,
            None,
            std::sync::Arc::new(crate::reasoner::MockReasoner::new("")),
        );
        assert!(!r.ready());
        let links = Links {
            rule_uid: Some("x".into()),
            ..Default::default()
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert!(rt.block_on(r.gather(&links)).is_err());
    }

    /// Put a full, realistic conclusion through the real verifier and print the marked
    /// rendering. Used to build the UI fixture from the shipped code path rather than by
    /// hand-writing what the marker "should" look like — a fixture written by hand is a
    /// fixture that agrees with whatever I believed, which is the thing under test.
    ///
    /// Run with: `cargo test renders_a_verified_conclusion -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn renders_a_verified_conclusion() {
        let ev = Evidence {
            rule: Some(
                Rule::parse(&json!({
                    "uid": "bdm9u17zp4g74c", "title": "TenantStorageHigh", "for": "5m",
                    "condition": "C",
                    "annotations": { "summary": "Tenant storage above 90% of its quota" },
                    "data": [
                        { "refId": "A", "datasourceUid": "prom",
                          "model": { "expr": "max by (environment, pod) (tenant_storage_ratio)" } },
                        { "refId": "C", "datasourceUid": "__expr__", "model": { "type": "threshold",
                          "conditions": [ { "evaluator": { "type": "gt", "params": [0.9] } } ] } }
                    ]
                }))
                .unwrap(),
            ),
            series: frames_to_series(&json!({ "results": { "A": { "frames": [
                frame(&["environment", "env-inlpzvbaa", "pod", "restate-0"],
                      &[0.61, 0.68, 0.74, 0.81, 0.88, 0.93, 0.96, 0.94, 0.71]),
                frame(&["environment", "env-inlpzvbaa", "pod", "restate-1"],
                      &[0.44, 0.45, 0.46, 0.44, 0.45, 0.47, 0.46, 0.45, 0.44]),
                frame(&["environment", "env-9xk2ppq", "pod", "restate-0"],
                      &[0.52, 0.51, 0.53, 0.52, 0.54, 0.53, 0.52, 0.51, 0.52]),
            ] } } })),
            from_ms: 1785790000000,
            to_ms: 1785790480000,
            series_omitted: 4,
            shortfall: None,
        };
        // Note the `1.8 GiB` in "Cannot tell": a plausible-sounding figure that appears
        // nowhere in the series. Everything else is copied from it.
        let answer = "- **State**: firing. `restate-0` in `env-inlpzvbaa` is at 0.94, above the 0.9 threshold, having peaked at 0.96.\n\
             - **Scope**: one pod of one tenant — `environment=env-inlpzvbaa, pod=restate-0`. The sibling `restate-1` never left 0.44–0.47.\n\
             - **Shape**: a steady ramp, not a spike: 0.61 climbing through 0.88 to 0.96, then easing to 0.94.\n\
             - **Threshold**: crossed 0.9 and reached 0.96, and is still past it.\n\
             - **Notable**: `env-9xk2ppq` is flat at 0.52 throughout, so this is not shared-infrastructure pressure.\n\
             - **Cannot tell**: whether this is growth or a stuck compaction; that needs 1.8 GiB of segment-level detail these series do not carry.\n\
             - **Hypothesis**: a workload change on this tenant rather than a leak, since the ramp is monotonic and the sibling is unaffected.";
        let checked = verify(answer, &ev);
        println!("{}", render_conclusion(&checked));
        println!("\n---EVIDENCE---\n{}", serde_json::to_string(&ev).unwrap());
        let flagged: Vec<&Checked> = checked
            .iter()
            .filter(|c| !c.unverified.is_empty())
            .collect();
        assert_eq!(flagged.len(), 1, "exactly the invented figure: {flagged:?}");
        assert_eq!(flagged[0].unverified, vec!["1.8".to_string()]);
    }

    /// A `/api/ds/query` frame for one labelled series.
    fn frame(label_pairs: &[&str], values: &[f64]) -> Value {
        let labels: serde_json::Map<String, Value> = label_pairs
            .chunks(2)
            .map(|kv| (kv[0].to_string(), json!(kv[1])))
            .collect();
        let times: Vec<i64> = (0..values.len())
            .map(|i| 1785790000000 + i as i64 * 60_000)
            .collect();
        json!({
            "schema": { "fields": [
                { "name": "Time", "type": "time" },
                { "name": "Value", "type": "number", "labels": labels }
            ]},
            "data": { "values": [times, values] }
        })
    }

    /// Run the parser over **every** Grafana alert in the real store.
    ///
    /// The unit tests above assert on one hand-written alert, which is exactly the input a
    /// parser is least likely to be wrong about. This one asserts on the corpus: that the
    /// silence link never leaks into a parsed result, and that the tier would actually have
    /// something to ask about a real alert rather than in principle.
    ///
    /// Ignored because it needs the operator's database. Run with:
    ///   `cargo test grafana_parses_the_real_alert_corpus -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn grafana_parses_the_real_alert_corpus() {
        let path = std::env::var("MUGGLEBOT_DB").unwrap_or_else(|_| "data/mugglebot.sqlite".into());
        let conn = rusqlite::Connection::open(&path).expect("open the store read-only");
        let mut stmt = conn
            .prepare(
                "SELECT title, raw FROM signals WHERE source='slack' \
                 AND lower(COALESCE(raw,'')) LIKE '%grafana%'",
            )
            .unwrap();
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(!rows.is_empty(), "no Grafana alerts in {path}");

        let (mut actionable, mut rule, mut dash, mut win, mut vars, mut panel) = (0, 0, 0, 0, 0, 0);
        let mut inert: Vec<String> = Vec::new();
        for (title, raw) in &rows {
            let parsed: Value = serde_json::from_str(raw).unwrap_or_default();
            let urls: Vec<String> = parsed
                .get("urls")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|u| u.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let links = parse_links(urls.iter().map(String::as_str), "");

            // The invariant. A silence URL must not survive parsing by any route — not as
            // the dashboard link, not anywhere in the payload handed to a workflow.
            let payload = serde_json::to_string(&links).unwrap();
            assert!(
                !payload.contains("silence"),
                "a silence link survived parsing of {title:?}: {payload}"
            );

            if links.actionable() {
                actionable += 1;
            } else {
                inert.push(title.chars().take(60).collect());
            }
            rule += links.rule_uid.is_some() as usize;
            dash += links.dashboard_uid.is_some() as usize;
            win += links.from.is_some() as usize;
            panel += links.panel_id.is_some() as usize;
            vars += !links.vars.is_empty() as usize;
        }
        println!("alerts:          {}", rows.len());
        println!("actionable:      {actionable}");
        println!("  rule uid:      {rule}");
        println!("  dashboard uid: {dash}");
        println!("  panel id:      {panel}");
        println!("  window:        {win}");
        println!("  template vars: {vars}");
        if !inert.is_empty() {
            println!("\nnothing to ask ({}):", inert.len());
            for t in inert.iter().take(10) {
                println!("  {t}");
            }
        }
        // Most of a Grafana alert corpus must be answerable, or the tier is decoration.
        assert!(
            actionable * 2 > rows.len(),
            "only {actionable} of {} alerts gave the tier anything to ask",
            rows.len()
        );
    }

    /// The alert's own range beats the configured lookback: whoever wrote the rule chose
    /// that window, and 63 of 200 real alert URLs carry one.
    #[test]
    fn the_alerts_own_window_wins_and_otherwise_a_lookback_is_used() {
        let r = Reader::new(
            GrafanaCfg::default(),
            None,
            std::sync::Arc::new(crate::reasoner::MockReasoner::new("")),
        );
        let linked = Links {
            from: Some("1754300000000".into()),
            to: Some("1754310000000".into()),
            ..Default::default()
        };
        assert_eq!(
            r.window(&linked),
            ("1754300000000".to_string(), "1754310000000".to_string())
        );
        let bare = Links::default();
        let (from, to) = r.window(&bare);
        assert!(from.starts_with("now-"), "{from}");
        assert_eq!(to, "now");
    }
}
