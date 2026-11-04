/**
 * Turns a PDF into text on the page, before it goes to the model.
 *
 * A page with text is read as it is. A page that is a picture of text (a scan)
 * goes through OCR, only that page, because OCR takes seconds per page and text
 * already in the file takes nothing. The two libraries are heavy, so they load
 * here on the first PDF, from this node and never from a CDN
 * (`scripts/build-document-reader.mjs`).
 */
import documentReader from "../brand/document-reader.json";

export const MAX_PDF_BYTES = 100 * 1024 * 1024;
export const MAX_PDF_PAGES = 1000;
export const MAX_OCR_PAGES = 100;

/** Below this many letters and digits a page is treated as a picture. */
const MIN_PAGE_CHARACTERS = 16;
/** About 200 DPI, what OCR needs from a scan, without an enormous canvas. */
const OCR_SCALE = 200 / 72;
const OCR_MAX_SIDE = 3000;
const OCR_LANGUAGES = ["por", "eng"];
const OCR_LSTM_ONLY = 1;

export type PdfReadErrorCode =
  | "too_large"
  | "too_many_pages"
  | "too_many_scanned_pages"
  | "password"
  | "invalid"
  | "empty";

export class PdfReadError extends Error {
  constructor(
    readonly code: PdfReadErrorCode,
    /** The number found, when the code is about a limit. */
    readonly count?: number,
  ) {
    super(code);
    this.name = "PdfReadError";
  }
}

export interface PdfProgress {
  phase: "text" | "ocr";
  page: number;
  pages: number;
}

export interface PdfPageHandle {
  number: number;
  text: () => Promise<string>;
  render?: () => Promise<HTMLCanvasElement>;
}

interface OpenPdf {
  pageCount: number;
  page: (number: number) => Promise<PdfPageHandle>;
  close: () => Promise<void>;
}

interface OpenOcr {
  recognize: (page: PdfPageHandle) => Promise<string>;
  close: () => Promise<void>;
}

export interface PdfEngine {
  open: (data: ArrayBuffer) => Promise<OpenPdf>;
  openOcr: () => Promise<OpenOcr>;
}

export interface PdfText {
  text: string;
  pages: number;
  ocrPages: number;
}

export function isPdfFile(file: File): boolean {
  return file.type === "application/pdf" || /\.pdf$/i.test(file.name);
}

interface PageItem {
  str: string;
  width: number;
  height: number;
  hasEOL: boolean;
  transform: number[];
}

/** Rebuilds the lines of a page, because a PDF stores pieces at positions and not text. */
export function pageItemsToText(items: PageItem[]): string {
  let text = "";
  let previous: PageItem | null = null;
  let lineEnded = false;
  for (const item of items) {
    if (previous) {
      const height = item.height || previous.height;
      const newLine = lineEnded || Math.abs(item.transform[5] - previous.transform[5]) > height / 2;
      const gap = item.transform[4] - (previous.transform[4] + previous.width);
      if (newLine) text = `${text.trimEnd()}\n`;
      else if (gap > height * 0.2 && !text.endsWith(" ")) text += " ";
    }
    text += item.str;
    lineEnded = item.hasEOL;
    previous = item;
  }
  return text.trim();
}

function characterCount(text: string): number {
  return text.match(/[\p{L}\p{N}]/gu)?.length ?? 0;
}

function throwIfCancelled(signal?: AbortSignal) {
  if (signal?.aborted) throw new DOMException("cancelled", "AbortError");
}

function assetUrl(path: string): string {
  return new URL(`${import.meta.env.BASE_URL}${documentReader.base}${path}`, window.location.href).href;
}

const browserEngine: PdfEngine = {
  async open(data) {
    // The legacy build: the macOS app runs in the system WebKit, as old as macOS 12.
    const pdfjs = await import("pdfjs-dist/legacy/build/pdf.mjs");
    pdfjs.GlobalWorkerOptions.workerSrc = assetUrl("pdf/pdf.worker.min.mjs");
    const task = pdfjs.getDocument({
      data,
      cMapUrl: assetUrl("pdf/cmaps/"),
      cMapPacked: true,
      standardFontDataUrl: assetUrl("pdf/standard_fonts/"),
      wasmUrl: assetUrl("pdf/wasm/"),
    });
    const document = await task.promise;
    return {
      pageCount: document.numPages,
      page: async (number) => {
        const page = await document.getPage(number);
        return {
          number,
          text: async () => {
            const content = await page.getTextContent();
            page.cleanup();
            return pageItemsToText(content.items.flatMap((item) => ("str" in item ? [item] : [])));
          },
          render: async () => {
            const { width, height } = page.getViewport({ scale: 1 });
            const scale = Math.min(OCR_SCALE, OCR_MAX_SIDE / Math.max(width, height));
            const viewport = page.getViewport({ scale });
            const canvas = window.document.createElement("canvas");
            canvas.width = Math.ceil(viewport.width);
            canvas.height = Math.ceil(viewport.height);
            await page.render({ canvas, viewport }).promise;
            page.cleanup();
            return canvas;
          },
        };
      },
      close: () => task.destroy(),
    };
  },
  async openOcr() {
    const { createWorker } = await import("tesseract.js");
    const worker = await createWorker(OCR_LANGUAGES, OCR_LSTM_ONLY, {
      workerPath: assetUrl("ocr/worker.min.js"),
      corePath: assetUrl("ocr/core"),
      langPath: assetUrl("ocr/lang"),
      cacheMethod: "none",
      workerBlobURL: false,
    });
    return {
      recognize: async (page) => {
        const canvas = await page.render!();
        const { data } = await worker.recognize(canvas);
        // A canvas holds its memory until it has no size.
        canvas.width = 0;
        canvas.height = 0;
        return data.text.trim();
      },
      close: async () => {
        await worker.terminate();
      },
    };
  },
};

function asReadError(error: unknown): unknown {
  const name = (error as { name?: string } | null)?.name;
  if (name === "PasswordException") return new PdfReadError("password");
  if (name === "InvalidPDFException") return new PdfReadError("invalid");
  return error;
}

export async function readPdfAsText(
  file: File,
  options: { signal?: AbortSignal; onProgress?: (progress: PdfProgress) => void; engine?: PdfEngine } = {},
): Promise<PdfText> {
  const { signal, onProgress, engine = browserEngine } = options;
  if (file.size > MAX_PDF_BYTES) throw new PdfReadError("too_large", file.size);
  throwIfCancelled(signal);

  let document: OpenPdf;
  try {
    document = await engine.open(await file.arrayBuffer());
  } catch (error) {
    throw asReadError(error);
  }
  try {
    const pages = document.pageCount;
    if (pages > MAX_PDF_PAGES) throw new PdfReadError("too_many_pages", pages);

    const texts: string[] = [];
    const scanned: number[] = [];
    for (let number = 1; number <= pages; number += 1) {
      throwIfCancelled(signal);
      onProgress?.({ phase: "text", page: number, pages });
      const text = await (await document.page(number)).text();
      texts.push(text);
      if (characterCount(text) < MIN_PAGE_CHARACTERS) scanned.push(number);
    }
    // Refused whole: a model answering from part of a contract, with nobody told, is worse.
    if (scanned.length > MAX_OCR_PAGES) throw new PdfReadError("too_many_scanned_pages", scanned.length);

    if (scanned.length > 0) {
      const ocr = await engine.openOcr();
      try {
        for (const [index, number] of scanned.entries()) {
          throwIfCancelled(signal);
          onProgress?.({ phase: "ocr", page: index + 1, pages: scanned.length });
          const text = await ocr.recognize(await document.page(number));
          if (text.length > texts[number - 1].length) texts[number - 1] = text;
          throwIfCancelled(signal);
        }
      } finally {
        await ocr.close();
      }
    }

    const text = texts
      .map((content, index) => (content.trim() ? `--- Page ${index + 1} ---\n${content}` : ""))
      .filter(Boolean)
      .join("\n\n");
    if (characterCount(text.replace(/^--- Page \d+ ---$/gm, "")) === 0) throw new PdfReadError("empty");
    return { text, pages, ocrPages: scanned.length };
  } finally {
    await document.close();
  }
}
