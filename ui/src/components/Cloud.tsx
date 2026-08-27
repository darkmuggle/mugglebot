import {
  createEffect,
  createResource,
  createSignal,
  For,
  on,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import { api } from "../api";
import type { CloudEntity } from "../types";
import PtyPane, { type PtyRequest } from "./Terminal";
import { Empty, Panel } from "./ui";

/// Kinds whose state `rcc edit` will open. Mirrors `EDITABLE_KINDS` in `src/pty.rs`, which is
/// where it is actually enforced — this copy only decides whether to draw the button.
const EDITABLE = ["account", "environment", "organization", "group"];
/// Kinds `rcc reconcile` accepts. Not the same list: a region reconciles but is not
/// hand-edited, an account edits but has nothing to apply.
const RECONCILABLE = ["environment", "region"];

const ACCESS_LEVELS = ["support", "admin"];
/// Lifetimes offered for a kubectl token. A subset of what `pty.rs` will accept — the
/// allowlist there is a validation boundary, this is a menu.
const DURATIONS = [
  { value: "24h", label: "24 hours" },
  { value: "7d", label: "7 days" },
  { value: "30d", label: "30 days" },
];

/// The Restate Cloud estate, searchable and walkable — see `src/cloud.rs`.
///
/// The context box on a subject answers "what is this id somebody pasted". This answers the
/// question you arrive with when nobody has pasted anything: *which* environments are on that
/// profile, what else is in this account, which region is this customer in. Same cache, no
/// CLI calls — the estate was read once by `mugglebot cloud-warm`.
///
/// Two panes rather than a drill-down page, because exploring a graph is a sequence of small
/// hops and a full page transition per hop loses the search you got there with.

interface KindCount {
  kind: string;
  count: number;
}

interface SearchResult {
  results: CloudEntity[];
  kinds: KindCount[];
}

interface Exploration {
  entity: CloudEntity;
  /// What it belongs to: an environment's account and region, an account's organization.
  parents: CloudEntity[];
  /// What belongs to it: an account's environments, an organization's accounts.
  children: CloudEntity[];
  /// The true count, which can exceed `children` — the query is capped.
  child_total: number;
  /// The dashboard this entity's metrics are on, or null for the kinds that have none —
  /// an account and an organization are billing structures, not things with a p99.
  grafana: string | null;
}

export default function CloudExplorer(props: {
  /// The entity in the URL, so `#/cloud/env_201k…` is a linkable location — which is what an
  /// alert's entity chip points at.
  selected: string | null;
  onSelect: (id: string | null) => void;
}) {
  const [query, setQuery] = createSignal("");
  const [kind, setKind] = createSignal<string | null>(null);
  const selected = () => props.selected;
  const setSelected = (id: string | null) => props.onSelect(id);

  const [found] = createResource(
    () => ({ query: query(), kind: kind() }),
    ({ query, kind }) =>
      api.tool<SearchResult>("search_cloud_entities", {
        query,
        kind: kind ?? "",
        limit: 200,
      }),
  );

  const [detail, { refetch: refetchDetail }] = createResource(selected, (id) =>
    api.tool<Exploration>("explore_cloud_entity", { id }),
  );

  // Re-reading one entity now, rather than waiting out the six-hour TTL.
  //
  // The cache is right for things that change on the order of never, and wrong at exactly the
  // two moments an operator is looking at this page on purpose: just after reconciling a
  // change they made, and just after fixing an `rcc` credential that had been failing. Both
  // leave the box showing something they know is out of date, with nothing to do about it.
  const [refreshing, setRefreshing] = createSignal(false);
  const refresh = async (id: string) => {
    setRefreshing(true);
    try {
      await api.tool("refresh_cloud_entity", { id });
      // Re-fetch the exploration rather than patching the entity in: the parents and children
      // are derived from edges the re-read may have changed.
      await refetchDetail();
    } catch {
      // The error is reported on the entity itself — a failed re-read is cached with the
      // reason, on top of the last known good copy — so there is nothing to say here.
    } finally {
      setRefreshing(false);
    }
  };

  const total = () => found()?.kinds.reduce((n, k) => n + k.count, 0) ?? 0;

  /// Nothing typed and no kind picked — so the list is a *sample of the estate*, not an
  /// answer to a question.
  ///
  /// Worth knowing because the two lists are built differently. A search ranks matches; a
  /// browse takes an equal slice of every kind, because ordering the whole estate by kind put
  /// 200 accounts on the first page and every user, region and group past the end of it —
  /// which reads as an estate that holds only accounts. The slice fixes what is shown; this
  /// is what stops the fix from being its own quiet lie.
  const browsing = () => !query().trim() && !kind();

  /// Environments (and regions) ticked in the results list, for a bulk reconcile.
  ///
  /// Keyed by id rather than held as a list of entities, so a selection survives the list
  /// being re-fetched under it — the search re-runs on every keystroke, and a selection that
  /// evaporated when you refined the filter would make "select these twelve" impossible to do
  /// in two steps.
  const [picked, setPicked] = createSignal<Set<string>>(new Set());
  const togglePicked = (id: string) =>
    setPicked((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  /// The reconcilable rows currently on screen. The scope of "select all", and what decides
  /// whether the tick column is drawn at all.
  const reconcilable = () =>
    (found()?.results ?? []).filter((e) => RECONCILABLE.includes(e.kind));

  /// What is ticked *and* still on screen.
  ///
  /// A filter change can hide a ticked row, and reconciling something the operator can no
  /// longer see is the one outcome a bulk action must not have. Narrowing the filter narrows
  /// the run, which is the reading of the screen in front of them.
  const pickedHere = () => reconcilable().filter((e) => picked().has(e.id));

  const allPickedHere = () =>
    reconcilable().length > 0 && pickedHere().length === reconcilable().length;
  const toggleAllHere = () =>
    setPicked((prev) => {
      const next = new Set(prev);
      const here = reconcilable().map((e) => e.id);
      if (here.every((id) => next.has(id))) here.forEach((id) => next.delete(id));
      else here.forEach((id) => next.add(id));
      return next;
    });

  // One terminal at a time, because one socket is one process.
  const [pty, setPty] = createSignal<PtyRequest | null>(null);
  // After an edit that actually wrote something, the change is in the control plane and NOT
  // on the cluster. Offering the reconcile right here is the difference between that being a
  // tip in a pane you are about to close and it being the next button.
  const [needsReconcile, setNeedsReconcile] = createSignal<string | null>(null);
  // The install the token modal is open for, or nothing. Held here rather than as a boolean
  // so the modal can name what it is about — and so it cannot outlive the selection, which is
  // the bug the document and reconcile panes above already had.
  const [tokenFor, setTokenFor] = createSignal<CloudEntity | null>(null);
  // Likewise for the resources modal, and for the same reason: it names the environment it is
  // about, so it must not survive the selection moving.
  const [resourcesFor, setResourcesFor] = createSignal<CloudEntity | null>(null);

  // A terminal opened *from an entity* belongs to it, and unlike the panes above it cannot
  // simply be hidden and restored: the pane owns a process, and re-mounting it would start a
  // second `rcc edit` — on the *previous* entity, since the request still names it. So
  // navigating away ends that session, which is also the honest reading of navigating away.
  //
  // A **bulk reconcile is not about the selected entity**, so navigating does not end it. It
  // was started from the results list, it names its own set of ids, and clicking a row to see
  // what one of them is — the obvious thing to do while twenty diffs go past — must not kill
  // a run that is part-way through applying to live clusters. Half a bulk reconcile is a state
  // nothing else in the system can describe.
  createEffect(
    on(
      () => props.selected,
      () => {
        const running = pty();
        const bulk = running?.kind === "rcc_reconcile" && running.ids.length > 1;
        if (!bulk) {
          setPty(null);
          setPtyRunning(false);
        }
        setTokenFor(null);
        setResourcesFor(null);
      },
      { defer: true },
    ),
  );

  /// Whether the open pane still has a process on the other end.
  ///
  /// Tracked here because starting a command now *replaces* the pane (see the `keyed` Show),
  /// and replacing it closes the socket, which kills the child. That is right for a session
  /// that has exited and wrong for one that has not: `rcc edit` holds an editor open on a
  /// control-plane document, and losing it to a mis-aimed click would discard the edit.
  const [ptyRunning, setPtyRunning] = createSignal(false);

  const openPty = (request: PtyRequest) => {
    if (
      ptyRunning() &&
      !confirm(
        "A command is still running in the terminal.\n\n" +
          "Starting another one ends it. If that is an editor, the edit is lost.",
      )
    ) {
      return;
    }
    setNeedsReconcile(null);
    setPtyRunning(true);
    setPty(request);
  };

  /// Reconcile one or many, straight into the terminal.
  ///
  /// **No browser confirmation.** There used to be a `confirm()` here, on the reasoning that
  /// `rcc`'s own prompt appears only *after* the run has started. That reasoning does not
  /// survive contact with the thing it guards: `rcc reconcile` prints the diff and waits, per
  /// environment, and nothing here can pass `--yes`. So the dialog asked "are you sure?" about
  /// an action whose next step is already "here is exactly what will change, are you sure?" —
  /// and a confirmation you answer twice is one you stop reading, which makes the second one
  /// worse rather than the first one better.
  ///
  /// It matters more for a bulk run: twenty environments would have been twenty browser
  /// dialogs in front of twenty real diffs. The diffs are the review; this just gets out of
  /// the way of them.
  const reconcile = (ids: string[]) => {
    if (ids.length) openPty({ kind: "rcc_reconcile", ids });
  };

  return (
    <div class="cloud-page">
      <Panel title="Estate" meta={`${total()} entities cached`}>
        <p class="muted panel-lede">
          Everything <code>rcc</code> and <code>nuon</code> know, read once and cached. Search
          matches ids, names, statuses <em>and field values</em> — so a profile, a cluster
          name or a provider region finds things, not just an id. Pick a result to walk what
          it belongs to and what belongs to it.
        </p>

        <input
          class="cloud-search"
          type="search"
          placeholder="environment name, acc_…, profile, us-east1, PROVISIONING_READY…"
          value={query()}
          onInput={(e) => setQuery(e.currentTarget.value)}
        />

        {/* Counts on the chips, because they say what the cache actually holds — a kind
            sitting at zero means the estate has not been warmed for it, which is a different
            problem from a search that found nothing. */}
        <div class="cloud-kinds">
          <button
            class="chip"
            classList={{ active: kind() === null }}
            onClick={() => setKind(null)}
          >
            all
          </button>
          <For each={found()?.kinds}>
            {(k) => (
              <button
                class="chip"
                classList={{ active: kind() === k.kind }}
                onClick={() => setKind(kind() === k.kind ? null : k.kind)}
              >
                {k.kind} <span class="chip-count">{k.count}</span>
              </button>
            )}
          </For>
        </div>
      </Panel>

      <div class="cloud-split">
        <Panel
          class="cloud-results"
          title={browsing() ? "A few of each" : "Matches"}
          meta={
            browsing()
              ? `${found()?.results.length ?? 0} of ${total()} — search or pick a kind`
              : found()?.results.length === 200
                ? "first 200"
                : undefined
          }
        >
          <Show
            when={found()?.results.length}
            fallback={
              /* An unwarmed cache and a genuine miss are different answers, and the operator
                 must be able to tell them apart before concluding the entity does not exist. */
              <Show
                when={!found.loading}
                fallback={<Empty kind="waiting">Searching…</Empty>}
              >
                <Empty kind={total() ? "clear" : "unset"}>
                  <Show
                    when={total()}
                    fallback={
                      <>
                        The estate has never been warmed. Run{" "}
                        <code>mugglebot cloud-warm</code> to read it once.
                      </>
                    }
                  >
                    Nothing matches.
                  </Show>
                </Empty>
              </Show>
            }
          >
            {/* The bulk strip, drawn only when the filter is showing something that can be
                reconciled. On any other list it would be a control with nothing to act on,
                and a permanently-disabled button teaches people to stop reading the row. */}
            <Show when={reconcilable().length > 1}>
              <div class="cloud-bulk">
                <label class="cloud-bulk-all">
                  {/* `indeterminate` is a DOM property with no HTML attribute, so it is set
                      on the element rather than passed as a prop — the dash is what tells you
                      a selection exists but is partial. */}
                  <input
                    type="checkbox"
                    checked={allPickedHere()}
                    ref={(el) => {
                      createEffect(() => {
                        el.indeterminate = pickedHere().length > 0 && !allPickedHere();
                      });
                    }}
                    onChange={toggleAllHere}
                  />
                  {/* Scoped to what is on screen, and says so: with 1019 environments cached
                      and 200 shown, "all" has to mean the ones you are looking at. */}
                  all {reconcilable().length} showing
                </label>
                <Show when={pickedHere().length}>
                  <span class="cloud-bulk-count">{pickedHere().length} selected</span>
                  <button onClick={() => setPicked(new Set())}>clear</button>
                  <button
                    class="mutating"
                    data-tip="Opens one rcc session — it shows each diff and asks before applying it"
                    onClick={() => reconcile(pickedHere().map((e) => e.id))}
                  >
                    reconcile {pickedHere().length}
                  </button>
                </Show>
              </div>
            </Show>

            <ul class="cloud-list">
              <For each={found()?.results}>
                {(e) => (
                  <li classList={{ picked: picked().has(e.id) }}>
                    {/* The tick is its own control, outside the row button: the row navigates
                        and the tick selects, and one element cannot do both without the
                        commoner action stealing the other. Rendered for reconcilable kinds
                        only — an account has nothing to apply. */}
                    <Show when={RECONCILABLE.includes(e.kind)}>
                      <input
                        class="cloud-pick"
                        type="checkbox"
                        checked={picked().has(e.id)}
                        aria-label={`Select ${e.name ?? e.id} for reconcile`}
                        onChange={() => togglePicked(e.id)}
                      />
                    </Show>
                    <button
                      class="cloud-row"
                      classList={{ active: selected() === e.id }}
                      onClick={() => setSelected(e.id)}
                    >
                      <span class="cloud-kind">{e.kind}</span>
                      <span class="cloud-name">{e.name ?? e.id}</span>
                      <Show when={e.status}>
                        <span class="cloud-status">{e.status}</span>
                      </Show>
                      <code class="cloud-id">{e.id}</code>
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </Panel>

        <Panel class="cloud-detail" title="Relationships">
          <Show
            when={detail()}
            fallback={<Empty kind="clear">Pick a result to explore what it connects to.</Empty>}
          >
            {(d) => (
              <>
                <EntityCard e={d().entity} />

                {/* The operations. Everything above this line is a read of the cache;
                    everything below it runs something real against the control plane. */}
                <div class="cloud-actions">
                  {/* `rcc get`, in the terminal, rather than the `cloud_entity_raw` tool.
                      The tool parses the output and returns a value, which this pane then
                      re-serialised — so the document on screen had its keys reordered and
                      anything `rcc` printed that was not JSON dropped. You open the raw
                      document precisely when you have stopped trusting the summary, and a
                      reconstruction is the wrong thing to be handed then. The tool stays on
                      the MCP surface, where a parsed value is what the caller wants. */}
                  {/* First, because when a box looks wrong "read it again" is the first thing
                      you want to try — and until now the only answer was to wait out the TTL.
                      Applies to every kind, unlike everything else in this row. */}
                  <button
                    disabled={refreshing()}
                    data-tip="Re-read this from rcc now, ignoring the cache"
                    onClick={() => refresh(d().entity.id)}
                  >
                    {refreshing() ? "refreshing…" : "refresh"}
                  </button>
                  <button onClick={() => openPty({ kind: "rcc_get", id: d().entity.id })}>
                    view json
                  </button>
                  {/* An anchor rather than a button, because it is a link and should behave
                      like one — middle-click, copy link address, open in a background tab.
                      Every other control here runs something; this one goes somewhere, and
                      the operator's habits about links are worth more than visual symmetry.

                      Only the kinds with metrics get it. The backend returns null for the
                      rest rather than a link to a dashboard with no data on it. */}
                  <Show when={d().grafana}>
                    {(url) => (
                      <a
                        class="cloud-action-link"
                        href={url()}
                        target="_blank"
                        rel="noreferrer"
                        data-tip="Open this in Grafana — the dashboard, with this entity already selected"
                      >
                        grafana ↗
                      </a>
                    )}
                  </Show>
                  <Show when={EDITABLE.includes(d().entity.kind)}>
                    <button
                      class="mutating"
                      onClick={() => openPty({ kind: "rcc_edit", id: d().entity.id })}
                    >
                      edit
                    </button>
                  </Show>
                  {/* The two overrides worth a button. `edit` can set them too, but only by
                      finding two fields in a JSON document and converting GiB to bytes and to a
                      Kubernetes quantity by hand. */}
                  <Show when={d().entity.kind === "environment"}>
                    <button class="mutating" onClick={() => setResourcesFor(d().entity)}>
                      resources…
                    </button>
                  </Show>
                  <Show when={RECONCILABLE.includes(d().entity.kind)}>
                    <button
                      class="mutating"
                      onClick={() => reconcile([d().entity.id])}
                    >
                      reconcile
                    </button>
                  </Show>
                  <Show when={d().entity.kind === "nuon-install"}>
                    {/* Operator mode only — the script's other mode mints a permanent
                        cluster-admin token, which has no business in a browser. */}
                    <button onClick={() => setTokenFor(d().entity)}>kube token…</button>
                  </Show>
                </div>

                {/* An edit that succeeded has changed stored state and changed nothing on
                    the cluster. Saying so, with the fix attached. */}
                <Show when={needsReconcile() === d().entity.id}>
                  <div class="cloud-followup">
                    State written. <strong>Not applied yet</strong> — an edit changes the
                    control plane's copy, not the cluster.
                    <button
                      class="mutating"
                      onClick={() => reconcile([d().entity.id])}
                    >
                      reconcile
                    </button>
                    {/* Dismisses the reminder, and says so rather than saying "discard":
                        the state change is already in the control plane and nothing here
                        takes it back. Leaving with no way to clear the notice is worse —
                        it then sits there implying work that may already be done. */}
                    <button onClick={() => setNeedsReconcile(null)}>
                      dismiss
                    </button>
                  </div>
                </Show>

                <Show when={resourcesFor()}>
                  {(env) => (
                    <ResourcesModal
                      env={env()}
                      onCancel={() => setResourcesFor(null)}
                      onConfirm={(memory_gib, storage_gib) => {
                        // Read the id before closing, for the same reason the token modal does:
                        // clearing the signal disposes the block the accessor belongs to.
                        const id = env().id;
                        setResourcesFor(null);
                        openPty({ kind: "rcc_override", id, memory_gib, storage_gib });
                      }}
                    />
                  )}
                </Show>

                <Show when={tokenFor()}>
                  {(install) => (
                    <KubeTokenModal
                      install={install()}
                      onCancel={() => setTokenFor(null)}
                      onConfirm={(access_level, duration) => {
                        // Read the id BEFORE closing. `install` is the `Show` accessor, and
                        // clearing the signal disposes the block it belongs to — reading it
                        // afterwards threw, so the modal shut and nothing ran at all.
                        const id = install().id;
                        setTokenFor(null);
                        openPty({ kind: "kube_token", install: id, access_level, duration });
                      }}
                    />
                  )}
                </Show>

                <Show when={d().parents.length}>
                  <h4 class="cloud-rel">Belongs to</h4>
                  <RelatedList entities={d().parents} onOpen={setSelected} />
                </Show>

                <Show when={d().child_total}>
                  <h4 class="cloud-rel">
                    Contains <span class="chip-count">{d().child_total}</span>
                    {/* Named out loud rather than silently truncated: an organization with
                        900 accounts must not read as the 200 that fit. */}
                    <Show when={d().child_total > d().children.length}>
                      {" "}
                      <span class="muted">· showing {d().children.length}</span>
                    </Show>
                  </h4>
                  <RelatedList entities={d().children} onOpen={setSelected} />
                </Show>
              </>
            )}
          </Show>
        </Panel>
      </div>

      {/* The terminal lives **below the split**, not inside the detail panel.
          It used to render inside `<Show when={detail()}>`, which tied it to there being a
          selected entity — fine while every command came from that entity's own actions row,
          and wrong the moment a bulk reconcile could be started from the results list with
          nothing selected: the session would run with its output rendered nowhere at all.
          Full width is also simply the right shape for a run of diffs. */}
      {/* `keyed`, and that is the whole reason reconcile worked from the actions row
          and did nothing from the follow-up strip.

          `PtyPane` opens its socket in `onMount` and reads `request` once, inside
          `ws.onopen`. Without `keyed`, swapping one request for another while the
          pane is open leaves the same component instance mounted: the prop changes,
          `onMount` does not re-run, no socket is opened, and the pane goes on showing
          the *previous* command's exited output. Pressing reconcile after an edit or
          an override — the one path that always has a pane already open — therefore
          did nothing at all, silently.

          Keying on the request object (always a fresh literal from `openPty`) tears
          the old pane down, which closes its socket and kills its child, and mounts a
          new one. */}
      <Show when={pty()} keyed>
        {(request) => (
          <PtyPane
            request={request}
            onExit={(code, transcript) => {
              // Only an edit leaves work behind, only if it worked, and only if it
              // actually changed something. `rcc edit` prints "No changes." and
              // exits 0 when you quit without touching the document, so keying off
              // the exit code alone claimed a write on every `:q!` — and then asked
              // for a reconcile that had nothing to apply.
              const r = request;
              setPtyRunning(false);
              // Setting resources is the same `rcc edit`, driven by a
              // non-interactive editor — so it leaves the same unapplied change
              // behind and is detected the same way.
              //
              // It used to key on `/Environment state updated/`, which `rcc` has
              // never printed: on success it says exactly `State updated.`. That
              // string was invented alongside the `rcc override` command that also
              // never existed, so the follow-up could not have fired even once the
              // command ran.
              if (
                code === 0 &&
                (r.kind === "rcc_edit" || r.kind === "rcc_override") &&
                !/No changes\./.test(transcript)
              )
                setNeedsReconcile(r.id);
            }}
            onClose={() => {
              setPtyRunning(false);
              setPty(null);
            }}
          />
        )}
      </Show>
    </div>
  );
}

/// The selected entity in full: its status and every curated field.
function EntityCard(props: { e: CloudEntity }) {
  /// An error with facts behind it is *stale*, not failed, and the difference is the whole
  /// point of keeping the last good copy: "this is what it was, and here is why we have not
  /// re-checked" is a usable answer, where an empty red box is not. Only an entity with
  /// nothing left to show is a failure.
  const empty = () => !props.e.name && !props.e.fields.length;
  return (
    <div class="cloud-entity" classList={{ failed: !!props.e.error && empty() }}>
      <div class="cloud-entity-head">
        <span class="cloud-kind">{props.e.kind}</span>
        <span class="cloud-name">{props.e.name ?? "—"}</span>
        <Show when={props.e.status}>
          <span class="cloud-status">{props.e.status}</span>
        </Show>
      </div>
      <code class="cloud-id">{props.e.id}</code>
      <Show when={props.e.error}>
        <div class="cloud-error" classList={{ stale: !empty() }}>
          <Show when={!empty()}>
            <strong>Showing the last good read.</strong>{" "}
          </Show>
          {props.e.error}
        </div>
      </Show>
      <Show when={props.e.fields.length}>
        <dl class="cloud-fields">
          <For each={props.e.fields}>
            {(f) => (
              <>
                <dt>{f.label}</dt>
                <dd>{f.value}</dd>
              </>
            )}
          </For>
        </dl>
      </Show>
    </div>
  );
}

/// A hop in either direction. Every row is a button, because the whole point is that the
/// next question is one click away rather than a fresh search.
function RelatedList(props: {
  entities: CloudEntity[];
  onOpen: (id: string) => void;
}) {
  return (
    <ul class="cloud-list">
      <For each={props.entities}>
        {(e) => (
          <li>
            <button class="cloud-row" onClick={() => props.onOpen(e.id)}>
              <span class="cloud-kind">{e.kind}</span>
              <span class="cloud-name">{e.name ?? e.id}</span>
              <Show when={e.status}>
                <span class="cloud-status">{e.status}</span>
              </Show>
              <code class="cloud-id">{e.id}</code>
            </button>
          </li>
        )}
      </For>
    </ul>
  );
}

/// Minting a kubectl token is a small form, not a click — it needs an access level and a
/// lifetime, and it has a real effect: an ephemeral credential and a kubeconfig context on
/// this machine. Two dropdowns wedged into the actions row said none of that; a modal states
/// the install it is about and what is being asked for before anything runs.
/// Memory and storage, in GiB.
///
/// The two fields an operator actually reaches for, and the two that are least pleasant to set
/// by hand: storage is persisted as a raw byte count and memory as a Kubernetes quantity on both
/// the request and the limit, so doing it through `edit` means finding two fields in a JSON
/// document and converting units in your head.
///
/// Blank means *leave it alone*, which is why there is no prefill: the entity cache carries the
/// tier but not the effective resources, and a box pre-filled with a guess is worse than an
/// empty one — you would be reading a number the environment may not actually have. What was
/// actually set is printed in the terminal as the edit is applied.
function ResourcesModal(props: {
  env: CloudEntity;
  onCancel: () => void;
  onConfirm: (memoryGiB?: number, storageGiB?: number) => void;
}) {
  const [memory, setMemory] = createSignal("");
  const [storage, setStorage] = createSignal("");

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onCancel();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  /// A field is valid when it is empty (unchanged) or a whole number above zero. Everything
  /// else is refused here as well as in the backend — a disabled button beats a terminal that
  /// opens only to print an error.
  const parse = (raw: string): number | undefined | "bad" => {
    const t = raw.trim();
    if (!t) return undefined;
    const n = Number(t);
    return Number.isInteger(n) && n > 0 ? n : "bad";
  };
  const mem = () => parse(memory());
  const sto = () => parse(storage());
  const bad = () => mem() === "bad" || sto() === "bad";
  const empty = () => mem() === undefined && sto() === undefined;

  return (
    <div class="modal-backdrop" onClick={props.onCancel}>
      <div
        class="modal resources-modal"
        role="dialog"
        aria-modal="true"
        onClick={(e) => e.stopPropagation()}
      >
        <div class="modal-head">
          <span class="cloud-kind">environment</span>
          <button class="modal-close" data-tip="Close (Esc)" onClick={props.onCancel}>
            ✕
          </button>
        </div>
        <h3 class="modal-title">Resources · {props.env.name ?? props.env.id}</h3>
        <div class="modal-meta">
          <code>{props.env.id}</code>
        </div>

        <div class="modal-field">
          <span class="modal-label">Memory per node</span>
          <div class="gib-input">
            <input
              type="number"
              min="1"
              step="1"
              placeholder="unchanged"
              value={memory()}
              onInput={(e) => setMemory(e.currentTarget.value)}
            />
            <span class="gib-unit">GiB</span>
          </div>
          <p class="muted modal-note">
            Sets the request and the limit together. Leave blank to keep the current value.
          </p>
        </div>

        <div class="modal-field">
          <span class="modal-label">Storage</span>
          <div class="gib-input">
            <input
              type="number"
              min="1"
              step="1"
              placeholder="unchanged"
              value={storage()}
              onInput={(e) => setStorage(e.currentTarget.value)}
            />
            <span class="gib-unit">GiB</span>
          </div>
          {/* Said before the button is pressed, not after: the manifest renders
              `max(override, live volume)`, so a smaller number is accepted and then does
              nothing. An operator who expects a shrink should find that out here. */}
          <p class="muted modal-note">
            A floor, not a size — volumes never shrink, so a value below the live volume changes
            nothing.
          </p>
        </div>

        {/* These are overrides: they are how an environment is given more than its tier
            provides, so nothing here is capped. */}
        <p class="muted modal-note">
          Overrides win over the tier. This is an <code>rcc edit</code> driven automatically —
          it merges these two fields into <code>resourceOverrides</code> and leaves everything
          else in the document alone. The write is optimistically locked and validated by the
          control plane, and it reaches the cluster only once you reconcile.
        </p>

        <div class="modal-actions">
          <button onClick={props.onCancel}>cancel</button>
          <button
            class="mutating"
            disabled={bad() || empty()}
            data-tip={
              bad()
                ? "Whole GiB above zero, or blank to leave unchanged"
                : empty()
                  ? "Set a memory or a storage value"
                  : "Runs rcc edit to merge these into resourceOverrides, then offers the reconcile"
            }
            onClick={() =>
              props.onConfirm(
                mem() === "bad" ? undefined : (mem() as number | undefined),
                sto() === "bad" ? undefined : (sto() as number | undefined),
              )
            }
          >
            review change
          </button>
        </div>
      </div>
    </div>
  );
}

function KubeTokenModal(props: {
  install: CloudEntity;
  onCancel: () => void;
  onConfirm: (accessLevel: string, duration: string) => void;
}) {
  const [level, setLevel] = createSignal("support");
  const [duration, setDuration] = createSignal("24h");

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onCancel();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  return (
    <div class="modal-backdrop" onClick={props.onCancel}>
      <div
        class="modal kube-modal"
        role="dialog"
        aria-modal="true"
        onClick={(e) => e.stopPropagation()}
      >
        <div class="modal-head">
          <span class="cloud-kind">nuon install</span>
          <button class="modal-close" data-tip="Close (Esc)" onClick={props.onCancel}>
            ✕
          </button>
        </div>
        <h3 class="modal-title">Kubectl access · {props.install.name ?? props.install.id}</h3>
        <div class="modal-meta">
          <code>{props.install.id}</code>
        </div>

        <div class="modal-field">
          <span class="modal-label">Access level</span>
          <div class="modal-options">
            <For each={ACCESS_LEVELS}>
              {(l) => (
                <button
                  class="chip"
                  classList={{ active: level() === l }}
                  onClick={() => setLevel(l)}
                >
                  {l}
                </button>
              )}
            </For>
          </div>
          {/* Named rather than implied: these are different amounts of power over a
              customer's cluster, and the default is the lesser one. */}
          <p class="muted modal-note">
            <Show
              when={level() === "admin"}
              fallback="Read-mostly support access — the default, and enough for nearly everything."
            >
              Full administrative access to the cluster. Take it only when support cannot do
              the job.
            </Show>
          </p>
        </div>

        <div class="modal-field">
          <span class="modal-label">Lifetime</span>
          <div class="modal-options">
            <For each={DURATIONS}>
              {(d) => (
                <button
                  class="chip"
                  classList={{ active: duration() === d.value }}
                  onClick={() => setDuration(d.value)}
                >
                  {d.label}
                </button>
              )}
            </For>
          </div>
          <p class="muted modal-note">
            The token expires on its own. A kubeconfig context is written for this install; no
            credential is printed into the page.
          </p>
        </div>

        <div class="modal-actions">
          <button onClick={props.onCancel}>cancel</button>
          <button class="mutating" onClick={() => props.onConfirm(level(), duration())}>
            get token
          </button>
        </div>
      </div>
    </div>
  );
}
