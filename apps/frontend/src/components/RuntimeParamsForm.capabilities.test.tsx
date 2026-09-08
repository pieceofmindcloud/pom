// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { AlertTooltip } from "./ui";
import { CapabilitySwitch, RuntimeParamsForm } from "./RuntimeParamsForm";
import type {
  DeploymentCapabilities,
  RuntimeCapability,
  RuntimeParams,
} from "../lib/types";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const THINKING_ALERT =
  "Capacidade não confirmada. Ligar é por conta e risco do usuário. Fonte: GGUF tokenizer.chat_template + tokenizer.ggml.tokens. o template ou o vocabulário do modelo não pode ser confirmado";
const TOPOLOGY_ALERT =
  "A topologia limita esta opção; o controle continua acionável. Fonte: topologia do plano. controle visível, mas estes peers precisam manter embeddings: 5b03580398da127c0ddf24b9fdc02afe745ed22019db479f3976d9f930db694b: peer intermediário L0-26 precisa exportar o residual para o próximo peer; 6dece790cce94281a5f56ab8c2ee0ce6f7417ba914774c842f5c0d46f8d9b62c: peer intermediário L27-43 precisa exportar o residual para o próximo peer";
const MTP_ALERT =
  "Capacidade não confirmada. Ligar é por conta e risco do usuário. Fonte: arquivo GGUF selecionado. metadata ou tensores nextn do GGUF selecionado não puderam ser confirmados O runtime foi consultado, mas não substitui a prova do modelo; ligá-lo é por conta e risco do usuário.";

const FORM_PARAMS: RuntimeParams = {
  inference_mode: "auto",
  n_gpu_layers: 99,
  n_ctx: 8192,
  offload_kqv: true,
  fit_memory: true,
  fit_memory_margin_mb: 256,
  n_batch: 512,
  max_new_tokens: 512,
  max_sessions: 1,
  prefill_chunk_tokens: 512,
  temperature: 0.7,
  top_p: 0.95,
  top_k: 40,
  repeat_penalty: 1.1,
  repeat_last_n: 64,
  no_thinking: true,
  no_embeddings: false,
  speculative_decoding_enabled: false,
  speculative_k: 4,
  speculative_lookup_window: 3,
  mtp_enabled: false,
  mtp_draft_tokens: 3,
  mtp_min_probability: 0.1,
};

function hiddenStateCapabilities(hiddenStateF8: RuntimeCapability): DeploymentCapabilities {
  return {
    fit_memory: hiddenStateF8,
    no_thinking: hiddenStateF8,
    no_embeddings: hiddenStateF8,
    speculative_decoding: hiddenStateF8,
    mtp: hiddenStateF8,
    hidden_state_f8: hiddenStateF8,
  };
}

function mount(
  state: "supported" | "unsupported" | "unknown",
  alwaysVisible = false,
  warning?: string,
  source = "metadados GGUF reais",
  reason = state === "unknown" ? "template ausente" : "resultado apurado",
) {
  const onChange = vi.fn();
  render(
    <LocaleProvider initialLocale="pt-BR">
      <CapabilitySwitch
        capability={{
          state,
          source,
          reason,
          warning,
        }}
        alwaysVisible={alwaysVisible}
        checked={false}
        onChange={onChange}
        label="Opção condicional"
      />
    </LocaleProvider>,
  );
  return onChange;
}

function alertTrigger() {
  return screen.getByRole("button", { name: "Ver aviso para Opção condicional" });
}

function openTooltip() {
  fireEvent.mouseEnter(alertTrigger());
  return screen.getByRole("tooltip");
}

describe("CapabilitySwitch", () => {
  it("mantém compatibilidade com planos que ainda não trazem capacidades", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <CapabilitySwitch
          checked={false}
          onChange={vi.fn()}
          label="Opção sem survey"
        />
      </LocaleProvider>,
    );

    expect(screen.getByRole("checkbox", { name: "Opção sem survey" })).toBeTruthy();
  });

  it("oculta uma opção comprovadamente não suportada", () => {
    mount("unsupported");
    expect(screen.queryByRole("checkbox", { name: "Opção condicional" })).toBeNull();
  });

  it("mantém desconhecida visível, acionável e com aviso de risco", () => {
    mount("unknown");
    const checkbox = screen.getByRole("checkbox", { name: "Opção condicional" });
    expect(checkbox.getAttribute("aria-checked")).toBe("false");
    expect((checkbox as HTMLButtonElement).disabled).toBe(false);
    fireEvent.focus(alertTrigger());
    expect(screen.getByRole("tooltip").textContent).toContain("template ausente");
    expect(screen.getByRole("tooltip").textContent).toContain("por conta e risco");
    fireEvent.click(checkbox);
  });

  it("permite a opção suportada e preserva no_embeddings visível", () => {
    const onChange = mount("supported", true);
    fireEvent.click(screen.getByRole("checkbox", { name: "Opção condicional" }));
    expect(onChange).toHaveBeenCalledWith(true);
  });

  it("mantém acionável uma limitação do runtime e mostra o aviso", () => {
    const onChange = mount("supported", false, "peer sem protocolo de lote");
    const checkbox = screen.getByRole("checkbox", { name: "Opção condicional" });
    expect((checkbox as HTMLButtonElement).disabled).toBe(false);
    expect(openTooltip().textContent).toContain("peer sem protocolo");
    fireEvent.click(checkbox);
    expect(onChange).toHaveBeenCalledWith(true);
  });

  it("explica a limitação topológica sem atribuí-la ao runtime", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <CapabilitySwitch
          capability={{
            state: "supported",
            source: "topologia do plano",
            reason: "controle visível, peer intermediário precisa exportar residual",
          }}
          checked={false}
        onChange={vi.fn()}
        label="Desativar embeddings"
      />
    </LocaleProvider>,
    );
    fireEvent.focus(screen.getByRole("button", { name: "Ver aviso para Desativar embeddings" }));
    expect(screen.getByRole("tooltip").textContent).toContain("topologia limita");
    expect(screen.getByRole("tooltip").textContent).not.toContain("runtime");
  });

  it("preserva inteira no tooltip a origem e a mensagem de thinking", () => {
    mount(
      "unknown",
      false,
      undefined,
      "GGUF tokenizer.chat_template + tokenizer.ggml.tokens",
      "o template ou o vocabulário do modelo não pode ser confirmado",
    );

    expect(openTooltip().textContent).toBe(THINKING_ALERT);
  });

  it("quebra identificadores longos sem perder o diagnóstico topológico", () => {
    mount(
      "supported",
      true,
      undefined,
      "topologia do plano",
      "controle visível, mas estes peers precisam manter embeddings: 5b03580398da127c0ddf24b9fdc02afe745ed22019db479f3976d9f930db694b: peer intermediário L0-26 precisa exportar o residual para o próximo peer; 6dece790cce94281a5f56ab8c2ee0ce6f7417ba914774c842f5c0d46f8d9b62c: peer intermediário L27-43 precisa exportar o residual para o próximo peer",
    );

    const tooltip = openTooltip();
    expect(tooltip.textContent).toBe(TOPOLOGY_ALERT);
    expect(tooltip.className).toMatch(/break-words/);
    expect(tooltip.className).toMatch(/overflow-wrap/);
  });

  it("preserva a mensagem longa de MTP completa, inclusive o aviso do runtime", () => {
    mount(
      "unknown",
      false,
      "O runtime foi consultado, mas não substitui a prova do modelo; ligá-lo é por conta e risco do usuário.",
      "arquivo GGUF selecionado",
      "metadata ou tensores nextn do GGUF selecionado não puderam ser confirmados",
    );

    expect(openTooltip().textContent).toBe(MTP_ALERT);
  });

  it("abre por foco e clique, sem depender somente de hover", () => {
    mount("unknown");
    const trigger = alertTrigger();

    fireEvent.focus(trigger);
    expect(screen.getByRole("tooltip")).toBeTruthy();
    fireEvent.click(trigger);
    fireEvent.blur(trigger);
    expect(screen.getByRole("tooltip")).toBeTruthy();
    fireEvent.click(trigger);
    expect(screen.queryByRole("tooltip")).toBeNull();
  });

  it("mantém o tooltip dentro da viewport quando o ícone está abaixo da dobra", () => {
    const originalRect = HTMLElement.prototype.getBoundingClientRect;
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function () {
      if (this.getAttribute("aria-label") === "Ver aviso para Alerta fora da dobra") {
        return {
          x: 476,
          y: 900,
          top: 900,
          right: 500,
          bottom: 924,
          left: 476,
          width: 24,
          height: 24,
          toJSON: () => ({}),
        } as DOMRect;
      }
      return originalRect.call(this);
    });

    render(<AlertTooltip label="Alerta fora da dobra" message="Mensagem completa" />);
    fireEvent.click(screen.getByRole("button", { name: "Ver aviso para Alerta fora da dobra" }));

    expect(screen.getByRole("tooltip").style.top).toBe("600px");
  });
});

describe("RuntimeParamsForm", () => {
  it("mantém f16 como padrão e grava f8 quando o cluster confirma a capacidade", () => {
    const onChange = vi.fn();
    render(
      <LocaleProvider initialLocale="pt-BR">
        <RuntimeParamsForm
          value={FORM_PARAMS}
          onChange={onChange}
          capabilities={hiddenStateCapabilities({
              state: "supported",
              source: "capacidades runtime dos peers selecionados",
              reason: "todos os peers selecionados anunciam estado oculto f8",
            })}
        />
      </LocaleProvider>,
    );

    const select = screen.getByRole("combobox");
    expect((select as HTMLSelectElement).value).toBe("f16");
    fireEvent.change(select, { target: { value: "f8" } });
    expect(onChange).toHaveBeenCalledWith(
      expect.objectContaining({ hidden_state_dtype: "f8" }),
    );
  });

  it("não oferece f8 quando a capacidade do cluster foi recusada", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <RuntimeParamsForm
          value={FORM_PARAMS}
          onChange={vi.fn()}
          capabilities={hiddenStateCapabilities({
              state: "unsupported",
              source: "capacidades runtime dos peers selecionados",
              reason: "pelo menos um peer selecionado não anuncia estado oculto f8",
            })}
        />
      </LocaleProvider>,
    );

    expect((screen.getByRole("option", { name: /^f8/ }) as HTMLOptionElement).disabled).toBe(true);
  });
});
