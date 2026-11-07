// The generate_image tool the Chat offers its LLM in Automatic mode: the
// schema, the reassembly of a call streamed in fragments, and the size an
// aspect stands for.

export const GENERATE_IMAGE = "generate_image";

export const generateImageTool = {
  type: "function",
  function: {
    name: GENERATE_IMAGE,
    description:
      "Create an image for the user. Call it when the user asks for a picture, drawing, photo, illustration, or an edit of an attached image. Write `prompt` as a detailed visual description in English that a diffusion model can follow (subject, setting, style, lighting, composition), using the conversation for context.",
    parameters: {
      type: "object",
      properties: {
        prompt: { type: "string", description: "Detailed visual description, in English." },
        negative_prompt: { type: "string", description: "What the image must not show." },
        aspect: { type: "string", enum: ["square", "landscape", "portrait", "wide"] },
        use_attached_image: { type: "boolean", description: "Start from the image the user attached." },
      },
      required: ["prompt"],
    },
  },
} as const;

export interface ToolCall {
  name: string;
  arguments: string;
}

/** Adds one streamed `tool_calls` delta (OpenAI shape) to the calls read so far. */
export function accumulateToolCalls(calls: ToolCall[], delta: unknown): ToolCall[] {
  if (!Array.isArray(delta)) return calls;
  const next = calls.map((call) => ({ ...call }));
  for (const part of delta as Array<{ index?: number; function?: { name?: string; arguments?: string } }>) {
    const index = typeof part.index === "number" ? part.index : next.length > 0 ? next.length - 1 : 0;
    while (next.length <= index) next.push({ name: "", arguments: "" });
    if (part.function?.name) next[index].name += part.function.name;
    if (part.function?.arguments) next[index].arguments += part.function.arguments;
  }
  return next;
}

export interface ImageToolArgs {
  prompt: string;
  negative?: string;
  aspect?: "square" | "landscape" | "portrait" | "wide";
  useAttachedImage: boolean;
}

/** The first complete generate_image call, or null. */
export function imageCallOf(calls: ToolCall[]): ImageToolArgs | null {
  for (const call of calls) {
    if (call.name !== GENERATE_IMAGE) continue;
    try {
      const args = JSON.parse(call.arguments || "{}") as Record<string, unknown>;
      const prompt = typeof args.prompt === "string" ? args.prompt.trim() : "";
      if (!prompt) continue;
      const aspect = ["square", "landscape", "portrait", "wide"].includes(String(args.aspect))
        ? (args.aspect as ImageToolArgs["aspect"])
        : undefined;
      return {
        prompt,
        negative: typeof args.negative_prompt === "string" && args.negative_prompt.trim() ? args.negative_prompt.trim() : undefined,
        aspect,
        useAttachedImage: args.use_attached_image === true,
      };
    } catch {
      continue;
    }
  }
  return null;
}

/** The size an aspect asks for; the menu's own size, when set, wins. */
export function sizeFor(aspect: ImageToolArgs["aspect"], menuSize: string): { width?: number; height?: number } {
  const chosen = menuSize || { square: "1024x1024", landscape: "1216x832", portrait: "832x1216", wide: "1344x768" }[aspect ?? "square"] || "";
  const [width, height] = chosen.split("x").map(Number);
  return width && height ? { width, height } : {};
}
