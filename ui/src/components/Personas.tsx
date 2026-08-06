import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import type {
  Persona,
  PersonaCandidate,
  PersonaDetail,
  PersonaEvidence,
  PersonaPrediction,
  PersonaStats,
  PersonaSummary,
  PersonaTrait,
  PersonaContextEntry,
  SmeArea,
} from "../types";

/// The personas page — the people MuggleBot models, and what it believes about them.
///
/// Deliberately **not** on the board. The board answers "what does my work need from me";
/// this answers "who is this work with", and the two lists have nothing in common. A persona
/// never competes for attention: it is a lens read on request.
///
/// Three things this page has to get right, because the feature is worthless — or worse than
/// worthless — without them:
///
/// 1. **Every claim shows its citations.** A trait is one falsifiable sentence plus the
///    excerpts behind it, and clicking through to the excerpts is one interaction away. A
///    profile you cannot check is a profile you should not act on.
/// 2. **Contested claims look contested.** Counter-evidence is rendered, not resolved. A
///    reviewer who blocks on tests four times in seven is a different fact from one who always
///    does, and flattening that would be the more confident and less useful display.
/// 3. **What was refused is visible.** On a first pass the removed list is routinely longer
///    than the profile. Hiding it would make the filter undebuggable and would hide how much
///    of the model's output is noise.
export default function Personas(props: { onChat: (persona: string) => void }) {
  const [list, { refetch }] = createResource(() => api.listPersonas());
  const [selected, setSelected] = createSignal<string | null>(null);
  const [adding, setAdding] = createSignal(false);

  const open = (slug: string) => setSelected((s) => (s === slug ? null : slug));

  return (
    <div class="page">
      <Show
        when={list()?.enabled !== false}
        fallback={
          <section class="panel">
            <h3>Personas</h3>
            <p class="muted">
              Personas are off. Set <code>enabled = true</code> under{" "}
              <code>[personas]</code> in the config, then restart. It is off by default because
              this is the one feature here that models <em>people</em> rather than work, and
              that should be a decision rather than something the daemon starts doing.
            </p>
          </section>
        }
      >
        <section class="panel">
          <div class="panel-head">
            <h3>PERSONAS ({list()?.personas?.length ?? 0})</h3>
            <span class="tl-actions">
              <button onClick={() => setAdding((a) => !a)}>
                {adding() ? "close" : "+ model someone"}
              </button>
              <button disabled={list.loading} onClick={() => void refetch()}>
                refresh
              </button>
            </span>
          </div>
          <p class="muted">
            A candid behavioural model of one colleague, built from things they actually wrote.
            Select personas on an issue or pull request to predict how it will land — nothing
            here is ever posted anywhere.
          </p>

          <Show when={adding()}>
            <AddPersona
              onDone={() => {
                setAdding(false);
                void refetch();
              }}
            />
          </Show>

          <For
            each={list()?.personas}
            fallback={
              <p class="lane-empty">
                {list.loading
                  ? "Reading…"
                  : "Nobody modelled yet. Use “model someone” — the proposals are ranked by how much you actually deal with them."}
              </p>
            }
          >
            {(p) => (
              <PersonaRow p={p} open={selected() === p.slug} onToggle={() => open(p.slug)} />
            )}
          </For>
        </section>

        <Show when={selected()}>
          <PersonaPanel
            slug={selected()!}
            onChat={props.onChat}
            onGone={() => {
              setSelected(null);
              void refetch();
            }}
          />
        </Show>
      </Show>
    </div>
  );
}

/// One row: who they are, how much is behind the profile, and whether it is still filling in.
function PersonaRow(props: { p: PersonaSummary; open: boolean; onToggle: () => void }) {
  const p = () => props.p;
  return (
    <div class="persona-row" classList={{ open: props.open }} onClick={props.onToggle}>
      <span class="persona-name">
        {p().display_name}
        <Show when={p().role}>
          <span class="muted"> · {p().role}</span>
        </Show>
      </span>
      <span class="persona-ids">
        <For each={p().identities}>
          {(id) => (
            <span
              class={`chip src src-${id.source}`}
              classList={{ proposed: id.provenance === "proposed" }}
              data-tip={
                id.provenance === "proposed"
                  ? `Proposed, not confirmed — ${id.rationale ?? "a guess"}. No evidence is harvested through it until you confirm it.`
                  : `${id.source}: ${id.handle}`
              }
            >
              {id.handle}
              {id.provenance === "proposed" ? " ?" : ""}
            </span>
          )}
        </For>
        <Show when={!p().identities.length}>
          <span class="muted">no identity linked — nothing to harvest</span>
        </Show>
      </span>
      <span class="persona-counts">
        {/* Both numbers, because they answer different questions: how much has been read,
            and how much of it survived verification. A profile with 400 excerpts and 2
            traits is a real and informative state. */}
        <span data-tip="Excerpts harvested">{p().stats.evidence} ev</span>
        <span data-tip="Verified traits in the profile">{p().traits} traits</span>
        {/* The row's most important badge when it is present, and it means **you can fix
            this**: an unlinked source, an unsearchable handle, a token that cannot search.
            Deliberately not raised for a background pass waiting on GitHub budget — that is
            normal operation on a busy machine, and labelling it made a persona holding 289
            excerpts and 18 traits read as broken in perpetuity. The incomplete backfill has
            its own quieter badge below. */}
        <Show when={p().harvest_note}>
          <span class="badge badge-blocked" data-tip={p().harvest_note!}>
            needs you
          </span>
        </Show>
        <Show when={p().sme?.length}>
          <span
            class="chip sme-chip"
            classList={{ expert: !!p().sme![0].depth }}
            data-tip={
              p().sme![0].depth ??
              `${p().sme![0].reviews} reviews here — presence, not established expertise`
            }
          >
            {p().sme![0].area.split(/[:/]/).pop()}
          </span>
        </Show>
        <Show when={!p().backfill_complete && p().identities.length}>
          <span class="badge" data-tip={`History walk still going — reached ${p().walked_back_to ?? "the start"}`}>
            backfilling
          </span>
        </Show>
      </span>
      <time class="row-when">
        {p().profiled_at ? new Date(p().profiled_at!).toLocaleDateString() : "never profiled"}
      </time>
    </div>
  );
}

/// The full profile: counted facts, traits with citations, refusals, and recent predictions.
function PersonaPanel(props: {
  slug: string;
  onChat: (persona: string) => void;
  onGone: () => void;
}) {
  const [detail, { refetch }] = createResource(
    () => props.slug,
    (slug) => api.getPersona(slug),
  );
  const [busy, setBusy] = createSignal<string | null>(null);
  const [note, setNote] = createSignal<string | null>(null);

  const run = async (label: string, f: () => Promise<string | null>) => {
    setBusy(label);
    setNote(null);
    try {
      setNote(await f());
    } catch (e) {
      setNote(`${e}`);
    } finally {
      setBusy(null);
      void refetch();
    }
  };

  const harvest = () =>
    run("harvest", async () => {
      const r = await api.harvestPersona(props.slug);
      return r.harvesting
        ? "Harvesting — Slack and meetings are immediate, GitHub is a page at a time."
        : "Could not reach Restate to start a harvest.";
    });

  const profile = () =>
    run("profile", async () => (await api.refreshPersonaProfile(props.slug)).note);

  const forceProfile = () =>
    run("profile", async () => (await api.refreshPersonaProfile(props.slug, true)).note);

  const remove = async () => {
    if (
      !confirm(
        `Stop modelling ${detail()?.persona.display_name ?? props.slug}?\n\nEverything derived from them goes too: harvested excerpts, traits and predictions.`,
      )
    )
      return;
    await api.deletePersona(props.slug);
    props.onGone();
  };

  return (
    <section class="panel">
      <div class="panel-head">
        <h3>{detail()?.persona.display_name ?? props.slug}</h3>
        <span class="tl-actions">
          <button disabled={!!busy()} onClick={harvest}>
            {busy() === "harvest" ? "harvesting…" : "harvest"}
          </button>
          <button disabled={!!busy()} onClick={profile}>
            {busy() === "profile" ? "profiling…" : "re-profile"}
          </button>
          <button disabled={!!busy()} data-tip="Re-run even if nothing new was harvested" onClick={forceProfile}>
            force
          </button>
          <button onClick={() => props.onChat(props.slug)}>talk to them</button>
          <button onClick={remove}>delete</button>
        </span>
      </div>

      <Show when={note()}>
        <p class="muted">{note()}</p>
      </Show>

      <Show when={detail()} fallback={<p class="muted">Reading…</p>}>
        {/* Why the profile is thinner than it looks like it should be, first — before the
            counted facts, because it changes how to read them. */}
        <Show when={detail()!.harvest_note}>
          <div class="persona-caveats">
            <div>⚠ {detail()!.harvest_note}</div>
          </div>
        </Show>

        <Stats s={detail()!.stats} />

        {/* Stated up front rather than buried: a prediction from four excerpts and one from
            four hundred look identical, and the reader is about to act on which it is. */}
        <Show when={detail()!.caveats.length}>
          <div class="persona-caveats">
            <For each={detail()!.caveats}>{(c) => <div>⚠ {c}</div>}</For>
          </div>
        </Show>

        {/* The handles this profile is built through, and the control to add the missing
            one. Immediately below the stats, because "0 excerpts" is most often "no Slack
            handle linked" and the fix should be within reach of the symptom. */}
        <Identities persona={detail()!.persona} onChange={() => void refetch()} />

        {/* Who to ask, before how they review. Placed above the profile because it answers
            the earlier question: the most useful colleague for a change is often not the one
            whose review style you were curious about. */}
        <Show when={detail()!.sme.length}>
          <h4>WHERE THEY CONCENTRATE</h4>
          <div class="sme-list">
            <For each={detail()!.sme}>{(a) => <SmeRow a={a} />}</For>
          </div>
        </Show>

        <PersonaContext
          slug={props.slug}
          entries={detail()!.context}
          onChange={() => void refetch()}
        />

        <Show when={detail()!.persona.notes || detail()!.persona.role}>
          <p class="persona-note">
            <span class="muted">Your note (used verbatim, never filtered): </span>
            {detail()!.persona.role}
            {detail()!.persona.role && detail()!.persona.notes ? " — " : ""}
            {detail()!.persona.notes}
          </p>
        </Show>

        <h4>PROFILE</h4>
        <Show
          when={detail()!.traits.length}
          fallback={
            <p class="lane-empty">
              Nothing established yet. Harvest their activity, then run a profile pass — a
              claim with no citation is dropped rather than shown, so an empty profile means
              nothing survived rather than nothing was tried.
            </p>
          }
        >
          <For each={groupByFacet(detail()!.traits)}>
            {([facet, traits]) => (
              <div class="facet">
                <div class="facet-name">{facet.replace(/_/g, " ")}</div>
                <For each={traits}>
                  {(t) => <Trait t={t} evidence={detail()!.evidence} />}
                </For>
              </div>
            )}
          </For>
        </Show>

        {/* The refusals. Shown for the same reason `subject_explanations.removed` is: a
            profile that had claims taken out of it is one to read more carefully. */}
        <Show when={detail()!.removed.length}>
          <details class="persona-removed">
            <summary>
              {detail()!.removed.length} claim(s) refused by verification — what the model
              said that the evidence could not support
            </summary>
            <For each={detail()!.removed}>
              {(r) => (
                <div class="removed-item">
                  <span class="chip">{r.facet.replace(/_/g, " ")}</span>
                  <span class="removed-claim">{r.claim || "(the pass itself failed)"}</span>
                  <span class="muted"> — {r.why}</span>
                </div>
              )}
            </For>
          </details>
        </Show>

        <Show when={detail()!.predictions.length}>
          <h4>RECENT PREDICTIONS</h4>
          <For each={detail()!.predictions}>
            {(p) => <PredictionCard p={p} traits={detail()!.traits} compact />}
          </For>
        </Show>

        <details class="persona-evidence">
          <summary>{detail()!.evidence.length} excerpt(s) — their own words</summary>
          <For each={detail()!.evidence}>{(e) => <Excerpt e={e} />}</For>
        </details>
      </Show>
    </section>
  );
}

/// The counted facts. Never modelled — a model asked for an approval rate invents one, and
/// an invented number is indistinguishable from a counted one on screen.
function Stats(props: { s: PersonaStats }) {
  const s = () => props.s;
  const decided = () => s().approvals + s().changes_requested;
  return (
    <div class="persona-stats">
      <span data-tip="Harvested excerpts">{s().evidence} excerpts</span>
      <For each={s().by_source}>
        {([src, n]) => <span class={`chip src src-${src}`}>{src} {n}</span>}
      </For>
      <Show when={s().reviews}>
        <span data-tip="Review actions: summaries and inline comments">
          {s().reviews} review actions
        </span>
        {/* Nothing decided is a different fact from "never approves", and must not render
            as 0%. */}
        <Show
          when={decided()}
          fallback={<span class="muted">no decided reviews yet</span>}
        >
          <span data-tip={`${s().approvals} approved, ${s().changes_requested} changes requested`}>
            {Math.round((s().approvals / decided()) * 100)}% approval rate
          </span>
        </Show>
        <span data-tip="Share of their review activity that is inline on a line of the diff — high means they read the diff, low means they respond to the description">
          {Math.round(s().inline_ratio * 100)}% inline
        </span>
      </Show>
      <span data-tip="Median excerpt length — what to expect from them">
        ~{s().median_excerpt_chars} chars
      </span>
      <span data-tip="Share of their messages containing a question — asks versus tells">
        {Math.round(s().question_ratio * 100)}% questions
      </span>
      <Show when={s().first_seen && s().last_seen}>
        <span class="muted">
          {new Date(s().first_seen!).toLocaleDateString()} –{" "}
          {new Date(s().last_seen!).toLocaleDateString()}
        </span>
      </Show>
    </div>
  );
}

/// One trait, with its citations expandable.
function Trait(props: { t: PersonaTrait; evidence: PersonaEvidence[] }) {
  const t = () => props.t;
  const contested = () => {
    const total = t().evidence.length + t().counter_evidence.length;
    return t().counter_evidence.length > 0 && t().counter_evidence.length * 3 >= total;
  };
  const cited = (ids: string[]) =>
    ids
      .map((id) => props.evidence.find((e) => e.id === id))
      .filter((e): e is PersonaEvidence => !!e);

  return (
    <details class="trait">
      <summary>
        <span class="trait-claim">{t().claim}</span>
        <span class="trait-meta">
          <Show when={contested()}>
            <span class="badge badge-blocked" data-tip="Contradicted by a third or more of its own evidence — read as contested, not established">
              contested
            </span>
          </Show>
          <span data-tip="Confidence, bounded by how much evidence is behind it — one excerpt can never exceed 50%">
            {Math.round(t().confidence * 100)}%
          </span>
          <span class="muted">
            {t().evidence.length}
            {t().counter_evidence.length ? ` / ${t().counter_evidence.length} against` : ""}
          </span>
        </span>
      </summary>
      <div class="trait-cites">
        <For each={cited(t().evidence)}>{(e) => <Excerpt e={e} />}</For>
        <Show when={t().counter_evidence.length}>
          <div class="against-label">Against this claim:</div>
          <For each={cited(t().counter_evidence)}>{(e) => <Excerpt e={e} against />}</For>
        </Show>
      </div>
    </details>
  );
}

/// One verbatim excerpt. Verbatim is the point — a summarized citation cannot be checked.
function Excerpt(props: { e: PersonaEvidence; against?: boolean }) {
  const e = () => props.e;
  return (
    <div class="excerpt" classList={{ against: props.against }}>
      <div class="excerpt-head">
        <span class={`chip src src-${e().source}`}>{e().kind.replace(/_/g, " ")}</span>
        <Show when={e().state}>
          <span class="badge">{e().state}</span>
        </Show>
        <Show when={e().context}>
          <span class="muted">{e().context}</span>
        </Show>
        <time>{new Date(e().occurred_at).toLocaleDateString()}</time>
        <Show when={e().url}>
          <a href={e().url!} target="_blank" rel="noreferrer">
            source ↗
          </a>
        </Show>
      </div>
      <div class="excerpt-text">{e().excerpt}</div>
    </div>
  );
}

/// A prediction, labelled as one.
///
/// Exported because the subject detail pane renders the same card — the display rules are the
/// contract here, not a detail of one page: "would not engage" is a first-class answer, the
/// citations are always reachable, and the word *predicted* is never dropped.
export function PredictionCard(props: {
  p: PersonaPrediction;
  traits: PersonaTrait[];
  compact?: boolean;
  name?: string;
}) {
  const p = () => props.p;
  const traitFor = (id: string) => props.traits.find((t) => t.id === id);
  const kindLabel = () =>
    ({
      code_review: "predicted review",
      issue_response: "predicted reply",
      slack_engagement: "predicted engagement",
    })[p().kind];

  return (
    <div class="prediction" classList={{ quiet: !p().would_engage }}>
      <div class="prediction-head">
        <span class="persona-name">{props.name ?? p().persona}</span>
        <span class="chip">{kindLabel()}</span>
        <Show when={p().recommendation}>
          <span class={`badge rec-${p().recommendation}`}>
            {p().recommendation!.replace(/_/g, " ")}
          </span>
        </Show>
        <Show when={!p().would_engage}>
          {/* The most useful outcome, and the one a predictor is most tempted to skip. */}
          <span class="badge" data-tip="The profile says they would not engage with this — that is the prediction, not a missing answer">
            would not engage
          </span>
        </Show>
        <span class="muted" data-tip="Bounded by the strongest trait it rests on">
          {Math.round(p().confidence * 100)}%
        </span>
        <span class="muted" data-tip={`Built from watermark ${p().watermark}`}>
          {p().produced_by === "local" ? "⌂" : "☁"} {new Date(p().created_at).toLocaleString()}
        </span>
      </div>

      <Show when={p().summary}>
        <div class="prediction-summary">{p().summary}</div>
      </Show>

      <For each={p().points}>
        {(pt) => (
          <div class="prediction-point">
            <Show when={pt.path}>
              <code class="point-path">{pt.path}</code>
            </Show>
            <div>{pt.text}</div>
            <Show when={pt.line}>
              <pre class="point-line">{pt.line}</pre>
            </Show>
            {/* The citation. A point that cited no trait was dropped server-side — this is
                what stops a prediction being the base model's own review wearing a name. */}
            <div class="point-because">
              <For each={pt.because}>
                {(id) => (
                  <span class="chip" data-tip={traitFor(id)?.claim ?? "trait no longer in the profile"}>
                    {traitFor(id)?.facet.replace(/_/g, " ") ?? "?"}
                  </span>
                )}
              </For>
            </div>
          </div>
        )}
      </For>

      <Show when={p().caveats.length && !props.compact}>
        <div class="persona-caveats">
          <For each={p().caveats}>{(c) => <div>⚠ {c}</div>}</For>
        </div>
      </Show>
    </div>
  );
}

/// One area their activity concentrates in.
///
/// The critical rendering decision: **established expertise and mere presence must not look
/// alike.** An area the model found their comments specific in says "ask them"; an area they
/// are merely active in says "they are around". Flattening the two would be the more confident
/// and much less useful display — and would put somebody forward as an expert on the strength of
/// having commented a lot, which is frequently the signature of the person *learning* an area.
function SmeRow(props: { a: SmeArea }) {
  const a = () => props.a;
  return (
    <div class="sme-row" classList={{ expert: a().depth !== null }}>
      <span class="sme-area">
        <code>{a().area}</code>
        <span class="chip">{a().kind}</span>
      </span>
      <span class="sme-counts">
        <span data-tip="Review actions here — being trusted to judge a change, not just comment on one">
          {a().reviews} reviews
        </span>
        <span class="muted">of {a().excerpts} excerpts</span>
        <span class="muted" data-tip="Share of their whole harvested activity">
          {Math.round(a().share * 100)}%
        </span>
      </span>
      <Show
        when={a().depth}
        fallback={
          <span class="sme-depth none" data-tip="They are demonstrably active here, but nothing has established that their comments are specific — presence, not established expertise">
            presence only
          </span>
        }
      >
        <span class="sme-depth">{a().depth}</span>
      </Show>
    </div>
  );
}

/// What the operator knows about somebody, beyond the evidence.
///
/// Deliberately unfiltered: everything else on this page has been through verification, and this
/// is the one block that has not — because the filter exists to stop the *model* making
/// unfalsifiable claims, and applying it to something you stated would be the filter
/// second-guessing its own author. Labelled as such so the distinction is visible.
function PersonaContext(props: {
  slug: string;
  entries: PersonaContextEntry[];
  onChange: () => void;
}) {
  const [draft, setDraft] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [note, setNote] = createSignal<string | null>(null);

  const add = async () => {
    const content = draft().trim();
    if (!content || busy()) return;
    setBusy(true);
    setNote(null);
    try {
      const r = await api.addPersonaContext(props.slug, content);
      setDraft("");
      setNote(
        r.reprofiling
          ? "Added — re-profiling so it takes effect now."
          : "Added.",
      );
      props.onChange();
    } catch (e) {
      setNote(`${e}`);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (id: string) => {
    await api.removePersonaContext(id);
    props.onChange();
  };

  return (
    <div class="persona-context">
      <h4>WHAT YOU KNOW ABOUT THEM</h4>
      <p class="muted">
        Used verbatim and never filtered — this is the one block on this page that has not been
        through verification, because you asserted it rather than a model inferring it. A URL is
        fetched and summarized.
      </p>
      <For each={props.entries}>
        {(c) => (
          <div class="context-item">
            <span class="chip">{c.kind}</span>
            <span class="context-content">
              <Show when={c.kind === "url"} fallback={c.content}>
                <a href={c.content} target="_blank" rel="noreferrer">
                  {c.content}
                </a>
              </Show>
              <Show when={c.summary}>
                <span class="muted"> — {c.summary}</span>
              </Show>
            </span>
            <button class="linkish" onClick={() => remove(c.id)}>
              ✕
            </button>
          </div>
        )}
      </For>
      <div class="row">
        <input
          class="grow"
          placeholder="e.g. owns the release process · prefers async review · a link to their team charter"
          value={draft()}
          onInput={(e) => setDraft(e.currentTarget.value)}
          onKeyDown={(e) => e.key === "Enter" && add()}
        />
        <button disabled={!draft().trim() || busy()} onClick={add}>
          {busy() ? "adding…" : "add"}
        </button>
      </div>
      <Show when={note()}>
        <p class="muted">{note()}</p>
      </Show>
    </div>
  );
}

/// Create a persona: their name, and **both** handles at once.
///
/// One form with a GitHub field and a Slack field, rather than a source dropdown, because a
/// persona with one source is half a persona — the same colleague is terse on GitHub and chatty
/// in Slack, and the two facets are what makes a prediction about *engagement* possible at all.
/// A single-source form makes the one-source case the default by accident.
///
/// The two fields are also not equivalent in cost. Slack evidence is a SQL query over signals
/// already ingested, so it lands immediately; GitHub evidence is API calls walked back over
/// three months. If you fill in only one, fill in Slack.
function AddPersona(props: { onDone: () => void; prefill?: PersonaCandidate | null }) {
  const [candidates] = createResource(() => api.proposePersonas());
  const [name, setName] = createSignal("");
  const [role, setRole] = createSignal("");
  const [github, setGithub] = createSignal("");
  const [slack, setSlack] = createSignal("");
  const [granola, setGranola] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  /// Seed the form from a proposal instead of creating on the spot.
  ///
  /// "model them" used to create immediately, which meant a persona could only ever be born
  /// with the one handle the proposal happened to be for — precisely the half-a-persona case
  /// this form exists to avoid. Now it fills in what is known and leaves the other field for
  /// you, which is one extra click and a profile that actually works.
  const seed = (c: PersonaCandidate) => {
    setError(null);
    setName(c.label ?? c.handle);
    if (c.source === "slack") setSlack(c.handle);
    if (c.source === "github") setGithub(c.handle);
    if (c.source === "granola") setGranola(c.handle);
    // The directory's aliases are the best available guess at the *other* handle: an email
    // local part is very often the GitHub login. Offered, never assumed — it lands in the
    // field for you to confirm or replace.
    if (c.source === "slack" && !github() && c.aliases?.length) {
      const guess = c.aliases.find(
        (a) => a !== c.handle.toLowerCase() && !a.includes(" ") && a.length > 2,
      );
      if (guess) setGithub(guess);
    }
  };

  const create = async () => {
    const identities = [
      { source: "github", handle: github().trim() },
      { source: "slack", handle: slack().trim() },
      { source: "granola", handle: granola().trim() },
    ].filter((i) => i.handle);
    if (!name().trim() || !identities.length || busy()) return;
    setBusy(true);
    setError(null);
    try {
      await api.createPersona({
        display_name: name().trim(),
        role: role() || undefined,
        identities,
      });
      props.onDone();
    } catch (e) {
      setError(`${e}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="persona-add">
      <div class="form">
        <div class="row">
          <input
            placeholder="Their name"
            value={name()}
            onInput={(e) => setName(e.currentTarget.value)}
          />
          <input
            placeholder="role, in your words (optional) — used verbatim"
            value={role()}
            onInput={(e) => setRole(e.currentTarget.value)}
          />
        </div>
        <div class="identity-fields">
          <label>
            <span class={`chip src src-github`}>GitHub</span>
            <input
              placeholder="login, e.g. pcholakov"
              value={github()}
              onInput={(e) => setGithub(e.currentTarget.value)}
            />
          </label>
          <label>
            <span class={`chip src src-slack`}>Slack</span>
            <input
              placeholder="name, @handle or U… id"
              value={slack()}
              onInput={(e) => setSlack(e.currentTarget.value)}
            />
          </label>
          <label>
            <span class={`chip src src-granola`}>Granola</span>
            <input
              placeholder="speaker name in transcripts (optional)"
              value={granola()}
              onInput={(e) => setGranola(e.currentTarget.value)}
            />
          </label>
        </div>
        <div class="row">
          <button disabled={!name().trim() || busy()} onClick={create}>
            {busy() ? "creating…" : "Create"}
          </button>
        </div>
        <p class="muted">
          Fill in as many as you know — the same person is usually terser on GitHub than in
          Slack, and the profile tracks each separately. A Slack name or <code>@handle</code> is
          resolved against the workspace directory, so you do not need the <code>U…</code> id.
          Evidence is only harvested through a handle you confirmed.
        </p>
        <Show when={error()}>
          <p class="err">{error()}</p>
        </Show>
      </div>

      <h4>PROPOSED — ranked by how much you deal with them</h4>
      <For
        each={candidates()?.candidates}
        fallback={
          <p class="muted">
            {candidates.loading
              ? "Reading the signal log…"
              : "Nobody new in the signal log. Anyone already modelled is excluded, as is automation."}
          </p>
        }
      >
        {(c) => (
          <div class="candidate">
            <span class={`chip src src-${c.source}`}>{c.source}</span>
            {/* The label, when the directory could supply one. A ranked list of opaque
                `U06T7445RHD` rows cannot be acted on. */}
            <span class="persona-name">{c.label ?? c.handle}</span>
            <Show when={c.label && c.label !== c.handle}>
              <span class="muted mono">{c.handle}</span>
            </Show>
            <span class="muted">{c.interactions} interaction(s)</span>
            <Show when={c.is_bot}>
              <span class="badge">bot</span>
            </Show>
            <Show when={c.sample}>
              <span class="candidate-sample">“{c.sample}”</span>
            </Show>
            <button onClick={() => seed(c)}>use</button>
          </div>
        )}
      </For>
    </div>
  );
}

/// Add or remove a handle on an existing persona.
///
/// The tool existed from the start and nothing called it, which meant a persona created with a
/// GitHub login could never gain a Slack one — so half the profile was permanently
/// unreachable. This is the missing control.
function Identities(props: { persona: Persona; onChange: () => void }) {
  const [source, setSource] = createSignal("slack");
  const [handle, setHandle] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  const link = async () => {
    if (!handle().trim() || busy()) return;
    setBusy(true);
    setError(null);
    try {
      await api.linkPersonaIdentity(props.persona.slug, source(), handle().trim());
      setHandle("");
      props.onChange();
    } catch (e) {
      setError(`${e}`);
    } finally {
      setBusy(false);
    }
  };

  const unlink = async (src: string, h: string) => {
    if (!confirm(`Stop harvesting through ${src} ${h}?\n\nAlready-harvested excerpts stay.`))
      return;
    await api.unlinkPersonaIdentity(src, h);
    props.onChange();
  };

  return (
    <div class="identity-manage">
      <For each={props.persona.identities}>
        {(id) => (
          <span class="identity-pill">
            <span
              class={`chip src src-${id.source}`}
              classList={{ proposed: id.provenance === "proposed" }}
            >
              {id.source}
            </span>
            <span class="mono">{id.handle}</span>
            <Show when={id.rationale}>
              <span class="muted" data-tip={id.rationale!}>
                ⓘ
              </span>
            </Show>
            <button class="linkish" onClick={() => unlink(id.source, id.handle)}>
              ✕
            </button>
          </span>
        )}
      </For>
      <span class="identity-add">
        <select value={source()} onChange={(e) => setSource(e.currentTarget.value)}>
          <option value="slack">Slack</option>
          <option value="github">GitHub</option>
          <option value="granola">Granola</option>
        </select>
        <input
          placeholder="name, @handle or id"
          value={handle()}
          onInput={(e) => setHandle(e.currentTarget.value)}
          onKeyDown={(e) => e.key === "Enter" && link()}
        />
        <button disabled={!handle().trim() || busy()} onClick={link}>
          {busy() ? "linking…" : "link"}
        </button>
      </span>
      <Show when={error()}>
        <p class="err">{error()}</p>
      </Show>
    </div>
  );
}

/// Traits grouped by facet, in a fixed order so the page does not reshuffle between passes.
///
/// The order is the order the questions get asked in real life: what they block on first,
/// what they ignore second, and register last.
const FACET_ORDER = [
  "reviews_for",
  "ignores",
  "bar",
  "hobby_horses",
  "expertise",
  "blind_spots",
  "style",
  "escalation",
  "slack_register",
  "meeting_register",
];

function groupByFacet(traits: PersonaTrait[]): [string, PersonaTrait[]][] {
  const groups = new Map<string, PersonaTrait[]>();
  for (const t of traits) {
    const list = groups.get(t.facet) ?? [];
    list.push(t);
    groups.set(t.facet, list);
  }
  return [...groups.entries()].sort(
    (a, b) =>
      (FACET_ORDER.indexOf(a[0]) + 1 || 99) - (FACET_ORDER.indexOf(b[0]) + 1 || 99),
  );
}
