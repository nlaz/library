// ---------------------------------------------------------------------------
// launch screen: what the window holds while the stores open and — on a
// machine that has never run this app — while three hundred megabytes of
// model comes down from HuggingFace.
//
// It is drawn as an accession card: every step of startup is listed from the
// first frame and stamped as it completes, so the wait is legible rather than
// merely indicated. Two rules shape the rest. It must never be the last thing
// a user sees, so any startup that stalls past the point where search
// actually works offers a way in. And it must not flash: a warm launch
// resolves in under a second, and a panel that appears for 300ms reads as a
// glitch, so nothing is drawn until a grace period has passed.
// ---------------------------------------------------------------------------

import { launchView, type LaunchRow } from "./launch-model";
import type { StartupStatus } from "./types";

const $screen = document.getElementById("launch")!;
const $rows = document.getElementById("launch-rows")!;
const $skip = document.getElementById("launch-skip") as HTMLButtonElement;

/** Below this, a launch is "instant" and never draws — see the module note. */
const SHOW_AFTER = 400;
/** Once the engine is up, search works; a slow model download shouldn't hold
 * anyone hostage past this point. */
const OFFER_SKIP_AFTER = 4000;

const MARK: Record<LaunchRow["state"], string> = { done: "✓", active: "›", pending: "" };

let dismissed = false;
let shown = false;

function dismiss() {
  dismissed = true;
  $screen.hidden = true;
}

/** Draw a status. Idempotent — the same status arriving twice repaints the
 * same rows and nothing else. */
function render(s: StartupStatus) {
  if (dismissed) return;
  const view = launchView(s);
  if (!view.show) {
    dismiss();
    return;
  }
  if (!shown) return; // still inside the grace period: nothing is drawn yet

  // Rebuilt rather than diffed: the list is three rows long and only changes
  // a few times per launch, so the simplest correct thing is the right one.
  $rows.replaceChildren(
    ...view.rows.map((r) => {
      const row = document.createElement("div");
      row.className = `lrow ${r.state}`;
      const mark = document.createElement("span");
      mark.className = "lmark";
      mark.textContent = MARK[r.state];
      const label = document.createElement("span");
      label.textContent = r.label;
      const note = document.createElement("span");
      note.className = "lnote";
      note.textContent = r.note;
      row.append(mark, label, note);
      return row;
    }),
  );
}

/**
 * Own the launch screen for one startup.
 *
 * `poll` is the latched status (the event stream alone can't be trusted:
 * with a warm cache the whole sequence can finish before this module runs).
 * `onSkip` fires when someone chooses to go in early — the models keep
 * downloading behind them, and figure search lights up when they land.
 */
export async function initLaunch(opts: {
  poll(): Promise<StartupStatus>;
  subscribe(cb: (s: StartupStatus) => void): void;
  onSkip?(): void;
}) {
  opts.subscribe(render);

  const graceTimer = setTimeout(() => {
    if (dismissed) return;
    shown = true;
    $screen.hidden = false;
    void opts.poll().then(render);
  }, SHOW_AFTER);

  const skipTimer = setTimeout(() => {
    if (!dismissed) $skip.hidden = false;
  }, OFFER_SKIP_AFTER);

  $skip.addEventListener("click", () => {
    dismiss();
    opts.onSkip?.();
  });

  // subscribe-then-recheck: closes the window between the listener landing
  // and a status that may already have been emitted
  render(await opts.poll());
  if (dismissed) {
    clearTimeout(graceTimer);
    clearTimeout(skipTimer);
  }
}
