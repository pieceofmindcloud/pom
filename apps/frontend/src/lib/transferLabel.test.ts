import { describe, expect, it } from "vitest";
import { transferHash, transferTitle } from "./transferLabel";

/**
 * A notificação mostrava o sha256 do arquivo — `df48d094c3d1023c0dab9e…` — e o
 * operador não tinha como saber de que modelo era nem que camadas trazia. O
 * `.pom` viaja com a oferta e chega antes do primeiro byte de peso; é dele que
 * sai o nome que se lê.
 */
describe("transferTitle", () => {
  const hashName =
    "df48d094c3d1023c0dab9e9d3b1bf982226db9490d1f42150aab016836248158.gguf";

  it("apresenta a fatia pelo modelo e pelas camadas, não pelo hash", () => {
    expect(
      transferTitle({
        name: hashName,
        model_label: "qwen3:8b",
        start_layer: 0,
        end_layer: 15,
      }),
    ).toBe("qwen3:8b · L0–15");
  });

  it("uma camada só não vira faixa", () => {
    expect(
      transferTitle({ name: hashName, model_label: "qwen3:8b", start_layer: 7, end_layer: 7 }),
    ).toBe("qwen3:8b · L7");
  });

  /**
   * Sem manifesto não se inventa faixa nem modelo: um GGUF inteiro não
   * pertence a `.pom` nenhum e continua se chamando pelo próprio nome.
   */
  it("um modelo inteiro continua se chamando pelo nome do arquivo", () => {
    expect(transferTitle({ name: "llama-3.1-8b-Q4_K_M.gguf" })).toBe(
      "llama-3.1-8b-Q4_K_M.gguf",
    );
  });

  it("sem rótulo do modelo, a faixa ainda ajuda mais que só o hash", () => {
    expect(transferTitle({ name: hashName, start_layer: 16, end_layer: 31 })).toBe(
      `L16–31 · ${hashName}`,
    );
  });

  it("um nó de versão anterior, que não manda nada disso, não quebra a tela", () => {
    expect(transferTitle({ name: hashName })).toBe(hashName);
    expect(
      transferTitle({ name: hashName, model_label: null, start_layer: null, end_layer: null }),
    ).toBe(hashName);
  });
});

/**
 * O hash não some: ele é o que identifica a parte entre máquinas e o que
 * nomeia o arquivo em disco. Só deixa de ser o título.
 */
describe("transferHash", () => {
  it("encurta o sha256 para caber ao lado do nome", () => {
    expect(transferHash("df48d094c3d1023c0dab9e9d3b1bf982226db9490d1f42150aab016836248158.gguf"))
      .toBe("df48d094c3d1");
  });

  it("não repete um nome que já é legível", () => {
    expect(transferHash("llama-3.1-8b-Q4_K_M.gguf")).toBeNull();
    expect(transferHash("fatia_0_15.gguf")).toBeNull();
  });
});
