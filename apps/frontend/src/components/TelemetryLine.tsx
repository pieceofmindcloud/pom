/**
 * The open-source telemetry of one answer: a single compact line under the
 * message with generation speed, time to first token, end-to-end time, prompt
 * processing and the pipeline that served it.
 *
 * The detailed, visual telemetry (pipeline diagram, per-phase charts, raw
 * trace) is not part of the POM: an active plugin may mount it through the
 * `chat.telemetry` slot, which receives the same data and renders its own
 * trigger at the end of this line (`children`).
 */
import type { ReactNode } from "react";
import clsx from "clsx";
import { Gauge, Route, Timer, Zap } from "lucide-react";
import { ms, num } from "../lib/format";
import type { GenerationTimings, RequestTrace } from "../lib/chatStream";
import { useI18n } from "../i18n/provider";

export interface TelemetryLineProps {
  trace?: RequestTrace;
  timings?: GenerationTimings;
  /** Human-readable pipeline (`pipeline:abc→def`) that served the answer. */
  source?: string;
  className?: string;
  /** Trailing controls, such as a plugin's advanced-telemetry trigger. */
  children?: ReactNode;
}

function Item({ icon, title, children }: { icon: ReactNode; title: string; children: ReactNode }) {
  return (
    <span className="inline-flex min-w-0 items-center gap-1" title={title}>
      <span className="shrink-0 text-faint/80">{icon}</span>
      <span className="min-w-0 truncate">{children}</span>
    </span>
  );
}

/** The basic numbers of a generation, from the timings or the trace alone. */
export function basicTelemetry(trace?: RequestTrace, timings?: GenerationTimings) {
  const generationRate = timings?.generation_tokens_per_sec ?? null;
  const generatedTokens = timings?.generation_tokens ?? trace?.output_tokens ?? null;
  const promptTokens = timings?.prefill_tokens ?? trace?.input_tokens ?? null;
  const prefillMs = timings?.prefill_ms ?? trace?.prefill_ms ?? null;
  const prefillRate = timings?.prefill_tokens_per_sec
    ?? (promptTokens != null && prefillMs ? (promptTokens * 1000) / prefillMs : null);
  const ttftMs = timings?.client_ttft_ms ?? timings?.ttft_ms ?? trace?.client_ttft_ms ?? trace?.server_ttft_ms ?? null;
  const totalMs = timings?.total_ms ?? null;
  return { generationRate, generatedTokens, promptTokens, prefillRate, ttftMs, totalMs };
}

export function TelemetryLine({ trace, timings, source, className, children }: TelemetryLineProps) {
  const { t } = useI18n();
  const basic = basicTelemetry(trace, timings);
  const iconClass = "h-3 w-3";
  return (
    <div
      className={clsx(
        "flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[0.68rem] text-faint",
        className,
      )}
      data-testid="telemetry-line"
    >
      {(basic.generationRate != null || basic.generatedTokens != null) && (
        <Item icon={<Zap className={iconClass} />} title={t("chat.telemetryLine.generationHint")}>
          {basic.generationRate != null && (
            <span className="text-muted">{t("chat.telemetryLine.rate", { value: num(basic.generationRate, 1) })}</span>
          )}
          {basic.generatedTokens != null && (
            <span>{basic.generationRate != null ? " · " : ""}{t("chat.telemetryLine.tokens", { count: basic.generatedTokens })}</span>
          )}
        </Item>
      )}
      {basic.ttftMs != null && (
        <Item icon={<Timer className={iconClass} />} title={t("chat.telemetryLine.ttftHint")}>
          {t("chat.telemetryLine.ttft", { value: ms(basic.ttftMs) })}
        </Item>
      )}
      {basic.totalMs != null && (
        <Item icon={<Gauge className={iconClass} />} title={t("chat.telemetryLine.totalHint")}>
          {t("chat.telemetryLine.total", { value: ms(basic.totalMs) })}
        </Item>
      )}
      {basic.promptTokens != null && (
        <Item icon={<span className="text-[0.62rem] leading-none">⇢</span>} title={t("chat.telemetryLine.prefillHint")}>
          {t("chat.telemetryLine.prefill", {
            count: basic.promptTokens,
            rate: basic.prefillRate != null ? num(basic.prefillRate, 1) : "—",
          })}
        </Item>
      )}
      {source && (
        <Item icon={<Route className={iconClass} />} title={t("chat.telemetryLine.sourceHint", { source })}>
          <span className="inline-block max-w-[18rem] truncate align-bottom">{source}</span>
        </Item>
      )}
      {children}
    </div>
  );
}
