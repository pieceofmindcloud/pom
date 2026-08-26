// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "./i18n/provider";
import { ProfileProvider, type Profile } from "./lib/profile";

/**
 * Esconder o item do menu não é bloquear: o endereço continua digitável, e um
 * favorito antigo entra direto. Este teste é o que prova que a rota fechada
 * **não renderiza a tela**, e que a aberta continua renderizando.
 *
 * Por isso ele monta o `App` de verdade, com as telas trocadas por marcadores:
 * o que está sob prova é o guarda, não o conteúdo de cada tela.
 */
vi.mock("./lib/queries", () => ({
  useIsSetupMode: () => false,
  // Nó completo o bastante para a barra superior e o rodapé do menu
  // renderizarem: um `data: null` derruba o `NodeChip` no `node.data!` e o
  // teste passa a medir o `ErrorBoundary` em vez do guarda.
  useNode: () => ({
    data: {
      hostname: "salem",
      short_id: "a1b2",
      status: { state: "idle" },
      config: null,
      tls_required: false,
      build_commit: "abc1234",
    },
    isLoading: false,
    isError: false,
  }),
  useCluster: () => ({ data: null }),
  useInvalidate: () => vi.fn(),
}));

vi.mock("./components/Notifications", () => ({ Notifications: () => null }));
vi.mock("./components/Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));

// `vi.mock` sobe para o topo do arquivo, antes de qualquer `const`, então cada
// marcador é escrito à mão em vez de sair de um auxiliar compartilhado.
vi.mock("./routes/DeployModel", () => ({ DeployModel: () => <div>tela-implantar</div> }));
vi.mock("./routes/Deployments", () => ({ Deployments: () => <div>tela-implantacoes</div> }));
vi.mock("./routes/LogsByToken", () => ({ LogsByToken: () => <div>tela-logs</div> }));
vi.mock("./routes/LogsLive", () => ({ LogsLive: () => <div>tela-logs-ao-vivo</div> }));
vi.mock("./routes/WholeModels", () => ({ WholeModels: () => <div>tela-modelos</div> }));
vi.mock("./routes/Dashboard", () => ({ Dashboard: () => <div>tela-painel</div> }));
vi.mock("./routes/Chat", () => ({ Chat: () => <div>tela-chat</div> }));

import App from "./App";

function open(path: string, profile: Profile) {
  return render(
    // Mesmas flags que o `main.tsx` liga: sem elas o teste roda num modo de
    // roteador que a aplicação não usa, e o console enche de aviso de futuro.
    <MemoryRouter
      initialEntries={[path]}
      future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
    >
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile={profile}>
          <App />
        </ProfileProvider>
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("guarda de perfil por rota", () => {
  afterEach(cleanup);

  const fechadas = [
    ["/implantar", "tela-implantar"],
    ["/implantacoes", "tela-implantacoes"],
    ["/logs", "tela-logs"],
    ["/logs/ao-vivo", "tela-logs-ao-vivo"],
  ] as const;

  for (const [path, marker] of fechadas) {
    it(`abre ${path} para o administrador`, async () => {
      open(path, "admin");
      expect(await screen.findByText(marker)).toBeTruthy();
      expect(screen.queryByText("Tela restrita a outro perfil")).toBeNull();
    });

    it(`barra ${path} para o perfil do nó`, async () => {
      open(path, "node");
      expect(screen.queryByText(marker)).toBeNull();
      expect(await screen.findByText("Tela restrita a outro perfil")).toBeTruthy();
    });
  }

  it("não mexe nas rotas que ninguém restringiu", async () => {
    open("/modelos", "node");
    expect(await screen.findByText("tela-modelos")).toBeTruthy();
  });

  /**
   * O chat renderiza fora do `<Routes>`, então um guarda por rota o deixaria
   * de fora. Hoje ele é livre, e este teste é o que avisa se a exceção do chat
   * voltar a escapar do guarda quando alguém o restringir.
   */
  it("passa o chat pelo mesmo guarda", async () => {
    open("/chat", "node");
    expect(await screen.findByText("tela-chat")).toBeTruthy();
  });
});
