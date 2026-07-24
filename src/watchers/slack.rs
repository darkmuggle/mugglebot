//! Slack watcher (Phase 1).
//!
//! Polls the Slack Web API (conditional on a stored per-channel cursor) over the
//! designated **watched** and **alert** channels. Watched channels emit a signal
//! only on an @-mention of you or a keyword hit; alert channels treat every post
//! as an alert at higher base severity. Messages authored by your own `user_id`
//! are tagged (`is_self`) so live-assist can flag them.
//!
//! When `search_mentions` is enabled, an additional pass hits `search.messages`
//! to catch mentions of you in *any* conversation you can see — including
//! channels outside `channels`, private channels, and DMs. That endpoint needs a
//! **user** token (`xoxp-…`); the stored `slack` credential must be one.
//!
//! The network shape lives in [`poll`]; the interesting logic — turning a raw
//! message into a normalized [`Signal`] — is the pure [`normalize_message`] and
//! [`normalize_search_match`], which are unit-tested.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{debug, warn};
use url::Url;

use super::{PollBatch, Watcher};
use crate::config::{self, SlackSource};
use crate::signal::{Entity, Severity, Signal, SignalKind, Source, State};

const API: &str = "https://slack.com/api";

/// Keywords that escalate an alert-channel post to Critical.
const CRITICAL_WORDS: &[&str] = &[
    "down", "outage", "critical", "page", "sev1", "sev0", "firing", "paging",
];

pub struct SlackWatcher {
    client: reqwest::Client,
    token: String,
    user_id: Option<String>,
    channels: Vec<String>,
    alert_channels: Vec<String>,
    keywords: Vec<String>,
    /// When set, poll `search.messages` for this query each cycle to catch
    /// mentions in conversations outside `channels` (needs a user token).
    mention_query: Option<String>,
    interval: Duration,
    state: Mutex<SlackState>,
}

#[derive(Default)]
struct SlackState {
    /// Channel name (without leading `#`) → channel id. Also seeded with id→id.
    ids: HashMap<String, String>,
    resolved: bool,
    /// Channel id → newest seen `ts`.
    cursors: HashMap<String, String>,
    /// Newest `ts` seen via `search.messages`, so each cycle only returns
    /// mentions newer than the last.
    search_cursor: Option<String>,
}

/// A single message, reduced to the fields we normalize from. Bot posts (Grafana
/// Cloud Alerts, PagerDuty, …) carry almost nothing in `text` — the Value/Labels
/// detail lives in `attachments` and Block Kit `blocks`, so we parse those too.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SlackMessage {
    #[serde(default)]
    pub user: Option<String>,
    /// Bot display name (`username`) when a message is posted by an app rather
    /// than a human — the human-facing author for e.g. "Cloud Alerts".
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub text: String,
    pub ts: String,
    /// Present on replies — the root message's ts, i.e. the conversation this
    /// message belongs to. Absent on a top-level message.
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    /// Legacy secondary attachments (Grafana's default Slack contact point uses
    /// these): pretext/title/text/fallback/fields carry the real content.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Block Kit blocks — the modern rich layout. Kept as raw JSON and walked for
    /// every text node rather than modeling the full (recursive) block grammar.
    #[serde(default)]
    pub blocks: Value,
}

/// A legacy Slack message attachment. Only the text-bearing fields matter to us.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub pretext: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_link: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    /// Plain-text summary of the whole attachment — used only when `text` is empty.
    #[serde(default)]
    pub fallback: Option<String>,
    #[serde(default)]
    pub fields: Vec<AttachmentField>,
    #[serde(default)]
    pub footer: Option<String>,
    /// Block Kit blocks nested inside the attachment.
    #[serde(default)]
    pub blocks: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AttachmentField {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

/// A single `search.messages` match. Unlike a raw history message it carries its
/// own `channel` (search spans conversations) and a `permalink` deep-link.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchMatch {
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub text: String,
    pub ts: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub permalink: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub blocks: Value,
    pub channel: SearchChannel,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchChannel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

impl SlackWatcher {
    pub fn new(cfg: &SlackSource, token: String) -> Result<Self> {
        let interval =
            config::parse_duration(&cfg.poll_interval).unwrap_or(Duration::from_secs(30));
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent("mugglebot")
                .build()
                .context("building HTTP client")?,
            token,
            user_id: cfg.user_id.clone(),
            channels: cfg.channels.clone(),
            alert_channels: cfg.alert_channels.clone(),
            keywords: cfg
                .keywords
                .iter()
                .map(|k| k.to_ascii_lowercase())
                .collect(),
            mention_query: mention_query(cfg),
            interval,
            state: Mutex::new(SlackState::default()),
        })
    }

    async fn resolve_ids(&self) -> Result<()> {
        if self.state.lock().unwrap().resolved {
            return Ok(());
        }
        #[derive(Deserialize)]
        struct ListResp {
            ok: bool,
            #[serde(default)]
            channels: Vec<Chan>,
            #[serde(default)]
            error: Option<String>,
            #[serde(default)]
            response_metadata: Option<Meta>,
        }
        #[derive(Deserialize)]
        struct Chan {
            id: String,
            name: String,
        }
        #[derive(Deserialize)]
        struct Meta {
            #[serde(default)]
            next_cursor: String,
        }

        let mut map = HashMap::new();
        let mut cursor = String::new();
        for _ in 0..20 {
            let mut req = self
                .client
                .get(format!("{API}/conversations.list"))
                .bearer_auth(&self.token)
                .query(&[
                    ("types", "public_channel,private_channel"),
                    ("limit", "1000"),
                ]);
            if !cursor.is_empty() {
                req = req.query(&[("cursor", cursor.as_str())]);
            }
            let resp: ListResp = req
                .send()
                .await
                .context("slack conversations.list")?
                .json()
                .await
                .context("parsing conversations.list")?;
            if !resp.ok {
                warn!("slack conversations.list error: {:?}", resp.error);
                break;
            }
            for c in resp.channels {
                map.insert(c.name.to_ascii_lowercase(), c.id);
            }
            cursor = resp
                .response_metadata
                .map(|m| m.next_cursor)
                .unwrap_or_default();
            if cursor.is_empty() {
                break;
            }
        }
        let mut st = self.state.lock().unwrap();
        st.ids = map;
        st.resolved = true;
        Ok(())
    }

    /// Resolve a configured channel reference (`#eng`, `eng`, or a raw `C…` id).
    fn channel_id(&self, st: &SlackState, reference: &str) -> Option<String> {
        let name = reference.trim_start_matches('#').to_ascii_lowercase();
        if let Some(id) = st.ids.get(&name) {
            return Some(id.clone());
        }
        // Treat an unresolved reference that looks like an id as itself.
        if reference.starts_with('C') || reference.starts_with('G') || reference.starts_with('D') {
            return Some(reference.to_string());
        }
        None
    }

    async fn history(&self, channel_id: &str, oldest: Option<&str>) -> Result<Vec<SlackMessage>> {
        #[derive(Deserialize)]
        struct HistResp {
            ok: bool,
            #[serde(default)]
            messages: Vec<SlackMessage>,
            #[serde(default)]
            error: Option<String>,
        }
        let mut req = self
            .client
            .get(format!("{API}/conversations.history"))
            .bearer_auth(&self.token)
            .query(&[("channel", channel_id), ("limit", "100")]);
        if let Some(o) = oldest {
            req = req.query(&[("oldest", o)]);
        }
        let resp: HistResp = req
            .send()
            .await
            .context("slack conversations.history")?
            .json()
            .await
            .context("parsing conversations.history")?;
        if !resp.ok {
            anyhow::bail!("slack history error: {:?}", resp.error);
        }
        Ok(resp.messages)
    }

    /// Search every conversation the token can see for mentions of you.
    ///
    /// Returns `(signals to emit, newest ts seen)`. On the first run (`since` is
    /// `None`) it *seeds* the cursor from the newest match without emitting, so
    /// enabling the feature doesn't flood the board with your whole mention
    /// history; subsequent polls emit only matches newer than the cursor.
    async fn search_mentions(
        &self,
        query: &str,
        since: Option<&str>,
    ) -> Result<(Vec<Signal>, Option<String>)> {
        #[derive(Deserialize)]
        struct SearchResp {
            ok: bool,
            #[serde(default)]
            error: Option<String>,
            #[serde(default)]
            messages: Option<SearchMessages>,
        }
        #[derive(Deserialize)]
        struct SearchMessages {
            #[serde(default)]
            matches: Vec<SearchMatch>,
            #[serde(default)]
            paging: Paging,
        }
        #[derive(Default, Deserialize)]
        struct Paging {
            #[serde(default)]
            pages: u32,
        }

        let seeding = since.is_none();
        let mut out = Vec::new();
        let mut newest = since.map(str::to_string);
        // Bound paging so a busy account can't spin here; matches come newest
        // first, so we stop as soon as we cross the cursor.
        'pages: for page in 1u32..=10 {
            let resp: SearchResp = self
                .client
                .get(format!("{API}/search.messages"))
                .bearer_auth(&self.token)
                .query(&[
                    ("query", query),
                    ("sort", "timestamp"),
                    ("sort_dir", "desc"),
                    ("count", "100"),
                    ("page", &page.to_string()),
                ])
                .send()
                .await
                .context("slack search.messages")?
                .json()
                .await
                .context("parsing search.messages")?;
            if !resp.ok {
                anyhow::bail!("slack search error: {:?}", resp.error);
            }
            let Some(messages) = resp.messages else { break };
            if messages.matches.is_empty() {
                break;
            }
            for m in &messages.matches {
                if newest.as_deref().map(|c| m.ts.as_str() > c).unwrap_or(true) {
                    newest = Some(m.ts.clone());
                }
                if let Some(cur) = since {
                    if m.ts.as_str() <= cur {
                        break 'pages;
                    }
                }
                if !seeding {
                    if let Some(sig) = normalize_search_match(m, self.user_id.as_deref()) {
                        out.push(sig);
                    }
                }
            }
            // Seeding only needs the newest page to set the cursor.
            if seeding || page >= messages.paging.pages.max(1) {
                break;
            }
        }
        Ok((out, newest))
    }
}

#[async_trait]
impl Watcher for SlackWatcher {
    fn name(&self) -> &'static str {
        "slack"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn poll(&self) -> Result<PollBatch> {
        self.resolve_ids().await?;

        // Build the (id, name, is_alert) work list from config.
        let mut targets: Vec<(String, String, bool)> = Vec::new();
        {
            let st = self.state.lock().unwrap();
            for c in &self.channels {
                if let Some(id) = self.channel_id(&st, c) {
                    targets.push((id, c.trim_start_matches('#').to_string(), false));
                } else {
                    debug!("slack: unresolved watched channel {c}");
                }
            }
            for c in &self.alert_channels {
                if let Some(id) = self.channel_id(&st, c) {
                    targets.push((id, c.trim_start_matches('#').to_string(), true));
                } else {
                    debug!("slack: unresolved alert channel {c}");
                }
            }
        }

        let mut out = Vec::new();
        for (id, name, is_alert) in targets {
            let oldest = self.state.lock().unwrap().cursors.get(&id).cloned();
            let messages = match self.history(&id, oldest.as_deref()).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("slack history {name}: {e:#}");
                    continue;
                }
            };
            let mut newest = oldest.clone();
            for m in &messages {
                if newest.as_deref().map(|c| m.ts.as_str() > c).unwrap_or(true) {
                    newest = Some(m.ts.clone());
                }
                if let Some(sig) = normalize_message(
                    m,
                    &name,
                    &id,
                    is_alert,
                    self.user_id.as_deref(),
                    &self.keywords,
                ) {
                    out.push(sig);
                }
            }
            if let Some(n) = newest {
                self.state.lock().unwrap().cursors.insert(id, n);
            }
        }

        // Mentions of you anywhere else — private channels, DMs, channels not in
        // `channels` — via search.messages (needs a user token).
        if let Some(query) = &self.mention_query {
            let since = self.state.lock().unwrap().search_cursor.clone();
            match self.search_mentions(query, since.as_deref()).await {
                Ok((sigs, newest)) => {
                    out.extend(sigs);
                    if let Some(n) = newest {
                        self.state.lock().unwrap().search_cursor = Some(n);
                    }
                }
                Err(e) => warn!("slack search_mentions: {e:#}"),
            }
        }

        Ok(PollBatch::incremental(out))
    }
}

/// Turn a raw Slack message into a [`Signal`], or `None` when it doesn't warrant
/// one (a plain message in a watched channel with no mention/keyword, or a
/// non-message subtype like `channel_join`).
pub fn normalize_message(
    m: &SlackMessage,
    channel_name: &str,
    channel_id: &str,
    is_alert: bool,
    user_id: Option<&str>,
    keywords: &[String],
) -> Option<Signal> {
    // Skip join/leave/topic-change noise.
    if let Some(sub) = &m.subtype {
        if matches!(
            sub.as_str(),
            "channel_join" | "channel_leave" | "channel_topic" | "channel_purpose" | "bot_add"
        ) {
            return None;
        }
    }
    // Pull content from every place Slack puts it — top-level text, legacy
    // attachments, and Block Kit blocks — so a bot alert's Value/Labels detail is
    // not dropped (Grafana's `text` is often empty).
    let segments = message_segments(&m.text, &m.attachments, &m.blocks);
    let text = clean_segments(&segments);
    let lower = text.to_ascii_lowercase();
    let is_self = matches!((user_id, &m.user), (Some(uid), Some(u)) if uid == u);

    let mentions_me = user_id
        .map(|uid| m.text.contains(&format!("<@{uid}>")))
        .unwrap_or(false);
    let keyword_hit = keywords.iter().any(|k| lower.contains(k.as_str()));

    let (kind, severity) = if is_alert {
        let sev = if CRITICAL_WORDS.iter().any(|w| lower.contains(w)) {
            Severity::Critical
        } else {
            Severity::Warning
        };
        (SignalKind::Alert, sev)
    } else if mentions_me {
        (SignalKind::Mention, Severity::Notice)
    } else if keyword_hit {
        (SignalKind::ThreadReply, Severity::Notice)
    } else {
        // Nothing to surface from a plain watched-channel message.
        return None;
    };

    let occurred_at = ts_to_datetime(&m.ts).unwrap_or_else(Utc::now);
    let external_id = format!("{channel_id}/{}", m.ts);
    let urls = extract_urls(&segments.join("\n"));
    let actor = m.user.clone().or_else(|| m.username.clone());
    let mut entities = extract_entities(&text, channel_name, actor.as_deref());
    entities.extend(github_ref_entities(&urls));
    entities.push(slack_thread_entity(
        channel_id,
        &m.ts,
        m.thread_ts.as_deref(),
    ));
    let title = summarize(&text, channel_name);

    Some(Signal {
        id: Signal::make_id(Source::Slack, &external_id),
        source: Source::Slack,
        external_id,
        kind,
        title,
        body: Some(text),
        url: None,
        actor,
        entities,
        severity,
        state: State::Unseen,
        occurred_at,
        ingested_at: Utc::now(),
        thread: None,
        raw: serde_json::json!({
            "channel": channel_name,
            "channel_id": channel_id,
            "ts": m.ts,
            "thread_ts": m.thread_ts,
            "is_alert": is_alert,
            "is_self": is_self,
            "mentions_me": mentions_me,
            "urls": urls,
        }),
        tags: Vec::new(),
    })
}

/// Build the `search.messages` query, or `None` when mention-search is off or
/// there's nothing to search for. Defaults to a raw @-mention of `user_id`; an
/// explicit `mention_query` overrides it (e.g. to also match your name).
fn mention_query(cfg: &SlackSource) -> Option<String> {
    if !cfg.search_mentions {
        return None;
    }
    if let Some(q) = cfg.mention_query.as_ref().map(|q| q.trim()) {
        if !q.is_empty() {
            return Some(q.to_string());
        }
    }
    cfg.user_id.as_ref().map(|uid| format!("<@{uid}>"))
}

/// Normalize a `search.messages` match into a [`Mention`] signal. Every match is
/// a mention of you by definition, so it's always surfaced — and unlike the
/// channel-poll path it carries a real permalink as the deep-link `url`.
pub fn normalize_search_match(m: &SearchMatch, user_id: Option<&str>) -> Option<Signal> {
    if let Some(sub) = &m.subtype {
        if matches!(
            sub.as_str(),
            "channel_join" | "channel_leave" | "channel_topic" | "channel_purpose" | "bot_add"
        ) {
            return None;
        }
    }
    let channel_name = m
        .channel
        .name
        .clone()
        .unwrap_or_else(|| m.channel.id.clone());
    let segments = message_segments(&m.text, &m.attachments, &m.blocks);
    let text = clean_segments(&segments);
    let is_self = matches!((user_id, &m.user), (Some(uid), Some(u)) if uid == u);
    let occurred_at = ts_to_datetime(&m.ts).unwrap_or_else(Utc::now);
    let external_id = format!("{}/{}", m.channel.id, m.ts);
    let urls = extract_urls(&segments.join("\n"));
    let actor = m.user.clone().or_else(|| m.username.clone());
    let mut entities = extract_entities(&text, &channel_name, actor.as_deref());
    entities.extend(github_ref_entities(&urls));
    entities.push(slack_thread_entity(
        &m.channel.id,
        &m.ts,
        m.thread_ts.as_deref(),
    ));
    let title = summarize(&text, &channel_name);

    Some(Signal {
        id: Signal::make_id(Source::Slack, &external_id),
        source: Source::Slack,
        external_id,
        kind: SignalKind::Mention,
        title,
        body: Some(text),
        url: m.permalink.clone(),
        actor,
        entities,
        severity: Severity::Notice,
        state: State::Unseen,
        occurred_at,
        ingested_at: Utc::now(),
        thread: None,
        raw: serde_json::json!({
            "channel": channel_name,
            "channel_id": m.channel.id,
            "ts": m.ts,
            "thread_ts": m.thread_ts,
            "via": "search",
            "is_self": is_self,
            "mentions_me": true,
            "urls": urls,
        }),
        tags: Vec::new(),
    })
}

/// Gather every textual segment of a message, in reading order, from `text`,
/// legacy `attachments` (pretext / title / text|fallback / fields / footer), and
/// Block Kit `blocks` (including blocks nested in attachments). Returns the raw
/// Slack-mrkdwn segments, deduped — cleaning happens in [`clean_segments`]. This
/// is the fidelity fix: a Grafana alert's body lives entirely in attachments, so
/// parsing `text` alone yields an empty message.
fn message_segments(text: &str, attachments: &[Attachment], blocks: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    push_seg(&mut out, text);
    collect_block_text(blocks, &mut out);
    for a in attachments {
        if let Some(s) = &a.pretext {
            push_seg(&mut out, s);
        }
        match (&a.title, &a.title_link) {
            (Some(t), Some(l)) if is_http_url(l) => push_seg(&mut out, &format!("{t} — {l}")),
            (Some(t), _) => push_seg(&mut out, t),
            _ => {}
        }
        // Prefer the rich `text`; fall back to the plain-text `fallback` only when
        // there's no `text`, since `fallback` usually just duplicates it.
        match (&a.text, &a.fallback) {
            (Some(t), _) if !t.trim().is_empty() => push_seg(&mut out, t),
            (_, Some(f)) => push_seg(&mut out, f),
            _ => {}
        }
        for f in &a.fields {
            let title = f.title.as_deref().unwrap_or("").trim();
            let value = f.value.as_deref().unwrap_or("").trim();
            match (title.is_empty(), value.is_empty()) {
                (false, false) => push_seg(&mut out, &format!("{title}: {value}")),
                (false, true) => push_seg(&mut out, title),
                (true, false) => push_seg(&mut out, value),
                (true, true) => {}
            }
        }
        collect_block_text(&a.blocks, &mut out);
        if let Some(s) = &a.footer {
            push_seg(&mut out, s);
        }
    }
    out
}

/// Clean each raw segment's Slack mrkdwn and join into one body, re-deduping since
/// cleaning can collapse two segments to the same text.
fn clean_segments(segments: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    for s in segments {
        let cleaned = clean_mrkdwn(s);
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() && !out.iter().any(|e| e == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out.join("\n")
}

/// Push a trimmed segment if it's non-empty and not already present.
fn push_seg(out: &mut Vec<String>, s: &str) {
    let t = s.trim();
    if !t.is_empty() && !out.iter().any(|e| e == t) {
        out.push(t.to_string());
    }
}

/// Recursively collect every text node from a Block Kit tree. Slack text nodes
/// are `{"type":"mrkdwn"|"plain_text","text":"…"}` and rich-text leaves are
/// `{"type":"text","text":"…"}`; walking for any string-valued `"text"` key
/// captures them all without modeling the (deeply recursive) block grammar.
fn collect_block_text(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(t)) = map.get("text") {
                push_seg(out, t);
            }
            for val in map.values() {
                collect_block_text(val, out);
            }
        }
        Value::Array(arr) => arr.iter().for_each(|it| collect_block_text(it, out)),
        _ => {}
    }
}

fn summarize(text: &str, channel: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let head: String = first.chars().take(90).collect();
    if head.is_empty() {
        format!("#{channel} message")
    } else if first.chars().count() > 90 {
        format!("{head}…")
    } else {
        head
    }
}

/// Strip Slack mrkdwn link syntax to plain text: `<url|label>`→`label`,
/// `<url>`→`url`, `<@U…>`→`@U…`, `<#C…|name>`→`#name`.
fn clean_mrkdwn(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut inner = String::new();
            for c2 in chars.by_ref() {
                if c2 == '>' {
                    break;
                }
                inner.push(c2);
            }
            if let Some(rest) = inner.strip_prefix('@') {
                out.push('@');
                out.push_str(rest.split('|').next().unwrap_or(rest));
            } else if let Some(rest) = inner.strip_prefix('#') {
                let label = rest.split('|').nth(1).unwrap_or("");
                out.push('#');
                out.push_str(if label.is_empty() {
                    rest.split('|').next().unwrap_or(rest)
                } else {
                    label
                });
            } else if let Some((url, label)) = inner.split_once('|') {
                // Preserve real links as Markdown so they survive cleaning and
                // render clickable; other piped forms keep just their label.
                if is_http_url(url) {
                    out.push('[');
                    out.push_str(label);
                    out.push_str("](");
                    out.push_str(url);
                    out.push(')');
                } else {
                    out.push_str(label);
                }
            } else {
                // Bare `<url>` (or any other token) — kept verbatim; the Markdown
                // renderer's linkify turns a bare URL into a link.
                out.push_str(&inner);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Pull http(s) URLs out of raw Slack text, in order, deduped. Slack wraps links
/// as `<url>` or `<url|label>`, so we scan the *raw* message (before mrkdwn
/// cleaning, which would drop the url from a `<url|label>`). Trailing sentence
/// punctuation is trimmed.
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("http") {
        let tail = &rest[pos..];
        if tail.starts_with("http://") || tail.starts_with("https://") {
            let end = tail
                .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '|'))
                .unwrap_or(tail.len());
            let url = tail[..end].trim_end_matches(|c: char| {
                matches!(c, '.' | ',' | ')' | ']' | '}' | '!' | '?' | ';' | ':')
            });
            if url.len() > "https://".len() && !out.iter().any(|u| u == url) {
                out.push(url.to_string());
            }
            rest = &tail[end..];
        } else {
            rest = &tail[4..];
        }
    }
    out
}

fn extract_entities(text: &str, channel: &str, author: Option<&str>) -> Vec<Entity> {
    let mut ents = vec![Entity::new("channel", format!("#{channel}"))];
    if let Some(a) = author {
        ents.push(Entity::new("person", a));
    }
    // `owner/repo`-shaped tokens link Slack chatter to GitHub signals.
    for tok in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')')) {
        let t = tok.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '-' && c != '.' && c != '_'
        });
        if let Some((a, b)) = t.split_once('/') {
            if !a.is_empty()
                && !b.is_empty()
                && !t.contains("//")
                && a.chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
                && b.chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                ents.push(Entity::new("repo", t));
            }
        }
        if let Some(env) = environment_entity(t) {
            if !ents
                .iter()
                .any(|e| e.kind == "environment" && e.value == env.value)
            {
                ents.push(env);
            }
        }
    }
    ents
}

/// Recognize a Restate control-plane resource id — `env-2…`, `acc-1…`, `org-1…`
/// — and surface it as a strong `environment` entity. The prefix is hard (per
/// the alert format); the rest is a long base32-ish suffix, e.g.
/// `env-201kbhtqassagmd9t46x1s2sebq`. This is the correlation anchor for tenant
/// alerts: every alert naming the same id groups onto one thread instead of
/// spawning one-thread-per-alert, so the fuzzy classifier never has to guess.
fn environment_entity(tok: &str) -> Option<Entity> {
    const PREFIXES: &[&str] = &["env-2", "acc-1", "org-1"];
    if !PREFIXES.iter().any(|p| tok.starts_with(p)) {
        return None;
    }
    // The suffix after the `env-`/`acc-`/`org-` kind: base32-ish and long enough
    // to be a real id (guards against a stray `env-2` or `acc-1x`).
    let ident = tok.split_once('-')?.1;
    if ident.len() < 12
        || !ident
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return None;
    }
    Some(Entity::new("environment", tok))
}

/// Group replies in one Slack conversation together. A reply carries its root's
/// `thread_ts`; a top-level message keys on its own `ts`, so it shares the key
/// with any replies that later reference it.
fn slack_thread_entity(channel_id: &str, ts: &str, thread_ts: Option<&str>) -> Entity {
    let convo = thread_ts.unwrap_or(ts);
    Entity::new("slack_thread", format!("{channel_id}/{convo}"))
}

/// Parse GitHub PR/issue/discussion references out of a message's URLs, emitting
/// the SAME entity the GitHub watcher does (`{kind}:{owner/repo}#{n}` — see
/// `github.rs` `subject_entity`), so a Slack message linking a PR/issue groups
/// into the same thread as the GitHub signal about it. Handles both web
/// (`github.com/o/r/pull/7`) and API (`api.github.com/repos/o/r/pulls/7`) forms.
fn github_ref_entities(urls: &[String]) -> Vec<Entity> {
    let mut out: Vec<Entity> = Vec::new();
    for u in urls {
        if let Some(e) = github_ref_entity(u) {
            if !out.iter().any(|x| x.kind == e.kind && x.value == e.value) {
                out.push(e);
            }
        }
    }
    out
}

fn github_ref_entity(url: &str) -> Option<Entity> {
    let parsed = Url::parse(url).ok()?;
    let segs: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    // (owner, repo, kind-segment, number) positioned differently on web vs API.
    let (owner, repo, kind_seg, num) = match parsed.host_str()? {
        "github.com" | "www.github.com" if segs.len() >= 4 => (segs[0], segs[1], segs[2], segs[3]),
        "api.github.com" if segs.len() >= 5 && segs[0] == "repos" => {
            (segs[1], segs[2], segs[3], segs[4])
        }
        _ => return None,
    };
    let n: u64 = num.parse().ok()?;
    let kind = match kind_seg {
        "pull" | "pulls" => "pr",
        "issues" => "issue",
        "discussions" => "discussion",
        _ => return None,
    };
    Some(Entity::new(kind, format!("{owner}/{repo}#{n}")))
}

/// Does this token look like a clickable link (http/https/mailto)?
fn is_http_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("mailto:")
}

fn ts_to_datetime(ts: &str) -> Option<DateTime<Utc>> {
    let secs_str = ts.split('.').next()?;
    let secs: i64 = secs_str.parse().ok()?;
    let micros: u32 = ts
        .split('.')
        .nth(1)
        .and_then(|f| f.parse().ok())
        .unwrap_or(0);
    DateTime::from_timestamp(secs, micros * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str, user: &str) -> SlackMessage {
        SlackMessage {
            user: Some(user.into()),
            text: text.into(),
            ts: "1700000000.000100".into(),
            thread_ts: None,
            subtype: None,
            ..Default::default()
        }
    }

    #[test]
    fn grafana_alert_content_comes_from_attachments() {
        // A Grafana Cloud Alerts post: `text` is empty, everything lives in an
        // attachment (title + text + fields). We must recover all of it.
        let mut m = msg("", "U1");
        m.user = None; // a bot post carries `username`, not `user`
        m.username = Some("Cloud Alerts".into());
        m.attachments = vec![Attachment {
            title: Some(
                "[FIRING:1] TenantStorageHigh (environment env-201kbhtqassagmd9t46x1s2sebq \
                 storage-restate-0 tenant warning)"
                    .into(),
            ),
            text: Some("*Firing*\nValue: fire=1, pct_used=50.57".into()),
            fields: vec![
                AttachmentField {
                    title: Some("alertname".into()),
                    value: Some("TenantStorageHigh".into()),
                },
                AttachmentField {
                    title: Some("component".into()),
                    value: Some("environment".into()),
                },
            ],
            ..Default::default()
        }];
        let s = normalize_message(&m, "cloud-alerts", "C1", true, None, &[]).expect("alert");
        let body = s.body.as_deref().unwrap_or("");
        assert!(body.contains("TenantStorageHigh"), "title captured: {body}");
        assert!(
            body.contains("Value: fire=1, pct_used=50.57"),
            "value captured: {body}"
        );
        assert!(
            body.contains("alertname: TenantStorageHigh"),
            "field captured: {body}"
        );
        assert!(
            body.contains("component: environment"),
            "field captured: {body}"
        );
        // The title is a real headline now, not the empty-body fallback.
        assert!(s.title.contains("TenantStorageHigh"));
        assert_ne!(s.title, "#cloud-alerts message");
        // "firing" in the recovered content escalates the alert to Critical.
        assert_eq!(s.severity, Severity::Critical);
        // The environment id is surfaced as a strong correlation entity.
        assert!(
            s.entities
                .iter()
                .any(|e| e.kind == "environment" && e.value == "env-201kbhtqassagmd9t46x1s2sebq"),
            "environment entity: {:?}",
            s.entities
        );
        // Bot display name stands in for the missing user as the actor.
        assert_eq!(s.actor.as_deref(), Some("Cloud Alerts"));
    }

    #[test]
    fn block_kit_text_is_recovered() {
        let mut m = msg("", "U1");
        m.blocks = serde_json::json!([
            {"type": "header", "text": {"type": "plain_text", "text": "ControlPlaneCPUCritical"}},
            {"type": "section", "text": {"type": "mrkdwn", "text": "env-201kae9veyje5j1fk49829dj2a8 is hot"}},
        ]);
        let s = normalize_message(&m, "cloud-alerts", "C1", true, None, &[]).expect("alert");
        let body = s.body.as_deref().unwrap_or("");
        assert!(
            body.contains("ControlPlaneCPUCritical"),
            "header block: {body}"
        );
        assert!(
            body.contains("env-201kae9veyje5j1fk49829dj2a8 is hot"),
            "section block: {body}"
        );
        assert!(s
            .entities
            .iter()
            .any(|e| e.kind == "environment" && e.value == "env-201kae9veyje5j1fk49829dj2a8"));
    }

    #[test]
    fn environment_ids_match_prefixes_only() {
        assert!(environment_entity("env-201kbhtqassagmd9t46x1s2sebq").is_some());
        assert!(environment_entity("acc-1abcdefghijklmnop").is_some());
        assert!(environment_entity("org-1abcdefghijklmnop").is_some());
        // Wrong leading digit / not the hard prefix.
        assert!(environment_entity("env-301kbhtqassagmd9").is_none());
        assert!(environment_entity("acc-201kbhtqassagmd9").is_none());
        // Too short to be a real id, and unrelated dashed tokens.
        assert!(environment_entity("env-2").is_none());
        assert!(environment_entity("restate-0").is_none());
        assert!(environment_entity("storage-restate-0").is_none());
    }

    #[test]
    fn alert_channel_post_is_alert() {
        let s = normalize_message(
            &msg("Service is DOWN", "U1"),
            "alerts",
            "C1",
            true,
            None,
            &[],
        )
        .expect("alert emitted");
        assert!(matches!(s.kind, SignalKind::Alert));
        assert_eq!(s.severity, Severity::Critical);
        assert!(s
            .entities
            .iter()
            .any(|e| e.kind == "channel" && e.value == "#alerts"));
    }

    #[test]
    fn watched_channel_plain_message_is_ignored() {
        assert!(normalize_message(
            &msg("just chatting", "U1"),
            "eng",
            "C2",
            false,
            Some("UME"),
            &[]
        )
        .is_none());
    }

    #[test]
    fn mention_is_surfaced_and_self_tagged() {
        let m = SlackMessage {
            user: Some("UME".into()),
            text: "hey <@UME> look at acme/widgets".into(),
            ts: "1700000000.000200".into(),
            thread_ts: None,
            subtype: None,
            ..Default::default()
        };
        let s = normalize_message(&m, "eng", "C2", false, Some("UME"), &[]).unwrap();
        assert!(matches!(s.kind, SignalKind::Mention));
        assert_eq!(s.raw["is_self"], true);
        assert_eq!(s.raw["mentions_me"], true);
        assert!(s
            .entities
            .iter()
            .any(|e| e.kind == "repo" && e.value == "acme/widgets"));
    }

    #[test]
    fn keyword_hit_is_surfaced() {
        let s = normalize_message(
            &msg("anyone seeing prod down issues", "U9"),
            "eng",
            "C2",
            false,
            Some("UME"),
            &["prod down".into()],
        );
        assert!(s.is_some());
    }

    #[test]
    fn join_subtype_skipped() {
        let mut m = msg("joined", "U1");
        m.subtype = Some("channel_join".into());
        assert!(normalize_message(&m, "eng", "C2", true, None, &[]).is_none());
    }

    #[test]
    fn mrkdwn_cleaned() {
        // Links are preserved as Markdown so they stay clickable after cleaning.
        assert_eq!(
            clean_mrkdwn("see <https://x.io|the docs>"),
            "see [the docs](https://x.io)"
        );
        assert_eq!(clean_mrkdwn("bare <https://x.io>"), "bare https://x.io");
        assert_eq!(clean_mrkdwn("ping <@U123>"), "ping @U123");
    }

    #[test]
    fn mention_query_defaults_to_self_mention() {
        let cfg = SlackSource {
            search_mentions: true,
            user_id: Some("UME".into()),
            ..Default::default()
        };
        assert_eq!(mention_query(&cfg).as_deref(), Some("<@UME>"));
    }

    #[test]
    fn mention_query_honors_override_and_off_switch() {
        let base = SlackSource {
            user_id: Some("UME".into()),
            mention_query: Some("<@UME> OR ben".into()),
            ..Default::default()
        };
        // Off unless search_mentions is set, even with a query present.
        assert_eq!(mention_query(&base), None);
        let on = SlackSource {
            search_mentions: true,
            ..base
        };
        assert_eq!(mention_query(&on).as_deref(), Some("<@UME> OR ben"));
    }

    #[test]
    fn extracts_urls_from_slack_markup() {
        // `<url|label>` — the url survives extraction even though mrkdwn cleaning
        // would keep only the label.
        assert_eq!(
            extract_urls("see <https://status.example.com/incidents/42|the status page>"),
            vec!["https://status.example.com/incidents/42"]
        );
        // Bare url with trailing punctuation, plus dedup.
        assert_eq!(
            extract_urls("look at https://example.com/a. also https://example.com/a again"),
            vec!["https://example.com/a"]
        );
        // No url.
        assert!(extract_urls("just a plain message").is_empty());
    }

    #[test]
    fn github_link_becomes_matching_pr_entity() {
        // A Slack message linking a GitHub PR emits the SAME entity the GitHub
        // watcher does (`pr:octo/repo#17`), so correlation groups them together.
        let m = SlackMessage {
            user: Some("U9".into()),
            text: "<@UME> see <https://github.com/octo/repo/pull/17|the fix>".into(),
            ts: "1700000000.000500".into(),
            thread_ts: None,
            subtype: None,
            ..Default::default()
        };
        let s = normalize_message(&m, "eng", "C2", false, Some("UME"), &[]).unwrap();
        assert!(
            s.entities
                .iter()
                .any(|e| e.kind == "pr" && e.value == "octo/repo#17"),
            "expected pr:octo/repo#17, got {:?}",
            s.entities
        );
    }

    #[test]
    fn reply_carries_conversation_thread_entity() {
        let m = SlackMessage {
            user: Some("U9".into()),
            text: "<@UME> following up".into(),
            ts: "1700000000.000600".into(),
            thread_ts: Some("1700000000.000100".into()),
            subtype: None,
            ..Default::default()
        };
        let s = normalize_message(&m, "eng", "C2", false, Some("UME"), &[]).unwrap();
        assert!(
            s.entities
                .iter()
                .any(|e| e.kind == "slack_thread" && e.value == "C2/1700000000.000100"),
            "reply should key on its root thread_ts, got {:?}",
            s.entities
        );
    }

    #[test]
    fn mention_signal_carries_extracted_urls() {
        let m = SlackMessage {
            user: Some("U9".into()),
            text: "<@UME> check <https://example.com/x|this>".into(),
            ts: "1700000000.000400".into(),
            thread_ts: None,
            subtype: None,
            ..Default::default()
        };
        let s = normalize_message(&m, "eng", "C2", false, Some("UME"), &[]).unwrap();
        assert_eq!(s.raw["urls"][0], "https://example.com/x");
    }

    #[test]
    fn search_match_is_mention_with_permalink() {
        let m = SearchMatch {
            user: Some("U9".into()),
            text: "hey <@UME> can you look? see acme/widgets".into(),
            ts: "1700000000.000300".into(),
            thread_ts: None,
            subtype: None,
            permalink: Some("https://acme.slack.com/archives/D1/p1700000000000300".into()),
            channel: SearchChannel {
                id: "D1".into(),
                name: None, // a DM has no channel name
            },
            ..Default::default()
        };
        let s = normalize_search_match(&m, Some("UME")).unwrap();
        assert!(matches!(s.kind, SignalKind::Mention));
        assert_eq!(s.severity, Severity::Notice);
        assert_eq!(s.url.as_deref(), Some(m.permalink.as_deref().unwrap()));
        assert_eq!(s.external_id, "D1/1700000000.000300");
        assert_eq!(s.raw["via"], "search");
        assert_eq!(s.raw["is_self"], false);
        // Falls back to the channel id as the label when a DM has no name.
        assert!(s
            .entities
            .iter()
            .any(|e| e.kind == "channel" && e.value == "#D1"));
        assert!(s
            .entities
            .iter()
            .any(|e| e.kind == "repo" && e.value == "acme/widgets"));
    }
}
