/**
 * Apagar um GGUF do disco desta máquina.
 *
 * O nó avisa, mas não impede: uma exclusão que esbarra em algo (fatia no ar,
 * transferência em curso, blob do Ollama) volta 409 com a lista de avisos, e só
 * então perguntamos se o operador quer mesmo. Os dois passos moram aqui porque
 * a tela de modelos inteiros e a de fatias fazem exatamente a mesma coisa, e
 * duas cópias divergiriam no texto do aviso — que é a parte que importa.
 */
import { api } from "./api";

export interface DeleteWarning {
  kind: "serving" | "transfer" | "ollama_store";
  message: string;
}

export type DeleteOutcome =
  | { status: "deleted"; freedBytes: number }
  | { status: "cancelled" };

function warningsOf(error: unknown): DeleteWarning[] {
  const body = (error as { body?: { warnings?: DeleteWarning[] } })?.body;
  return body?.warnings ?? [];
}

/**
 * `confirm` recebe o texto e devolve se o operador topou. Injetado para a tela
 * escolher como perguntar — e para o teste não precisar de um `window`.
 */
export async function deleteModel(
  path: string,
  name: string,
  confirm: (message: string) => boolean,
): Promise<DeleteOutcome> {
  if (!confirm(`Apagar ${name} do disco desta máquina?\n\n${path}`)) {
    return { status: "cancelled" };
  }

  try {
    const r = await api.post<{ freed_bytes: number }>("/api/ui/models/delete", { path });
    return { status: "deleted", freedBytes: r?.freed_bytes ?? 0 };
  } catch (error) {
    const warnings = warningsOf(error);
    if (warnings.length === 0) throw error;

    const lista = warnings.map((w) => `• ${w.message}`).join("\n");
    if (!confirm(`Atenção:\n\n${lista}\n\nApagar mesmo assim?`)) {
      return { status: "cancelled" };
    }
    const r = await api.post<{ freed_bytes: number }>("/api/ui/models/delete", {
      path,
      force: true,
    });
    return { status: "deleted", freedBytes: r?.freed_bytes ?? 0 };
  }
}
