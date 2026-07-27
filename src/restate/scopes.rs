//! vqueue scopes and limit keys, in one place (Phase 6).
//!
//! A scope is a namespace for concurrency control: matching invocations are held in
//! a virtual queue until a slot frees. The limits are configured in
//! `[restate.limits]` and applied to the server's rule book at boot.
//!
//! Naming them here rather than at the call sites is the point — a scope string
//! typo'd at one of a dozen call sites silently opts that call out of the limit,
//! which shows up as "why is the laptop on fire" rather than as an error.

/// One Ollama, one GPU. A 33B model with four concurrent requests is slower *and*
/// worse than a queue of one — the single strongest fit for a concurrency limit here.
pub const LOCAL_LLM: &str = "local-llm";

/// Every concurrent invocation is real money. Limit keys per tier bound the
/// expensive one separately.
pub const CLOUD_LLM: &str = "cloud-llm";
pub const TIER_SONNET: &str = "sonnet";
pub const TIER_OPUS: &str = "opus";

/// Keep burst concurrency under the API's tolerance; indexing an org is otherwise a
/// self-inflicted rate limit. (Concurrency is not rate — the per-hour budget is a
/// token bucket, see AGENTS.md.)
pub const GITHUB: &str = "github";

/// One Chrome, one investigation at a time. Replaces a claim-a-row worker loop.
pub const BROWSER: &str = "browser";

/// Two clones at once is disk-bound; two clones *of the same repo* is a corrupt
/// working tree, which is what the per-repo limit key prevents.
pub const CHECKOUT: &str = "checkout";

/// The org crawl — one at a time, and deliberately not in [`GITHUB`], which allows four.
///
/// Two concurrent crawls enumerate the same repos, select the same uncarded ones in the same
/// order, and clone them into the same directory. This is the declarative version of that
/// guarantee: the scheduler may submit as often as it likes and Restate runs one.
///
/// It is only a guarantee when `[restate].vqueues = true`. With vqueues off nothing is held
/// back, which is why the crawl's batch size is *also* sized to finish inside the scheduler's
/// catch-up cadence — belt as well as braces, since the failure is a corrupt checkout.
pub const REPO_INDEX: &str = "repo-index";

/// Apply `[restate.limits]` to the server's rule book.
///
/// Configuration is the source of truth rather than someone's shell history: a limit
/// that exists only as a `restate rules set` you ran once is a limit that silently
/// disappears the next time the cluster is wiped — and wiping is *required* to enable
/// vqueues in the first place.
///
/// Best-effort. A server without the experimental flags rejects this, which is a
/// warning and not a failed boot: the daemon works without concurrency limits, it
/// just works less politely.
pub async fn apply_rules(cfg: &crate::config::RestateConfig) -> anyhow::Result<()> {
    if !cfg.vqueues {
        // Loud when the operator has clearly *tried* to set limits. `[restate.limits]` and
        // `[restate].vqueues` are separate settings and the limits do nothing without the
        // flag, so a tuned block with the flag off is a config that reads as configured and
        // behaves as unconfigured — worth a warning naming the one line that fixes it.
        if cfg.limits != crate::config::RestateLimits::default() {
            tracing::warn!(
                "restate: [restate.limits] is customized but [restate].vqueues = false, so \
                 none of it is applied. Set `vqueues = true` under [restate] — note it needs \
                 the server's experimental flag and a fresh cluster."
            );
        } else {
            tracing::debug!("restate: [restate].vqueues = false — no concurrency limits applied");
        }
        return Ok(());
    }
    let l = &cfg.limits;
    let rules = serde_json::json!([
        rule("*", 32, "global default"),
        rule(LOCAL_LLM, l.local_llm, "one Ollama, one GPU"),
        rule(CLOUD_LLM, l.cloud_llm, "bound metered spend"),
        rule(GITHUB, l.github, "stay under the API's burst tolerance"),
        rule(BROWSER, l.browser, "one Chrome, one investigation"),
        rule(
            CHECKOUT,
            l.checkout,
            "disk-bound; per-repo limit keys prevent a corrupt tree"
        ),
        rule(REPO_INDEX, l.repo_index, "one org crawl at a time"),
    ]);
    let url = format!("{}/limits/rules", cfg.admin.trim_end_matches('/'));
    let resp = reqwest::Client::new().put(&url).json(&rules).send().await;
    match resp {
        Ok(r) if r.status().is_success() => {
            tracing::info!(
                "restate: concurrency limits applied (local-llm {}, cloud-llm {}, github {}, \
                 browser {}, checkout {}, repo-index {})",
                l.local_llm,
                l.cloud_llm,
                l.github,
                l.browser,
                l.checkout,
                l.repo_index
            );
            Ok(())
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            tracing::warn!(
                "restate: applying concurrency limits failed ({status}): {body} — \
                 vqueues need RESTATE_EXPERIMENTAL_ENABLE_VQUEUES on a fresh cluster"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!("restate: admin API unreachable for rule setup: {e}");
            Ok(())
        }
    }
}

fn rule(pattern: &str, concurrency: u32, description: &str) -> serde_json::Value {
    serde_json::json!({
        "pattern": pattern,
        "description": description,
        "limits": { "concurrency": concurrency },
    })
}

#[cfg(test)]
mod tests {
    use crate::config::RestateLimits;

    /// The trap this warns about, observed live: a config with a hand-tuned
    /// `[restate.limits]` block and no `vqueues = true`. Every limit was inert — including
    /// the one meant to stop four concurrent org crawls — while the config read as though
    /// they were in force.
    #[test]
    fn a_customized_limits_block_is_distinguishable_from_an_untouched_one() {
        let untouched = RestateLimits::default();
        assert_eq!(untouched, RestateLimits::default());

        let tuned = RestateLimits {
            local_llm: 2,
            ..RestateLimits::default()
        };
        assert_ne!(
            tuned,
            RestateLimits::default(),
            "a tuned block must be detectable, or the warning can never fire"
        );
    }

    /// One crawl at a time is the default, and it is the value the hazard depends on.
    #[test]
    fn the_org_crawl_defaults_to_one_at_a_time() {
        assert_eq!(RestateLimits::default().repo_index, 1);
        // Not sharing `github`'s allowance, which is sized for API burst tolerance.
        assert!(RestateLimits::default().github > 1);
    }
}
