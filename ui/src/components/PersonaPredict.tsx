import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { api } from "../api";
import { PredictionCard } from "./Personas";
import type { PersonaPrediction, PersonaSummary, PersonaTrait } from "../types";

/// "How will this land?" — select personas against this subject and read the predictions.
///
/// The panel that answers the question the whole persona feature exists for. Three display
/// decisions carry most of its value:
///
/// **It takes a set.** The useful answer to "how will this land" is the *set* of reactions —
/// one reviewer who will block and one who will not care are a single answer together, and two
/// separate button presses apart.
///
/// **It polls, because the work is a workflow.** A prediction is a model call behind the
/// single local lane, so the submit returns as soon as the ingress accepts it. Without the
/// poll the panel would look broken for the minute the pass takes; with it, the pass is
/// visibly in flight and the cards appear as they land.
///
/// **A refused submission is a success.** `submitted: false` means this exact question has
/// already been answered at this watermark, so the answer is already on screen. Reporting that
/// as a failure would train the operator to press the button twice.
export default function PersonaPredict(props: { subjectKey: string }) {
  const [personas] = createResource(() => api.listPersonas());
  /// Only personas with a profile. One without traits predicts nothing — the backend refuses,
  /// so offering it here would be a button that returns a refusal.
  const profiled = (): PersonaSummary[] =>
    personas()?.personas.filter((p) => p.traits > 0) ?? [];
  const unprofiled = (): PersonaSummary[] =>
    personas()?.personas.filter((p) => p.traits === 0) ?? [];

  const [selected, setSelected] = createSignal<string[]>([]);
  const [predictions, { refetch }] = createResource(
    () => props.subjectKey,
    (key) => api.predictionsFor(key),
  );
  const [busy, setBusy] = createSignal(false);
  const [note, setNote] = createSignal<string | null>(null);
  /// Personas whose pass is in flight. Rendered as pending rows, so a prediction that takes a
  /// minute is visibly running rather than absent.
  const [awaiting, setAwaiting] = createSignal<string[]>([]);

  let poll: number | undefined;
  onCleanup(() => window.clearInterval(poll));

  const toggle = (slug: string) =>
    setSelected((prev) =>
      prev.includes(slug) ? prev.filter((s) => s !== slug) : [...prev, slug],
    );

  const nameOf = (slug: string) =>
    profiled().find((p) => p.slug === slug)?.display_name ?? slug;

  const predict = async () => {
    const slugs = selected();
    if (!slugs.length || busy()) return;
    setBusy(true);
    setNote(null);
    try {
      const r = await api.predictPersonas(props.subjectKey, slugs);
      const started = r.predictions.filter((p) => p.submitted).map((p) => p.persona);
      const already = r.predictions.filter((p) => !p.submitted).map((p) => p.persona);
      setAwaiting(started);
      setNote(
        [
          started.length ? `${started.length} pass(es) running — a minute or so each.` : "",
          // Not a failure: the same question at the same watermark has one answer, and it is
          // already below.
          already.length
            ? `${already.map(nameOf).join(", ")}: already predicted at this point — the answer is below.`
            : "",
        ]
          .filter(Boolean)
          .join(" "),
      );
      void refetch();

      // Poll while anything is outstanding. Bounded, so a pass that dies terminally leaves a
      // stale "running" row for two minutes rather than polling until the tab closes.
      //
      // "Landed" is `created_at` newer than the submission, not merely "a row exists": a
      // re-prediction after new activity replaces the row in place, so an existing row from
      // ten minutes ago would otherwise count as this pass finishing instantly.
      window.clearInterval(poll);
      if (started.length) {
        const submittedAt = Date.now();
        let ticks = 0;
        poll = window.setInterval(async () => {
          ticks += 1;
          const rows = await api.predictionsFor(props.subjectKey).catch(() => null);
          if (rows) {
            const landed = new Set(
              rows
                .filter((p) => new Date(p.created_at).getTime() >= submittedAt - 1000)
                .map((p) => p.persona),
            );
            setAwaiting((prev) => prev.filter((s) => !landed.has(s)));
            void refetch();
          }
          if (ticks > 30 || awaiting().length === 0) {
            window.clearInterval(poll);
            setAwaiting([]);
          }
        }, 4000);
      }
    } catch (e) {
      setNote(`${e}`);
    } finally {
      setBusy(false);
    }
  };

  /// The traits a prediction's citations point at, gathered per persona so the cards can show
  /// *which* trait each point follows from rather than an opaque id.
  const [traits] = createResource(
    () => predictions()?.map((p) => p.persona).join(","),
    async (keys) => {
      const out: Record<string, PersonaTrait[]> = {};
      for (const slug of new Set(keys.split(",").filter(Boolean))) {
        out[slug] = await api
          .getPersona(slug, 0)
          .then((d) => d.traits)
          .catch(() => []);
      }
      return out;
    },
  );

  return (
    <section class="panel">
      <div class="panel-head">
        <h3>How will this land?</h3>
        <Show when={selected().length}>
          <button disabled={busy()} onClick={predict}>
            {busy() ? "submitting…" : `PREDICT (${selected().length})`}
          </button>
        </Show>
      </div>

      <Show
        when={personas()?.enabled !== false}
        fallback={
          <p class="muted">
            Personas are off — set <code>enabled = true</code> under <code>[personas]</code>.
          </p>
        }
      >
        <Show
          when={profiled().length}
          fallback={
            <p class="muted">
              {unprofiled().length
                ? `${unprofiled().length} persona(s) exist but none has a profile yet — harvest and profile them on the Personas page.`
                : "Nobody is modelled yet. The Personas page proposes candidates ranked by how much you deal with them."}
            </p>
          }
        >
          <p class="muted">
            Predicted, not real: what these people would probably say, from what they have
            actually written. Never posted anywhere.
          </p>
          <div class="persona-select">
            <For each={profiled()}>
              {(p) => (
                <button
                  type="button"
                  class="chip persona-chip"
                  classList={{ on: selected().includes(p.slug) }}
                  data-tip={`${p.traits} established trait(s) from ${p.stats.evidence} excerpt(s)`}
                  onClick={() => toggle(p.slug)}
                >
                  {p.display_name}
                </button>
              )}
            </For>
          </div>
        </Show>

        <Show when={note()}>
          <p class="muted">{note()}</p>
        </Show>

        <For each={awaiting()}>
          {(slug) => (
            <div class="prediction pending">
              <span class="persona-name">{nameOf(slug)}</span>
              <span class="muted"> — reading the change and their profile…</span>
            </div>
          )}
        </For>

        <For each={predictions()}>
          {(p: PersonaPrediction) => (
            <PredictionCard
              p={p}
              traits={traits()?.[p.persona] ?? []}
              name={nameOf(p.persona)}
            />
          )}
        </For>
      </Show>
    </section>
  );
}
