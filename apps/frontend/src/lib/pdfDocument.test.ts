import { describe, expect, it, vi } from "vitest";

import {
  MAX_OCR_PAGES,
  MAX_PDF_BYTES,
  MAX_PDF_PAGES,
  PdfReadError,
  isPdfFile,
  pageItemsToText,
  readPdfAsText,
  type PdfEngine,
  type PdfPageHandle,
} from "./pdfDocument";

const TEXT = "This page has plenty of real text on it.";

function pdf(size = 4): File {
  const file = new File([new Uint8Array(size)], "report.pdf", { type: "application/pdf" });
  Object.defineProperty(file, "arrayBuffer", { value: async () => new ArrayBuffer(size) });
  return file;
}

function engine(pages: string[], ocr: (page: number) => string = () => "text read by ocr") {
  const recognize = vi.fn(async (page: PdfPageHandle) => ocr(page.number));
  const close = vi.fn(async () => undefined);
  const fake: PdfEngine = {
    open: vi.fn(async () => ({
      pageCount: pages.length,
      page: async (number: number) => ({ number, text: async () => pages[number - 1] }),
      close,
    })),
    openOcr: vi.fn(async () => ({ recognize, close })),
  };
  return { fake, recognize, close };
}

describe("isPdfFile", () => {
  it("knows a PDF by its type or its name", () => {
    expect(isPdfFile(new File([""], "a.bin", { type: "application/pdf" }))).toBe(true);
    expect(isPdfFile(new File([""], "A.PDF"))).toBe(true);
    expect(isPdfFile(new File([""], "a.txt", { type: "text/plain" }))).toBe(false);
  });
});

describe("pageItemsToText", () => {
  const item = (str: string, x: number, y: number, width = 10, hasEOL = false) => ({
    str,
    width,
    height: 10,
    hasEOL,
    transform: [10, 0, 0, 10, x, y],
  });

  it("keeps the lines of the page", () => {
    expect(pageItemsToText([item("first", 0, 100), item("second", 0, 80)])).toBe("first\nsecond");
  });

  it("separates pieces with a gap and glues the ones without", () => {
    expect(pageItemsToText([item("Hel", 0, 100, 10), item("lo", 10, 100, 10), item("world", 30, 100)])).toBe(
      "Hello world",
    );
  });

  it("honors the line end the PDF declares", () => {
    expect(pageItemsToText([item("end", 0, 100, 10, true), item("next", 10, 100)])).toBe("end\nnext");
  });
});

describe("readPdfAsText", () => {
  it("reads a text PDF without starting the OCR", async () => {
    const { fake, close } = engine([TEXT, TEXT]);
    const result = await readPdfAsText(pdf(), { engine: fake });
    expect(result.text).toBe(`--- Page 1 ---\n${TEXT}\n\n--- Page 2 ---\n${TEXT}`);
    expect(result).toMatchObject({ pages: 2, ocrPages: 0 });
    expect(fake.openOcr).not.toHaveBeenCalled();
    expect(close).toHaveBeenCalled();
  });

  it("runs the OCR only on the pages without text and reports its progress", async () => {
    const { fake, recognize } = engine([TEXT, "12", TEXT, ""]);
    const progress = vi.fn();
    const result = await readPdfAsText(pdf(), { engine: fake, onProgress: progress });
    expect(recognize.mock.calls.map(([page]) => page.number)).toEqual([2, 4]);
    expect(result.ocrPages).toBe(2);
    expect(result.text).toContain("--- Page 2 ---\ntext read by ocr");
    expect(progress).toHaveBeenCalledWith({ phase: "ocr", page: 2, pages: 2 });
  });

  it("refuses a file above the limit before reading it", async () => {
    const { fake } = engine([TEXT]);
    const big = new File([""], "big.pdf");
    Object.defineProperty(big, "size", { value: MAX_PDF_BYTES + 1 });
    await expect(readPdfAsText(big, { engine: fake })).rejects.toMatchObject({ code: "too_large" });
    expect(fake.open).not.toHaveBeenCalled();
  });

  it("refuses too many pages", async () => {
    const { fake, close } = engine(new Array(MAX_PDF_PAGES + 1).fill(TEXT));
    await expect(readPdfAsText(pdf(), { engine: fake })).rejects.toMatchObject({ code: "too_many_pages" });
    expect(close).toHaveBeenCalled();
  });

  it("refuses too many scanned pages instead of sending part of them", async () => {
    const { fake, recognize } = engine(new Array(MAX_OCR_PAGES + 1).fill(""));
    await expect(readPdfAsText(pdf(), { engine: fake })).rejects.toMatchObject({ code: "too_many_scanned_pages" });
    expect(recognize).not.toHaveBeenCalled();
  });

  it("refuses a PDF where nothing could be read", async () => {
    const { fake } = engine(["", ""], () => "   ");
    await expect(readPdfAsText(pdf(), { engine: fake })).rejects.toMatchObject({ code: "empty" });
  });

  it("stops and closes everything when cancelled", async () => {
    const controller = new AbortController();
    const { fake, close } = engine(["", ""], () => {
      controller.abort();
      return "text";
    });
    await expect(readPdfAsText(pdf(), { engine: fake, signal: controller.signal })).rejects.toMatchObject({
      name: "AbortError",
    });
    expect(close).toHaveBeenCalledTimes(2);
  });

  it("names a protected PDF and an invalid one", async () => {
    const failing = (name: string): PdfEngine => ({
      open: async () => {
        throw Object.assign(new Error("x"), { name });
      },
      openOcr: async () => {
        throw new Error("not reached");
      },
    });
    await expect(readPdfAsText(pdf(), { engine: failing("PasswordException") })).rejects.toMatchObject({
      code: "password",
    });
    await expect(readPdfAsText(pdf(), { engine: failing("InvalidPDFException") })).rejects.toBeInstanceOf(PdfReadError);
  });
});
