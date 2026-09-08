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
  it("mostra a cabeça escolhida e as alternativas com o critério", () => {
    mount(selected);

    expect(screen.getByText("ksia")).toBeTruthy();
    expect(screen.getByText(/RTT 12 ms · fresco/i)).toBeTruthy();
    expect(screen.getByText("salem")).toBeTruthy();
    expect(screen.getAllByText(/Cobertura.*RTT fresco.*carga/i)).toHaveLength(2);
  });

  it("mostra um estado acionável quando nenhum nó pode iniciar", () => {
    mount({
      ...selected,
      status: "no_candidate",
      selected_node_id: null,
      candidates: [],
    });

    expect(screen.getByText(/Nenhum nó anuncia a cabeça/i)).toBeTruthy();
    expect(screen.getByText(/inferência não pode começar/i)).toBeTruthy();
  });

  it("identifica o fallback quando ainda não há RTT fresco", () => {
    mount({
      ...selected,
      candidates: selected.candidates.map((candidate) => ({
        ...candidate,
        rtt_state: "stale",
        rtt_age_ms: 301_000,
      })),
    });

    expect(screen.getByText("Fallback de RTT")).toBeTruthy();
    expect(screen.getByText(/carga em voo.*id do nó/i)).toBeTruthy();
    expect(screen.getAllByText(/vencido/i).length).toBe(2);
  });
});
