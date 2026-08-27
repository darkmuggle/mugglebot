//! Restate Cloud entity resolution — turning an id somebody pasted into a ticket or a
//! Slack thread into the box of facts you would otherwise open four terminals to gather.
//!
//! A support thread says `env_201kpvffchrm1sq5p0qph42dqev is throwing 500s`. On its own
//! that is an opaque token: it does not say whose environment it is, which region it runs
//! in, what profile it is on, or which Nuon install to look at. Every one of those is a
//! `rcc get` away, and the operator runs the same four commands every time. This module
//! runs them once, on ingest, and hangs the answer off the signal.
//!
//! **Only curated fields are kept.** `rcc get` dumps raw virtual-object state, and that
//! state carries credentials — `secretKeyHash` on an api key, `controlPlaneApiKeyFull` and
//! `signingKeyArn` on an environment. The admin API redacts the worst of them server-side,
//! but "the server redacted it" is not a property this code should depend on when the
//! output is about to be cached in SQLite and pasted into a model prompt. So each kind has
//! an explicit field list (see [`summarize`]) and everything else is dropped on the floor.
//!
//! Resolution is **transitive and upward**: an environment names its account and its
//! region, an account names its organization, a region names its Nuon install. Following
//! those gets the whole hierarchy from one mention. The edges only ever point at parents,
//! which is what keeps the closure small and guarantees it terminates — an organization
//! with 23 accounts does not fan back out into 23 more lookups.

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::process::Command;
use tracing::{debug, warn};

use crate::config;
use crate::store::Store;

/// The id prefixes the platform issues, and what each one is.
///
/// Mirrors `SERVICES` / `LABELS` in the cloud repo's `src/rcc/rcc-helpers.ts`, which is the
/// authoritative table — `rcc get` routes on exactly this set. A prefix missing here is an
/// id MuggleBot walks straight past, so it is worth re-checking against that file when the
/// platform grows a new resource.
const KINDS: &[(&str, &str)] = &[
    ("usr", "user"),
    ("acc", "account"),
    ("env", "environment"),
    ("rgn", "region"),
    ("key", "apikey"),
    ("org", "organization"),
    ("grp", "group"),
];

/// The kind given to a Nuon install, which has no `rcc` prefix of its own — it is named by
/// a region's `nuonInstallId` and read with `nuon`, not `rcc`.
pub const NUON_KIND: &str = "nuon-install";

/// How a nuon-native region is keyed: `<install>.nuon`. See [`kind_of`].
const NUON_REGION_SUFFIX: &str = ".nuon";

/// Shortest id body this will accept, and the reason the scanner is not just a prefix match.
///
/// `env_`, `key_`, `org_` and `acc_` are all ordinary identifier prefixes in ordinary prose
/// and ordinary code: `env_vars`, `key_management`, `org_name`, `acc_id`. Real platform ids
/// are base62 and long — `rgn_26p9031nve` is the shortest shape in service at 11 characters,
/// and every other kind runs to 23–26. Requiring ten characters *and* at least one digit
/// rejects every English-word false positive above while keeping every real id.
const MIN_ID_BODY: usize = 10;

/// One resolved entity, reduced to what belongs in a context box.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub id: String,
    /// `environment`, `account`, `organization`, `region`, `user`, `group`, `apikey`,
    /// or [`NUON_KIND`].
    pub kind: String,
    pub name: Option<String>,
    pub status: Option<String>,
    /// Curated label/value pairs, in display order. Never the raw state.
    pub fields: Vec<Field>,
    /// Ids this entity names — its parents. The closure follows these, and the panel
    /// renders them as links to the other boxes.
    pub related: Vec<String>,
    /// Ids this entity *contains* — the people and groups on an account or an organization.
    ///
    /// A second edge list, pointing the other way, and it exists for one reason: nothing else
    /// names a user. Environments, accounts and regions are all reachable from `rcc`'s listing
    /// commands, so [`Resolver::enumerate_all`] can seed them directly; there is no
    /// `rcc list-users`, and a user's own document names its accounts rather than the other way
    /// about. The only place a `usr_` id appears at all is inside the account or organization
    /// that granted it access — so that is where the estate walk has to find them.
    ///
    /// Kept out of `related` deliberately. `related` is what [`Resolver::resolve_closure`]
    /// follows on ingest, and it is bounded precisely *because* it only ever points at parents:
    /// one environment reaches four ancestors. An organization contains 198 members, and a
    /// Slack message mentioning one environment must not turn into 198 lookups and a context
    /// box full of strangers. So only [`Resolver::warm`] — the operator asking for the whole
    /// estate on purpose — follows these.
    ///
    /// Read back for display as well as for the walk, and the estate is why. The panel's
    /// "Contains" list is the *reverse* of `related`, which finds an account's owners fine —
    /// they name the account. It finds an organization's members not at all: their document
    /// lists `accountMemberships`, and an org member has none, so they name nothing. That was
    /// 884 of 1326 people on the real estate, cached and searchable and unreachable by
    /// walking. `explore_cloud_entity` merges the two, excluding anything already found.
    #[serde(default)]
    pub members: Vec<String>,
    /// Why this id could not be read, when it could not. A resolved-with-error entity is
    /// still worth keeping: "the id is real but the token expired" and "nobody has ever
    /// looked this up" are different states, and only one of them is worth retrying.
    pub error: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Field {
    pub label: String,
    pub value: String,
}

fn field(label: &str, value: impl Into<String>) -> Field {
    Field {
        label: label.into(),
        value: value.into(),
    }
}

/// Whether there is something to go and read for this id.
///
/// The guard on every traversal edge. A `related` list is built from whatever the platform
/// document said, and not everything named in one is a virtual object — a public region is a
/// bare name. Queueing one of those buys a failed subprocess and a box whose only content is
/// "unknown id prefix", cached and shown until somebody notices.
fn is_resolvable(id: &str) -> bool {
    kind_of(id).is_some() || is_nuon_install(id)
}

/// Nuon install ids are unprefixed and unpunctuated (`inltao53ge1b4pab3zo23si077`), so they
/// are recognized by shape. Distinguishable from a platform id because those all carry an
/// underscore, and from a public region name because those carry hyphens.
fn is_nuon_install(id: &str) -> bool {
    id.starts_with("inl") && id.len() > 12 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The kind for an id's prefix, or `None` when the prefix is not one the platform issues.
///
/// Also recognizes the **`.nuon` twin**: a nuon-native region has no `rgn_` id at all and is
/// keyed by `<install>.nuon` (`inl3jdnqyt2p0vqiiglfhe1da9.nuon`, "Balerion AI us-east-1").
/// It is a region virtual object with the same shape as any other, so it resolves the same
/// way — and it has to, because environments in one name it as their `region`.
pub fn kind_of(id: &str) -> Option<&'static str> {
    if let Some(stem) = id.strip_suffix(NUON_REGION_SUFFIX) {
        return is_nuon_install(stem).then_some("region");
    }
    let prefix = id.split('_').next()?;
    KINDS.iter().find(|(p, _)| *p == prefix).map(|(_, k)| *k)
}

/// Every platform id mentioned in `text`, canonicalized, in order of appearance, deduplicated.
///
/// Deliberately strict about what counts as an id — see [`MIN_ID_BODY`]. The cost of a
/// false positive is a shelled-out lookup that fails and a box full of nothing, on every
/// signal whose body happens to contain the word `env_vars`.
///
/// # Both separators, because the alerts use the other one
///
/// The control plane writes `env_201k…`, but Kubernetes cannot: a cluster, a namespace and a
/// pod are all named `env-201k…`, and that hyphenated form is what every Grafana alert label
/// carries. In this store, `env-` outnumbers `env_` **713 to nothing** — so matching only the
/// underscore meant the context box was blank on precisely the signals it was built for, and
/// the miss was invisible because a missing box looks exactly like an unmentioned id.
///
/// Both forms are accepted and normalized to the underscore, so one environment is one cache
/// entry however it was written. Normalization is lossless for environments, whose ids are
/// lowercase base32; account and organization ids are mixed-case base62, and the hyphenated
/// form of those has already lost the casing — see [`Store::find_cloud_entity_ci`], which
/// recovers it from the warmed estate.
pub fn scan(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut i = 0usize;
    while i < bytes.len() {
        // An id starts at a token boundary: the three prefix characters must not be
        // preceded by anything identifier-like, or `some_env_x` would match at `env_`.
        let boundary = i == 0 || !is_id_char(bytes[i - 1]);
        if !boundary {
            i += 1;
            continue;
        }
        let Some(prefix_len) = KINDS
            .iter()
            .map(|(p, _)| p.len())
            .find(|len| starts_with_prefix(bytes, i, *len))
        else {
            i += 1;
            continue;
        };
        let body_start = i + prefix_len + 1;
        let mut end = body_start;
        while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
            end += 1;
        }
        let body = &text[body_start..end];
        if body.len() >= MIN_ID_BODY
            && body.bytes().any(|b| b.is_ascii_digit())
            && !is_truncated(&text[end..])
        {
            // Canonical form: whichever separator was written, store the underscore.
            let id = format!("{}_{}", &text[i..i + prefix_len], body);
            if seen.insert(id.clone()) {
                out.push(id);
            }
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

fn is_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether a match ends at an elision, meaning what was matched is half an id.
///
/// Alert titles are truncated to fit — `…TenantPodRescheduled (environment
/// env-201j20x11txnd0x0m…` — and the scanner, which stops at the first non-alphanumeric
/// byte, was happily reading the fragment as a whole id. That produced 27 permanently
/// unresolvable entities, each retrying every ten minutes forever, and a failed box on the
/// subjects that carried them.
///
/// Rejecting on the *marker* rather than on a length rule is deliberate. Real ids do have one
/// fixed body length per kind — 27 for an environment, 23 for an account, 10 for a region,
/// measured across 1930 of them — and checking that would also work today. But an id format
/// the platform changes later would then stop matching *silently*, which is precisely the
/// failure the hyphen bug already demonstrated is impossible to notice. A truncation marker
/// cannot produce a false negative on a well-formed id.
fn is_truncated(rest: &str) -> bool {
    rest.starts_with('…') || rest.starts_with("...")
}

/// Whether a known prefix of `len` characters sits at `i`, followed by a separator.
///
/// Either separator: `env_201k…` is how the control plane writes it, `env-201k…` is how
/// Kubernetes — and therefore every alert label — has to.
fn starts_with_prefix(bytes: &[u8], i: usize, len: usize) -> bool {
    if i + len + 1 > bytes.len() || !matches!(bytes[i + len], b'_' | b'-') {
        return false;
    }
    KINDS
        .iter()
        .any(|(p, _)| p.len() == len && p.as_bytes() == &bytes[i..i + len])
}

/// Reads platform state through `rcc` and Nuon state through `nuon`, and caches what it
/// finds.
///
/// Both are read-only invocations of CLIs the operator already has configured — this holds
/// no credentials of its own and cannot obtain any. `nuon` is additionally passed
/// `--read-only`, which makes "cannot mutate" a property of the subprocess rather than of
/// this code being careful.
pub struct Resolver {
    cfg: config::Cloud,
    store: Arc<Store>,
    /// Set when `rcc` was seen falling back to `kubectl` — see [`Self::run`].
    degraded: std::sync::atomic::AtomicBool,
}

/// What `rcc` prints to stderr when it has no usable admin token and reads the virtual
/// object through a `kubectl exec` instead. Matched as a substring because the rest of that
/// line is the credential error, which is the part worth quoting back to the operator.
const KUBECTL_FALLBACK: &str = "falling back to kubectl";

/// The realm `rcc` defaults to when none is configured, for the remediation message.
const DEFAULT_REALM: &str = "us";

impl Resolver {
    pub fn new(cfg: config::Cloud, store: Arc<Store>) -> Self {
        Self {
            cfg,
            store,
            degraded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// The command that fixes an expired credential, named exactly.
    ///
    /// Worth carrying around as a string: an operator reading "cloud lookups are failing" has
    /// to go and find out that the fix is two commands and a realm flag, and the realm is the
    /// part they will get wrong.
    pub fn login_hint(&self) -> String {
        let realm = if self.cfg.context.trim().is_empty() {
            DEFAULT_REALM
        } else {
            self.cfg.context.trim()
        };
        format!("run: restate cloud login && rcc login -c {realm}")
    }

    /// Ask the admin gateway who we are: it checks the gateway, the token and the role
    /// binding in one call, which is exactly the set that expires.
    ///
    /// Recorded as source health under `cloud`, so an expired token shows up on the board as
    /// an unhealthy source with its own fix attached rather than as lookups that quietly stop
    /// producing boxes.
    pub async fn check_auth(&self) -> Result<String> {
        let who = self
            .run(&self.cfg.rcc_path, &self.ctx_args(vec!["whoami".into()]))
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}; {}", self.login_hint()))?;
        let identity = who
            .lines()
            .find(|l| l.starts_with("user:"))
            .unwrap_or("authenticated")
            .trim()
            .to_string();
        Ok(identity)
    }

    /// Record whether the platform is readable, with the fix when it is not.
    pub async fn record_health(&self) {
        let (ok, detail) = match self.check_auth().await {
            // A working token that nonetheless fell back mid-sweep is still a problem worth
            // showing: those reads went through `kubectl`, which is slow and returns state
            // the admin API would have redacted.
            Ok(identity) if !self.is_degraded() => (true, identity),
            Ok(identity) => (
                false,
                format!(
                    "{identity}, but reads fell back to kubectl — {}",
                    self.login_hint()
                ),
            ),
            Err(e) => (false, format!("{e:#}")),
        };
        if let Err(e) = self.store.record_health("cloud", ok, Some(&detail), None) {
            warn!("cloud: recording health failed: {e:#}");
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `-c <realm>` appended when one is configured.
    fn ctx_args(&self, mut args: Vec<String>) -> Vec<String> {
        if !self.cfg.context.trim().is_empty() {
            args.push("-c".into());
            args.push(self.cfg.context.clone());
        }
        args
    }

    /// Mentioned ids whose cached copy is missing or expired, capped at the entity budget.
    ///
    /// The cap is on the *seeds*, and the closure each one opens up is capped again inside
    /// [`Self::resolve_closure`] — so a sweep is bounded twice, and a thread that pastes a
    /// list of thirty environments is spread across ticks rather than forking thirty CLIs.
    pub fn stale_ids(&self, store: &Store) -> Result<Vec<String>> {
        store.stale_cloud_ids(self.ttl(), self.error_ttl(), self.cfg.max_entities as i64)
    }

    /// Resolve `seeds` and everything they name, newest-first, writing each to the cache.
    ///
    /// Breadth-first from the mentioned ids outward, so when the entity budget runs out it
    /// is the *furthest* ancestor that gets dropped rather than the environment somebody
    /// actually asked about.
    pub async fn resolve_closure(&self, seeds: &[String]) -> Vec<Entity> {
        let mut queue: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for s in seeds {
            if seen.insert(s.clone()) {
                queue.push(s.clone());
            }
        }
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < queue.len() && out.len() < self.cfg.max_entities {
            let id = queue[i].clone();
            i += 1;
            let entity = match self.resolve_one(&id).await {
                Ok(e) => e,
                Err(e) => {
                    debug!("cloud: resolving {id}: {e:#}");
                    // Over the top of what was last known, never instead of it — see
                    // [`Self::failed`]. This is the path that blanked 132 environments.
                    self.failed(&id, &format!("{e:#}"))
                }
            };
            // `related` only — never `members`. See the note on [`Entity::members`]: the
            // parent edges are what make this closure small and terminating, and an
            // organization's member list would undo both.
            for r in &entity.related {
                if is_resolvable(r) && seen.insert(r.clone()) {
                    queue.push(r.clone());
                }
            }
            if let Err(e) = self.store.put_cloud_entity(&entity) {
                warn!("cloud: caching {id} failed: {e:#}");
            }
            out.push(entity);
        }
        if i < queue.len() {
            debug!(
                "cloud: stopped at {} entities, {} more named",
                self.cfg.max_entities,
                queue.len() - i
            );
        }
        out
    }

    /// One entity, from the cache when the cached copy is still fresh.
    async fn resolve_one(&self, id: &str) -> Result<Entity> {
        // An id that came from a hyphenated alert label has lost its casing if its kind uses
        // mixed-case ids. The warmed estate is the only place the real one exists, so ask it
        // before spending a subprocess on a lookup that cannot succeed.
        let id = &match self.store.find_cloud_entity_ci(id)? {
            Some(canonical) if canonical != id => {
                debug!("cloud: {id} is really {canonical}");
                canonical
            }
            _ => id.to_string(),
        };
        if let Some(cached) = self.store.get_cloud_entity(id)? {
            // A failed lookup is cached too, but briefly: an expired token is fixed by the
            // operator running `rcc login`, and re-reading it every six hours would make
            // that fix invisible until tomorrow.
            let ttl = if cached.error.is_some() {
                self.error_ttl()
            } else {
                self.ttl()
            };
            if Utc::now() - cached.fetched_at < ttl {
                return Ok(cached);
            }
        }
        self.read_one(id).await
    }

    /// Read one entity from the CLIs, with no cache in the way.
    async fn read_one(&self, id: &str) -> Result<Entity> {
        if is_nuon_install(id) {
            return self.nuon_install(id).await;
        }
        let kind = kind_of(id).with_context(|| format!("unknown id prefix in {id}"))?;
        // A user is the one kind whose virtual object does not say who it is. See [`Self::user`].
        if kind == "user" {
            return self.user(id).await;
        }
        let state = self.rcc_get(id).await?;
        Ok(summarize(kind, id, &state))
    }

    /// Re-read one entity now, whatever the cache says, and store the result.
    ///
    /// The escape hatch from a six-hour TTL. Everything else here is on a timer or a warm
    /// pass, which is right for a cache of things that change on the order of never — and
    /// wrong for the two moments an operator actually wants a fresh read: just after
    /// reconciling a change they made, and just after fixing the credential that made the
    /// boxes go blank.
    pub async fn refresh(&self, id: &str) -> Result<Entity> {
        let id = match self.store.find_cloud_entity_ci(id)? {
            Some(canonical) => canonical,
            None => id.to_string(),
        };
        let entity = match self.read_one(&id).await {
            Ok(e) => e,
            Err(e) => {
                debug!("cloud: refreshing {id}: {e:#}");
                self.failed(&id, &format!("{e:#}"))
            }
        };
        self.store.put_cloud_entity(&entity)?;
        Ok(entity)
    }

    /// What to cache when a read fails: the error, over the top of the last known good copy.
    ///
    /// **A failed read must not be able to erase a good one**, and it could — a failure built
    /// an empty `Entity` and the store upsert replaced every column with it. Found live: an
    /// admin token expired, `rcc` fell back to `kubectl` and that failed too, and the
    /// ten-minute error TTL meant every mentioned id was retried and blanked on a loop. 132
    /// environments lost their name, tier, cluster and region — *and* their `accountId` edge,
    /// so they also dropped out of their account's "Contains" list and out of the closure
    /// behind every subject that mentioned them. The estate quietly dismantled itself while
    /// the only visible symptom was empty boxes.
    ///
    /// Stale-but-known beats blank, every time. The facts were true six hours ago and the
    /// error says why they are not being re-checked, which is strictly more than an empty box
    /// says. `fetched_at` still moves, because it is the retry clock — the content is what is
    /// preserved, not the pretence that it is current.
    fn failed(&self, id: &str, error: &str) -> Entity {
        let prior = self.store.get_cloud_entity(id).ok().flatten();
        let (name, status, fields, related, members) = match prior {
            Some(p) => (p.name, p.status, p.fields, p.related, p.members),
            None => (None, None, Vec::new(), Vec::new(), Vec::new()),
        };
        Entity {
            id: id.to_string(),
            kind: kind_of(id).map(str::to_string).unwrap_or_else(|| {
                if is_nuon_install(id) {
                    NUON_KIND.into()
                } else {
                    "unknown".into()
                }
            }),
            name,
            status,
            fields,
            related,
            members,
            error: Some(concise(error)),
            fetched_at: Utc::now(),
        }
    }

    fn ttl(&self) -> chrono::Duration {
        chrono::Duration::from_std(
            config::parse_duration(&self.cfg.ttl).unwrap_or(std::time::Duration::from_secs(21_600)),
        )
        .unwrap_or_else(|_| chrono::Duration::hours(6))
    }

    /// Short, so `rcc login` takes effect on the next tick rather than the next day.
    fn error_ttl(&self) -> chrono::Duration {
        chrono::Duration::minutes(10)
    }

    async fn rcc_get(&self, id: &str) -> Result<Value> {
        let args = self.ctx_args(vec!["get".to_string(), id.to_string()]);
        let out = self.run(&self.cfg.rcc_path, &args).await?;
        serde_json::from_str(strip_comments(&out))
            .with_context(|| format!("parsing `rcc get {id}` output"))
    }

    /// One user, read with `rcc describe` rather than `rcc get` — because a user's virtual
    /// object does not contain their name or their email address.
    ///
    /// Every other kind is a single admin-API read: `rcc get env_…` returns a document with a
    /// `name` in it and the summary writes itself. A `usr_` document is
    /// `{userId, version, accountMemberships, limits, accounts}` and nothing else. The identity
    /// — the only part that answers the question anybody actually asks about a user id — lives
    /// in a **separate DynamoDB table** (`us-RestateCloud-Users`), written by the WorkOS/Cognito
    /// integration and never mirrored into the state store.
    ///
    /// `rcc describe` is the one command that reads both and prints them together: the virtual
    /// object as JSON under `-o json`, then a `---` rule, then the DynamoDB profile as
    /// `Name:`/`Email:` lines. So this parses both halves of one invocation.
    ///
    /// Two costs worth naming, because they are the reason this is not simply how every kind is
    /// read. `describe` goes through a `kubectl exec` into the control-plane pod rather than the
    /// admin gateway, and it needs AWS credentials for the DynamoDB query. Neither is fatal —
    /// the profile lookup failing prints a note to stderr and exits zero, so a user with no AWS
    /// session still resolves, just without a name. That is why the profile is parsed
    /// best-effort and never required.
    async fn user(&self, id: &str) -> Result<Entity> {
        let mut args = vec![
            "describe".to_string(),
            id.to_string(),
            "-o".into(),
            "json".into(),
        ];
        if !self.cfg.aws_profile.trim().is_empty() {
            args.push("--aws-profile".into());
            args.push(self.cfg.aws_profile.trim().to_string());
        }
        let out = self.run(&self.cfg.rcc_path, &self.ctx_args(args)).await?;
        let (json, profile) = split_describe(&out);
        let state: Value = serde_json::from_str(json)
            .with_context(|| format!("parsing `rcc describe {id}` output"))?;
        let mut entity = summarize("user", id, &state);
        apply_user_profile(&mut entity, profile);
        Ok(entity)
    }

    async fn nuon_install(&self, install_id: &str) -> Result<Entity> {
        let args = vec![
            "installs".to_string(),
            "get".into(),
            "-i".into(),
            install_id.to_string(),
            "--output".into(),
            "json".into(),
            // Belt and braces: the CLI refuses to mutate anything under this flag, so
            // read-only is enforced by the tool rather than by the argument list above.
            "--read-only".into(),
        ];
        let out = self.run(&self.cfg.nuon_path, &args).await?;
        let state: Value = serde_json::from_str(&out)
            .with_context(|| format!("parsing `nuon installs get {install_id}` output"))?;
        // Nuon reports a missing install on **stdout** as a JSON envelope with exit 1, so
        // `run`'s stderr-based message for a failed process comes out blank. Read it here
        // instead: "record not found" against a region's `nuonInstallId` is worth saying out
        // loud — it means the region points at an install somebody deleted.
        if let Some(err) = state.get("error").and_then(Value::as_str) {
            bail!("{err}");
        }
        Ok(summarize_nuon(install_id, &state))
    }

    /// Run a read-only CLI with no stdin and a hard timeout.
    ///
    /// `stdin` is null on purpose: `rcc` can decide it needs a login, and a daemon must get
    /// an error back rather than a subprocess parked forever on a prompt nobody will see.
    async fn run(&self, bin: &str, args: &[String]) -> Result<String> {
        let bin = expand_tilde(bin);
        let timeout =
            config::parse_duration(&self.cfg.timeout).unwrap_or(std::time::Duration::from_secs(30));
        let child = Command::new(&bin)
            .args(args)
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning `{bin}`"))?;
        let out = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .with_context(|| format!("`{bin}` timed out after {}", self.cfg.timeout))?
            .with_context(|| format!("running `{bin}`"))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A *successful* read that went through `kubectl` instead of the admin gateway. This
        // is the shape an expired credential actually takes: `rcc` does not fail, it falls
        // back, so nothing here would otherwise notice. Two reasons it must be noticed —
        // a `kubectl exec` per id is far slower than the gateway, and the fallback returns
        // virtual-object state the gateway would have redacted. (The field allowlist is what
        // keeps the second one from mattering; this is what gets the token fixed.)
        if stderr.contains(KUBECTL_FALLBACK) {
            self.degraded
                .store(true, std::sync::atomic::Ordering::Relaxed);
            warn!("cloud: `rcc` fell back to kubectl — {}", self.login_hint());
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            // Stderr first, then stdout: `nuon` reports a missing install as a JSON envelope
            // on *stdout* and exits 1, which made every one of those failures read as
            // "`nuon` exited with status 1:" and nothing else.
            let raw = if stderr.trim().is_empty() {
                &stdout
            } else {
                &stderr
            };
            bail!("`{bin}` exited with {}: {}", out.status, cli_error(raw));
        }
        Ok(stdout.trim().to_string())
    }

    /// The complete virtual-object document, read live.
    ///
    /// The deliberate opposite of everything else here: no field allowlist, no summarizing,
    /// and **not cached**. It is what an operator gets from `rcc get` in their own terminal,
    /// which is the point — "show me the whole document" is a real need that a curated
    /// twelve-field box cannot serve.
    ///
    /// Because it is uncurated it carries what the cache exists to keep out — `secretKeyHash`
    /// on an api key, `kubeCa` on a region, whatever the platform grows next. So it is
    /// fetched on demand and handed straight to the caller: it never reaches SQLite, and it
    /// never reaches a model prompt.
    pub async fn raw_state(&self, id: &str) -> Result<Value> {
        if kind_of(id).is_none() {
            bail!("{id} is not a platform id");
        }
        self.rcc_get(id).await
    }

    // ---- bulk pre-population ----

    /// Every environment, account and region the control plane knows about.
    ///
    /// Enumerated through the *listing* commands rather than by guessing ids: one
    /// `rcc regions` for the region index, then one `rcc list-envs --region` per region.
    /// That is ~40 calls to name ~2000 entities, and the alternative — reading each
    /// environment to discover its account — is the thing this is trying to make unnecessary.
    ///
    /// The ids are returned in dependency order (regions, then environments, then accounts)
    /// so a warm that is interrupted has still finished the small, widely-shared sets.
    ///
    /// **Users, groups and organizations are not listed here, and cannot be.** There is no
    /// `rcc list-users`: a `usr_` id exists in the platform only inside the account or
    /// organization that granted it access. So they are not seeded — they are *discovered*,
    /// by [`Self::warm`] following the `members` edge off the accounts and organizations this
    /// does seed. The accounts are the whole estate, so the users are too.
    pub async fn enumerate_all(&self) -> Result<Vec<String>> {
        let regions_out = self
            .run(&self.cfg.rcc_path, &self.ctx_args(vec!["regions".into()]))
            .await
            .context("listing regions")?;
        let regions = parse_region_table(&regions_out);

        let mut ids: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let push = |id: &str, ids: &mut Vec<String>, seen: &mut HashSet<String>| {
            if !id.is_empty() && seen.insert(id.to_string()) {
                ids.push(id.to_string());
            }
        };
        for r in &regions {
            // Only the prefixed ones are virtual objects. A public region like `eu-west-1` is
            // named in the index but has no `rgn_` state to read.
            if kind_of(r).is_some() {
                push(r, &mut ids, &mut seen);
            }
        }

        let mut envs: Vec<String> = Vec::new();
        let mut accounts: Vec<String> = Vec::new();
        for region in &regions {
            let args = self.ctx_args(vec![
                "list-envs".into(),
                "--region".into(),
                region.clone(),
                "-o".into(),
                "json".into(),
            ]);
            let out = match self.run(&self.cfg.rcc_path, &args).await {
                Ok(out) => out,
                // One unreadable region must not abandon the other forty-one.
                Err(e) => {
                    warn!("cloud: listing envs in {region}: {e:#}");
                    continue;
                }
            };
            let rows: Vec<Value> = serde_json::from_str(&out).unwrap_or_default();
            for row in rows {
                if let Some(id) = row.get("environmentId").and_then(Value::as_str) {
                    push(id, &mut envs, &mut seen);
                }
                if let Some(id) = row.get("accountId").and_then(Value::as_str) {
                    push(id, &mut accounts, &mut seen);
                }
            }
        }
        debug!(
            "cloud: enumerated {} regions, {} environments, {} accounts",
            ids.len(),
            envs.len(),
            accounts.len()
        );
        ids.append(&mut envs);
        ids.append(&mut accounts);
        Ok(ids)
    }

    /// Resolve `seeds` and their closure into the cache, `concurrency` at a time.
    ///
    /// Separate from [`Self::resolve_closure`] because the budgets are for opposite problems.
    /// That one is bounded hard because it runs on a timer against whatever a Slack message
    /// happened to mention; this one is an operator asking for the whole estate on purpose,
    /// so the only limits are the concurrency and the work actually enumerated.
    ///
    /// Already-fresh entities cost nothing — [`Self::resolve_one`] serves them from the cache
    /// — which is what makes re-running this cheap and makes it safe to interrupt.
    pub async fn warm(
        self: &Arc<Self>,
        seeds: Vec<String>,
        concurrency: usize,
        mut progress: impl FnMut(usize, usize),
    ) -> WarmReport {
        let mut report = WarmReport::default();
        let mut queue = seeds;
        let mut seen: HashSet<String> = queue.iter().cloned().collect();
        let mut next = 0usize;

        while next < queue.len() {
            let wave: Vec<String> = queue[next..]
                .iter()
                .take(concurrency.max(1))
                .cloned()
                .collect();
            next += wave.len();

            let mut set = tokio::task::JoinSet::new();
            for id in wave {
                let me = self.clone();
                set.spawn(async move {
                    let out = me.resolve_one(&id).await;
                    (id, out)
                });
            }
            while let Some(joined) = set.join_next().await {
                let Ok((id, out)) = joined else { continue };
                match out {
                    Ok(entity) => {
                        // Both directions here, unlike in `resolve_closure`: upward to the
                        // ancestors, and downward into the members, which is the only way a
                        // `usr_` or a `grp_` is ever discovered at all. See [`Entity::members`].
                        for r in entity.related.iter().chain(&entity.members) {
                            if is_resolvable(r) && seen.insert(r.clone()) {
                                queue.push(r.clone());
                            }
                        }
                        if let Err(e) = self.store.put_cloud_entity(&entity) {
                            warn!("cloud: caching {id} failed: {e:#}");
                        }
                        *report.by_kind.entry(entity.kind).or_insert(0) += 1;
                    }
                    Err(e) => {
                        debug!("cloud: warming {id}: {e:#}");
                        report.failed.push((id, concise(&format!("{e:#}"))));
                    }
                }
            }
            progress(next.min(queue.len()), queue.len());
        }
        report
    }
}

/// Region selectors from `rcc regions`' output table, first column, header dropped.
///
/// A text table is a fragile thing to parse, so this keeps only what the shape guarantees:
/// the first whitespace-delimited field of each row. Display names contain spaces and the
/// Nuon column is sometimes `-`, but neither is read here — the id is all the caller wants,
/// and every later step goes through `-o json`.
///
/// Public regions (`eu-west-1`) come back alongside the `rgn_` ids. They are kept here and
/// filtered by the caller, because `list-envs --region` accepts both and an environment in a
/// public region still needs enumerating.
fn parse_region_table(out: &str) -> Vec<String> {
    out.lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// What one [`Resolver::warm`] pass did.
#[derive(Debug, Default)]
pub struct WarmReport {
    pub by_kind: std::collections::BTreeMap<String, usize>,
    pub failed: Vec<(String, String)>,
}

impl WarmReport {
    pub fn resolved(&self) -> usize {
        self.by_kind.values().sum()
    }
}

/// Reduce a raw virtual-object state document to the fields worth showing.
///
/// The allowlist *is* the redaction: `secretKeyHash`, `controlPlaneApiKeyFull`,
/// `signingKeyArn`, `kubeToken` and anything else the platform grows are all absent
/// because they are not named here, not because something stripped them.
fn summarize(kind: &str, id: &str, state: &Value) -> Entity {
    let s = |k: &str| state.get(k).and_then(Value::as_str).map(str::to_string);
    let mut fields = Vec::new();
    let mut related = Vec::new();
    let mut members = Vec::new();
    let mut name = s("name");
    let mut status = s("status");

    match kind {
        "environment" => {
            if let Some(v) = s("tierId") {
                fields.push(field("tier", v));
            }
            if let Some(v) = s("profileId") {
                fields.push(field("profile", v));
            }
            if let Some(v) = s("clusterName") {
                fields.push(field("cluster", v));
            }
            if let Some(ms) = state.get("creationMillis").and_then(Value::as_i64) {
                if let Some(t) = Utc.timestamp_millis_opt(ms).single() {
                    fields.push(field("created", t.format("%Y-%m-%d").to_string()));
                }
            }
            related.extend(s("accountId"));
            // A BYOC environment's `region` is an `rgn_` virtual object, and following it is
            // how the Nuon install gets reached. A **public** region is not: `eu-west-1` is a
            // bare name with no state behind it, and emitting it as an edge produced a box
            // that could only ever say "unknown id prefix". Named inline instead — the
            // information is worth having, it just is not a thing to go and read.
            match s("region") {
                Some(r) if kind_of(&r).is_some() => related.push(r),
                Some(r) => fields.push(field("region", r)),
                None => {}
            }
        }
        "account" => {
            if let Some(v) = s("billingMode") {
                fields.push(field("billing", v));
            }
            if let Some(n) = state.get("environments").and_then(Value::as_array) {
                fields.push(field("environments", n.len().to_string()));
            }
            related.extend(s("organizationId"));
            // The two shapes an account grants access in, and which one is in use depends on
            // whether the account is org-owned. A self-serve account has a primary owner and
            // a list of secondaries; an org account has none of those and grants through role
            // bindings instead. Reading both rather than branching on `organizationId`, since
            // an account mid-migration can carry either.
            members.extend(s("primaryOwnerUserId"));
            members.extend(string_array(state, "secondaryOwnersUserIds"));
            members.extend(user_ids(state, "members"));
            members.extend(user_ids(state, "userRoleBindings"));
            if !members.is_empty() {
                fields.push(field("people", members.len().to_string()));
            }
        }
        "organization" => {
            if let Some(n) = state.get("accounts").and_then(Value::as_array) {
                fields.push(field("accounts", n.len().to_string()));
            }
            if let Some(n) = state.get("members").and_then(Value::as_array) {
                fields.push(field("members", n.len().to_string()));
            }
            // A directory-synced organization carries its whole membership — 198 people on the
            // largest one here — each with the email address the user's own document does not
            // have. This is where nearly every `usr_` in the estate is found.
            members.extend(user_ids(state, "members"));
            members.extend(user_ids(state, "userRoleBindings"));
            members.extend(ids_at(state, "groups", "groupId"));
            let domains: Vec<&str> = state
                .pointer("/workos/domains")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !domains.is_empty() {
                fields.push(field("domains", domains.join(", ")));
            }
        }
        "region" => {
            name = s("regionDisplayName").or(name);
            if let Some(v) = s("type") {
                fields.push(field("type", v));
            }
            if let Some(v) = s("providerRegion") {
                fields.push(field("provider region", v));
            }
            if let Some(v) = s("domainName") {
                fields.push(field("domain", v));
            }
            if let Some(v) = s("nuonInstallId") {
                fields.push(field("nuon install", v.clone()));
                related.push(v);
            }
            // A region is owned by an account or an organization, and the ref says which.
            if let Some(owner) = state.pointer("/ownerRef/id").and_then(Value::as_str) {
                related.push(owner.to_string());
            }
        }
        // Everything a `usr_` document actually contains: the accounts it can reach and the
        // role it holds on each. The name and the email address are not in here — they are
        // added afterwards by [`apply_user_profile`] from the other half of `rcc describe`.
        //
        // The accounts are `related` rather than `members` even though a user plainly does not
        // *belong to* twenty-four accounts: `related` is the edge the panel reverses to build
        // its "Contains" list, so pointing this way is what puts the people underneath the
        // account they work on, which is where an operator looks for them.
        "user" => {
            let roles = account_roles(state);
            if !roles.is_empty() {
                fields.push(field("accounts", roles.len().to_string()));
                // The distinct roles rather than the pairing: twenty-four accounts all held as
                // ADMIN is one useful fact and twenty-four useless rows.
                let mut distinct: Vec<&str> = roles.iter().filter_map(|(_, r)| *r).collect();
                distinct.sort_unstable();
                distinct.dedup();
                if !distinct.is_empty() {
                    fields.push(field("roles", distinct.join(", ")));
                }
            }
            related.extend(roles.into_iter().map(|(id, _)| id));
        }
        "group" => {
            related.extend(s("organizationId"));
            if let Some(n) = state.get("members").and_then(Value::as_array) {
                fields.push(field("members", n.len().to_string()));
            }
            if let Some(v) = state.pointer("/source/type").and_then(Value::as_str) {
                fields.push(field("source", v));
            }
            members.extend(user_ids(state, "members"));
        }
        "apikey" => {
            name = s("description").or(name);
            status = s("state").or(status);
            if let Some(v) = s("roleId") {
                fields.push(field("role", v));
            }
            related.extend(s("accountId"));
        }
        _ => {}
    }

    // One id can be named twice — an account's primary owner is often also in `members`, and
    // an organization's owners appear in both `members` and `userRoleBindings`. Deduplicated
    // here rather than in the walk so the "people" count above is a count of people.
    members.retain({
        let mut seen = HashSet::new();
        move |id: &String| seen.insert(id.clone())
    });

    Entity {
        id: id.to_string(),
        kind: kind.to_string(),
        name,
        status,
        fields,
        related,
        members,
        error: None,
        fetched_at: Utc::now(),
    }
}

/// The `usr_` ids inside a list of `{userId, …}` objects — `members` on an organization or a
/// group, `userRoleBindings` on either, `members` on a self-serve account. Four keys with the
/// same shape and one accessor, because the platform spells the same relationship four ways.
fn user_ids(state: &Value, key: &str) -> Vec<String> {
    ids_at(state, key, "userId")
}

/// The `field` of every object in the array at `key`, skipping anything malformed.
fn ids_at(state: &Value, key: &str, field: &str) -> Vec<String> {
    state
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.get(field).and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A plain array of ids, e.g. `secondaryOwnersUserIds`. Null rather than absent is the shape
/// the platform writes for an account that has been migrated to an organization.
fn string_array(state: &Value, key: &str) -> Vec<String> {
    state
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A user's accounts and the role held on each, from whichever of the two lists is populated.
///
/// `accountMemberships` is the current shape and carries the role; `accounts` is the older one
/// and carries an `ownershipType` instead. Both appear on live documents — and under
/// `rcc describe -o json` the `accounts` key is replaced by the *string*
/// `"24 (use --extra to list)"`, which is why this reads the array defensively rather than
/// assuming a shape.
fn account_roles(state: &Value) -> Vec<(String, Option<&str>)> {
    let read = |key: &str, role_key: &str| -> Vec<(String, Option<&str>)> {
        state
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        let id = v.get("accountId").and_then(Value::as_str)?;
                        Some((id.to_string(), v.get(role_key).and_then(Value::as_str)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = read("accountMemberships", "role");
    for (id, kind) in read("accounts", "ownershipType") {
        if !out.iter().any(|(seen, _)| *seen == id) {
            out.push((id, kind));
        }
    }
    out
}

// ---- Resource overrides ------------------------------------------------------------------

/// Bytes in a GiB. The platform persists the storage floor as a raw byte count, and doing that
/// conversion by hand in a JSON document is exactly the step this feature exists to remove.
const GIB: i64 = 1_073_741_824;

/// Apply a memory and/or storage override to an environment's state document, in place.
///
/// **A merge, not a write.** `rcc edit` submits the whole document back — `PutState` is a
/// replace — so anything under `resourceOverrides` this does not name has to survive
/// untouched: replicas, env-var tombstones, tolerations, throttling, a `cpu` request set by
/// hand last month. Rebuilding the object would silently delete every one of them, and
/// `rcc` would ask about it in a prompt this runs too far from anyone to answer.
///
/// The two conversions are the whole point. Memory is a Kubernetes quantity and has to be set
/// on **both** the request and the limit; storage is a raw byte count. Asking an operator for
/// `"8Gi"` in one field and `8589934592` in another, in a JSON document, is how a slipped
/// decimal point becomes an environment with 8 MiB of memory.
///
/// Returns what changed, for the transcript — an operator watching this scroll past should be
/// able to see what it did without diffing anything.
pub fn apply_resource_overrides(
    doc: &mut Value,
    memory_gib: Option<u32>,
    storage_gib: Option<u32>,
) -> Result<Vec<String>> {
    if memory_gib.is_none() && storage_gib.is_none() {
        bail!("nothing to change: set a memory or a storage value");
    }
    let Some(root) = doc.as_object_mut() else {
        bail!("environment state is not a JSON object");
    };
    // `entry`-style, because an environment with no overrides yet is the common case and a
    // missing key must become an object rather than an error.
    let overrides = root
        .entry("resourceOverrides")
        .or_insert_with(|| Value::Object(Default::default()));
    if !overrides.is_object() {
        *overrides = Value::Object(Default::default());
    }
    let overrides = overrides.as_object_mut().expect("just ensured an object");

    let mut changed = Vec::new();
    if let Some(gib) = memory_gib {
        let quantity = Value::String(format!("{gib}Gi"));
        let resources = overrides
            .entry("resources")
            .or_insert_with(|| Value::Object(Default::default()));
        if !resources.is_object() {
            *resources = Value::Object(Default::default());
        }
        let resources = resources.as_object_mut().expect("just ensured an object");
        // Request and limit together. A request without a matching limit is a pod that can be
        // evicted for using what it asked for, which is not what "give it 16GiB" means.
        for slot in ["requests", "limits"] {
            let side = resources
                .entry(slot)
                .or_insert_with(|| Value::Object(Default::default()));
            if !side.is_object() {
                *side = Value::Object(Default::default());
            }
            side.as_object_mut()
                .expect("just ensured an object")
                .insert("memory".into(), quantity.clone());
        }
        changed.push(format!("memory request and limit → {gib}Gi"));
    }
    if let Some(gib) = storage_gib {
        let bytes = i64::from(gib) * GIB;
        overrides.insert("minimumStorageRequestBytes".into(), Value::from(bytes));
        // Named as a floor, because that is what the manifest does with it: it renders
        // `max(override, live volume)`, so a value below the current size changes nothing.
        changed.push(format!("storage floor → {gib}Gi ({bytes} bytes)"));
    }
    Ok(changed)
}

// ---- Grafana deep links ----------------------------------------------------------------
//
// The observability half of "what is this id". The context box answers whose environment it
// is and where it runs; this answers *how is it doing right now*, which is the next thing
// anybody asks and is otherwise four clicks and a dashboard variable away.
//
// Constructed rather than looked up, and it costs nothing: the dashboards live in
// `restate-cloud/grafana` with **stable string UIDs** (`cloud-environment`, `cloud-region`),
// identical in dev and prod, and the values they key on are already fields this module
// caches. No API call, no token, no new state.

/// The dashboard an environment's metrics live on, and the variable it keys on.
///
/// `restate_instance` is the **Kubernetes namespace** — `cloud-environment.ts` builds every
/// query as `namespace="$restate_instance"`. So the value is the hyphenated cluster name, not
/// the `env_` id, which is the same distinction the scanner already has to make.
const ENV_DASHBOARD: &str = "cloud-environment";
/// The per-region dashboard. `cloud-region.ts` populates its `region` variable from the
/// deploy config's region names, which for BYOC *are* the Nuon install ids.
const REGION_DASHBOARD: &str = "cloud-region";

/// Which Grafana holds a realm's metrics.
///
/// Two stacks, and `rcc`'s realm is what says which one an entity belongs to: an environment
/// read from the `us` control plane has its metrics in the prod stack, and pointing at the
/// other one would produce a dashboard that renders perfectly with no data in it — the most
/// expensive kind of wrong link. `eu` shares the prod stack, exactly as it shares `rcc`'s
/// prod credential.
pub fn grafana_host(realm: &str) -> &'static str {
    match realm.trim() {
        "dev" => "https://restate.grafana.net",
        _ => "https://restateprod.grafana.net",
    }
}

/// The Grafana dashboard for this entity, if it has one.
///
/// `None` is a real answer and the common one — an account, an organization, a user and a key
/// have no metrics of their own. Only the three kinds that name something the metrics are
/// labelled by get a link.
pub fn grafana_url(entity: &Entity, host: &str) -> Option<String> {
    let host = host.trim_end_matches('/');
    let field = |label: &str| {
        entity
            .fields
            .iter()
            .find(|f| f.label == label)
            .map(|f| f.value.clone())
    };
    match entity.kind.as_str() {
        "environment" => {
            // The cached `clusterName`, falling back to the id in Kubernetes spelling. The
            // fallback is not decoration: the field is absent on an environment that failed
            // to read cleanly, and the namespace for every environment in service is its id
            // with the separator swapped — the same transformation `scan` does in reverse.
            let ns = field("cluster").unwrap_or_else(|| entity.id.replace('_', "-"));
            Some(format!(
                "{host}/d/{ENV_DASHBOARD}?var-restate_instance={ns}"
            ))
        }
        // A region is addressed by the install underneath it. A region with no install — a
        // public one, or one naming an install that has been deleted — has no dashboard, and
        // saying so by returning `None` beats a link to an empty region picker.
        "region" => field("nuon install")
            .map(|install| format!("{host}/d/{REGION_DASHBOARD}?var-region={install}")),
        // The install *is* the region as far as the metrics are concerned, so it reaches the
        // same dashboard by its own id — which is what the region above is carrying anyway.
        NUON_KIND => Some(format!(
            "{host}/d/{REGION_DASHBOARD}?var-region={}",
            entity.id
        )),
        _ => None,
    }
}

/// Split `rcc describe -o json` into its two halves: the virtual object, and the profile.
///
/// The command prints a `#` header, then the state as JSON, then a line containing only `---`,
/// then the DynamoDB profile as plain text. Splitting on the rule rather than on brace-matching
/// because the rule is what `rcc` actually guarantees — and a document with no profile (no AWS
/// session, or a user the table has never seen) simply has no second half, which is why this
/// returns an empty string rather than failing.
fn split_describe(out: &str) -> (&str, &str) {
    let body = strip_comments(out);
    match body.split_once("\n---") {
        Some((json, profile)) => (json, profile),
        None => (body, ""),
    }
}

/// Fold the DynamoDB profile into a user entity: their name, and their email as a field.
///
/// The email is what makes a user searchable, and searching is most of the point — the question
/// is nearly always "who is `usr_10uXl8…`" or, going the other way, "what does `connor@repl.it`
/// have". `Store::search_cloud_entities` matches field values, so putting it here is what makes
/// the second question work.
///
/// Best-effort by design: `rcc` reports a failed profile lookup on stderr and still exits zero,
/// so a missing name means "no AWS session" or "not in the table", never "this user is not
/// real". A user with neither still resolves, and still carries their accounts.
fn apply_user_profile(entity: &mut Entity, profile: &str) {
    let value = |label: &str| {
        profile.lines().find_map(|l| {
            let rest = l.strip_prefix(label)?.trim();
            (!rest.is_empty()).then(|| rest.to_string())
        })
    };
    if let Some(n) = value("Name:") {
        entity.name = Some(n);
    }
    if let Some(e) = value("Email:") {
        entity.fields.insert(0, field("email", e));
    }
}

/// The Nuon install behind a BYOC region: is the runner up, did the sandbox converge, and
/// are the components actually deployed. The full document is several hundred kilobytes of
/// component and terraform config, so this keeps the six lines an operator reads first.
fn summarize_nuon(install_id: &str, state: &Value) -> Entity {
    let s = |k: &str| state.get(k).and_then(Value::as_str).map(str::to_string);
    let mut fields = Vec::new();
    if let Some(v) = s("cloud_platform") {
        fields.push(field("platform", v));
    }
    if let Some(v) = s("runner_status") {
        let detail = s("runner_status_description").unwrap_or_default();
        fields.push(field(
            "runner",
            if detail.is_empty() {
                v
            } else {
                format!("{v} — {detail}")
            },
        ));
    }
    if let Some(v) = s("composite_component_status") {
        let detail = s("composite_component_status_description").unwrap_or_default();
        fields.push(field(
            "components",
            if detail.is_empty() {
                v
            } else {
                format!("{v} — {detail}")
            },
        ));
    }
    Entity {
        id: install_id.to_string(),
        kind: NUON_KIND.to_string(),
        name: s("name"),
        status: s("sandbox_status"),
        fields,
        related: Vec::new(),
        members: Vec::new(),
        error: None,
        fetched_at: Utc::now(),
    }
}

/// Everything before the JSON: `rcc` prefixes a selector-resolved read with a `#` note
/// (`# inl….nuon → inl….nuon  (Balerion AI us-east-1)`), which is friendly on a terminal and
/// fatal to a parser. Only leading lines are dropped, so a `#` inside the document is safe.
fn strip_comments(out: &str) -> &str {
    let mut rest = out.trim_start();
    while rest.starts_with('#') {
        rest = match rest.split_once('\n') {
            Some((_, tail)) => tail.trim_start(),
            None => "",
        };
    }
    rest
}

/// A CLI's failure, reduced to the sentence worth showing.
///
/// Handles the JSON-envelope shape as well as plain text, because `nuon` reports errors as
/// `{"description":…,"error":…,"user_error":true}` and the whole envelope in a one-line panel
/// row buries the only part that means anything.
fn cli_error(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(strip_comments(raw)) {
        if let Some(msg) = v.get("error").and_then(Value::as_str) {
            return crate::tools::truncate_for_prompt(msg, 200);
        }
    }
    concise(raw)
}

/// First non-empty line, capped — a CLI's stderr can be a page of usage text, and the
/// panel has one row for it.
fn concise(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    crate::tools::truncate_for_prompt(&line, 200)
}

fn expand_tilde(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest).to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string()),
        None => path.to_string(),
    }
}

/// Render resolved entities as a grounding block for a model prompt.
///
/// Empty when there is nothing resolved, so callers can concatenate it unconditionally.
pub fn prompt_block(entities: &[Entity]) -> String {
    // Anything with something to say, which is no longer the same as "read without error".
    // A failed re-read now keeps the last good copy (see [`Resolver::failed`]), and those
    // facts are worth grounding on: an environment's account and region were true six hours
    // ago and are almost certainly true now. Only an entity that is genuinely empty — never
    // read, or read and failed with nothing behind it — is dropped, because a heading over
    // nothing is worse than no heading.
    let usable: Vec<&Entity> = entities
        .iter()
        .filter(|e| e.name.is_some() || !e.fields.is_empty())
        .collect();
    if usable.is_empty() {
        return String::new();
    }
    let mut out = String::from("Restate Cloud entities mentioned here:\n");
    for e in usable {
        out.push_str(&format!("- {} {}", e.kind, e.id));
        if let Some(n) = &e.name {
            out.push_str(&format!(" \"{n}\""));
        }
        if let Some(s) = &e.status {
            out.push_str(&format!(" [{s}]"));
        }
        for f in &e.fields {
            out.push_str(&format!("; {}: {}", f.label, f.value));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The form every Grafana alert label actually carries. Kubernetes cannot put an
    /// underscore in a cluster or namespace name, so the alerts say `env-201k…` — and in this
    /// store that form outnumbers `env_` 713 to nothing. Matching only the underscore left
    /// the context box blank on exactly the signals it exists for.
    #[test]
    fn the_hyphenated_form_the_alerts_use_is_found_and_canonicalized() {
        let alert = "[FIRING:1] TenantStorageHigh · cluster env-201kpe1278xqa4ne187w8fbp640 \
                     is above 90%";
        assert_eq!(scan(alert), vec!["env_201kpe1278xqa4ne187w8fbp640"]);
        // Both spellings of one environment are one entity, not two cache entries.
        assert_eq!(
            scan("env-201kpe1278xqa4ne187w8fbp640 and env_201kpe1278xqa4ne187w8fbp640"),
            vec!["env_201kpe1278xqa4ne187w8fbp640"]
        );
    }

    /// Alert titles are elided to fit, and the scanner stops at the first non-alphanumeric
    /// byte — so half an id read as a whole one. Found live: 27 permanently unresolvable
    /// entities, each retrying every ten minutes forever.
    #[test]
    fn half_an_id_cut_off_by_an_ellipsis_is_not_an_id() {
        let title = "[FIRING:11] TenantPodRescheduled Restate Cloud Alerts us \
                     (environment env-201j20x11txnd0x0m…";
        assert!(scan(title).is_empty(), "matched a truncated id");
        // Three dots do the same job in a plain-text feed.
        assert!(scan("environment env-201j20x11txnd0x0m...").is_empty());
        // The whole id in the same sentence is still found.
        assert_eq!(
            scan("env-201j20x11txnd0x0mt52ne20297 fired, see env-201j20x11txnd0x0m…"),
            vec!["env_201j20x11txnd0x0mt52ne20297"]
        );
    }

    /// A pod name is the environment plus a suffix, and the id stops where the id stops.
    #[test]
    fn a_pod_name_still_yields_its_environment() {
        assert_eq!(
            scan("env-201kpe1278xqa4ne187w8fbp640-restate-0 crashlooped"),
            vec!["env_201kpe1278xqa4ne187w8fbp640"]
        );
    }

    /// The hyphen must not weaken the filter that keeps ordinary prose out.
    #[test]
    fn hyphenated_english_is_still_not_an_id() {
        for text in [
            "env-vars",
            "acc-id",
            "org-name",
            "key-management",
            "usr-local",
        ] {
            assert!(scan(text).is_empty(), "false positive in {text:?}");
        }
    }

    /// The shapes that actually appear in tickets and alert bodies.
    #[test]
    fn it_finds_real_ids_in_prose() {
        let text = "Customer reports env_201kpvffchrm1sq5p0qph42dqev is 500ing. \
                    Account acc_10vk05bJNjt1nLXvId4zh14, region rgn_26p9031nve.";
        assert_eq!(
            scan(text),
            vec![
                "env_201kpvffchrm1sq5p0qph42dqev",
                "acc_10vk05bJNjt1nLXvId4zh14",
                "rgn_26p9031nve"
            ]
        );
    }

    /// The reason the scanner is not a prefix match. Every one of these appears in ordinary
    /// logs and code, and each would otherwise cost a failed subprocess per signal.
    #[test]
    fn ordinary_words_that_start_with_a_prefix_are_not_ids() {
        for text in [
            "env_vars are unset",
            "check key_management first",
            "org_name and acc_id columns",
            "usr_local is on the PATH",
            "env_variables",
            // Long enough, but no digit — an identifier, not a base62 id.
            "grp_administrators",
        ] {
            assert!(scan(text).is_empty(), "false positive in {text:?}");
        }
    }

    /// `some_env_xyz…` is one identifier, not an id hiding inside one.
    #[test]
    fn a_prefix_mid_identifier_is_not_a_boundary() {
        assert!(scan("my_env_201kpvffchrm1sq5p0qph42dqev").is_empty());
    }

    /// Punctuation ends an id — a trailing full stop or bracket is not part of it.
    #[test]
    fn punctuation_terminates_an_id() {
        assert_eq!(
            scan("(env_201kpvffchrm1sq5p0qph42dqev), and acc_10vk05bJNjt1nLXvId4zh14."),
            vec![
                "env_201kpvffchrm1sq5p0qph42dqev",
                "acc_10vk05bJNjt1nLXvId4zh14"
            ]
        );
    }

    /// One box per id, however many times the thread repeats it.
    #[test]
    fn repeats_collapse_to_one() {
        let id = "env_201kpvffchrm1sq5p0qph42dqev";
        assert_eq!(scan(&format!("{id} … again {id} … and {id}")), vec![id]);
    }

    /// The real environment document, trimmed to the keys this reads. The point of the
    /// assertion is what is *absent*: three credential-bearing fields are in the input.
    #[test]
    fn an_environment_yields_its_parents_and_none_of_its_secrets() {
        let state = serde_json::json!({
            "environmentId": "env_201kpvffchrm1sq5p0qph42dqev",
            "name": "janeway",
            "status": "PROVISIONING_READY",
            "tierId": "CUSTOM",
            "profileId": "replit.3-node.xlarge-vqueues",
            "clusterName": "env-201kpvffchrm1sq5p0qph42dqev",
            "accountId": "acc_10vk05bJNjt1nLXvId4zh14",
            "region": "rgn_26p9031nve",
            "controlPlaneApiKeyFull": "key_10vhyuj8CtkbrCWlbSonfia.SECRET",
            "signingKeyArn": "Secret/env-201kpvffchrm1sq5p0qph42dqev/signing-key",
            "signingPublicKey": "publickeyv1_UthnVjGqCoa9a78",
        });
        let e = summarize("environment", "env_201kpvffchrm1sq5p0qph42dqev", &state);
        assert_eq!(e.name.as_deref(), Some("janeway"));
        assert_eq!(e.status.as_deref(), Some("PROVISIONING_READY"));
        assert_eq!(
            e.related,
            vec!["acc_10vk05bJNjt1nLXvId4zh14", "rgn_26p9031nve"]
        );
        let rendered = serde_json::to_string(&e).unwrap();
        for secret in ["SECRET", "signingKeyArn", "signingPublicKey"] {
            assert!(
                !rendered.contains(secret),
                "{secret} leaked into {rendered}"
            );
        }
    }

    /// An api key's `secretKeyHash` is the one field on that document nobody should see,
    /// and `rcc get` returns it unredacted.
    #[test]
    fn an_api_key_never_carries_its_hash() {
        let state = serde_json::json!({
            "keyId": "key_10vkhgbtpzFTNSWc2RnOY1S",
            "accountId": "acc_10vk05bJNjt1nLXvId4zh14",
            "description": "Long-lived API key",
            "roleId": "rst:role::ManagementApiAccess",
            "secretKeyHash": "8LyuvaoDge+tssnzQ24+1oX2gaggbTJXf4TPg1ZhOVY=",
            "state": "KEY_ACTIVE",
        });
        let e = summarize("apikey", "key_10vkhgbtpzFTNSWc2RnOY1S", &state);
        assert_eq!(e.name.as_deref(), Some("Long-lived API key"));
        assert_eq!(e.status.as_deref(), Some("KEY_ACTIVE"));
        assert!(!serde_json::to_string(&e).unwrap().contains("8Lyuvao"));
    }

    /// A region is the hop that reaches Nuon: it names the install, and the install is what
    /// the closure follows next.
    #[test]
    fn a_region_names_its_nuon_install_and_its_owner() {
        let state = serde_json::json!({
            "regionName": "rgn_26p9031nve",
            "regionDisplayName": "prod/us-east1",
            "type": "nuon-gcp",
            "providerRegion": "us-east1",
            "status": "ACTIVE",
            "domainName": "inltao53ge1b4pab3zo23si077.byoc.restate.cloud",
            "nuonInstallId": "inltao53ge1b4pab3zo23si077",
            "ownerRef": { "id": "acc_10vk05bJNjt1nLXvId4zh14", "kind": "ACCOUNT" },
            "kubeToken": "eyJhbGciOi.SECRET",
        });
        let e = summarize("region", "rgn_26p9031nve", &state);
        assert_eq!(e.name.as_deref(), Some("prod/us-east1"));
        assert_eq!(
            e.related,
            vec!["inltao53ge1b4pab3zo23si077", "acc_10vk05bJNjt1nLXvId4zh14"]
        );
        assert!(!serde_json::to_string(&e).unwrap().contains("SECRET"));
    }

    /// The real `usr_` document, which is very nearly empty: the accounts it can reach, the
    /// role on each, and nothing that says who the person is.
    ///
    /// The accounts go in `related` rather than `members` on purpose — that is the edge the
    /// panel reverses, so pointing this way is what files a person under the account they work
    /// on, which is where an operator goes looking for them.
    #[test]
    fn a_user_names_the_accounts_they_can_reach() {
        let state = serde_json::json!({
            "userId": "usr_10uXl8CPozSuQiKolKUfqVI",
            "version": 2,
            "accountMemberships": [
                { "accountId": "acc_10vk05bJNjt1nLXvId4zh14", "role": "ADMIN" },
                { "accountId": "acc_10vk05uciDrYR54uUl66H5T", "role": "ADMIN" },
            ],
            "limits": { "accountsLimit": 3 },
            "accounts": [
                { "accountId": "acc_10vk05bJNjt1nLXvId4zh14", "ownershipType": "OWNED" },
                { "accountId": "acc_10uVy0R2fBetGiOTxDbeH5x", "ownershipType": "SECONDARY_OWNED" },
            ],
        });
        let e = summarize("user", "usr_10uXl8CPozSuQiKolKUfqVI", &state);
        // Both lists, merged, the membership shape winning where they overlap.
        assert_eq!(
            e.related,
            vec![
                "acc_10vk05bJNjt1nLXvId4zh14",
                "acc_10vk05uciDrYR54uUl66H5T",
                "acc_10uVy0R2fBetGiOTxDbeH5x"
            ]
        );
        assert!(e
            .fields
            .iter()
            .any(|f| f.label == "accounts" && f.value == "3"));
        // Distinct roles, not one row per account.
        assert!(e
            .fields
            .iter()
            .any(|f| f.label == "roles" && f.value == "ADMIN, SECONDARY_OWNED"));
        // Nothing has named the person yet — that is the other half of `rcc describe`.
        assert!(e.name.is_none());
    }

    /// `rcc describe -o json` is two documents in one stream, and the identity is in the half
    /// that is not JSON. Everything a user box is actually read for is in that half.
    #[test]
    fn a_users_name_and_email_come_from_the_half_that_is_not_json() {
        let out = "\
# user: usr_10uXl8CPozSuQiKolKUfqVI

{
  \"accountMemberships\": [{ \"accountId\": \"acc_10vk05bJNjt1nLXvId4zh14\", \"role\": \"ADMIN\" }],
  \"accounts\": \"24 (use --extra to list)\",
  \"userId\": \"usr_10uXl8CPozSuQiKolKUfqVI\"
}

---
Name:     Connor Brewster
Email:    connor@repl.it
Accounts: 24
  - acc_10vk05bJNjt1nLXvId4zh14  undefined  [OWNED]";
        let (json, profile) = split_describe(out);
        let state: Value = serde_json::from_str(json).expect("the JSON half parses");
        let mut e = summarize("user", "usr_10uXl8CPozSuQiKolKUfqVI", &state);
        // `accounts` is a *string* under `describe`, not an array — reading it as one must not
        // panic or invent an account.
        assert_eq!(e.related, vec!["acc_10vk05bJNjt1nLXvId4zh14"]);
        apply_user_profile(&mut e, profile);
        assert_eq!(e.name.as_deref(), Some("Connor Brewster"));
        // First field, because it is the one anybody reads — and searchable, which is how
        // "what does connor@repl.it have" is answered at all.
        assert_eq!(e.fields[0].label, "email");
        assert_eq!(e.fields[0].value, "connor@repl.it");
    }

    /// No AWS session means no profile half. A user must still resolve — "we could not read
    /// the directory" and "this id is not real" are different answers.
    #[test]
    fn a_user_with_no_profile_still_resolves() {
        let out = "# user: usr_1\n{\"userId\": \"usr_1\", \"accountMemberships\": []}";
        let (json, profile) = split_describe(out);
        assert_eq!(profile, "");
        let mut e = summarize("user", "usr_1", &serde_json::from_str(json).unwrap());
        apply_user_profile(&mut e, profile);
        assert!(e.name.is_none() && e.error.is_none());
    }

    /// Where users actually come from. Nothing lists them, so the estate walk finds them by
    /// reading the account that granted them access — and an account grants it two different
    /// ways depending on whether it is org-owned.
    #[test]
    fn an_account_names_its_people_downward_and_its_organization_upward() {
        let self_serve = summarize(
            "account",
            "acc_10uVyWtP1qKryJWaTrheJQ0",
            &serde_json::json!({
                "accountId": "acc_10uVyWtP1qKryJWaTrheJQ0",
                "name": "production-fior-digital",
                "primaryOwnerUserId": "usr_10uVy0B3rU77ujBXhXokEp3",
                "secondaryOwnersUserIds": ["usr_10uSxlnhXZbXe4QmGNwxkAt", "usr_10uUkti78PdtPk0QT5HHHHe"],
            }),
        );
        assert_eq!(
            self_serve.members,
            vec![
                "usr_10uVy0B3rU77ujBXhXokEp3",
                "usr_10uSxlnhXZbXe4QmGNwxkAt",
                "usr_10uUkti78PdtPk0QT5HHHHe"
            ]
        );
        // Downward, and *only* downward: the ingest closure reads `related`, and it must not
        // drag three strangers into the context box of an alert about one environment.
        assert!(self_serve.related.is_empty());

        // An org-owned account has no owner fields at all and grants through role bindings.
        let org_owned = summarize(
            "account",
            "acc_10vk05bJNjt1nLXvId4zh14",
            &serde_json::json!({
                "accountId": "acc_10vk05bJNjt1nLXvId4zh14",
                "name": "production",
                "organizationId": "org_10vj37k7NcqTaIavuJRsOcv",
                "userRoleBindings": [
                    { "userId": "usr_10uXl8CPozSuQiKolKUfqVI", "roles": ["restate/AccountAdmin"] },
                    { "userId": "usr_10vgOdP02CMNfWzXYVfJYDH", "roles": ["restate/AccountAdmin"] },
                ],
            }),
        );
        assert_eq!(
            org_owned.members,
            vec!["usr_10uXl8CPozSuQiKolKUfqVI", "usr_10vgOdP02CMNfWzXYVfJYDH"]
        );
        assert_eq!(org_owned.related, vec!["org_10vj37k7NcqTaIavuJRsOcv"]);
        assert!(org_owned
            .fields
            .iter()
            .any(|f| f.label == "people" && f.value == "2"));
    }

    /// A directory-synced organization is where most of the estate's people are, and its owners
    /// are named twice — once as members, once as role bindings. One person, one entry.
    #[test]
    fn an_organization_names_its_members_and_its_groups_once_each() {
        let e = summarize(
            "organization",
            "org_10vj37k7NcqTaIavuJRsOcv",
            &serde_json::json!({
                "organizationId": "org_10vj37k7NcqTaIavuJRsOcv",
                "name": "Replit",
                "accounts": [{ "accountId": "acc_10vk05bJNjt1nLXvId4zh14" }],
                "members": [
                    { "userId": "usr_10vifGx1fTzG4tqpt2pt0Rp", "email": "shin.yoon@repl.it" },
                    { "userId": "usr_10uXl8CPozSuQiKolKUfqVI", "email": "connor@repl.it" },
                ],
                "userRoleBindings": [
                    { "userId": "usr_10uXl8CPozSuQiKolKUfqVI", "roles": ["restate/OrganizationOwner"] },
                ],
                "groups": [{ "groupId": "grp_10vk02G8Ne9ftq7xOI3FJmE", "name": "App-Restate-Engineering" }],
            }),
        );
        assert_eq!(
            e.members,
            vec![
                "usr_10vifGx1fTzG4tqpt2pt0Rp",
                "usr_10uXl8CPozSuQiKolKUfqVI",
                "grp_10vk02G8Ne9ftq7xOI3FJmE"
            ],
            "the owner is a member and a role binding, and must appear once"
        );
        assert!(e.related.is_empty());
    }

    /// A group is a `grp_` with state behind it, reached from its organization, and it is the
    /// other place a `usr_` is named.
    #[test]
    fn a_group_belongs_to_its_organization_and_names_its_members() {
        let e = summarize(
            "group",
            "grp_10vk02G8Ne9ftq7xOI3FJmE",
            &serde_json::json!({
                "groupId": "grp_10vk02G8Ne9ftq7xOI3FJmE",
                "organizationId": "org_10vj37k7NcqTaIavuJRsOcv",
                "status": "ACTIVE",
                "name": "App-Restate-Engineering",
                "source": { "type": "WORKOS_DIRECTORY_GROUP" },
                "members": [
                    { "userId": "usr_10vjZZTkhE2M6qYlaqg0GTo", "email": "darian.chang@repl.it" },
                ],
            }),
        );
        assert_eq!(e.related, vec!["org_10vj37k7NcqTaIavuJRsOcv"]);
        assert_eq!(e.members, vec!["usr_10vjZZTkhE2M6qYlaqg0GTo"]);
        assert_eq!(e.status.as_deref(), Some("ACTIVE"));
        assert!(e
            .fields
            .iter()
            .any(|f| f.label == "members" && f.value == "1"));
    }

    /// Nuon's install document is hundreds of kilobytes; what survives is the status trio.
    #[test]
    fn a_nuon_install_keeps_only_its_status() {
        let state = serde_json::json!({
            "id": "inltao53ge1b4pab3zo23si077",
            "name": "replit-prod-us-east1",
            "cloud_platform": "gcp",
            "sandbox_status": "active",
            "runner_status": "active",
            "runner_status_description": "process initialized",
            "composite_component_status": "pending",
            "composite_component_status_description": "Waiting on components",
            "app_sandbox_config": { "variables": { "project_id": "rp-restate-production" } },
        });
        let e = summarize_nuon("inltao53ge1b4pab3zo23si077", &state);
        assert_eq!(e.kind, NUON_KIND);
        assert_eq!(e.status.as_deref(), Some("active"));
        assert!(e
            .fields
            .iter()
            .any(|f| f.label == "components" && f.value.contains("Waiting on components")));
        assert!(!serde_json::to_string(&e)
            .unwrap()
            .contains("rp-restate-production"));
    }

    /// The block is concatenated into prompts unconditionally, so "nothing resolved" and
    /// "everything failed" must both produce nothing rather than a heading with no list.
    #[test]
    fn a_prompt_block_of_failures_is_empty() {
        let failed = Entity {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
            kind: "environment".into(),
            name: None,
            status: None,
            fields: vec![],
            related: vec![],
            members: vec![],
            error: Some("token expired".into()),
            fetched_at: Utc::now(),
        };
        assert!(prompt_block(&[]).is_empty());
        assert!(prompt_block(&[failed]).is_empty());
    }

    /// A **public** region is a bare name with no virtual object behind it, so an environment
    /// in one must name its region without asking anything to go and read it. Found live: the
    /// first warm pass cached a permanent `unknown id prefix in eu-west-1` box.
    #[test]
    fn an_environment_in_a_public_region_names_it_without_following_it() {
        let public = summarize(
            "environment",
            "env_1",
            &serde_json::json!({ "name": "e", "accountId": "acc_10vk05bJNjt1nLXvId4zh14", "region": "eu-west-1" }),
        );
        assert_eq!(public.related, vec!["acc_10vk05bJNjt1nLXvId4zh14"]);
        assert!(public
            .fields
            .iter()
            .any(|f| f.label == "region" && f.value == "eu-west-1"));

        // A BYOC region still gets followed — that edge is how Nuon is reached.
        let byoc = summarize(
            "environment",
            "env_1",
            &serde_json::json!({ "name": "e", "accountId": "acc_10vk05bJNjt1nLXvId4zh14", "region": "rgn_26p9031nve" }),
        );
        assert_eq!(
            byoc.related,
            vec!["acc_10vk05bJNjt1nLXvId4zh14", "rgn_26p9031nve"]
        );
    }

    /// A failed read must not be able to erase a good one.
    ///
    /// Found live, and it had been quietly running for days: the prod admin token expired,
    /// `rcc` fell back to `kubectl` and that failed too, and every mentioned id was retried on
    /// the ten-minute error TTL and overwritten with an empty entity. **132 environments** lost
    /// their name, tier, cluster and region — and their `accountId` edge with them, so they
    /// dropped out of their account's "Contains" list and out of the closure behind every
    /// subject that mentioned them. The only symptom was boxes going blank.
    #[tokio::test]
    async fn a_failed_read_keeps_the_last_good_copy() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let good = summarize(
            "environment",
            "env_201kpvffchrm1sq5p0qph42dqev",
            &serde_json::json!({
                "name": "janeway",
                "status": "PROVISIONING_READY",
                "tierId": "CUSTOM",
                "accountId": "acc_10vk05bJNjt1nLXvId4zh14",
            }),
        );
        store.put_cloud_entity(&good).unwrap();

        // `rcc_path` points at nothing, so every read fails — the same shape as an expired
        // credential, without needing one.
        let resolver = Resolver::new(
            config::Cloud {
                enabled: true,
                rcc_path: "/nonexistent/rcc".into(),
                ..Default::default()
            },
            store.clone(),
        );
        let after = resolver
            .refresh("env_201kpvffchrm1sq5p0qph42dqev")
            .await
            .expect("a failed read is still an answer");

        assert!(after.error.is_some(), "and it says why");
        assert_eq!(after.name.as_deref(), Some("janeway"), "the name survived");
        assert_eq!(after.status.as_deref(), Some("PROVISIONING_READY"));
        assert_eq!(after.fields, good.fields, "every field survived");
        assert_eq!(
            after.related,
            vec!["acc_10vk05bJNjt1nLXvId4zh14"],
            "and the edge — losing this is what emptied the account's Contains list"
        );
        // Persisted that way too, not merely returned that way.
        let stored = store
            .get_cloud_entity("env_201kpvffchrm1sq5p0qph42dqev")
            .unwrap()
            .unwrap();
        assert_eq!(stored.name.as_deref(), Some("janeway"));
        assert!(stored.error.is_some());

        // Stale-but-known is still worth grounding a prompt on; only a genuinely empty entity
        // is dropped.
        assert!(prompt_block(&[after]).contains("janeway"));
        let never_read = resolver.failed("env_201kpe1278xqa4ne187w8fbp640", "boom");
        assert!(never_read.name.is_none());
        assert!(prompt_block(&[never_read]).is_empty());
    }

    /// The edit is a **merge**, and the reason is that `PutState` is a whole-document replace.
    ///
    /// Everything already under `resourceOverrides` — replicas, a hand-set cpu request, env-var
    /// tombstones, throttling — has to come back out untouched. Rebuilding the object would
    /// delete every one of them, and `rcc` would ask about it in a confirmation prompt that
    /// runs with no human attached to answer it.
    #[test]
    fn setting_resources_keeps_every_override_it_was_not_asked_about() {
        let mut doc = serde_json::json!({
            "environmentId": "env_201kpvffchrm1sq5p0qph42dqev",
            "name": "janeway",
            "resourceOverrides": {
                "replicas": 3,
                "throttling": { "actions": { "rate": 1000 } },
                "resources": {
                    "requests": { "cpu": "2", "memory": "4Gi" },
                    "limits": { "cpu": "4", "memory": "8Gi" }
                }
            }
        });
        let changed = apply_resource_overrides(&mut doc, Some(16), None).unwrap();

        let o = &doc["resourceOverrides"];
        // Both sides moved, and only the memory on them.
        assert_eq!(o["resources"]["requests"]["memory"], "16Gi");
        assert_eq!(o["resources"]["limits"]["memory"], "16Gi");
        assert_eq!(
            o["resources"]["requests"]["cpu"], "2",
            "cpu request survived"
        );
        assert_eq!(o["resources"]["limits"]["cpu"], "4", "cpu limit survived");
        // And nothing else in the object was touched.
        assert_eq!(o["replicas"], 3);
        assert_eq!(o["throttling"]["actions"]["rate"], 1000);
        // Storage was not asked about, so it is not set at all — an absent floor and a floor
        // of zero are different things.
        assert!(o.get("minimumStorageRequestBytes").is_none());
        // The document's own fields are untouched, which is what makes the replace safe.
        assert_eq!(doc["name"], "janeway");
        assert_eq!(changed.len(), 1);
    }

    /// The unit conversions, which are the whole reason this is not "edit the JSON yourself".
    ///
    /// Memory is a Kubernetes quantity and storage is a raw byte count, in the same object.
    #[test]
    fn gib_becomes_a_quantity_for_memory_and_bytes_for_storage() {
        let mut doc = serde_json::json!({ "name": "e" });
        let changed = apply_resource_overrides(&mut doc, Some(8), Some(100)).unwrap();
        let o = &doc["resourceOverrides"];
        assert_eq!(o["resources"]["requests"]["memory"], "8Gi");
        assert_eq!(o["resources"]["limits"]["memory"], "8Gi");
        // 100 GiB, the figure from the runbook's own example.
        assert_eq!(o["minimumStorageRequestBytes"], 107_374_182_400i64);
        assert_eq!(changed.len(), 2);
        assert!(changed[1].contains("107374182400"), "{changed:?}");
    }

    /// An environment with no overrides yet is the common case, not an error.
    #[test]
    fn an_environment_with_no_overrides_grows_the_object() {
        let mut doc = serde_json::json!({ "name": "e" });
        apply_resource_overrides(&mut doc, None, Some(64)).unwrap();
        assert_eq!(
            doc["resourceOverrides"]["minimumStorageRequestBytes"],
            68_719_476_736i64
        );
    }

    /// Nothing to change is refused rather than written: an edit that rewrites the document
    /// identically still costs a `PutState` and a confirmation.
    #[test]
    fn setting_neither_resource_is_refused() {
        let mut doc = serde_json::json!({ "name": "e" });
        assert!(apply_resource_overrides(&mut doc, None, None).is_err());
        assert!(doc.get("resourceOverrides").is_none(), "and wrote nothing");
    }

    /// The two links, built from fields the cache already holds.
    ///
    /// The values are not interchangeable with the ids, which is the whole reason this is a
    /// function rather than a format string at the call site: `cloud-environment.ts` keys
    /// every query on `namespace="$restate_instance"`, so an environment is addressed by its
    /// **hyphenated cluster name**, and `cloud-region.ts` populates its picker from the deploy
    /// config's region names, which for BYOC are **Nuon install ids** rather than `rgn_` ids.
    /// Getting either wrong renders a dashboard perfectly with no data on it.
    #[test]
    fn an_environment_and_a_region_each_link_to_their_own_dashboard() {
        let host = "https://restateprod.grafana.net";

        let env = summarize(
            "environment",
            "env_201k479bd9ch7aw899my63z2pkr",
            &serde_json::json!({
                "name": "production",
                "clusterName": "env-201k479bd9ch7aw899my63z2pkr",
            }),
        );
        assert_eq!(
            grafana_url(&env, host).as_deref(),
            Some(
                "https://restateprod.grafana.net/d/cloud-environment\
                 ?var-restate_instance=env-201k479bd9ch7aw899my63z2pkr"
            )
        );

        // No `clusterName` — an environment whose read was partial. The namespace is the id in
        // Kubernetes spelling, so the link is still right rather than absent.
        let bare = summarize(
            "environment",
            "env_201k479bd9ch7aw899my63z2pkr",
            &serde_json::json!({ "name": "production" }),
        );
        assert_eq!(grafana_url(&bare, host), grafana_url(&env, host));

        let region = summarize(
            "region",
            "rgn_26p9031nve",
            &serde_json::json!({
                "regionDisplayName": "prod/us-east1",
                "nuonInstallId": "inltao53ge1b4pab3zo23si077",
            }),
        );
        assert_eq!(
            grafana_url(&region, host).as_deref(),
            Some(
                "https://restateprod.grafana.net/d/cloud-region\
                 ?var-region=inltao53ge1b4pab3zo23si077"
            )
        );
        // The install reaches the same dashboard by its own id — it *is* the region, as far
        // as the metrics are labelled.
        let install = summarize_nuon("inltao53ge1b4pab3zo23si077", &serde_json::json!({}));
        assert_eq!(grafana_url(&install, host), grafana_url(&region, host));
    }

    /// The kinds with nothing to watch, and the region that cannot be watched.
    ///
    /// `None` is the answer, not a link to the dashboard's default selection: a region picker
    /// showing a stranger's install is a worse answer than no button.
    #[test]
    fn the_kinds_without_metrics_get_no_link() {
        let host = "https://restateprod.grafana.net";
        for (kind, id, state) in [
            (
                "account",
                "acc_10vk05bJNjt1nLXvId4zh14",
                serde_json::json!({}),
            ),
            (
                "organization",
                "org_10vj37k7NcqTaIavuJRsOcv",
                serde_json::json!({}),
            ),
            ("user", "usr_10uXl8CPozSuQiKolKUfqVI", serde_json::json!({})),
            (
                "apikey",
                "key_10vkhgbtpzFTNSWc2RnOY1S",
                serde_json::json!({}),
            ),
            // A public region, or one naming an install that has been deleted: a region
            // document with no `nuonInstallId` at all.
            (
                "region",
                "rgn_24s0xbp4g4",
                serde_json::json!({ "type": "aws" }),
            ),
        ] {
            assert_eq!(
                grafana_url(&summarize(kind, id, &state), host),
                None,
                "{kind} should have no dashboard"
            );
        }
    }

    /// Two stacks, and the realm is what says which estate an entity belongs to. `eu` shares
    /// the prod stack exactly as it shares `rcc`'s prod credential.
    #[test]
    fn the_realm_picks_the_grafana_stack() {
        assert_eq!(grafana_host("dev"), "https://restate.grafana.net");
        for prod in ["us", "eu", "prod", "", "  "] {
            assert_eq!(
                grafana_host(prod),
                "https://restateprod.grafana.net",
                "{prod:?} should be the prod stack"
            );
        }
    }

    /// A configured host with a trailing slash must not produce a doubled one.
    #[test]
    fn a_trailing_slash_on_the_host_is_not_carried_into_the_link() {
        let env = summarize("environment", "env_1", &serde_json::json!({}));
        let url = grafana_url(&env, "https://restateprod.grafana.net/").unwrap();
        assert!(
            url.starts_with("https://restateprod.grafana.net/d/"),
            "{url}"
        );
    }

    /// The traversal guard, independent of any one document's shape.
    #[test]
    fn only_things_with_state_behind_them_are_worth_queueing() {
        assert!(is_resolvable("env_201kpvffchrm1sq5p0qph42dqev"));
        assert!(is_resolvable("inltao53ge1b4pab3zo23si077"));
        // A nuon-native region: no `rgn_` id, but a real region object behind it.
        assert!(is_resolvable("inl3jdnqyt2p0vqiiglfhe1da9.nuon"));
        assert!(!is_resolvable("eu-west-1"));
        assert!(!is_resolvable("us-east-1"));
        assert!(!is_resolvable(""));
    }

    /// A nuon-native region is a region, and must not be mistaken for the install it is named
    /// after — different objects, read by different CLIs. Found live: two of these were the
    /// only entities a full estate warm could not resolve.
    #[test]
    fn a_nuon_twin_is_a_region_and_the_bare_install_is_not() {
        assert_eq!(kind_of("inl3jdnqyt2p0vqiiglfhe1da9.nuon"), Some("region"));
        assert!(!is_nuon_install("inl3jdnqyt2p0vqiiglfhe1da9.nuon"));
        assert_eq!(kind_of("inl3jdnqyt2p0vqiiglfhe1da9"), None);
        assert!(is_nuon_install("inl3jdnqyt2p0vqiiglfhe1da9"));
        // Not every dotted thing ending in `.nuon` is one.
        assert_eq!(kind_of("something.nuon"), None);
    }

    /// An environment in a nuon-native region names it as its `region`, so that edge has to
    /// survive the public-region filter.
    #[test]
    fn an_environment_in_a_nuon_native_region_still_follows_it() {
        let e = summarize(
            "environment",
            "env_1",
            &serde_json::json!({ "name": "e", "region": "inl3jdnqyt2p0vqiiglfhe1da9.nuon" }),
        );
        assert_eq!(e.related, vec!["inl3jdnqyt2p0vqiiglfhe1da9.nuon"]);
    }

    /// `rcc` prefixes a selector-resolved read with a `#` note, which is fatal to a parser.
    #[test]
    fn the_note_rcc_prints_above_a_resolved_read_is_not_part_of_the_document() {
        let out = "# inl3jdnqyt2p0vqiiglfhe1da9.nuon → inl3jdnqyt2p0vqiiglfhe1da9.nuon  (Balerion AI us-east-1)\n\
                   {\"regionDisplayName\": \"Balerion AI us-east-1\", \"note\": \"# not a comment\"}";
        let v: Value = serde_json::from_str(strip_comments(out)).expect("parses");
        assert_eq!(
            v.get("regionDisplayName").and_then(Value::as_str),
            Some("Balerion AI us-east-1")
        );
        // A `#` inside the document is untouched — only leading lines are dropped.
        assert_eq!(
            v.get("note").and_then(Value::as_str),
            Some("# not a comment")
        );
        // A document with no note is unchanged.
        assert_eq!(strip_comments("{\"a\":1}"), "{\"a\":1}");
    }

    /// `nuon` reports a missing install on stdout and exits 1, so the useful sentence is in
    /// the envelope rather than in stderr. Every such failure used to read as
    /// "`nuon` exited with status 1:" and nothing else.
    #[test]
    fn a_json_envelope_failure_yields_its_sentence() {
        let raw = r#"{"description":"not found","error":"unable to get install inlrkygbl28g4s5dbi5bx6wqcq: unable to get install: record not found","user_error":true}"#;
        assert_eq!(
            cli_error(raw),
            "unable to get install inlrkygbl28g4s5dbi5bx6wqcq: unable to get install: record not found"
        );
        // Plain stderr still comes through as its first line.
        assert_eq!(cli_error("boom\nsecond line"), "boom");
    }

    /// Real `rcc regions` output. The display-name column contains spaces and the Nuon column
    /// is `-` for a public region, so "split on whitespace" has to mean the *first* field
    /// only — and the public region has to survive, since environments live in it.
    #[test]
    fn the_region_table_yields_ids_including_the_public_ones() {
        let out = "\
REGION ID                        DISPLAY NAME                                   TYPE        PROVIDER REGION  NUON INSTALL
eu-west-1                        EU                                             aws         -                -
rgn_24s0xbp4g4                   everpure/us-west1                              nuon-gcp    us-west1         inlb9va7lubgiile0htf348u5r
rgn_3fp5f6s6vv                   BYOC GCP Canary (this region is dev-only now)  nuon-gcp    europe-west2     inlxjgu6pj4l2u7vbb0063cafj";
        assert_eq!(
            parse_region_table(out),
            vec!["eu-west-1", "rgn_24s0xbp4g4", "rgn_3fp5f6s6vv"]
        );
        // Only the prefixed ones are virtual objects with state to read.
        let readable = parse_region_table(out)
            .into_iter()
            .filter(|id| kind_of(id).is_some())
            .count();
        assert_eq!(readable, 2);
    }

    /// The real thing, against the operator's own `rcc` and `nuon`. Ignored by default —
    /// it needs a logged-in CLI and reaches the production control plane, read-only.
    ///
    ///   cargo test cloud::tests::resolves_a_real_environment -- --ignored --nocapture \
    ///     MUGGLEBOT_ENV=env_…
    ///
    /// What it is actually checking is the part unit tests cannot: that the closure walks
    /// environment → account → organization and environment → region → Nuon install against
    /// live documents, so a field the platform renames is caught here rather than by an empty
    /// panel three weeks later.
    #[tokio::test]
    #[ignore]
    async fn resolves_a_real_environment() {
        let id = std::env::var("MUGGLEBOT_ENV")
            .expect("set MUGGLEBOT_ENV to the env_… you want resolved");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let resolver = Resolver::new(
            config::Cloud {
                enabled: true,
                ..Default::default()
            },
            store,
        );
        let entities = resolver.resolve_closure(std::slice::from_ref(&id)).await;
        for e in &entities {
            println!("{:?}", e);
        }
        let kinds: Vec<&str> = entities.iter().map(|e| e.kind.as_str()).collect();
        for expected in [
            "environment",
            "account",
            "organization",
            "region",
            NUON_KIND,
        ] {
            assert!(kinds.contains(&expected), "no {expected} in {kinds:?}");
        }
        assert!(
            entities.iter().all(|e| e.error.is_none()),
            "every hop resolved"
        );
    }

    /// The user path against the operator's own `rcc`, read-only. Ignored by default — it
    /// needs a logged-in CLI *and* an AWS session, and it reaches production.
    ///
    ///   cargo test cloud::tests::resolves_a_real_user -- --ignored --nocapture
    ///     MUGGLEBOT_ACCOUNT=acc_…
    ///
    /// What it checks is the half no unit test can: that an account really does name its
    /// people, that `rcc describe` really does print two documents in one stream, and that the
    /// profile half really does carry a name — three things that are true of a shape rather
    /// than of a schema, and that will break silently if the platform moves any of them.
    #[tokio::test]
    #[ignore]
    async fn resolves_a_real_user() {
        let account = std::env::var("MUGGLEBOT_ACCOUNT")
            .expect("set MUGGLEBOT_ACCOUNT to an acc_… whose people you want read");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let resolver = Resolver::new(
            config::Cloud {
                enabled: true,
                ..Default::default()
            },
            store,
        );
        let acc = resolver.resolve_one(&account).await.expect("account reads");
        println!("{acc:?}");
        assert!(!acc.members.is_empty(), "an account with nobody on it");

        for id in &acc.members {
            let user = resolver.resolve_one(id).await.expect("user reads");
            println!("{user:?}");
            assert_eq!(user.kind, "user");
            assert!(user.error.is_none());
        }
    }

    #[test]
    fn a_prompt_block_names_the_kind_the_id_and_the_fields() {
        let e = summarize(
            "environment",
            "env_201kpvffchrm1sq5p0qph42dqev",
            &serde_json::json!({ "name": "janeway", "status": "ACTIVE", "tierId": "CUSTOM" }),
        );
        let block = prompt_block(&[e]);
        assert!(block.contains("environment env_201kpvffchrm1sq5p0qph42dqev \"janeway\" [ACTIVE]"));
        assert!(block.contains("tier: CUSTOM"));
    }
}
