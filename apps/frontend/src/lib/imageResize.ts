/** Maximum side sent to a vision projector. Projectors commonly operate on
 * 2K-scale inputs; capping here keeps requests bounded without changing ratio. */
export const MAX_VISION_IMAGE_SIDE = 2048;

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

/** Decode and re-encode every image so the original bytes never reach the model. */
export async function resizeImageForVision(file: File): Promise<string> {
  const bitmap = typeof createImageBitmap === "function"
    ? await createImageBitmap(file)
    : null;
  const source: CanvasImageSource = bitmap ?? await imageElement(file);
  const width = bitmap?.width ?? (source as HTMLImageElement).naturalWidth;
  const height = bitmap?.height ?? (source as HTMLImageElement).naturalHeight;
  const dimensions = resizeDimensions(width, height);
  const canvas = document.createElement("canvas");
  canvas.width = dimensions.width;
  canvas.height = dimensions.height;
  const context = canvas.getContext("2d");
  if (!context) {
    bitmap?.close();
    throw new Error("o navegador não oferece um canvas 2D");
  }
  context.fillStyle = "#fff";
  context.fillRect(0, 0, dimensions.width, dimensions.height);
  context.drawImage(source, 0, 0, dimensions.width, dimensions.height);
  bitmap?.close();
  return canvas.toDataURL("image/jpeg", 0.86);
}
