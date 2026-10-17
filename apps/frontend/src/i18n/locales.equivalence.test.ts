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
  // The p2p transport moved from libp2p to iroh: the copy names neither.
  "en:network.addrHint":
    "Leave empty to auto-detect (the public address the relays observed, else this machine's local network address).",
  "pt-BR:network.addrHint":
    "Deixe vazio para detectar automaticamente (um endereço público observado pelos relés, senão o endereço de rede local desta máquina).",
  "en:network.addrPrecedence":
    "Pinned address wins if set; otherwise a public address observed by the relays; otherwise this machine's local network address.",
  "pt-BR:network.addrPrecedence":
    "O endereço fixado vence quando definido; senão, um endereço público observado pelos relés; senão, o endereço de rede local desta máquina.",
  "en:network.firewallUacDetail":
    "Approve the Windows prompt once to allow discovery, QUIC, p2p endpoint and relay traffic.",
  "pt-BR:network.firewallUacDetail":
    "Aprove o pedido do Windows uma vez para liberar descoberta, QUIC, endpoint p2p e relé.",
  "en:network.logsHowItWorks":
    "The admin only tells the network to collect: every node sends a copy to each admin over a separate p2p protocol, and the Logs plugin on each admin keeps arrival order and deployment id in its own database. Only the admins need the plugin.",
  "pt-BR:network.logsHowItWorks":
    "O admin só avisa a rede para coletar: cada nó envia uma cópia para cada admin por um protocolo p2p separado, e o plugin de Logs de cada admin grava a ordem de chegada e o deploy_id no próprio banco. Só os admins precisam do plugin.",
  "en:network.networkChatHowItWorks":
    "Messages and files travel through the p2p network and are validated by the main node.",
  "pt-BR:network.networkChatHowItWorks":
    "Mensagens e arquivos viajam pela rede p2p e são validados pelo nó principal.",
  "en:network.tlsHowItWorks":
    "TLS between nodes matters most when inference crosses the internet. If everything runs locally over 127.0.0.1, communication between nodes mainly uses the p2p endpoints (QUIC), which are encrypted by nature - TLS is optional in that case and may even reduce inference speed.",
  "pt-BR:network.tlsHowItWorks":
    "O TLS entre nós vale sobretudo quando a inferência atravessa a internet. Se tudo corre localmente por 127.0.0.1, a comunicação entre nós passa principalmente por p2p, que já é segura por natureza - o TLS torna-se opcional nesse caso e pode inclusive reduzir a velocidade da inferência.",
  "en:network.traversalCircuits":
    "Relay addresses",
  "pt-BR:network.traversalCircuits":
    "Endereços de relé",
  "en:network.traversalReservations":
    "Relays connected",
  "pt-BR:network.traversalReservations":
    "Relés conectados",
  "en:networkChat.hint":
    "Enter sends. Files are sent through the p2p network.",
  "pt-BR:networkChat.hint":
    "Enter envia. Arquivos são enviados pela rede p2p.",
  "en:preferences.accessSubtitle":
    "Choose where the administration interface listens. Inference pipeline routes remain available over IPv4 and IPv6 as a fallback for the p2p tunnel.",
  "pt-BR:preferences.accessSubtitle":
    "Escolha de onde a interface de administração aceita conexões. As rotas da pipeline de inferência continuam disponíveis por IPv4 e IPv6 como reserva para o túnel p2p.",
  "en:setupWizard.tlsDetail":
    "Optional extra encryption for control links. The p2p endpoints already encrypt peer connections (QUIC).",
  "pt-BR:setupWizard.tlsDetail":
    "Criptografia extra opcional para os links de controle. Os endpoints p2p já criptografam as conexões entre nós (QUIC).",
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
// Keys of the repetition-cycle detector, removed from the POM with it, and
// of the one-machine limit a whole model no longer has.
const removed = (key: string) =>
  key.startsWith("deploy.cycleDetection") ||
  key.includes(".knob.cycle_detection_") ||
  key.startsWith("deploy.wholeSingleMachine");
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
