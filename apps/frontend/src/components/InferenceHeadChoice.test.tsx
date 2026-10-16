// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { InferenceHeadsResponse } from "../lib/types";
import { InferenceHeadChoice } from "./InferenceHeadChoice";

afterEach(cleanup);

const selected: InferenceHeadsResponse = {
  model: "qwen35",
  status: "selected",
  selected_node_id: "fast",
  selection_policy: "coverage_then_weighted_rtt_load",
  rtt_valid_for_secs: 300,
  candidates: [
    {
      node_id: "fast",
      short_id: "fast",
      hostname: "ksia",
      start_layer: 0,
      end_layer: 31,
      total_layers: 32,
      load: 1,
      runtime_capacity: { n_ctx: 8192, max_new_tokens: 256 },
      rtt_ms: 12,
      rtt_age_ms: 2000,
      rtt_state: "fresh",
      selected: true,
    },
    {
      node_id: "slow",
      short_id: "slow",
      hostname: "salem",
      start_layer: 0,
      end_layer: 31,
      total_layers: 32,
      load: 0,
      runtime_capacity: { n_ctx: 8192, max_new_tokens: 256 },
      rtt_ms: 80,
      rtt_age_ms: 2000,
      rtt_state: "fresh",
      selected: false,
    },
  ],
};

function mount(response: InferenceHeadsResponse) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <InferenceHeadChoice model="qwen35" response={response} />
    </LocaleProvider>,
  );
}

describe("InferenceHeadChoice", () => {
  it("shows no choice when there is only one candidate", () => {
    mount({
      ...selected,
      candidates: [selected.candidates[0]],
    });

    expect(screen.queryByRole("combobox", { name: "Cabeça de inferência" })).toBeNull();
    expect(screen.queryByText("Entrada da inferência")).toBeNull();
    expect(screen.queryByText("ksia")).toBeNull();
  });

  it("shows a combo with the full diagnostic when there are several candidates", () => {
    mount(selected);

    const combo = screen.getByRole("combobox", { name: "Cabeça de inferência" });
    expect((combo as HTMLSelectElement).value).toBe("fast");
    expect(screen.getByRole("option", {
      name: /ksia.*fast.*RTT 12 ms.*fresco.*1 em voo.*L0-31 de L32.*contexto 8\.192.*perna 256 tokens.*Entrada selecionada/i,
    })).toBeTruthy();
    expect(screen.getByRole("option", {
      name: /salem.*slow.*RTT 80 ms.*fresco.*0 em voo.*L0-31 de L32.*contexto 8\.192.*perna 256 tokens/i,
    })).toBeTruthy();
    expect(screen.getByRole("option", {
      name: /Cobertura primeiro.*RTT fresco.*carga em voo.*RTT válido por 5min 0s/i,
    })).toBeTruthy();
  });

  it("shows an actionable state when no node can start", () => {
    mount({
      ...selected,
      status: "no_candidate",
      selected_node_id: null,
      candidates: [],
    });

    expect(screen.getByText(/Nenhum nó anuncia a cabeça/i)).toBeTruthy();
    expect(screen.getByText(/inferência não pode começar/i)).toBeTruthy();
  });

  it("says when the candidate load could not be read", () => {
    mount({
      ...selected,
      candidates: selected.candidates.map((candidate) =>
        candidate.node_id === "slow" ? { ...candidate, load: null } : candidate,
      ),
    });

    expect(screen.getByRole("option", { name: /salem.*carga em voo indisponível/i })).toBeTruthy();
    expect(screen.queryByRole("option", { name: /0 em voo/i })).toBeNull();
  });

  it("identifies the fallback when there is no fresh RTT yet", () => {
    mount({
      ...selected,
      candidates: selected.candidates.map((candidate) => ({
        ...candidate,
        rtt_state: "stale",
        rtt_age_ms: 301_000,
      })),
    });

    expect(screen.getByRole("combobox", { name: "Cabeça de inferência" })).toBeTruthy();
    expect(screen.getByRole("option", {
      name: /Fallback de RTT.*carga em voo.*id do nó/i,
    })).toBeTruthy();
    expect(screen.getAllByRole("option", { name: /vencido/i })).toHaveLength(2);
  });
});
