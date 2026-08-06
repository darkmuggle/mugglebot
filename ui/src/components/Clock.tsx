import { createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js";

/// The three zones the team spans. Named as IANA identifiers rather than fixed offsets,
/// so every offset here is asked of the browser's own tz database at the instant being
/// shown. That is what "accounts for daylight saving" means in practice: nothing in this
/// file knows any rule, and none of it has to be revisited when a rule changes.
///
/// It matters because the zones don't move together. The US springs forward on the second
/// Sunday in March and the EU on the last, so for the ~two weeks between, Denver↔London
/// is six hours instead of the usual seven and Denver↔Berlin is seven instead of eight.
/// A table of fixed offsets is wrong twice a year; this is never wrong.
///
/// UTC leads the row as the `anchor`. Every offset on the strip is written relative to
/// it, so with UTC absent those chips are a subtraction you have to do in your head, and
/// with it present each city's time is one you can check. It is set apart by a rule
/// rather than mixed in, because it is not a place — the three cities are in west-to-east
/// order and inserting a non-place into that run breaks it.
const ZONES: { label: string; tz: string; anchor?: boolean }[] = [
  { label: "UTC", tz: "UTC", anchor: true },
  { label: "Denver", tz: "America/Denver" },
  { label: "London", tz: "Europe/London" },
  { label: "Berlin", tz: "Europe/Berlin" },
];

/// The reason to look at another city's clock is rarely the time itself — it is "can
/// I raise this with them now, or does it wait until their morning". That lives in the
/// hover text rather than in the styling: three zones set three different ways read as
/// three different *kinds* of thing, when they are one row of clocks.
///
/// A blunt weekday 09:00–18:00, not anyone's real calendar: this is a hint about when
/// to expect a reply, and dressing a guess up as a schedule would only invite it to
/// be trusted.
const WORK_START = 9;
const WORK_END = 18;

const DAY_MS = 86_400_000;
const HOUR_MS = 3_600_000;

type Reading = {
  label: string;
  tz: string;
  time: string;
  /// The live UTC offset, e.g. `UTC−6`. Derived from the zone's own rules at *this*
  /// instant, so it reads `UTC−6` in July and `UTC−7` in January without anything
  /// here knowing that Denver observes daylight saving.
  utc: string;
  /// The zone's weekday, shown only when its calendar date isn't yours.
  day: string;
  /// `+1` when it is already tomorrow there, `−1` when it is still yesterday.
  dayShift: string;
  atWork: boolean;
  here: boolean;
  /// What the offset is now and when it next changes, for the hover text.
  rule: string;
  /// UTC — the reference, not a place. No offset chip (its own offset is zero by
  /// definition, and `UTC+0` beside the label would be saying it twice), no working
  /// hours, and no changeover to look for.
  anchor: boolean;
};

function parts(tz: string, at: Date): Record<string, string> {
  const got = new Intl.DateTimeFormat("en-CA", {
    timeZone: tz,
    // `hour12: false` yields a 24:00 hour on some engines; h23 is the one that
    // reliably means midnight is 00.
    hourCycle: "h23",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    weekday: "short",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).formatToParts(at);
  const out: Record<string, string> = {};
  for (const p of got) out[p.type] = p.value;
  return out;
}

/// A zone's offset from UTC at a given instant, in minutes.
///
/// Read back out of the zone's own formatted wall clock rather than from a table: format
/// the instant in the target zone, reinterpret those fields as if they were UTC, and the
/// difference is the offset. Whatever daylight-saving rule the zone follows — and whenever
/// its government last changed it — is already baked into what the formatter returned.
function offsetMinutes(tz: string, at: Date): number {
  const p = parts(tz, at);
  const asUtc = Date.parse(
    `${p.year}-${p.month}-${p.day}T${p.hour}:${p.minute}:${p.second}Z`,
  );
  return Math.round((asUtc - Math.floor(at.getTime() / 1000) * 1000) / 60_000);
}

function utcLabel(minutes: number): string {
  const size = Math.abs(minutes);
  const h = Math.floor(size / 60);
  const m = size % 60;
  // U+2212 minus rather than a hyphen: it is the width of a digit, so the offsets
  // stay in column with each other.
  const sign = minutes < 0 ? "−" : "+";
  return `UTC${sign}${h}${m ? `:${String(m).padStart(2, "0")}` : ""}`;
}

/// The zone's own name for what it is currently observing — `MDT`, `BST`, `CEST`.
///
/// ICU only carries these abbreviations for the locales that use them: `en-US` knows
/// `MDT` but renders Berlin as `GMT+2`, and `en-GB` knows `CEST` but renders Denver as
/// `GMT-6`. So ask both and take whichever answered with a name instead of another
/// offset; if neither does, the offset alone says it.
function abbrev(tz: string, at: Date): string {
  for (const locale of ["en-US", "en-GB"]) {
    const got = new Intl.DateTimeFormat(locale, { timeZone: tz, timeZoneName: "short" })
      .formatToParts(at)
      .find((p) => p.type === "timeZoneName")?.value;
    if (got && !/^(GMT|UTC)/.test(got)) return got;
  }
  return "";
}

/// When this zone's offset next changes, and to what — `null` if it does not within a
/// year, which is the answer for a zone that has stopped observing daylight saving.
///
/// Found by search rather than by knowing any rule: step forward a day at a time until
/// the offset differs, then bisect that day down to the hour. A transition is a property
/// of the tz database, and asking it beats encoding "last Sunday in October" here — those
/// dates are political and they do move.
function nextShift(tz: string, from: Date): { on: Date; minutes: number } | null {
  const base = offsetMinutes(tz, from);
  let lo = from.getTime();
  let hi = 0;
  for (let d = 1; d <= 400; d++) {
    const t = from.getTime() + d * DAY_MS;
    if (offsetMinutes(tz, new Date(t)) !== base) {
      hi = t;
      lo = t - DAY_MS;
      break;
    }
  }
  if (!hi) return null;
  while (hi - lo > HOUR_MS) {
    const mid = lo + Math.floor((hi - lo) / 2);
    if (offsetMinutes(tz, new Date(mid)) === base) lo = mid;
    else hi = mid;
  }
  return { on: new Date(hi), minutes: offsetMinutes(tz, new Date(hi)) };
}

/// `nextShift` costs a few hundred formats, and the clock re-reads every second. The
/// answer only changes when a transition passes, so it is cached per zone per day —
/// which also means a session left open across a changeover picks up the new one.
const shifts = new Map<string, string>();

function ruleFor(tz: string, at: Date, today: string): string {
  const key = `${tz}|${today}`;
  const cached = shifts.get(key);
  if (cached !== undefined) return cached;
  const now = utcLabel(offsetMinutes(tz, at));
  const name = abbrev(tz, at);
  const next = nextShift(tz, at);
  const when = next
    ? ` · ${utcLabel(next.minutes)} from ${new Intl.DateTimeFormat("en-GB", {
        timeZone: tz,
        day: "numeric",
        month: "short",
      }).format(next.on)}`
    : "";
  const rule = `${name ? `${name}, ` : ""}${now}${when}`;
  shifts.set(key, rule);
  return rule;
}

/// Whole days between two `YYYY-MM-DD` dates, as a signed count. Comparing the
/// dates rather than the offsets is what makes this right across a DST boundary.
function dayDelta(there: string, here: string): number {
  const ms = Date.parse(`${there}T00:00:00Z`) - Date.parse(`${here}T00:00:00Z`);
  return Math.round(ms / DAY_MS);
}

function read(at: Date, localZone: string): Reading[] {
  const mine = parts(localZone, at);
  const myDate = `${mine.year}-${mine.month}-${mine.day}`;
  return ZONES.map(({ label, tz, anchor }) => {
    const p = parts(tz, at);
    const date = `${p.year}-${p.month}-${p.day}`;
    const delta = dayDelta(date, myDate);
    const hour = Number(p.hour);
    const weekend = p.weekday === "Sat" || p.weekday === "Sun";
    return {
      label,
      tz,
      time: `${p.hour}:${p.minute}:${p.second}`,
      utc: anchor ? "" : utcLabel(offsetMinutes(tz, at)),
      day: p.weekday,
      // The day marker earns its place on the anchor as much as anywhere: from a Denver
      // evening it is already tomorrow in UTC, and a log line stamped in UTC is then a
      // date you would otherwise read wrong.
      dayShift: delta > 0 ? `+${delta}` : delta < 0 ? `−${-delta}` : "",
      atWork: !anchor && !weekend && hour >= WORK_START && hour < WORK_END,
      here: tz === localZone,
      rule: anchor ? "" : ruleFor(tz, at, myDate),
      anchor: anchor === true,
    };
  });
}

/// The clock strip along the foot of every view.
///
/// It ticks on a one-second interval re-aligned to the wall clock each time, because
/// a plain `setInterval(1000)` drifts against the second boundary and the digits then
/// skip a value every minute or so — visible on something whose whole job is to show
/// the time.
export default function Clock() {
  const localZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  const [now, setNow] = createSignal(new Date());
  const readings = createMemo(() => read(now(), localZone));

  onMount(() => {
    let timer: number;
    const tick = () => {
      const at = new Date();
      setNow(at);
      timer = window.setTimeout(tick, 1000 - at.getMilliseconds());
    };
    tick();
    onCleanup(() => window.clearTimeout(timer));
  });

  return (
    <footer class="lcars-foot">
      <div class="foot-cap" />
      <For each={readings()}>
        {(z) => (
          <div
            class="zone"
            classList={{ anchor: z.anchor }}
            data-tip={
              z.anchor
                ? "Coordinated Universal Time — what every offset on this strip is measured against"
                : `${z.tz} · ${z.rule} · ${
                    z.atWork ? "working hours" : "outside working hours"
                  }${z.here ? " · your zone" : ""}`
            }
          >
            <span class="zone-label">{z.label}</span>
            <span class="zone-time">{z.time}</span>
            <Show when={z.utc}>
              <span class="zone-utc">{z.utc}</span>
            </Show>
            <Show when={z.dayShift}>
              <span class="zone-day">
                {z.day} {z.dayShift}
              </span>
            </Show>
          </div>
        )}
      </For>
    </footer>
  );
}
