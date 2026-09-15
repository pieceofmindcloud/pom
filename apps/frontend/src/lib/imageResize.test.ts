// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  estimateImageTokens,
  imageBudgetBytes,
  resizeDimensions,
  resizeImageForVision,
} from "./imageResize";

describe("resizeDimensions", () => {
  it("caps a landscape image at the projector limit", () => {
    expect(resizeDimensions(4000, 2000)).toEqual({ width: 2048, height: 1024 });
  });

  it("preserves a portrait image that is already within the limit", () => {
    expect(resizeDimensions(1000, 2000)).toEqual({ width: 1000, height: 2000 });
  });
});

describe("image context budget", () => {
  it("leaves a safety margin in the same four-bytes-per-token estimate used by chat", () => {
    expect(imageBudgetBytes(230144)).toBe(828516);
    expect(estimateImageTokens(828516)).toBeLessThanOrEqual(Math.floor(230144 * 0.9));
  });
});

describe("resizeImageForVision", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  function dataUrlForBytes(bytes: number): string {
    return `data:image/jpeg;base64,${btoa("a".repeat(bytes))}`;
  }

  function installCanvas() {
    const canvas = {
      width: 0,
      height: 0,
      getContext: () => ({
        fillStyle: "",
        fillRect: vi.fn(),
        drawImage: vi.fn(),
      }),
      toDataURL: vi.fn(() => dataUrlForBytes(Math.ceil((canvas.width * canvas.height) / 20))),
    } as unknown as HTMLCanvasElement;
    vi.spyOn(document, "createElement").mockImplementation((tagName: string) => {
      if (tagName === "canvas") return canvas;
      return document.createElementNS("http://www.w3.org/1999/xhtml", tagName);
    });
    vi.stubGlobal("createImageBitmap", vi.fn(async () => ({ width: 4000, height: 2000, close: vi.fn() })));
    return canvas;
  }

  it("reduces a large image until the encoded bytes fit the token budget", async () => {
    const canvas = installCanvas();
    const result = await resizeImageForVision(
      new File(["original"], "paisagem.png", { type: "image/png" }),
      imageBudgetBytes(230144),
    );

    expect(result.resized).toBe(true);
    expect(result.size).toBeLessThanOrEqual(828516);
    expect(estimateImageTokens(result.size)).toBeLessThanOrEqual(Math.floor(230144 * 0.9));
    expect(Math.max(result.width, result.height)).toBeLessThanOrEqual(2048);
    expect(Math.max(result.width, result.height)).toBeGreaterThanOrEqual(512);
    expect(canvas.toDataURL).toHaveBeenCalled();
  });

  it("reports when even the minimum image cannot fit a tiny context", async () => {
    installCanvas();

    await expect(
      resizeImageForVision(
        new File(["original"], "paisagem.png", { type: "image/png" }),
        100,
      ),
    ).rejects.toThrow("contexto disponível");
  });
});
