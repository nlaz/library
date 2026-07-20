// ---------------------------------------------------------------------------
// toast: transient notices bottom-left; sticky (errors) stay until clicked
// ---------------------------------------------------------------------------

import { $toast } from "./dom";

let toastTimer: ReturnType<typeof setTimeout> | undefined;

export function notify(msg: string, opts?: { sticky?: boolean }) {
  $toast.textContent = msg;
  $toast.classList.toggle("error", !!opts?.sticky);
  $toast.hidden = false;
  clearTimeout(toastTimer);
  if (!opts?.sticky) toastTimer = setTimeout(() => ($toast.hidden = true), 4000);
}
$toast.addEventListener("click", () => ($toast.hidden = true));
