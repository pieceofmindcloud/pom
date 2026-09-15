/** The existing attachment estimate treats four bytes as one input token. */
export const IMAGE_BYTES_PER_TOKEN = 4;
/** Keep room for the prompt and the projector's own framing. */
export const IMAGE_CONTEXT_SAFETY_MARGIN = 0.9;
/** Maximum side sent to a vision projector. */
export const MAX_VISION_IMAGE_SIDE = 2048;
/** Do not make a normal image smaller than this side while fitting it. */
export const MIN_VISION_IMAGE_SIDE = 512;

export interface ResizedVisionImage {
  data: string;
  size: number;
  width: number;
  height: number;
  resized: boolean;
}

export class ImageResizeLimitError extends Error {
  constructor() {
    super("a imagem ainda excede o contexto disponível mesmo em 512 px");
    this.name = "ImageResizeLimitError";
  }
}

export function imageBudgetBytes(contextTokens: number, usedBytes = 0): number {
  if (!Number.isFinite(contextTokens) || contextTokens <= 0) return 0;
  const budget = Math.floor(contextTokens * IMAGE_CONTEXT_SAFETY_MARGIN) * IMAGE_BYTES_PER_TOKEN;
  return Math.max(0, budget - Math.max(0, Math.ceil(usedBytes)));
}

export function estimateImageTokens(bytes: number): number {
  return Math.ceil(Math.max(0, bytes) / IMAGE_BYTES_PER_TOKEN);
}

export function resizeDimensions(
  width: number,
  height: number,
  maxSide = MAX_VISION_IMAGE_SIDE,
): { width: number; height: number } {
  if (width <= 0 || height <= 0) throw new Error("imagem sem dimensões válidas");
  const scale = Math.min(1, maxSide / Math.max(width, height));
  return {
    width: Math.max(1, Math.round(width * scale)),
    height: Math.max(1, Math.round(height * scale)),
  };
}

function imageElement(file: File): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const image = new Image();
    image.onload = () => {
      URL.revokeObjectURL(url);
      resolve(image);
    };
    image.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error("não foi possível ler a imagem"));
    };
    image.src = url;
  });
}

function dataUrlBytes(data: string): number {
  const comma = data.indexOf(",");
  if (comma >= 0 && data.slice(0, comma).includes(";base64")) {
    try {
      return atob(data.slice(comma + 1)).length;
    } catch {
      // Fall through to the encoded URL length for a malformed test double.
    }
  }
  return new TextEncoder().encode(data).byteLength;
}

function fileDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error("não foi possível ler a imagem"));
    reader.readAsDataURL(file);
  });
}

function candidateMaxSides(initialMaxSide: number, minimumMaxSide: number): number[] {
  const sides: number[] = [];
  for (let side = initialMaxSide; side > minimumMaxSide; side = Math.floor(side * 0.82)) {
    sides.push(side);
  }
  sides.push(minimumMaxSide);
  return Array.from(new Set(sides));
}

/** Decode and re-encode an image until its estimated bytes fit the context. */
export async function resizeImageForVision(
  file: File,
  maxBytes?: number,
): Promise<ResizedVisionImage> {
  const bitmap = typeof createImageBitmap === "function"
    ? await createImageBitmap(file)
    : null;
  const source: CanvasImageSource = bitmap ?? await imageElement(file);
  const width = bitmap?.width ?? (source as HTMLImageElement).naturalWidth;
  const height = bitmap?.height ?? (source as HTMLImageElement).naturalHeight;
  const originalMaxSide = Math.max(width, height);
  const dimensions = resizeDimensions(width, height);
  const minimumMaxSide = Math.min(MIN_VISION_IMAGE_SIDE, originalMaxSide);

  const originalFits = maxBytes == null || file.size <= maxBytes;
  if (originalFits && dimensions.width === width && dimensions.height === height) {
    bitmap?.close();
    return {
      data: await fileDataUrl(file),
      size: file.size,
      width,
      height,
      resized: false,
    };
  }

  if (maxBytes != null && maxBytes <= 0) {
    bitmap?.close();
    throw new ImageResizeLimitError();
  }

  const canvas = document.createElement("canvas");
  const context = canvas.getContext("2d");
  if (!context) {
    bitmap?.close();
    throw new Error("o navegador não oferece um canvas 2D");
  }

  try {
    const qualities = [0.86, 0.74, 0.62, 0.5, 0.4, 0.3, 0.22, 0.15];
    const maxSides = maxBytes == null
      ? [Math.max(dimensions.width, dimensions.height)]
      : candidateMaxSides(Math.max(dimensions.width, dimensions.height), minimumMaxSide);
    for (const maxSide of maxSides) {
      const candidate = resizeDimensions(width, height, maxSide);
      canvas.width = candidate.width;
      canvas.height = candidate.height;
      context.fillStyle = "#fff";
      context.fillRect(0, 0, candidate.width, candidate.height);
      context.drawImage(source, 0, 0, candidate.width, candidate.height);
      for (const quality of qualities) {
        const data = canvas.toDataURL("image/jpeg", quality);
        const size = dataUrlBytes(data);
        if (maxBytes == null || size <= maxBytes) {
          return {
            data,
            size,
            width: candidate.width,
            height: candidate.height,
            resized: true,
          };
        }
      }
    }
  } finally {
    bitmap?.close();
  }

  throw new ImageResizeLimitError();
}
