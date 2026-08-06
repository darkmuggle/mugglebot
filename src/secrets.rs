//! The credential store.
//!
//! Source tokens, model API keys, and authed-context secrets live in the SQLite
//! DB — not the macOS Keychain, which is scoped to a login session and to the
//! identity of the process asking, and so can't be read by a background endpoint
//! without a GUI prompt. The honest form of the security argument is in AGENTS.md:
//! the same file already holds every signal body MuggleBot has ingested, so the
//! file is the sensitive artifact either way.
//!
//! Three rules this module exists to enforce:
//!
//! 1. **Write-only from outside.** [`Secrets::status`] answers *whether* a secret
//!    is set and when it changed. Nothing outside this module can ask for a value
//!    except [`Secrets::get`], which callers use at the moment of the request —
//!    never at boot — so rotating a token takes effect on the next poll.
//! 2. **One place can decrypt.** The store handles opaque bytes; the key lives
//!    here and nowhere else.
//! 3. **Values never reach a log.** Every value loaded or written is registered
//!    with the [`Scrubber`], which rewrites the formatted log stream. Belt and
//!    braces against a `{:?}` on a struct that happens to carry a token.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use crate::store::Store;

/// Byte 0 of a stored value: how to read the rest.
const TAG_PLAINTEXT: u8 = 0x00;
const TAG_SEALED: u8 = 0x01;

/// AES-GCM nonce length.
const NONCE_LEN: usize = 12;

/// Where the per-database KDF salt lives.
const SALT_KEY: &str = "secrets.kdf_salt";
const SALT_LEN: usize = 16;

/// The credential names the config page offers. Anything else can still be stored
/// (authed context sources name their own), this is just the UI's list.
pub const KNOWN_SECRETS: &[&str] = &[
    "github",
    "slack",
    "granola",
    // incident.io. Listed here because this list *is* the config page's form: a credential
    // absent from it cannot be entered through the UI at all, however ready the rest of the
    // integration is.
    "incident",
    // Grafana. A **Viewer** service-account token: this tier only ever reads, and a
    // Viewer token cannot silence an alert or save a dashboard even if something tried.
    "grafana",
    "ollama",
    "anthropic",
    "openai",
];

/// What the outside world may know about a secret.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecretStatus {
    pub name: String,
    pub set: bool,
    pub updated_at: Option<DateTime<Utc>>,
}

pub struct Secrets {
    store: Arc<Store>,
    /// `None` → values are stored as plaintext bytes. `Some` → sealed with
    /// AES-256-GCM under a key derived from `$MUGGLEBOT_MASTER_KEY`.
    key: Option<[u8; 32]>,
    scrubber: Scrubber,
}

impl Secrets {
    /// Open the credential store. With `encrypt`, requires `$MUGGLEBOT_MASTER_KEY`
    /// and re-seals any plaintext rows on the way through — enabling encryption is
    /// a one-time upgrade rather than something that only applies to new writes.
    ///
    /// Without `encrypt`, already-sealed rows are still readable *if* the master
    /// key is present; if it isn't, reading one is a clear error rather than a
    /// mysterious empty token.
    ///
    /// `master` is the passphrase from `$MUGGLEBOT_MASTER_KEY`, read by the caller
    /// rather than here: a module that reaches into the process environment can't be
    /// tested two ways in one process, and the environment is the daemon's business.
    pub fn open(
        store: Arc<Store>,
        encrypt: bool,
        master: Option<String>,
        scrubber: Scrubber,
    ) -> Result<Self> {
        let master = master.filter(|v| !v.is_empty());
        if encrypt && master.is_none() {
            bail!(
                "[secrets] encrypt = true but $MUGGLEBOT_MASTER_KEY is unset — \
                 refusing to start rather than silently storing plaintext"
            );
        }
        let key = match master {
            Some(pass) => Some(derive_key(&store, &pass)?),
            None => None,
        };
        let me = Self {
            store,
            key,
            scrubber,
        };
        if encrypt {
            me.reseal_plaintext()?;
        }
        me.register_all_with_scrubber()?;
        Ok(me)
    }

    /// Fetch a secret by name. Call this at the point of use.
    pub fn get(&self, name: &str) -> Result<Option<String>> {
        let Some(raw) = self.store.secret_raw(name)? else {
            return Ok(None);
        };
        let value = self.unseal(name, &raw)?;
        self.scrubber.register(&value);
        Ok(Some(value))
    }

    /// Like [`Self::get`] but swallows errors into `None` — for the many call
    /// sites where a missing credential is an ordinary "that source is off".
    pub fn get_opt(&self, name: &str) -> Option<String> {
        match self.get(name) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("reading secret '{name}' failed: {e:#}");
                None
            }
        }
    }

    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        if value.is_empty() {
            bail!("refusing to store an empty secret for '{name}'");
        }
        self.scrubber.register(value);
        self.store.secret_put_raw(name, &self.seal(value)?)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        self.store.secret_delete(name)
    }

    /// Whether each of `names` is set, and when it last changed. Never values.
    pub fn status(&self, names: &[&str]) -> Result<Vec<SecretStatus>> {
        let stored = self.store.secret_names()?;
        let mut out: Vec<SecretStatus> = names
            .iter()
            .map(|n| {
                let hit = stored.iter().find(|(name, _)| name == n);
                SecretStatus {
                    name: (*n).to_string(),
                    set: hit.is_some(),
                    updated_at: hit.map(|(_, at)| *at),
                }
            })
            .collect();
        // Anything stored that isn't a known name — an authed context source's
        // credential — is still reported, so the config page can't hide a token
        // the operator forgot they set.
        for (name, at) in stored {
            if !names.contains(&name.as_str()) {
                out.push(SecretStatus {
                    name,
                    set: true,
                    updated_at: Some(at),
                });
            }
        }
        Ok(out)
    }

    pub fn scrubber(&self) -> &Scrubber {
        &self.scrubber
    }

    /// A plaintext credential store over an in-memory DB, for tests that only need
    /// the handle to construct something else.
    #[cfg(test)]
    pub fn for_tests(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self::open(store, false, None, Scrubber::new()).expect("test secrets"))
    }

    // ---- sealing ------------------------------------------------------------

    fn seal(&self, value: &str) -> Result<Vec<u8>> {
        let Some(key) = self.key else {
            let mut out = Vec::with_capacity(value.len() + 1);
            out.push(TAG_PLAINTEXT);
            out.extend_from_slice(value.as_bytes());
            return Ok(out);
        };
        use aes_gcm::aead::{Aead, KeyInit, OsRng};
        use aes_gcm::{AeadCore, Aes256Gcm, Key};
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, value.as_bytes())
            .map_err(|_| anyhow::anyhow!("sealing secret failed"))?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
        out.push(TAG_SEALED);
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn unseal(&self, name: &str, raw: &[u8]) -> Result<String> {
        match raw.split_first() {
            Some((&TAG_PLAINTEXT, rest)) => Ok(String::from_utf8(rest.to_vec())
                .with_context(|| format!("secret '{name}' is not valid UTF-8"))?),
            Some((&TAG_SEALED, rest)) => {
                let Some(key) = self.key else {
                    bail!(
                        "secret '{name}' is encrypted but $MUGGLEBOT_MASTER_KEY is unset — \
                         set it, or delete and re-enter the secret"
                    );
                };
                if rest.len() <= NONCE_LEN {
                    bail!("secret '{name}' is truncated");
                }
                use aes_gcm::aead::{Aead, KeyInit};
                use aes_gcm::{Aes256Gcm, Key, Nonce};
                let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
                let (nonce, ct) = rest.split_at(NONCE_LEN);
                let pt = cipher.decrypt(Nonce::from_slice(nonce), ct).map_err(|_| {
                    anyhow::anyhow!(
                        "unsealing secret '{name}' failed — wrong $MUGGLEBOT_MASTER_KEY?"
                    )
                })?;
                Ok(String::from_utf8(pt)
                    .with_context(|| format!("secret '{name}' is not valid UTF-8"))?)
            }
            Some((tag, _)) => bail!("secret '{name}' has unknown format tag {tag:#04x}"),
            None => bail!("secret '{name}' is empty"),
        }
    }

    fn reseal_plaintext(&self) -> Result<()> {
        let mut resealed = 0usize;
        for (name, raw) in self.store.secrets_raw()? {
            if raw.first() != Some(&TAG_PLAINTEXT) {
                continue;
            }
            let value = self.unseal(&name, &raw)?;
            self.store.secret_put_raw(&name, &self.seal(&value)?)?;
            resealed += 1;
        }
        if resealed > 0 {
            tracing::info!("sealed {resealed} previously-plaintext secret(s)");
        }
        Ok(())
    }

    /// Teach the scrubber every stored value once at startup, so a token that is
    /// never read through `get` still can't be logged by something that has it
    /// from config.
    fn register_all_with_scrubber(&self) -> Result<()> {
        for (name, raw) in self.store.secrets_raw()? {
            if let Ok(v) = self.unseal(&name, &raw) {
                self.scrubber.register(&v);
            }
        }
        Ok(())
    }
}

/// Derive the sealing key from the operator's passphrase and a per-database salt.
/// Argon2id, so a weak passphrase costs an attacker something.
fn derive_key(store: &Store, pass: &str) -> Result<[u8; 32]> {
    let salt = match store.meta_get(SALT_KEY)? {
        Some(s) if s.len() == SALT_LEN => s,
        _ => {
            use aes_gcm::aead::rand_core::RngCore;
            let mut s = vec![0u8; SALT_LEN];
            aes_gcm::aead::OsRng.fill_bytes(&mut s);
            store.meta_put(SALT_KEY, &s)?;
            s
        }
    };
    let mut key = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(pass.as_bytes(), &salt, &mut key)
        .map_err(|e| anyhow::anyhow!("deriving key from $MUGGLEBOT_MASTER_KEY: {e}"))?;
    Ok(key)
}

/// Rewrites secret values out of the formatted log stream.
///
/// Field-level redaction would need every call site to opt in, and the failure
/// mode we actually care about is the one nobody opted into — a `{:?}` on a
/// request struct, an error message quoting a URL with a token in it. Filtering
/// the bytes on their way to stderr catches those without asking anyone to
/// remember.
#[derive(Clone, Default)]
pub struct Scrubber {
    values: Arc<RwLock<HashSet<String>>>,
}

impl Scrubber {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start scrubbing this value. Very short values are ignored: redacting every
    /// occurrence of a three-character string would destroy the logs, and a
    /// three-character credential has bigger problems.
    pub fn register(&self, value: &str) {
        if value.len() < 8 {
            return;
        }
        if let Ok(mut set) = self.values.write() {
            set.insert(value.to_string());
        }
    }

    fn scrub(&self, mut line: String) -> String {
        let Ok(set) = self.values.read() else {
            return line;
        };
        for v in set.iter() {
            if line.contains(v.as_str()) {
                line = line.replace(v.as_str(), "***");
            }
        }
        line
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Scrubber {
    type Writer = ScrubbingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ScrubbingWriter {
            scrubber: self.clone(),
        }
    }
}

pub struct ScrubbingWriter {
    scrubber: Scrubber,
}

impl std::io::Write for ScrubbingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let out = match std::str::from_utf8(buf) {
            Ok(s) => self.scrubber.scrub(s.to_string()).into_bytes(),
            // Non-UTF-8 log output can't contain a token we'd recognise; pass it on.
            Err(_) => buf.to_vec(),
        };
        let mut err = std::io::stderr();
        err.write_all(&out)?;
        // Report the caller's length: we may have written fewer bytes after
        // redaction, and a short write makes `tracing` retry the tail.
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().unwrap())
    }

    fn open(store: Arc<Store>, encrypt: bool, master: Option<&str>) -> Result<Secrets> {
        Secrets::open(store, encrypt, master.map(str::to_string), Scrubber::new())
    }

    #[test]
    fn plaintext_roundtrip_and_status() {
        let s = open(store(), false, None).unwrap();
        assert!(s.get("github").unwrap().is_none());
        s.set("github", "ghp_abcdefghijklmnop").unwrap();
        assert_eq!(s.get("github").unwrap().unwrap(), "ghp_abcdefghijklmnop");

        let st = s.status(&["github", "slack"]).unwrap();
        let gh = st.iter().find(|x| x.name == "github").unwrap();
        assert!(gh.set && gh.updated_at.is_some());
        assert!(!st.iter().find(|x| x.name == "slack").unwrap().set);

        s.delete("github").unwrap();
        assert!(s.get("github").unwrap().is_none());
    }

    #[test]
    fn sealed_values_are_not_stored_in_the_clear() {
        let db = store();
        let s = open(db.clone(), true, Some("correct horse battery staple")).unwrap();
        s.set("slack", "xoxb-secret-token-value").unwrap();

        let raw = db.secret_raw("slack").unwrap().unwrap();
        assert_eq!(raw[0], TAG_SEALED);
        assert!(!String::from_utf8_lossy(&raw).contains("xoxb-secret-token-value"));
        assert_eq!(s.get("slack").unwrap().unwrap(), "xoxb-secret-token-value");
    }

    #[test]
    fn enabling_encryption_seals_existing_plaintext() {
        let db = store();
        {
            let plain = open(db.clone(), false, None).unwrap();
            plain.set("granola", "gran_plaintext_value").unwrap();
            assert_eq!(db.secret_raw("granola").unwrap().unwrap()[0], TAG_PLAINTEXT);
        }
        let sealed = open(db.clone(), true, Some("another passphrase entirely")).unwrap();
        assert_eq!(db.secret_raw("granola").unwrap().unwrap()[0], TAG_SEALED);
        assert_eq!(
            sealed.get("granola").unwrap().unwrap(),
            "gran_plaintext_value"
        );
    }

    #[test]
    fn encryption_without_a_master_key_refuses_to_start() {
        assert!(open(store(), true, None).is_err());
        assert!(open(store(), true, Some("")).is_err());
    }

    #[test]
    fn the_wrong_passphrase_errors_rather_than_returning_nonsense() {
        let db = store();
        open(db.clone(), true, Some("the right passphrase"))
            .unwrap()
            .set("github", "ghp_the_real_token")
            .unwrap();
        let wrong = open(db, false, Some("a different passphrase")).unwrap();
        assert!(wrong.get("github").is_err());
    }

    #[test]
    fn scrubber_rewrites_registered_values_and_ignores_short_ones() {
        let s = Scrubber::new();
        s.register("ghp_a_long_enough_token");
        s.register("abc"); // too short to redact safely
        assert_eq!(
            s.scrub("auth: ghp_a_long_enough_token done".into()),
            "auth: *** done"
        );
        assert_eq!(s.scrub("abc".into()), "abc");
    }
}
