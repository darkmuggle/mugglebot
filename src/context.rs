//! Context library (Phase 3) — curated external reference material.
//!
//! Runbooks, architecture docs, on-call guides, status pages — the background a
//! new teammate would read. Two source kinds: **URL** (fetched, ETag/`Last-Modified`
//! aware, refreshed on a schedule; authenticated URLs pull a stored credential
//! and send it as a header) and **File** (a local path, re-ingested on mtime
//! change). Both run the same pipeline: fetch/read → normalize to text →
//! summarize → embed → store, indexed for semantic recall.

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use url::Url;

use crate::config;
use crate::embed::{self, Embedder};
use crate::reasoner::Reasoner;
use crate::store::Store;
use crate::tags::{self, TagSuggestion};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSourceKind {
    Url,
    File,
}

impl ContextSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextSourceKind::Url => "url",
            ContextSourceKind::File => "file",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "url" => Some(ContextSourceKind::Url),
            "file" => Some(ContextSourceKind::File),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub id: String,
    pub kind: ContextSourceKind,
    /// The URL or filesystem path.
    pub location: String,
    /// Credential-store account holding the secret for an authenticated URL.
    pub credential: Option<String>,
    /// Header name the credential is injected as (default `Authorization`).
    pub header: Option<String>,
    /// Topical tags for categorical routing (auto-suggested on ingest, editable).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tags were set by a human and must not be overwritten by auto-tagging.
    #[serde(default)]
    pub tags_pinned: bool,
    pub summary: Option<String>,
    /// Normalized text (possibly truncated) — the retrievable body.
    pub raw: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// File mtime as RFC3339, for change detection.
    pub mtime: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub refresh_interval: String,
    pub created_at: DateTime<Utc>,
}

impl Context {
    /// Is this source due for a refresh, given its interval and last fetch?
    pub fn is_due(&self, now: DateTime<Utc>, default_interval: &str) -> bool {
        let Some(fetched) = self.fetched_at else {
            return true;
        };
        let interval = config::parse_duration(&self.refresh_interval)
            .or_else(|_| config::parse_duration(default_interval))
            .unwrap_or(std::time::Duration::from_secs(6 * 3600));
        let elapsed = now.signed_duration_since(fetched);
        elapsed.to_std().map(|e| e >= interval).unwrap_or(true)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextHit {
    #[serde(flatten)]
    pub context: Context,
    pub score: f32,
}

/// Longest normalized body we keep — enough for a runbook, capped so one giant
/// page can't dominate storage or a prompt.
const MAX_BODY: usize = 20_000;

pub struct ContextManager {
    store: Arc<Store>,
    /// Authed URL sources name a credential; it's resolved at fetch time so a
    /// rotated token takes effect on the next refresh.
    secrets: Arc<crate::secrets::Secrets>,
    embedder: Arc<dyn Embedder>,
    /// Cheap/ambient reasoner: summaries and the fast initial tag pass.
    reasoner: Arc<dyn Reasoner>,
    /// Heavy reasoner: the refining tag pass that runs after the cheap one.
    refiner: Arc<dyn Reasoner>,
    client: reqwest::Client,
    default_interval: String,
}

impl ContextManager {
    pub fn new(
        store: Arc<Store>,
        secrets: Arc<crate::secrets::Secrets>,
        embedder: Arc<dyn Embedder>,
        reasoner: Arc<dyn Reasoner>,
        refiner: Arc<dyn Reasoner>,
        default_interval: String,
    ) -> Self {
        Self {
            store,
            secrets,
            embedder,
            reasoner,
            refiner,
            client: reqwest::Client::builder()
                .user_agent("mugglebot")
                .build()
                .expect("http client"),
            default_interval,
        }
    }

    pub fn list(&self) -> Result<Vec<Context>> {
        self.store.list_context()
    }

    pub fn get(&self, id: &str) -> Result<Option<Context>> {
        self.store.get_context(id)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.store.delete_context(id)
    }

    /// Register a new source and do the first ingest. `credential`/`header` apply
    /// to authenticated URLs only.
    pub async fn add(
        &self,
        kind: ContextSourceKind,
        location: &str,
        credential: Option<String>,
        header: Option<String>,
        refresh_interval: Option<String>,
        tags: Option<Vec<String>>,
    ) -> Result<Context> {
        // Human-supplied tags are pinned so the auto-tagger on ingest leaves them
        // alone; absent tags let the two-tier auto-tagger fill them in.
        let pinned_tags = tags.map(tags::normalize_tags).filter(|t| !t.is_empty());
        let ctx = Context {
            id: format!("ctx/{}", crate::store::new_id()),
            kind,
            location: location.to_string(),
            credential,
            header,
            summary: None,
            raw: None,
            etag: None,
            last_modified: None,
            mtime: None,
            fetched_at: None,
            refresh_interval: refresh_interval.unwrap_or_else(|| self.default_interval.clone()),
            created_at: Utc::now(),
            tags: pinned_tags.clone().unwrap_or_default(),
            tags_pinned: pinned_tags.is_some(),
        };
        self.store.put_context(&ctx, None)?;
        if let Some(tags) = &pinned_tags {
            let now = Utc::now();
            for t in tags {
                self.store.ensure_tag(t, "", now)?;
            }
        }
        self.refresh(&ctx.id).await?;
        self.store
            .get_context(&ctx.id)?
            .ok_or_else(|| anyhow!("context vanished after add"))
    }

    /// Set (pin) a context's tags from a human edit, registering any new tags in
    /// the vocabulary. Returns the updated entry.
    pub fn set_tags(&self, id: &str, tags: Vec<String>) -> Result<Context> {
        let names = tags::normalize_tags(tags);
        let now = Utc::now();
        for n in &names {
            self.store.ensure_tag(n, "", now)?;
        }
        self.store.set_context_tags(id, &names, true)?;
        self.store
            .get_context(id)?
            .ok_or_else(|| anyhow!("no context {id}"))
    }

    /// One-time summary backfill: give every tag that still lacks a summary a
    /// generated one (a classification pass over the content filed under it).
    /// Runs on the scheduler; once a tag has a summary — generated here or edited
    /// by hand — it's skipped, so future updates stay manual. Returns the count
    /// filled this pass.
    pub async fn backfill_tag_summaries(&self) -> usize {
        let Ok(all) = self.store.list_tags() else {
            return 0;
        };
        let blanks: Vec<_> = all
            .into_iter()
            .filter(|t| t.summary.trim().is_empty())
            .collect();
        if blanks.is_empty() {
            return 0;
        }
        let contexts = self.store.list_context().unwrap_or_default();
        let memories = self.store.list_memories().unwrap_or_default();
        let mut filled = 0;
        for tag in blanks {
            let mut samples: Vec<String> = Vec::new();
            for c in &contexts {
                if c.tags.iter().any(|t| t == &tag.name) {
                    if let Some(s) = c.summary.as_deref().filter(|s| !s.trim().is_empty()) {
                        samples.push(s.to_string());
                    }
                }
            }
            for m in &memories {
                if m.tags.iter().any(|t| t == &tag.name) {
                    samples.push(m.summary.clone());
                }
            }
            samples.truncate(5);
            if let Some(summary) =
                tags::summarize_tag(self.reasoner.as_ref(), &tag.name, &samples).await
            {
                if self
                    .store
                    .set_tag_summary(&tag.name, &summary, Utc::now())
                    .is_ok()
                {
                    filled += 1;
                }
            }
        }
        filled
    }

    /// Re-fetch/re-read a source. Returns `true` if the content changed (and was
    /// re-summarized + re-embedded), `false` if unchanged (304 / same mtime).
    pub async fn refresh(&self, id: &str) -> Result<bool> {
        let Some(mut ctx) = self.store.get_context(id)? else {
            return Err(anyhow!("no context {id}"));
        };
        let fetched = match ctx.kind {
            ContextSourceKind::Url => self.fetch_url(&mut ctx).await?,
            ContextSourceKind::File => self.read_file(&mut ctx)?,
        };
        let Some(body) = fetched else {
            // Unchanged; still stamp fetched_at so the scheduler backs off.
            ctx.fetched_at = Some(Utc::now());
            self.store.put_context(&ctx, None)?;
            return Ok(false);
        };
        let normalized = normalize(&ctx.location, &body);
        let summary = self.summarize(&ctx.location, &normalized).await;
        ctx.summary = Some(summary);
        ctx.raw = Some(truncate(&normalized, MAX_BODY));
        ctx.fetched_at = Some(Utc::now());
        let embed_input = format!(
            "{}\n{}",
            ctx.summary.as_deref().unwrap_or(""),
            ctx.raw.as_deref().unwrap_or("")
        );
        let vec = self.embedder.embed(&embed_input).await?;
        self.store.put_context(&ctx, Some(&embed::to_blob(&vec)))?;
        if !ctx.tags_pinned {
            self.autotag(&ctx, &normalized).await;
        }
        Ok(true)
    }

    /// Two-pass auto-tagging: a first pass proposes initial tags so the entry is
    /// routable immediately, then a refining pass revises them. Each pass persists,
    /// and registers new tags (with their summaries) in the vocabulary. Skipped
    /// entirely for human-pinned tags. Best-effort: a failing pass is logged and
    /// leaves whatever the previous pass wrote.
    async fn autotag(&self, ctx: &Context, text: &str) {
        let body = format!("{}\n{}", ctx.summary.as_deref().unwrap_or(""), text);
        // Cheap initial pass.
        let vocab = self.store.list_tags().unwrap_or_default();
        if let Some(sugg) = tags::suggest(self.reasoner.as_ref(), &vocab, &body).await {
            if let Err(e) = self.apply_suggestions(&ctx.id, &sugg) {
                tracing::warn!("context {}: initial autotag store failed: {e:#}", ctx.id);
            }
        }
        // Heavy refining pass — sees the cheap pass's tags in the vocabulary.
        let vocab = self.store.list_tags().unwrap_or_default();
        if let Some(sugg) = tags::suggest(self.refiner.as_ref(), &vocab, &body).await {
            if let Err(e) = self.apply_suggestions(&ctx.id, &sugg) {
                tracing::warn!("context {}: refine autotag store failed: {e:#}", ctx.id);
            }
        }
    }

    /// Persist a tag suggestion set to a context (unpinned) and register the tags
    /// in the vocabulary, filling blank summaries. No-op on an empty suggestion so
    /// a failed pass never wipes tags a prior pass wrote.
    fn apply_suggestions(&self, id: &str, sugg: &[TagSuggestion]) -> Result<()> {
        if sugg.is_empty() {
            return Ok(());
        }
        let now = Utc::now();
        let names: Vec<String> = sugg.iter().map(|s| s.name.clone()).collect();
        for s in sugg {
            self.store.ensure_tag(&s.name, &s.summary, now)?;
        }
        self.store.set_context_tags(id, &names, false)?;
        Ok(())
    }

    /// One scheduler tick: refresh every source that's due. Errors on one source
    /// are logged and don't stop the others. Returns ids that actually changed.
    pub async fn refresh_due(&self) -> Vec<String> {
        let now = Utc::now();
        let sources = match self.store.list_context() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("context: listing sources failed: {e:#}");
                return vec![];
            }
        };
        let mut changed = Vec::new();
        for ctx in sources {
            if !ctx.is_due(now, &self.default_interval) {
                continue;
            }
            match self.refresh(&ctx.id).await {
                Ok(true) => changed.push(ctx.id),
                Ok(false) => {}
                Err(e) => tracing::warn!("context refresh {} failed: {e:#}", ctx.id),
            }
        }
        changed
    }

    /// Sync a managed directory of static reference files into the library.
    ///
    /// Layout: `<root>/<tag>/<file...>` — each **top-level subdirectory is an
    /// automatic tag** applied (pinned) to every file beneath it, so dropping a
    /// runbook into `contexts/database/` tags it `database` with no LLM pass.
    /// Files are (re)ingested on content change (mtime), and entries whose backing
    /// file has disappeared are removed. Idempotent — safe to call on a timer as
    /// the directory watcher.
    pub async fn sync_dir(&self, root: &std::path::Path) -> Result<usize> {
        if !root.exists() {
            std::fs::create_dir_all(root)
                .with_context(|| format!("creating contexts dir {}", root.display()))?;
            return Ok(0);
        }
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let root_str = root.to_string_lossy().to_string();

        // Existing File entries that live under the managed root, keyed by location.
        let existing: std::collections::HashMap<String, Context> = self
            .store
            .list_context()?
            .into_iter()
            .filter(|c| c.kind == ContextSourceKind::File && c.location.starts_with(&root_str))
            .map(|c| (c.location.clone(), c))
            .collect();

        let mut on_disk: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut changed = 0usize;

        for (path, tag) in managed_files(&root) {
            let loc = path.to_string_lossy().to_string();
            on_disk.insert(loc.clone());
            let tags = tag.into_iter().collect::<Vec<_>>();
            match existing.get(&loc) {
                Some(ctx) => {
                    // Keep the folder tag authoritative and reload on change.
                    if ctx.tags != tags || !ctx.tags_pinned {
                        for t in &tags {
                            self.store.ensure_tag(t, "", Utc::now())?;
                        }
                        self.store.set_context_tags(&ctx.id, &tags, true)?;
                    }
                    match self.refresh(&ctx.id).await {
                        Ok(true) => changed += 1,
                        Ok(false) => {}
                        Err(e) => tracing::warn!("contexts-dir refresh {loc} failed: {e:#}"),
                    }
                }
                None => {
                    match self
                        .add(ContextSourceKind::File, &loc, None, None, None, Some(tags))
                        .await
                    {
                        Ok(_) => changed += 1,
                        Err(e) => tracing::warn!("contexts-dir ingest {loc} failed: {e:#}"),
                    }
                }
            }
        }

        // Drop managed entries whose file is gone.
        for (loc, ctx) in &existing {
            if !on_disk.contains(loc) {
                if let Err(e) = self.store.delete_context(&ctx.id) {
                    tracing::warn!("contexts-dir remove {loc} failed: {e:#}");
                } else {
                    changed += 1;
                }
            }
        }
        Ok(changed)
    }

    pub async fn search(&self, query: &str, k: usize) -> Result<Vec<ContextHit>> {
        let q = self.embedder.embed(query).await?;
        let rows = self.store.all_context_embeddings()?;
        let mut scored: Vec<ContextHit> = rows
            .into_iter()
            .map(|(ctx, blob)| ContextHit {
                score: embed::cosine(&q, &embed::from_blob(&blob)),
                context: ctx,
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }

    /// Returns `Some(body)` when changed, `None` when the server says 304.
    async fn fetch_url(&self, ctx: &mut Context) -> Result<Option<String>> {
        use reqwest::header::{HeaderName, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH};
        let mut req = self.client.get(&ctx.location);
        if let Some(etag) = &ctx.etag {
            if let Ok(v) = HeaderValue::from_str(etag) {
                req = req.header(IF_NONE_MATCH, v);
            }
        }
        if let Some(lm) = &ctx.last_modified {
            if let Ok(v) = HeaderValue::from_str(lm) {
                req = req.header(IF_MODIFIED_SINCE, v);
            }
        }
        if let Some(account) = &ctx.credential {
            if let Some(secret) = self.secrets.get(account)? {
                let header = ctx.header.clone().unwrap_or_else(|| "Authorization".into());
                if let (Ok(name), Ok(val)) = (
                    HeaderName::from_bytes(header.as_bytes()),
                    HeaderValue::from_str(&secret),
                ) {
                    req = req.header(name, val);
                }
            } else {
                tracing::warn!(
                    "context {}: credential '{account}' not stored; fetching unauthenticated",
                    ctx.id
                );
            }
        }
        let resp = req.send().await.context("fetching context url")?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        ctx.etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        ctx.last_modified = resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let resp = resp.error_for_status().context("context url status")?;
        Ok(Some(resp.text().await.context("reading context body")?))
    }

    fn read_file(&self, ctx: &mut Context) -> Result<Option<String>> {
        let path = expand_tilde(&ctx.location);
        let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        let mtime: DateTime<Utc> = meta
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        let mtime_str = mtime.to_rfc3339();
        if ctx.mtime.as_deref() == Some(mtime_str.as_str()) {
            return Ok(None);
        }
        ctx.mtime = Some(mtime_str);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    async fn summarize(&self, location: &str, text: &str) -> String {
        let excerpt = truncate(text, 8_000);
        let prompt = format!(
            "You are grounding an ops-awareness assistant. Summarize the following reference \
             document in 3-5 sentences an on-call engineer could act on. Note what it covers and \
             when it applies. Source: {location}\n\n---\n{excerpt}"
        );
        match self.reasoner.summarize(&prompt).await {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => truncate(text.trim(), 400),
        }
    }

    /// Fetch a bare **public** URL (no stored source, no credentials, no
    /// persistence) and return a one-paragraph summary — used to enrich a Slack
    /// message that links out. Refuses non-public targets (loopback, private /
    /// link-local IPs, `localhost`, `*.internal` / `*.local` …) so a link posted
    /// in a channel can't drive an SSRF fetch against the host's network.
    pub async fn summarize_public_url(&self, url: &str) -> Result<String> {
        if !is_public_url(url) {
            return Err(anyhow!("not a public http(s) url: {url}"));
        }
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("fetching linked url")?
            .error_for_status()
            .context("linked url status")?;
        let body = resp.text().await.context("reading linked body")?;
        let normalized = normalize(url, &body);
        if normalized.trim().is_empty() {
            return Err(anyhow!("linked url had no readable text"));
        }
        Ok(self.summarize_link(url, &normalized).await)
    }

    async fn summarize_link(&self, location: &str, text: &str) -> String {
        let excerpt = truncate(text, 8_000);
        let prompt = format!(
            "An engineer saw this page linked in a Slack message. Summarize it in a SINGLE concise \
             paragraph (about 2-4 sentences): what the page is and its key point. No preamble, no \
             lists. Source: {location}\n\n---\n{excerpt}"
        );
        match self.reasoner.summarize(&prompt).await {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => truncate(text.trim(), 400),
        }
    }
}

/// Is this a URL we'll fetch on behalf of a link posted in a channel? Only
/// `http(s)`, and never a loopback / private / link-local address or an
/// obviously-internal hostname — an SSRF guard, since the target text is
/// attacker-influenceable. Best-effort: it doesn't resolve DNS.
fn is_public_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            !(d == "localhost"
                || d.ends_with(".localhost")
                || d.ends_with(".local")
                || d.ends_with(".internal")
                || d.ends_with(".intranet")
                || d.ends_with(".lan")
                || d.ends_with(".corp")
                || d.ends_with(".home"))
        }
        Some(url::Host::Ipv4(ip)) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast())
        }
        Some(url::Host::Ipv6(ip)) => !(ip.is_loopback() || ip.is_unspecified()),
        None => false,
    }
}

/// Walk `<root>/<tag>/**` and yield each regular file paired with its
/// normalized top-level-subdirectory tag. Files sitting directly in `root`
/// (no subdirectory) carry no tag and are skipped — the subdir *is* the tag.
/// Hidden entries (dotfiles) are ignored.
fn managed_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, Option<String>)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for tag_dir in entries.flatten() {
        let path = tag_dir.path();
        if !path.is_dir() || is_hidden(&path) {
            continue;
        }
        let tag = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(tags::normalize_tag);
        collect_files(&path, &tag, &mut out);
    }
    out
}

fn collect_files(
    dir: &std::path::Path,
    tag: &Option<String>,
    out: &mut Vec<(std::path::PathBuf, Option<String>)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, tag, out);
        } else if path.is_file() {
            out.push((path, tag.clone()));
        }
    }
}

fn is_hidden(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

/// Best-effort HTML→text: drop `<script>`/`<style>`, strip tags, collapse
/// whitespace. Non-HTML passes through unchanged.
fn normalize(location: &str, body: &str) -> String {
    let looks_html = location.ends_with(".html")
        || location.ends_with(".htm")
        || body
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("<!doctype html")
        || body.contains("</html>")
        || body.contains("</body>");
    if !looks_html {
        return body.to_string();
    }
    strip_html(body)
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut in_tag = false;
    let mut skip_until: Option<&[u8]> = None;
    let lower = html.to_ascii_lowercase();
    while i < bytes.len() {
        if let Some(close) = skip_until {
            if lower.as_bytes()[i..].starts_with(close) {
                i += close.len();
                skip_until = None;
            } else {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'<' {
            if lower[i..].starts_with("<script") {
                skip_until = Some(b"</script>");
                i += 1;
                continue;
            }
            if lower[i..].starts_with("<style") {
                skip_until = Some(b"</style>");
                i += 1;
                continue;
            }
            in_tag = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'>' {
            in_tag = false;
            out.push(' ');
            i += 1;
            continue;
        }
        if !in_tag {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    // Collapse whitespace runs.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html_and_scripts() {
        let html = "<html><head><style>.x{color:red}</style></head><body><h1>Runbook</h1>\
                    <script>evil()</script><p>Restart the pod.</p></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Runbook"));
        assert!(text.contains("Restart the pod."));
        assert!(!text.contains("evil"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn non_html_passes_through() {
        assert_eq!(
            normalize("notes.md", "# Title\ncontent"),
            "# Title\ncontent"
        );
    }

    #[tokio::test]
    async fn autotags_file_on_ingest_and_registers_vocab() {
        use crate::embed::HashEmbedder;
        use crate::reasoner::MockReasoner;

        let dir = std::env::temp_dir().join(format!("mb-ctx-{}", crate::store::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("runbook.md");
        std::fs::write(
            &path,
            "How to recover the primary Postgres database after failover.",
        )
        .unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder);
        // Both passes return the same suggestion; the refining pass just re-confirms.
        let reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new(
            r#"[{"tag":"Database","summary":"DB recovery runbooks"}]"#,
        ));
        let mgr = ContextManager::new(
            store.clone(),
            crate::secrets::Secrets::for_tests(store.clone()),
            embedder,
            reasoner.clone(),
            reasoner,
            "6h".into(),
        );

        let ctx = mgr
            .add(
                ContextSourceKind::File,
                path.to_str().unwrap(),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            ctx.tags,
            vec!["database".to_string()],
            "auto-tagged on ingest"
        );
        assert!(!ctx.tags_pinned, "auto tags are not pinned");
        // The tag is registered in the vocabulary with its summary.
        let tag = store.get_tag("database").unwrap().unwrap();
        assert_eq!(tag.summary, "DB recovery runbooks");
        // And it's found by the categorical lookup.
        let hits = store.context_by_tags(&["database".to_string()]).unwrap();
        assert_eq!(hits.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn sync_dir_tags_by_subdir_and_reaps_deletions() {
        use crate::embed::HashEmbedder;
        use crate::reasoner::MockReasoner;

        let root = std::env::temp_dir().join(format!("mb-ctxdir-{}", crate::store::new_id()));
        let db_dir = root.join("database");
        std::fs::create_dir_all(&db_dir).unwrap();
        let file = db_dir.join("recovery.md");
        std::fs::write(&file, "how to recover the primary").unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder);
        // Auto-tagger would coin "autotag", but the folder tag is pinned and wins.
        let reasoner: Arc<dyn Reasoner> =
            Arc::new(MockReasoner::new(r#"[{"tag":"autotag","summary":"x"}]"#));
        let mgr = ContextManager::new(
            store.clone(),
            crate::secrets::Secrets::for_tests(store.clone()),
            embedder,
            reasoner.clone(),
            reasoner,
            "6h".into(),
        );

        mgr.sync_dir(&root).await.unwrap();
        let sources = store.list_context().unwrap();
        assert_eq!(sources.len(), 1, "one file ingested");
        assert_eq!(
            sources[0].tags,
            vec!["database".to_string()],
            "tagged by subdir"
        );
        assert!(sources[0].tags_pinned, "folder tag is authoritative");

        // Idempotent: a second sync doesn't duplicate.
        mgr.sync_dir(&root).await.unwrap();
        assert_eq!(store.list_context().unwrap().len(), 1);

        // Deleting the file and re-syncing reaps the entry.
        std::fs::remove_file(&file).unwrap();
        mgr.sync_dir(&root).await.unwrap();
        assert!(
            store.list_context().unwrap().is_empty(),
            "deleted file removed"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn human_pinned_tags_survive_ingest() {
        use crate::embed::HashEmbedder;
        use crate::reasoner::MockReasoner;

        let dir = std::env::temp_dir().join(format!("mb-ctx-{}", crate::store::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.md");
        std::fs::write(
            &path,
            "some content the auto-tagger would label differently",
        )
        .unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder);
        let reasoner: Arc<dyn Reasoner> =
            Arc::new(MockReasoner::new(r#"[{"tag":"autotag","summary":"x"}]"#));
        let mgr = ContextManager::new(
            store.clone(),
            crate::secrets::Secrets::for_tests(store.clone()),
            embedder,
            reasoner.clone(),
            reasoner,
            "6h".into(),
        );

        let ctx = mgr
            .add(
                ContextSourceKind::File,
                path.to_str().unwrap(),
                None,
                None,
                None,
                Some(vec!["Payments".into()]),
            )
            .await
            .unwrap();

        assert_eq!(ctx.tags, vec!["payments".to_string()], "pinned tags kept");
        assert!(ctx.tags_pinned);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn public_url_guard_blocks_internal_targets() {
        assert!(is_public_url("https://status.example.com/incidents/42"));
        assert!(is_public_url("http://example.org"));
        // Non-public / SSRF-y targets.
        assert!(!is_public_url("http://localhost:8080/admin"));
        assert!(!is_public_url("http://127.0.0.1/"));
        assert!(!is_public_url("http://169.254.169.254/latest/meta-data")); // cloud metadata
        assert!(!is_public_url("http://10.0.0.5/dashboard"));
        assert!(!is_public_url("https://grafana.internal/d/abc"));
        assert!(!is_public_url("ftp://example.com/file")); // wrong scheme
        assert!(!is_public_url("not a url"));
    }
}
