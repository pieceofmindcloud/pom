import { Check, Clipboard, X } from "lucide-react";
import { useMemo, useState } from "react";
import { Button, Mono } from "./ui";
import type { RequestLogEntry } from "../lib/types";
import { useI18n } from "../i18n/provider";

type InspectorTab = "request" | "response" | "telemetry";

const SENSITIVE_KEYS = new Set([
  "authorization",
  "proxy_authorization",
  "x_api_key",
  "api_key",
  "connection_key",
  "access_token",
  "refresh_token",
  "token",
  "secret",
  "password",
  "cookie",
  "set_cookie",
  "credential",
  "private_key",
]);

function normalizedKey(key: string) {
  return key.toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_|_$/g, "");
}

function isSensitiveKey(key: string) {
  const normalized = normalizedKey(key);
  return (
    [...SENSITIVE_KEYS].some((sensitive) => normalized === sensitive || normalized.endsWith(`_${sensitive}`)) ||
    normalized.endsWith("_token") ||
    normalized.endsWith("_secret") ||
    normalized.endsWith("_password") ||
    normalized.endsWith("_credential")
  );
}

/**
 * Request logs intentionally omit secrets server-side, but this second guard
 * protects copy-to-chat when a request body contains headers or credentials.
 */
export function redactSensitive(value: unknown, parentKey?: string): unknown {
  if (parentKey && isSensitiveKey(parentKey)) return "[REDACTED]";
  if (Array.isArray(value)) return value.map((item) => redactSensitive(item));
  if (value === null || typeof value !== "object") return value;

  const record = value as Record<string, unknown>;
  const namedField = [record.name, record.key, record.header].find(
    (field): field is string => typeof field === "string",
  );
  return Object.fromEntries(
    Object.entries(record).map(([key, child]) => [
      key,
      namedField && key === "value" && isSensitiveKey(namedField)
        ? "[REDACTED]"
        : redactSensitive(child, key),
    ]),
  );
}

function formatJson(value: unknown) {
  return JSON.stringify(redactSensitive(value), null, 2);
}

function requestBundle(entry: RequestLogEntry) {
  return {
    REQUEST: {
      id: entry.id,
      received_at: entry.received_at,
      route: entry.route,
      model: entry.model,
      api_key_identity: {
        id: entry.api_key_id,
        label: entry.api_key_label,
        node_id: entry.api_key_node_id,
      },
      entry_node_id: entry.entry_node_id,
      prompt: entry.prompt,
      request_details: entry.request_details,
    },
    RESPONSE: {
      completed_at: entry.completed_at,
      status: entry.status,
      result: entry.result,
      error: entry.error,
      source: entry.source,
      hops: entry.hops,
      prompt_tokens: entry.prompt_tokens,
      completion_tokens: entry.completion_tokens,
      duration_ms: entry.duration_ms,
    },
    TELEMETRIA: {
      timings: entry.timings ?? null,
      telemetry: entry.telemetry,
    },
  };
}

export function RequestInspector({
  entry,
  onClose,
}: {
  entry: RequestLogEntry;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const [tab, setTab] = useState<InspectorTab>("request");
  const [copied, setCopied] = useState(false);
  const bundle = useMemo(() => requestBundle(entry), [entry]);
  const sections: Record<InspectorTab, unknown> = {
    request: bundle.REQUEST,
    response: bundle.RESPONSE,
    telemetry: bundle.TELEMETRIA,
  };
  const labels: Record<InspectorTab, string> = {
    request: t("tokenLogs.inspectorRequest"),
    response: t("tokenLogs.inspectorResponse"),
    telemetry: t("tokenLogs.inspectorTelemetry"),
  };

  async function copyBundle() {
    const text = formatJson(bundle);
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
      } else {
        const helper = document.createElement("textarea");
        helper.value = text;
        helper.style.position = "fixed";
        helper.style.opacity = "0";
        document.body.appendChild(helper);
        helper.select();
        document.execCommand("copy");
        helper.remove();
      }
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    } catch {
      setCopied(false);
    }
  }

  return (
    <aside className="flex min-h-0 min-w-0 flex-col border-t border-line bg-surface lg:border-l lg:border-t-0">
      <div className="flex shrink-0 items-start justify-between gap-3 border-b border-line px-4 py-3">
        <div className="min-w-0">
          <p className="eyebrow">{t("tokenLogs.inspectorTitle")}</p>
          <Mono className="mt-1 block truncate text-[0.68rem] text-muted">{entry.id}</Mono>
        </div>
        <Button
          variant="ghost"
          size="sm"
          icon={<X className="h-3.5 w-3.5" />}
          aria-label={t("tokenLogs.closeInspector")}
          onClick={onClose}
        />
      </div>
      <div className="flex shrink-0 items-center justify-between gap-2 border-b border-line px-3 py-2">
        <div role="tablist" aria-label={t("tokenLogs.inspectorTabs")} className="flex min-w-0 gap-1">
          {(Object.keys(labels) as InspectorTab[]).map((key) => (
            <button
              key={key}
              type="button"
              role="tab"
              aria-selected={tab === key}
              className={
                tab === key
                  ? "rounded-md bg-iris/15 px-2 py-1 text-[0.65rem] font-semibold tracking-[0.08em] text-iris"
                  : "rounded-md px-2 py-1 text-[0.65rem] font-semibold tracking-[0.08em] text-faint hover:bg-raised hover:text-fg"
              }
              onClick={() => setTab(key)}
            >
              {labels[key]}
            </button>
          ))}
        </div>
        <Button
          variant="outline"
          size="sm"
          icon={copied ? <Check className="h-3.5 w-3.5" /> : <Clipboard className="h-3.5 w-3.5" />}
          onClick={() => void copyBundle()}
        >
          {copied ? t("tokenLogs.copied") : t("tokenLogs.copyRequestData")}
        </Button>
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-3">
        <label className="sr-only" htmlFor="request-inspector-json">{labels[tab]}</label>
        <textarea
          id="request-inspector-json"
          readOnly
          value={formatJson(sections[tab])}
          spellCheck={false}
          className="min-h-[18rem] w-full resize-y rounded-lg border border-line bg-ink p-3 font-mono text-[0.72rem] leading-relaxed text-fg outline-none focus:border-iris"
          aria-label={labels[tab]}
        />
        <p className="mt-2 text-[0.68rem] leading-relaxed text-faint">{t("tokenLogs.copyHint")}</p>
      </div>
    </aside>
  );
}
