// The tools the Chat offers its LLM in Automatic mode: generate_image (the
// chosen workflow, from a prompt), one `wf_<slug>` per published ComfyUI
// workflow with parameters, the reassembly of a call streamed in fragments,
// and the size an aspect stands for. The node builds the same definitions for
// the API (`crates/node/src/chat_tools.rs`).
import type { ChatImageWorkflow, ChatWorkflowParam } from "./types";

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

/** `wf_` and the published workflow's slug, `[a-z0-9_]` only; null for a built-in one. */
export function workflowToolName(workflow: ChatImageWorkflow): string | null {
  if (!workflow.id.startsWith("pub:")) return null;
  return `wf_${workflow.id.slice(4).replace(/[^a-z0-9_]/g, "_")}`.slice(0, 64);
}

interface WorkflowTool {
  type: "function";
  function: {
    name: string;
    description: string;
    parameters: { type: "object"; properties: Record<string, Record<string, unknown>>; required: string[] };
  };
}

function propertyOf(param: ChatWorkflowParam): Record<string, unknown> {
  const description = param.description ? `${param.label}: ${param.description}` : param.label;
  const schema: Record<string, unknown> =
    param.type === "number" ? { type: "number" }
    : param.type === "integer" ? { type: "integer" }
    : param.type === "boolean" ? { type: "boolean" }
    : param.type === "choice" ? { enum: param.options ?? [] }
    : { type: "string" };
  schema.description = description;
  if (param.min != null) schema.minimum = param.min;
  if (param.max != null) schema.maximum = param.max;
  if (param.default != null) schema.default = param.default;
  return schema;
}

/** The tools of the published workflows that list parameters (an image one is the attachment). */
export function workflowTools(workflows: ChatImageWorkflow[]): WorkflowTool[] {
  const names = new Set<string>([GENERATE_IMAGE]);
  const tools: WorkflowTool[] = [];
  for (const workflow of workflows) {
    const name = workflowToolName(workflow);
    if (!name || !workflow.params?.length || names.has(name)) continue;
    names.add(name);
    const properties: Record<string, Record<string, unknown>> = {};
    const required: string[] = [];
    for (const param of workflow.params) {
      if (param.type === "image") {
        properties.use_attached_image = { type: "boolean", description: `${param.label} (the image the user attached)` };
        continue;
      }
      if (properties[param.key]) continue;
      properties[param.key] = propertyOf(param);
      if (param.default == null && param.type !== "boolean") required.push(param.key);
    }
    tools.push({
      type: "function",
      function: {
        name,
        description: workflow.description || `Run the ComfyUI workflow "${workflow.name}".`,
        parameters: { type: "object", properties, required },
      },
    });
  }
  return tools;
}

export interface WorkflowCall {
  workflow: ChatImageWorkflow;
  /** The arguments the workflow's parameters name (an image is added by the caller). */
  params: Record<string, unknown>;
  useAttachedImage: boolean;
}

/** The first complete call of a published workflow's tool, or null. */
export function workflowCallOf(calls: ToolCall[], workflows: ChatImageWorkflow[]): WorkflowCall | null {
  for (const call of calls) {
    const workflow = workflows.find((w) => w.params?.length && workflowToolName(w) === call.name);
    if (!workflow) continue;
    let args: Record<string, unknown>;
    try {
      const parsed = JSON.parse(call.arguments || "{}") as unknown;
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) continue;
      args = parsed as Record<string, unknown>;
    } catch {
      continue;
    }
    const params: Record<string, unknown> = {};
    for (const param of workflow.params ?? []) {
      if (param.type !== "image" && args[param.key] != null) params[param.key] = args[param.key];
    }
    return { workflow, params, useAttachedImage: args.use_attached_image === true };
  }
  return null;
}
