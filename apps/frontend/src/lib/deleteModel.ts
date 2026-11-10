/**
 * Deleting a GGUF from this machine's disk.
 *
 * The node warns but does not prevent: a deletion that bumps into something
 * (a slice in service, a transfer in progress, an Ollama blob) returns 409 with
 * the list of warnings, and only then do we ask whether the operator really
 * wants it. The two steps live here because the whole-models screen and the
 * slices screen do exactly the same thing, and two copies would diverge in the
 * warning text - which is the part that matters. The confirmation callback
 * may be asynchronous so screens can use the shared in-app dialog.
 */
import { api } from "./api";
import { formatMessage, getActiveLocale } from "../i18n/locales";

export interface DeleteWarning {
  kind: "serving" | "transfer" | "ollama_store";
  message: string;
}

export type DeleteOutcome =
  | { status: "deleted"; freedBytes: number }
  | { status: "cancelled" };

function warningsOf(error: unknown): DeleteWarning[] {
  const body = (error as { body?: { warnings?: DeleteWarning[] } })?.body;
  return body?.warnings ?? [];
}

/**
 * `confirm` receives the text and returns whether the operator agreed. It is
 * injected so the screen can choose how to ask, including asynchronously with
 * the shared dialog, and so the test does not need a `window`.
 */
export async function deleteModel(
  path: string,
  name: string,
  confirm: (message: string) => boolean | Promise<boolean>,
): Promise<DeleteOutcome> {
  if (!(await confirm(formatMessage(getActiveLocale(), "models.deleteConfirm", { name }) + "\n\n" + path))) {
    return { status: "cancelled" };
  }

  try {
    const r = await api.post<{ freed_bytes: number }>("/api/ui/models/delete", { path });
    return { status: "deleted", freedBytes: r?.freed_bytes ?? 0 };
  } catch (error) {
    const warnings = warningsOf(error);
    if (warnings.length === 0) throw error;

    const list = warnings.map((w) => `• ${w.message}`).join("\n");
    if (!(await confirm(formatMessage(getActiveLocale(), "models.deleteWarning") + "\n\n" + list + "\n\n" + formatMessage(getActiveLocale(), "models.deleteForce")))) {
      return { status: "cancelled" };
    }
    const r = await api.post<{ freed_bytes: number }>("/api/ui/models/delete", {
      path,
      force: true,
    });
    return { status: "deleted", freedBytes: r?.freed_bytes ?? 0 };
  }
}
