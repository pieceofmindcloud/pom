import { Boxes, ChevronDown, Loader2, TriangleAlert } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { duration, ms, num } from "../lib/format";
import type { InferenceHeadCandidate, InferenceHeadsResponse } from "../lib/types";

const AUTO = "__head-auto";

export function InferenceHeadChoice({
  model,
  response,
  loading = false,
  failed = false,
  selectedNodeId,
  onChange,
}: {
  model: string;
  response?: InferenceHeadsResponse;
  loading?: boolean;
  failed?: boolean;
  selectedNodeId?: string | null;
  /** `null`: Auto - no machine is pinned, the gateway balances the heads by queue. */
  onChange?: (nodeId: string | null) => void;
}) {
  const { t } = useI18n();

  if (loading && !response) {
    return (
      <section className="mt-3 rounded-xl border border-line bg-raised/45 px-4 py-3" aria-live="polite">
        <div className="flex items-center gap-2 text-sm text-muted">
          <Loader2 className="h-4 w-4 animate-spin text-iris" />
          <span>{t("chat.headLoading", { model })}</span>
        </div>
      </section>
    );
  }

  if (failed && !response) {
    return (
      <section className="mt-3 rounded-xl border border-amber/35 bg-amber/[0.07] px-4 py-3" aria-live="polite">
        <div className="flex min-w-0 items-start gap-2.5">
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
          <div className="min-w-0">
            <p className="text-sm font-medium text-fg">{t("chat.headUnavailable")}</p>
            <p className="mt-1 text-xs leading-relaxed text-muted">{t("chat.headUnavailableDetail")}</p>
          </div>
        </div>
      </section>
    );
  }

  if (
    !response ||
    response.status === "invalid_model" ||
    response.status === "no_candidate" ||
    response.candidates.length === 0
  ) {
    return (
      <section className="mt-3 rounded-xl border border-amber/35 bg-amber/[0.07] px-4 py-3" aria-live="polite">
        <div className="flex min-w-0 items-start gap-2.5">
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
          <div className="min-w-0">
            <p className="break-words text-sm font-medium text-fg">
              {t("chat.headNoCandidate", { model })}
            </p>
            <p className="mt-1 text-xs leading-relaxed text-muted">{t("chat.headNoCandidateDetail")}</p>
          </div>
        </div>
      </section>
    );
  }

  // A single live head is not a choice. Keep the composer as quiet as the
  // model picker is when there is nothing to choose between.
  if (response.candidates.length === 1) return null;

  // the pinned machine; none is Auto, where the planner's own pick only tells the RTT state shown
  const pinned = response.candidates.find((candidate) => candidate.node_id === selectedNodeId);
  const selected = pinned ?? response.candidates.find(
    (candidate) => candidate.selected || candidate.node_id === response.selected_node_id,
  ) ?? response.candidates[0];
  const policy = t("chat.headPolicy", { validity: duration(response.rtt_valid_for_secs) });
  const fallback = selected.rtt_state !== "fresh"
    ? `${t("chat.headFallback")} · ${t("chat.headFallbackDetail")}`
    : undefined;

  return (
    <div className="mt-3 min-w-0" aria-live="polite">
      <div className="group relative inline-flex h-8 min-w-0 items-center rounded-lg border border-line bg-raised transition-colors duration-150 hover:border-line-strong focus-within:border-iris focus-within:ring-2 focus-within:ring-iris/25">
        <Boxes className="pointer-events-none absolute left-2.5 h-3.5 w-3.5 text-faint transition-colors duration-150 group-hover:text-muted" />
        <select
          value={pinned?.node_id ?? AUTO}
          aria-label={t("chat.headPicker")}
          title={`${policy} · ${t("chat.headPickerHint")}`}
          onChange={(event) => onChange?.(event.target.value === AUTO ? null : event.target.value)}
          className="h-8 w-full min-w-0 max-w-[18rem] cursor-pointer appearance-none truncate bg-transparent pl-8 pr-7 text-[0.78rem] text-fg focus:outline-none"
        >
          <option value="__head-policy" disabled>
            {t("chat.headCandidateCount", { count: response.candidates.length })} · {policy}
          </option>
          {fallback && (
            <option value="__head-fallback" disabled>
              {fallback}
            </option>
          )}
          {response.status === "incomplete_pipeline" && (
            <option value="__head-incomplete" disabled>
              {t("chat.headIncomplete", { model })} · {t("chat.headIncompleteDetail")}
            </option>
          )}
          <option value={AUTO}>{t("chat.headAuto")}</option>
          {response.candidates.map((candidate) => (
            <option key={candidate.node_id} value={candidate.node_id}>
              {candidateLabel(candidate, candidate.node_id === pinned?.node_id, t)}
            </option>
          ))}
        </select>
        <ChevronDown className="pointer-events-none absolute right-2 h-3.5 w-3.5 text-faint transition-colors duration-150 group-hover:text-muted" />
      </div>
    </div>
  );
}

function candidateLabel(
  candidate: InferenceHeadCandidate,
  selected: boolean,
  t: ReturnType<typeof useI18n>["t"],
) {
  const name = candidate.hostname || candidate.short_id;
  const rtt = candidate.rtt_state === "fresh"
    ? t("chat.headRttFresh", { value: ms(candidate.rtt_ms) })
    : candidate.rtt_state === "stale"
      ? t("chat.headRttStale", {
          value: ms(candidate.rtt_ms),
          age: duration(Math.ceil((candidate.rtt_age_ms ?? 0) / 1000)),
        })
      : t("chat.headRttUnknown");
  const layers = t("chat.headLayers", {
    start: candidate.start_layer,
    end: candidate.end_layer,
    total: candidate.total_layers,
  });
  const capacity = candidate.runtime_capacity
    ? t("chat.headCapacity", {
        context: num(candidate.runtime_capacity.n_ctx),
        leg: num(candidate.runtime_capacity.max_new_tokens),
      })
    : undefined;

  return [
    name,
    candidate.node_id,
    rtt,
    candidate.load == null ? t("dashboard.inFlightUnavailable") : t("chat.headLoad", { count: candidate.load }),
    layers,
    capacity,
    selected ? t("chat.headSelected") : undefined,
  ].filter(Boolean).join(" · ");
}
