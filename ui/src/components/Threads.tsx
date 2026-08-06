import { createMemo, createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { api } from "../api";
import type { Corroborated, ThreadAnalysis, ThreadVerdict } from "../types";
import { renderMarkdown } from "../markdown";

const POLL_MS = 3000;

/// Stance drives the colour, and the vocabulary is deliberately blunt.
///
/// `criticism` is what the operator asked for, so it gets the strongest treatment on the
/// page. It is red rather than amber because amber in this app means "a warning about the
/// system"; this is a judgement about a person, and borrowing the system's warning colour
/// would make a code review of your behaviour look like a failing service.
const STANCE_LABEL: Record<string, string> = {
  criticism: "called out",
  credit: "credit",
  observation: "observed",
};

function Finding(props: { entry: Corroborated; messages: ThreadVerdict["thread"]["messages"] }) {
  const [open, setOpen] = createSignal(false);
  const f = () => props.entry.finding;
  const cited = createMemo(() =>
    f().cites
      .map((id) => props.messages.find((m) => m.id === id))
      .filter((m): m is ThreadVerdict["thread"]["messages"][number] => !!m),
  );
  return (
    <div class="finding-card" classList={{ [`stance-${f().stance}`]: true }}>
      <div class="finding-head">
        <span class="stance">{STANCE_LABEL[f().stance] ?? f().stance}</span>
        {/* The whole reason there are two models. "Both models" is the strongest thing
            this page can say; one model alone is a claim, not a consensus. */}
        <Show
          when={props.entry.also}
          fallback={
            <span class="corroboration one" data-tip={`Only ${f().source} raised this`}>
              {f().source} only
            </span>
          }
        >
          <span
            class="corroboration both"
            data-tip="Claude and ChatGPT reached this independently, without seeing each other's answer"
          >
            both models
          </span>
        </Show>
        <button class="cite-toggle" onClick={() => setOpen(!open())}>
          {open() ? "▾" : "▸"} {f().cites.join(" ")}
        </button>
      </div>
      <p class="finding-claim">{f().claim}</p>
      {/* A claim that leaned on a persona trait is a claim about a *pattern*, not about one
          message — which is a materially stronger thing to say about someone, so it is
          marked rather than left implicit in the prose. */}
      <Show when={f().from_traits.length}>
        <div class="from-traits" data-tip="This leans on the persona profile, so it is a claim about a pattern rather than about one message">
          from profile: {f().from_traits.map((t) => t.split("/")[1] ?? t).join(", ")}
        </div>
      </Show>
      {/* The other model's wording, when it agreed. Kept verbatim rather than merged: they
          agreed on the person and the message, not on the sentence, and pretending otherwise
          would be the synthesis this design avoids. */}
      <Show when={props.entry.also}>
        <p class="finding-also">
          <span class="muted">{props.entry.also!.source}:</span> {props.entry.also!.claim}
        </p>
      </Show>
      {/* The messages it rests on, verbatim. A candid claim you cannot check against what
          was actually written is just an accusation with a citation format. */}
      <Show when={open()}>
        <div class="cited-messages">
          <For each={cited()}>
            {(m) => (
              <div class="cited" classList={{ yours: m.is_you }}>
                <span class="cited-id">{m.id}</span>
                <span class="cited-author">@{m.author}</span>
                <span class="cited-text">{m.text}</span>
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

function Verdict(props: { verdict: ThreadVerdict }) {
  const v = () => props.verdict;
  const you = createMemo(() => v().thread.participants.find((p) => p.is_you));
  return (
    <div class="thread-verdict">
      {/* A model that was asked and did not answer. Loud, and above everything else on the
          page, because the whole value here is two independent readers: with one missing,
          nothing below can be corroborated and "one model only" on every finding means
          "there was only one model" rather than "the models disagreed". Silently degrading
          from two opinions to one while still looking like two is the worst failure this
          feature can have. */}
      <Show when={v().failures.length}>
        <div class="panel-incomplete">
          <strong>Only one model answered.</strong> Nothing below could be corroborated, so
          the missing "both models" marks mean nothing here.
          <For each={v().failures}>{(f) => <div class="panel-failure">{f}</div>}</For>
        </div>
      </Show>

      {/* Each model's read of the thread, side by side and unreconciled. Where they differ
          on what the thread was even about, that is the first thing worth knowing. */}
      <div class="model-summaries">
        <For each={v().analyses}>
          {(a) => (
            <div class="model-summary">
              <div class="model-name">
                {a.provider} <span class="muted">{a.model}</span>
              </div>
              <div class="md" innerHTML={renderMarkdown(a.summary)} />
              <Show when={a.outcome}>
                <p class="outcome">
                  <span class="muted">Outcome:</span> {a.outcome}
                </p>
              </Show>
              {/* Findings this model lost to the checker. Shown, because an analysis that
                  lost four findings to invented quotes is telling you something about that
                  run — and silently returning two looks identical to a quiet thread. */}
              <Show when={a.dropped.length}>
                <details class="dropped">
                  <summary>{a.dropped.length} discarded by the checker</summary>
                  <For each={a.dropped}>
                    {(d) => (
                      <p class="dropped-item">
                        <span class="dropped-why">{d.why}</span> — {d.claim}
                      </p>
                    )}
                  </For>
                </details>
              </Show>
            </div>
          )}
        </For>
      </div>

      {/* The section that was asked for, first on the page. */}
      <section class="about-you">
        <h3>
          About you
          <Show when={you()}>
            <span class="muted"> — @{you()!.handle}</span>
          </Show>
        </h3>
        <Show
          when={v().about_you.length}
          fallback={
            <p class="nothing">
              <Show
                when={you()}
                fallback={<>You did not post in this thread, so there is nothing to say about your part in it.</>}
              >
                Neither model found anything to call out about your part in this thread. That
                is an answer, not an omission — the prompt allows it explicitly, because one
                that demanded criticism would manufacture it.
              </Show>
            </p>
          }
        >
          <For each={v().about_you}>
            {(entry) => <Finding entry={entry} messages={v().thread.messages} />}
          </For>
        </Show>
      </section>

      <Show when={v().about_others.length}>
        <section class="about-others">
          <h3>Everyone else</h3>
          <For each={v().about_others}>
            {(entry) => <Finding entry={entry} messages={v().thread.messages} />}
          </For>
        </section>
      </Show>

      {/* The cast, and — as much as the finding cards — who the models had no profile for.
          A claim about how someone "always" behaves is only supportable with a profile
          behind it, and this is where you can see there wasn't one. */}
      <details class="thread-cast">
        <summary>
          {v().thread.participants.length} participant(s) ·{" "}
          {v().thread.messages.length} message(s)
          <Show when={v().thread.truncated > 0}>
            <span class="muted"> · {v().thread.truncated} earlier not read</span>
          </Show>
        </summary>
        <For each={v().thread.participants}>
          {(p) => (
            <div class="cast-row">
              <span class="cast-handle" classList={{ yours: p.is_you }}>
                @{p.handle}
              </span>
              <span class="muted">{p.messages} msg</span>
              <Show
                when={p.persona}
                fallback={<span class="no-profile" data-tip="No persona, so no claim about how they usually behave">no profile</span>}
              >
                <span class="has-profile">{p.traits.length} trait(s) used</span>
              </Show>
            </div>
          )}
        </For>
      </details>
    </div>
  );
}

export default function Threads() {
  const [link, setLink] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  const [openId, setOpenId] = createSignal<string | null>(null);

  const [analyses, { refetch }] = createResource(
    async () => await api.tool<ThreadAnalysis[]>("list_thread_analyses", { limit: 30 }),
  );

  // Two cloud models on a thread takes a while, so the row fills in behind you. Poll only
  // while something is actually in flight — a screen that polls at rest is a screen that
  // wakes the machine for nothing.
  const inFlight = () =>
    (analyses() ?? []).some((a) => a.status === "pending" || a.status === "running");
  const timer = setInterval(() => {
    if (inFlight()) void refetch();
  }, POLL_MS);
  onCleanup(() => clearInterval(timer));

  const submit = async () => {
    if (!link().trim() || busy()) return;
    setBusy(true);
    setError("");
    try {
      const queued = await api.tool<ThreadAnalysis>("analyse_thread", { link: link() });
      setLink("");
      setOpenId(queued.id);
      await refetch();
    } catch (e) {
      // The link parser and the Slack fetch both produce messages written for this box —
      // "invite the app to that channel", "that is not a Slack link". Surfacing the raw
      // error is the point; paraphrasing it here would lose the instruction.
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const verdictOf = (a: ThreadAnalysis): ThreadVerdict | null => {
    if (!a.verdict) return null;
    try {
      return JSON.parse(a.verdict) as ThreadVerdict;
    } catch {
      return null;
    }
  };

  return (
    <div class="page threads-page">
      <div class="card">
        <div class="card-head">
          <h2>Thread analysis</h2>
          <span class="muted">
            Claude and ChatGPT read one Slack thread independently, using what the personas
            know about the people in it. Every finding cites the messages it rests on.
          </span>
        </div>
        <div class="thread-input">
          <input
            type="text"
            placeholder="Paste a Slack thread link (Slack → message → Copy link)…"
            value={link()}
            onInput={(e) => setLink(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void submit();
            }}
          />
          <button disabled={!link().trim() || busy()} onClick={() => void submit()}>
            {busy() ? "READING…" : "ANALYSE"}
          </button>
        </div>
        <Show when={error()}>
          <p class="thread-error">{error()}</p>
        </Show>
      </div>

      <Show
        when={(analyses() ?? []).length}
        fallback={
          <p class="empty">
            Nothing analysed yet. Paste a thread link above — it is read, never posted to.
          </p>
        }
      >
        <For each={analyses()}>
          {(a) => {
            const open = () => openId() === a.id;
            const v = createMemo(() => verdictOf(a));
            return (
              <div class="card thread-card" classList={{ open: open() }}>
                <div
                  class="thread-card-head"
                  onClick={() => setOpenId(open() ? null : a.id)}
                >
                  <span class={`state state-${a.status === "completed" ? "resolved" : a.status === "failed" ? "unseen" : "seen"}`}>
                    {a.status}
                  </span>
                  <span class="thread-title">
                    <Show when={v()} fallback={a.url}>
                      {v()!.thread.channel_name ?? a.channel} ·{" "}
                      {v()!.thread.messages.length} messages,{" "}
                      {v()!.thread.participants.map((p) => `@${p.handle}`).join(" ")}
                    </Show>
                  </span>
                  <Show when={v()}>
                    <span class="thread-counts">
                      <Show when={v()!.about_you.length}>
                        <span class="you-count" data-tip="Findings about your own part">
                          {v()!.about_you.length} about you
                        </span>
                      </Show>
                      <Show when={v()!.failures.length}>
                        <span class="one-model" data-tip={v()!.failures.join("; ")}>
                          one model only
                        </span>
                      </Show>
                      <Show when={v()!.contested > 0 && !v()!.failures.length}>
                        <span class="muted" data-tip="Raised by one model but not the other">
                          {v()!.contested} uncorroborated
                        </span>
                      </Show>
                    </span>
                  </Show>
                  <a href={a.url} target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()}>
                    open in Slack ↗
                  </a>
                </div>
                <Show when={a.error}>
                  <p class="thread-error">{a.error}</p>
                </Show>
                <Show when={a.status === "pending" || a.status === "running"}>
                  <p class="muted">
                    Reading the thread with both models — they run at the same time, blind to
                    each other.
                  </p>
                </Show>
                <Show when={open() && v()}>
                  <Verdict verdict={v()!} />
                </Show>
              </div>
            );
          }}
        </For>
      </Show>
    </div>
  );
}
