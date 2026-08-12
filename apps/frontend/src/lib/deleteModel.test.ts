// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { deleteModel } from "./deleteModel";
import { api } from "./api";
import { ApiError } from "./api";

afterEach(() => vi.restoreAllMocks());

describe("deleteModel", () => {
  it("não chama o nó quando o operador desiste na primeira pergunta", async () => {
    const post = vi.spyOn(api, "post");
    const out = await deleteModel("/m/f.gguf", "f.gguf", () => false);

    expect(out.status).toBe("cancelled");
    expect(post).not.toHaveBeenCalled();
  });

  it("apaga direto quando não há aviso nenhum", async () => {
    const post = vi.spyOn(api, "post").mockResolvedValue({ freed_bytes: 42 } as never);
    const out = await deleteModel("/m/f.gguf", "f.gguf", () => true);

    expect(out).toEqual({ status: "deleted", freedBytes: 42 });
    expect(post).toHaveBeenCalledTimes(1);
    expect(post).toHaveBeenCalledWith("/api/ui/models/delete", { path: "/m/f.gguf" });
  });

  /// O 409 traz o motivo. Perguntar de novo com o motivo à vista é a diferença
  /// entre avisar e simplesmente obedecer.
  it("mostra os avisos do 409 e só força depois de confirmar", async () => {
    const post = vi
      .spyOn(api, "post")
      .mockRejectedValueOnce(
        new ApiError(409, "conflito", {
          warnings: [{ kind: "serving", message: "servindo as camadas 0-15" }],
        }),
      )
      .mockResolvedValueOnce({ freed_bytes: 7 } as never);

    const perguntas: string[] = [];
    const out = await deleteModel("/m/f.gguf", "f.gguf", (m) => {
      perguntas.push(m);
      return true;
    });

    expect(out).toEqual({ status: "deleted", freedBytes: 7 });
    expect(perguntas[1]).toContain("servindo as camadas 0-15");
    expect(post).toHaveBeenLastCalledWith("/api/ui/models/delete", {
      path: "/m/f.gguf",
      force: true,
    });
  });

  it("não força quando o operador recusa diante dos avisos", async () => {
    const post = vi.spyOn(api, "post").mockRejectedValueOnce(
      new ApiError(409, "conflito", {
        warnings: [{ kind: "transfer", message: "transferência em curso" }],
      }),
    );

    let primeira = true;
    const out = await deleteModel("/m/f.gguf", "f.gguf", () => {
      const resposta = primeira;
      primeira = false;
      return resposta;
    });

    expect(out.status).toBe("cancelled");
    expect(post).toHaveBeenCalledTimes(1);
  });

  /// Um erro sem avisos é um erro de verdade e tem que subir para o toast, em
  /// vez de virar um "cancelado" silencioso.
  it("propaga um erro que não seja o 409 de avisos", async () => {
    vi.spyOn(api, "post").mockRejectedValue(new ApiError(500, "disco morreu"));

    await expect(deleteModel("/m/f.gguf", "f.gguf", () => true)).rejects.toThrow(
      "disco morreu",
    );
  });
});
