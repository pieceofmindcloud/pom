/**
 * Que rota cada perfil enxerga.
 *
 * Uma tabela só, e o menu e o roteador leem a mesma. Rotas de produto Enterprise
 * são a exceção de descoberta: continuam abertas para que o menu possa
 * apresentar o recurso mesmo antes da ativação. O bloqueio de dados acontece
 * no backend e o App mostra o estado de compra quando o entitlement falta.
 *
 * **Escopo:** isto é apresentação, não autorização - ver o cabeçalho de
 * `profile.tsx`. Bloquear aqui evita que a interface ofereça um trabalho que
 * não é daquele perfil. A autorização da superfície administrativa é feita no
 * servidor com o token de acesso; a connection key é uma credencial separada
 * para os nós e a pipeline.
 *
 * Rota que não aparece em `RESTRICTED` é livre para todo perfil, exceto o
 * `infer`, que tem uma vista reduzida explícita abaixo. O padrão é
 * permissivo de propósito: a restrição é a exceção que alguém escreveu
 * deliberadamente, e uma tela nova não deixa de abrir porque foi esquecida
 * numa lista.
 */
import type { Profile } from "./profile";
import { pluginRouteProfiles } from "./pluginUi";

/**
 * Prefixo de rota -> perfis que entram.
 *
 * A chave casa por prefixo de segmento, então uma futura filha de `/logs`
 * continua protegida, e `/implantar` **não** cobre `/implantarx`.
 */
const RESTRICTED: Readonly<Record<string, readonly Profile[]>> = {
  "/implantar": ["admin"],
  "/implantacoes": ["admin"],
  "/config/administracao": ["admin", "node"],
  "/config/rede/participantes": ["admin", "node"],
  // The cluster's connection key is the operator's credential: only the
  // admin sees it.
  "/config/keys/conexao": ["admin"],
};

/**
 * Vista local do infer. A rota de rede permanece visível para que o nó possa
 * pedir entrada no processamento, mas o backend retorna somente dados desta
 * máquina.
 */
const INFER_ROUTES = [
  "/redes-publicas",
  "/chat",
  "/network-chat",
  "/documentacao",
  "/config/keys/api",
  "/config/rede",
  "/config/minha-maquina",
  "/config/compartilhamento",
  "/config/preferencias",
  // Ancorar a confiança precede a autoridade e não é privilégio de perfil
  // nenhum: um `infer` também recebe a âncora do admin. A trava dessa tela é
  // do servidor (loopback, nó por ancorar, impressão digital conferida).
  "/config/confianca",
] as const;

const INFER_DENIED_ROUTES = [
  "/config/rede/participantes",
  "/config/preferencias/admin",
  // Request logs, deployment logs and statistics are the network's
  // operation, not an inference node's: hidden even where a plugin claims
  // the route and before activation.
  "/logs",
  "/estatisticas",
  "/implantacoes",
] as const;

/** `/implantarx` não está sob `/implantar`. */
function isUnder(path: string, prefix: string): boolean {
  const pathname = path.split(/[?#]/, 1)[0];
  return pathname === prefix || pathname.startsWith(prefix + "/");
}

export function isEnterprisePath(path: string): boolean {
  return isUnder(path, "/logs") || isUnder(path, "/estatisticas");
}

/** Os perfis que entram nesta rota, ou `null` quando ela é livre. */
export function routeProfiles(path: string): readonly Profile[] | null {
  // A chave mais específica vence para qualquer futura exceção de rota.
  let best: { prefix: string; profiles: readonly Profile[] } | null = null;
  for (const [prefix, profiles] of Object.entries(RESTRICTED)) {
    if (!isUnder(path, prefix)) continue;
    if (!best || prefix.length > best.prefix.length) best = { prefix, profiles };
  }
  return best?.profiles ?? null;
}

/** Se este perfil pode abrir esta rota, incluindo superfícies Enterprise. */
export function canAccessRoute(path: string, profile: Profile): boolean {
  // Rota fornecida por plugin: quem decide são os perfis do manifesto, que
  // valem também para `infer` (a lista reduzida abaixo é só do POM).
  if (profile === "infer" && INFER_DENIED_ROUTES.some((prefix) => isUnder(path, prefix))) return false;
  const pluginProfiles = pluginRouteProfiles(path);
  if (pluginProfiles) return pluginProfiles.includes(profile);
  if (isEnterprisePath(path)) return true;
  if (profile === "infer") return INFER_ROUTES.some((prefix) => isUnder(path, prefix));
  const allowed = routeProfiles(path);
  return allowed === null || allowed.includes(profile);
}

/** Enterprise surfaces are intentionally discoverable before activation. */
export function canAccessEnterprise(path: string, _active: boolean): boolean {
  return isEnterprisePath(path);
}

/**
 * A tela inicial de cada perfil.
 *
 * O `/` é o Painel, que o `infer` não abre. Sem isto, um nó de inferência
 * abre a interface direto na tela de recusa - um beco, ainda por cima
 * mandando trocar de perfil, coisa que deixou de existir. Fica aqui, ao lado
 * da tabela de rotas, para não virar uma segunda lista.
 */
export function homeRoute(profile: Profile): string {
  return canAccessRoute("/", profile) ? "/" : "/chat";
}

/**
 * Se este perfil enxerga o detalhe máquina a máquina do Painel.
 *
 * O Painel tem duas vistas do mesmo cluster: o **Fluxo**, que é o desenho da
 * rede, e os **Detalhes**, que são os cards com o estado interno de cada
 * máquina - runtime, memória, faixa de camadas, e os botões de liberar fatia,
 * remover do pipeline e tirar da rede. O mesmo card abre pelo Fluxo, ao clicar
 * numa máquina. Isso é operação do cluster, não do próprio nó: fora do `admin`
 * resta uma vista só, e onde há uma vista só não há o que escolher - o seletor
 * também não aparece.
 *
 * Fica ao lado da tabela de rotas porque é a mesma pergunta ("o que este perfil
 * enxerga"), e porque o Painel e o canvas precisam responder igual: esconder o
 * seletor e continuar abrindo o card pelo clique seria a mesma tela em dois
 * estados diferentes.
 */
export function canSeePeerDetails(profile: Profile): boolean {
  return profile === "admin";
}
