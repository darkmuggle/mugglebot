//! incident.io — the API client.
//!
//! Only what the board needs: list incidents, and know which of them are still burning.
//!
//! # Which are open
//!
//! incident.io models the lifecycle as a *status category*: `triage`, `active` and
//! `post-incident` are live, and `closed`, `declined`, `merged`, `canceled` and `paused` are
//! not. That vocabulary lives in [`is_open`], because it is the one fact the whole feature
//! turns on — "all open incidents tracked, resolved ones removed" is exactly this predicate,
//! and it should be readable in one place rather than inferred from a query string.
//!
//! Filtering happens **client-side**, deliberately. Asking the API to filter would be one
//! fewer page to read, but the filter-parameter syntax is a detail of their API that this
//! code would then depend on being right about — and getting it subtly wrong fails in the
//! worst way available: a query that returns 200 with a plausible-looking subset, so the
//! board quietly tracks some incidents and not others. Reading pages and applying [`is_open`]
//! locally is a handful of requests against an org with 148 incidents in total, and it is
//! obvious from the code what is included.

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;

const API: &str = "https://api.incident.io/v2";

/// Incidents per page. incident.io caps this at 50.
const PAGE_SIZE: usize = 50;

/// Pages read in one poll.
///
/// Open incidents are a small set — usually zero to a handful — and the listing is newest
/// first, so they are at the front. This bounds a poll against an org with years of history
/// rather than trying to enumerate all of it: a poll is a mirror of what is burning now, and
/// an incident older than 500 entries that is somehow still open is not a case worth paging
/// the whole archive for on a 60-second cadence.
const MAX_PAGES: usize = 10;

/// The status categories that mean "still happening".
///
/// `post-incident` counts: the outage is over but the work — the review, the follow-ups — is
/// not, and that is precisely the window in which mapping the incident to code is most
/// useful. `paused` deliberately does not: somebody has said "not now".
pub fn is_open(category: &str) -> bool {
    matches!(category, "triage" | "active" | "post_incident" | "post-incident")
}

/// One incident, reduced to what the board and the analysis need.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Incident {
    /// incident.io's ULID. Carried for API calls; not the subject key.
    pub id: String,
    /// `INC-448` — the human reference, and the subject key's identity.
    pub reference: String,
    pub name: String,
    pub summary: Option<String>,
    pub permalink: Option<String>,
    /// `triage` | `active` | `post_incident` | `closed` | …
    pub status_category: String,
    pub status_name: String,
    /// `Minor`, `Major`, … as the org has configured them. A name, not a number: the
    /// severity *scale* is per-org, so mapping it to our own [`crate::signal::Severity`]
    /// is done by name in the watcher where the mapping is visible.
    pub severity: Option<String>,
    pub reported_at: Option<String>,
    pub updated_at: Option<String>,
    /// Named timestamps (`Resolved at`, `Identified at`, …), flattened.
    pub timestamps: Vec<(String, String)>,
}

impl Incident {
    pub fn is_open(&self) -> bool {
        is_open(&self.status_category)
    }

    /// The text the code-mapping engine scores against.
    ///
    /// Name first, then summary. The name is the alert that fired — `TenantPodOOMKillLoop`,
    /// `ControlPlaneCPUCritical` — which is the densest symptom text available and exactly
    /// what `score::symptom_terms` is built to split.
    pub fn symptom_text(&self) -> String {
        match self.summary.as_deref().map(str::trim).filter(|s| {
            // incident.io writes the literal string "not set" for an absent summary, which
            // would otherwise be scored as if it were content.
            !s.is_empty() && *s != "not set"
        }) {
            Some(summary) => format!("{}\n\n{summary}", self.name),
            None => self.name.clone(),
        }
    }
}

pub struct IncidentClient {
    client: reqwest::Client,
    headers: HeaderMap,
    base: String,
}

impl IncidentClient {
    pub fn new(api_key: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", api_key.trim()))
            .context("the incident.io API key is not a valid header value")?;
        // The key is a bearer token; keep it out of any `{:?}` of the header map.
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent("mugglebot")
                .build()
                .context("building HTTP client")?,
            headers,
            base: API.to_string(),
        })
    }

    /// Point at a different host — for tests against a local server.
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// Every incident that is still open, newest first.
    pub async fn open_incidents(&self) -> Result<Vec<Incident>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let (page, next) = self.page(after.as_deref()).await?;
            let short = page.len() < PAGE_SIZE;
            out.extend(page.into_iter().filter(Incident::is_open));
            match next {
                Some(cursor) if !short => after = Some(cursor),
                _ => break,
            }
        }
        Ok(out)
    }

    /// One page, plus the cursor for the next.
    async fn page(&self, after: Option<&str>) -> Result<(Vec<Incident>, Option<String>)> {
        let mut url = format!("{}/incidents?page_size={PAGE_SIZE}", self.base);
        if let Some(after) = after {
            url.push_str(&format!("&after={after}"));
        }
        let resp = self
            .client
            .get(&url)
            .headers(self.headers.clone())
            .send()
            .await
            .context("listing incident.io incidents")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // The body is echoed because incident.io says *why* — an expired key and a
            // missing scope are both 401 and need different fixes.
            bail!(
                "incident.io returned {status}: {}",
                crate::tools::truncate_for_prompt(&body, 400)
            );
        }
        let page: IncidentsPage = serde_json::from_str(&body)
            .with_context(|| format!("decoding {}", crate::tools::truncate_for_prompt(&body, 200)))?;
        let next = page.pagination_meta.and_then(|p| p.after);
        Ok((page.incidents.into_iter().map(Incident::from).collect(), next))
    }
}

// ---- wire types --------------------------------------------------------------
//
// Every field is optional except the two that identify the incident. incident.io returns a
// wide object with org-configurable parts, and a required field this code does not truly
// need is a field that can fail a whole page — which is the failure mode that left a repo's
// commit index permanently empty elsewhere in this codebase.

#[derive(Deserialize)]
struct IncidentsPage {
    #[serde(default)]
    incidents: Vec<WireIncident>,
    #[serde(rename = "pagination_meta")]
    pagination_meta: Option<WirePagination>,
}

#[derive(Deserialize)]
struct WirePagination {
    #[serde(default)]
    after: Option<String>,
}

#[derive(Deserialize)]
struct WireIncident {
    id: String,
    reference: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    permalink: Option<String>,
    #[serde(default)]
    status: Option<WireStatus>,
    #[serde(default)]
    severity: Option<WireNamed>,
    #[serde(default)]
    reported_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    timestamps: Vec<WireTimestamp>,
}

#[derive(Deserialize)]
struct WireStatus {
    #[serde(default)]
    category: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct WireNamed {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct WireTimestamp {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: Option<String>,
}

impl From<WireIncident> for Incident {
    fn from(w: WireIncident) -> Self {
        let (status_category, status_name) = match w.status {
            Some(s) => (s.category, s.name),
            None => (String::new(), String::new()),
        };
        Incident {
            id: w.id,
            reference: w.reference,
            name: w.name,
            summary: w.summary,
            permalink: w.permalink,
            status_category,
            status_name,
            severity: w.severity.map(|s| s.name).filter(|s| !s.is_empty()),
            reported_at: w.reported_at,
            updated_at: w.updated_at,
            timestamps: w
                .timestamps
                .into_iter()
                .filter_map(|t| t.value.map(|v| (t.name, v)))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lifecycle vocabulary, which is the whole feature in one predicate.
    #[test]
    fn open_means_still_happening() {
        for open in ["triage", "active", "post_incident", "post-incident"] {
            assert!(is_open(open), "{open} is open");
        }
        // Over, in the several ways incident.io says it.
        for done in ["closed", "declined", "merged", "canceled", "cancelled"] {
            assert!(!is_open(done), "{done} is not open");
        }
        // Explicitly deferred by a human is not open — the board would otherwise carry work
        // somebody has already said to stop looking at.
        assert!(!is_open("paused"));
        // An unknown category is not open. A category this code has never heard of must not
        // silently populate the incidents board.
        assert!(!is_open(""));
        assert!(!is_open("something_new"));
    }

    /// A page taken verbatim from the org's own API, including the parts that would break a
    /// stricter decoder: `"summary": "not set"`, and a severity/status shape that is
    /// org-configured.
    #[test]
    fn a_real_page_decodes_and_yields_symptom_text() {
        let body = serde_json::json!({
            "incidents": [
                {
                    "id": "01KYW0F8MRXF0TS4J8ZRD0SHCH",
                    "external_id": 445,
                    "reference": "INC-445",
                    "name": "TenantPodOOMKillLoop",
                    "permalink": "https://app.incident.io/restatedev/incidents/445",
                    "summary": "A FREE-tier tenant OOM-killed 5 times due to 6,316 accumulated deployments bloating schema metadata to 26.4 MiB.",
                    "severity": { "id": "01HV3", "name": "Minor" },
                    "status": { "category": "closed", "id": "01HV3", "name": "Closed" },
                    "reported_at": "2026-07-31T11:55:36Z",
                    "updated_at": "2026-07-31T16:00:46Z",
                    "timestamps": [
                        { "name": "Reported at", "value": "2026-07-31T11:55:36Z" },
                        { "name": "Resolved at", "value": "2026-07-31T16:00:44Z" },
                        // A named timestamp that has not happened yet arrives with no value.
                        { "name": "Merged at" }
                    ]
                },
                {
                    "id": "01KYX1N4YPJ7TYPT84TWSW0673",
                    "reference": "INC-447",
                    "name": "TenantStorageCritical",
                    "summary": "not set",
                    "status": { "category": "active", "name": "Investigating" }
                }
            ],
            "pagination_meta": { "after": "01KYP050BZ3QTY0YP8RV0D3JB0", "page_size": 50 }
        });
        let page: IncidentsPage = serde_json::from_value(body).expect("the real shape decodes");
        let incidents: Vec<Incident> = page.incidents.into_iter().map(Incident::from).collect();
        assert_eq!(incidents.len(), 2);

        let closed = &incidents[0];
        assert_eq!(closed.reference, "INC-445");
        assert_eq!(closed.severity.as_deref(), Some("Minor"));
        assert!(!closed.is_open(), "closed is not open");
        // A valueless named timestamp is dropped rather than carried as an empty date.
        assert_eq!(closed.timestamps.len(), 2);
        assert!(closed.symptom_text().starts_with("TenantPodOOMKillLoop\n\n"));

        let open = &incidents[1];
        assert!(open.is_open());
        // `"not set"` is incident.io's literal placeholder, not a summary. Scoring it would
        // feed the words "not set" to the code matcher as if they described the outage.
        assert_eq!(open.symptom_text(), "TenantStorageCritical");
    }

    /// An incident missing everything optional still decodes. The point is that one
    /// unexpected entry cannot cost the whole page — the same failure that left
    /// `homebrew-tap` with no commit index at all.
    #[test]
    fn a_sparse_incident_does_not_fail_its_page() {
        let body = serde_json::json!({
            "incidents": [
                { "id": "01ABC", "reference": "INC-1" },
                { "id": "01DEF", "reference": "INC-2", "name": "Real one",
                  "status": { "category": "triage", "name": "Triage" } }
            ]
        });
        let page: IncidentsPage = serde_json::from_value(body).expect("sparse entries decode");
        let incidents: Vec<Incident> = page.incidents.into_iter().map(Incident::from).collect();
        assert_eq!(incidents.len(), 2);
        // No status at all is not "open" — see `open_means_still_happening`.
        assert!(!incidents[0].is_open());
        assert!(incidents[1].is_open());
    }
}
