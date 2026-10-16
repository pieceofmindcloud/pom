/**
 * Copies a text to the clipboard.
 *
 * `navigator.clipboard` only exists in a secure context (HTTPS or localhost). On a
 * POM reached over `http://<ip>:8080` it is `undefined`, and clicking "copy"
 * did nothing. That is why there is a fallback path with an off-screen `<textarea>` and
 * `document.execCommand("copy")`, which works over HTTP. It resolves `true`
 * when either of the two ways copied; `false` leaves the text selectable so the
 * person can copy it by hand.
 */
export async function copyText(text: string): Promise<boolean> {
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Permission denied or a document without focus: try the fallback path.
    }
  }
  return copyWithSelection(text);
}

function copyWithSelection(text: string): boolean {
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  // Off screen, without scrolling and without zoom on iOS; the focus has to exist for select().
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
