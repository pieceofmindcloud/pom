/**
 * Copia um texto para a área de transferência.
 *
 * `navigator.clipboard` só existe em contexto seguro (HTTPS ou localhost). Num
 * POM acessado por `http://<ip>:8080` ele é `undefined`, e o clique em "copiar"
 * não fazia nada. Por isso há um caminho de reserva com um `<textarea>` fora da
 * tela e `document.execCommand("copy")`, que funciona em HTTP. Resolve `true`
 * quando alguma das duas formas copiou; `false` deixa o texto selecionável para
 * a pessoa copiar à mão.
 */
export async function copyText(text: string): Promise<boolean> {
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Permissão negada ou documento sem foco: tenta o caminho de reserva.
    }
  }
  return copyWithSelection(text);
}

function copyWithSelection(text: string): boolean {
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  // Fora da tela, sem rolagem e sem zoom no iOS; o foco precisa existir para o select().
  Object.assign(area.style, { position: "fixed", top: "0", left: "0", opacity: "0", fontSize: "16px" });
  document.body.appendChild(area);
  const previous = document.activeElement as HTMLElement | null;
  try {
    area.select();
    area.setSelectionRange(0, text.length);
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    document.body.removeChild(area);
    previous?.focus?.();
  }
}
