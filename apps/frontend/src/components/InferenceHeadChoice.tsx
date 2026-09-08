import { Check, Gauge, Loader2, TriangleAlert, Wifi } from "lucide-react";
import { Badge, Dot } from "./ui";
import { useI18n } from "../i18n/provider";
import { duration, ms, num } from "../lib/format";
import type { InferenceHeadCandidate, InferenceHeadsResponse } from "../lib/types";

export function InferenceHeadChoice({
  model,
  response,
  loading = false,
  failed = false,
}: {
  model: string;
  response?: InferenceHeadsResponse;
  loading?: boolean;
  failed?: boolean;
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

  if (!response || response.status === "invalid_model" || response.status === "no_candidate") {
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

  const selected = response.candidates.find((candidate) => candidate.selected);
  const incomplete = response.status === "incomplete_pipeline";

  return (
    <section
      className="mt-3 min-w-0 overflow-hidden rounded-xl border border-line bg-raised/45"
      aria-label={t("chat.headChoice")}
      aria-live="polite"
    >
      <div className="flex min-w-0 flex-wrap items-start justify-between gap-3 border-b border-line px-4 py-3">
        <div className="min-w-0">
          <div className="eyebrow mb-1">{t("chat.headChoice")}</div>
          <p className="break-words text-sm font-medium text-fg">{t("chat.headChoiceDetail")}</p>
        </div>
        <Badge tone={incomplete ? "amber" : "iris"} className="shrink-0">
          {t("chat.headCandidateCount", { count: response.candidates.length })}
        </Badge>
      </div>

      {incomplete && (
        <div className="flex min-w-0 items-start gap-2.5 border-b border-amber/20 bg-amber/[0.06] px-4 py-3">
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
          <div className="min-w-0">
            <p className="text-sm font-medium text-fg">{t("chat.headIncomplete", { model })}</p>
            <p className="mt-1 text-xs leading-relaxed text-muted">{t("chat.headIncompleteDetail")}</p>
          </div>
        </div>
      )}

      <div className="min-w-0 px-4 py-3">
        {selected && (
          <div className="mb-3 rounded-lg border border-iris/40 bg-iris/[0.08] px-3 py-2.5">
            <div className="flex min-w-0 items-center gap-2 text-[0.68rem] font-medium uppercase tracking-[0.13em] text-iris">
              <Check className="h-3.5 w-3.5 shrink-0" />
              <span>{t("chat.headSelected")}</span>
            </div>
            {selected.rtt_state !== "fresh" && (
              <div className="mt-2 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
                <Badge tone="amber">
                  {t("chat.headFallback")}
                </Badge>
                <span className="min-w-0 break-words text-[0.68rem] text-muted">
                  {t("chat.headFallbackDetail")}
                </span>
              </div>
            )}
            <Candidate candidate={selected} selected />
          </div>
        )}

        <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-3 gap-y-1.5">
          <p className="text-[0.68rem] font-medium uppercase tracking-[0.12em] text-faint">
            {t("chat.headAlternatives")}
          </p>
          <p className="min-w-0 break-words text-right text-[0.68rem] text-faint">
            {t("chat.headPolicy", { validity: duration(response.rtt_valid_for_secs) })}
          </p>
        </div>
        <div className="mt-2 grid min-w-0 gap-2 sm:grid-cols-2">
          {response.candidates
            .filter((candidate) => !candidate.selected)
            .map((candidate) => (
                <Candidate key={candidate.node_id} candidate={candidate} />
            ))}
        </div>
      </div>
    </section>
  );
}

function Candidate({
  candidate,
  selected = false,
}: {
  candidate: InferenceHeadCandidate;
  selected?: boolean;
}) {
  const { t } = useI18n();
  const name = candidate.hostname || candidate.short_id;
  const rtt = candidate.rtt_state === "fresh"
    ? t("chat.headRttFresh", { value: ms(candidate.rtt_ms) })
    : candidate.rtt_state === "stale"
      ? t("chat.headRttStale", {
          value: ms(candidate.rtt_ms),
          age: duration(Math.ceil((candidate.rtt_age_ms ?? 0) / 1000)),
        })
      : t("chat.headRttUnknown");

  return (
    <div className={selected ? "mt-2 min-w-0" : "min-w-0 rounded-lg border border-line bg-surface/45 px-3 py-2.5"}>
      <div className="flex min-w-0 items-start justify-between gap-2">
        <div className="min-w-0">
          <p className="truncate text-sm font-semibold text-fg" title={name}>{name}</p>
          <p className="break-path mt-0.5 text-[0.68rem] text-faint">{candidate.node_id}</p>
        </div>
        {selected && <Dot tone="iris" pulse />}
      </div>
      <div className="mt-2 flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1.5 text-[0.68rem] text-muted">
        <span className="inline-flex min-w-0 items-center gap-1.5">
          <Wifi className="h-3.5 w-3.5 shrink-0 text-faint" />
          <span className="truncate">{rtt}</span>
        </span>
        <span className="inline-flex min-w-0 items-center gap-1.5">
          <Gauge className="h-3.5 w-3.5 shrink-0 text-faint" />
          <span>{candidate.load == null ? t("dashboard.inFlightUnavailable") : t("chat.headLoad", { count: candidate.load })}</span>
        </span>
      </div>
      <p className="mt-1.5 break-words text-[0.68rem] text-faint">
        {t("chat.headLayers", {
          start: candidate.start_layer,
          end: candidate.end_layer,
          total: candidate.total_layers,
        })}
        {candidate.runtime_capacity && (
          <span className="ml-2">{t("chat.headCapacity", {
            context: num(candidate.runtime_capacity.n_ctx),
            leg: num(candidate.runtime_capacity.max_new_tokens),
          })}</span>
        )}
      </p>
    </div>
  );
}
