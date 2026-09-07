/**
 * Que rota cada perfil enxerga.
 *
 * Uma tabela só, e o menu e o roteador leem a mesma: esconder o item do menu
 * sem barrar a rota deixa a tela a um endereço digitado de distância, e barrar
 * a rota sem esconder o item deixa o menu oferecendo o que não abre. Os dois
 * defeitos vêm de haver duas listas; aqui há uma.
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

/**
 * Prefixo de rota -> perfis que entram.
 *
 * A chave casa por prefixo de segmento, então uma futura filha de `/logs`
 * continua protegida, e `/implantar` **não** cobre `/implantarx`.
 */
const RESTRICTED: Readonly<Record<string, readonly Profile[]>> = {
  "/implantar": ["admin"],
  "/implantacoes": ["admin"],
  "/logs": ["admin"],
  "/estatisticas": ["admin"],
};

/**
 * Vista local do infer. A rota de rede permanece visível para que o nó possa
 * se auto-promover, mas o backend retorna somente dados desta máquina.
 */
const INFER_ROUTES = ["/chat", "/documentacao", "/config/keys/api", "/config/rede"] as const;

/** `/implantarx` não está sob `/implantar`. */
function isUnder(path: string, prefix: string): boolean {
  return path === prefix || path.startsWith(prefix + "/");
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

/** Se este perfil pode abrir esta rota. */
export function canAccessRoute(path: string, profile: Profile): boolean {
  if (profile === "infer") {
    return INFER_ROUTES.some((prefix) => isUnder(path, prefix));
  }
  const allowed = routeProfiles(path);
  return allowed === null || allowed.includes(profile);
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
