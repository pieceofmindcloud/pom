import { describe, expect, it } from "vitest";
import reference from "./locales.reference.json";
import { formatMessage, messageKeys, type MessageValues } from "./locales";

type ReferenceDump = {
  valuesByCase: Record<string, MessageValues>;
  records: Array<{
    locale: "en" | "pt-BR";
    key: string;
    case: string;
    result: string;
  }>;
};

const dump = reference as ReferenceDump;

// The dump is the frozen pre-ICU baseline. Keep intentional product-copy
// revisions here rather than rewriting the generated 1.8 MB single-line file.
const copyUpdates: Record<string, string> = {
  "en:chat.noModel": "One piece is missing: the network is not serving any model",
  "pt-BR:chat.noModel": "Falta uma peça: a rede não está servindo nenhum modelo",
  "en:chat.noModelDetail": "The machines are here, but there is no mind to put together yet. Deploy a model and every piece starts talking.",
  "pt-BR:chat.noModelDetail": "As máquinas estão aqui, mas ainda não há uma mente para montar. Implante um modelo e todas as peças começam a conversar.",
  "en:publicNetworks.noMatchesDetail": "Try another search or filter.",
  "pt-BR:publicNetworks.noMatchesDetail": "Tente outra busca ou outro filtro.",
  "en:plugins.state.active": "License active",
  "pt-BR:plugins.state.active": "Licença ativa",
  "en:deploy.knob.hidden_state_dtypeHint":
    "Controls bytes sent between inference peers. f16 remains the default; f32 preserves the in-memory activation values but doubles payload size. f8 and groupwise int4 are opt-in lossy formats and require every selected peer to advertise support.",
  "pt-BR:deploy.knob.hidden_state_dtypeHint":
    "Controla os bytes enviados entre peers de inferência. f16 continua como padrão; f32 preserva os valores das ativações em memória, mas dobra o tamanho do payload. f8 e int4 por grupo são formatos com perda opcionais e exigem suporte anunciado por todos os peers selecionados.",
  "en:paths.validation":
    "Saving a folder that does not exist yet creates it (only from an absolute path). A path that points to a file is refused.",
  "pt-BR:paths.validation":
    "Salvar uma pasta que ainda não existe cria a pasta (só a partir de um caminho absoluto). Um caminho que aponta para um arquivo é recusado.",
  "pt-BR:deploymentLogs.consoleTitle": "Console do cluster",
  "pt-BR:deploymentLogs.subtitle":
    "O console do processo do cluster. As linhas aparecem na ordem em que chegaram a esta réplica local.",
  "en:network.logsHowItWorks":
    "The admin only tells the network to collect: every node sends a copy to each admin over a separate libp2p protocol, and the Logs plugin on each admin keeps arrival order and deployment id in its own database. Only the admins need the plugin.",
  "pt-BR:network.logsHowItWorks":
    "O admin só avisa a rede para coletar: cada nó envia uma cópia para cada admin por um protocolo libp2p separado, e o plugin de Logs de cada admin grava a ordem de chegada e o deploy_id no próprio banco. Só os admins precisam do plugin.",
};

// Keys whose screens left the POM. The detailed execution telemetry now lives
// in the plugin that claims the `chat.telemetry` slot, with its own catalog.
const MOVED_TO_PLUGINS = new Set([
  "chat.generationSpeed",
  "chat.prefillSpeed",
  "chat.promptTokensUnit",
  "chat.rateUnavailable",
  "chat.tokensPerSecondUnit",
  "chat.totalTime",
  "chat.ttft",
  "chat.ttftClient",
  "chat.ttftPipeline",
]);
// Keys of the repetition-cycle detector, removed from the POM with it.
const removed = (key: string) =>
  key.startsWith("deploy.cycleDetection") || key.includes(".knob.cycle_detection_");
const moved = (key: string) =>
  key.startsWith("chat.telemetry.") || MOVED_TO_PLUGINS.has(key) || removed(key);

describe("ICU locale equivalence", () => {
  it("keeps the CSV key order stable", () => {
    expect(messageKeys("en")).toEqual([...messageKeys("en")].sort());
  });

  it("keeps every key from the reference dump in both locales", () => {
    for (const locale of ["en", "pt-BR"] as const) {
      const expected = new Set(dump.records
        .filter((record) => record.locale === locale && !moved(record.key))
        .map((record) => record.key));
      expect([...expected].filter((key) => !messageKeys(locale).includes(key))).toEqual([]);
    }
  });

  it("renders reference cases identically, with explicit product-copy revisions", () => {
    for (const record of dump.records) {
      if (moved(record.key)) continue;
      const expected = copyUpdates[`${record.locale}:${record.key}`] ?? record.result;
      expect(
        formatMessage(record.locale, record.key, dump.valuesByCase[record.case]),
        `${record.locale} ${record.key} ${record.case}`,
      ).toBe(expected);
    }
  });
});
