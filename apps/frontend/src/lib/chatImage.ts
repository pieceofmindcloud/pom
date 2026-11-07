// Generate image from the Chat: the request goes to the node's
// `POST /api/ui/chat/image`, which runs a ComfyUI workflow on the machine
// that lists it and answers NDJSON (progress, then the stored images). The
// images stay on this node, by hash; the conversation keeps only the hashes.
import { ADMIN_ACCESS_TOKEN_HEADER, ApiError, ensureChatApiKey, forgetChatApiKey, getAdminAccessToken } from "./api";
import type { ChatImage } from "./types";

export interface ChatImageRequest {
  workflowId: string;
  nodeId: string;
  prompt: string;
  width?: number;
  height?: number;
  /** A base image (data URL) for image to image. */
  image?: string;
  onProgress?: (step: number, of: number) => void;
  signal?: AbortSignal;
}

async function withKey(path: string, init: RequestInit): Promise<Response> {
  let response: Response | undefined;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const headers = new Headers(init.headers);
    headers.set("X-API-Key", await ensureChatApiKey(attempt > 0));
    // `/api/ui/**` also asks for the panel password when the admin is exposed on the network.
    const access = getAdminAccessToken();
    if (access) headers.set(ADMIN_ACCESS_TOKEN_HEADER, access);
    response = await fetch(path, { ...init, headers, credentials: "same-origin" });
    if ((response.status === 401 || response.status === 403) && attempt === 0) {
      forgetChatApiKey();
      continue;
    }
    break;
  }
  return response as Response;
}

/** Runs the workflow and resolves with the images, or rejects with the node's or plugin's reason. */
export async function startChatImage(request: ChatImageRequest): Promise<ChatImage[]> {
  const response = await withKey("/api/ui/chat/image", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      workflow_id: request.workflowId,
      node_id: request.nodeId,
      prompt: request.prompt,
      ...(request.width && request.height ? { width: request.width, height: request.height } : {}),
      ...(request.image ? { image: request.image } : {}),
    }),
    signal: request.signal,
  });
  if (!response.ok || !response.body) {
    const text = await response.text().catch(() => "");
    let message = text || `HTTP ${response.status}`;
    try {
      message = (JSON.parse(text) as { error?: string }).error ?? message;
    } catch {
      // not JSON
    }
    throw new ApiError(response.status, message);
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (value) buffer += decoder.decode(value, { stream: true });
    let end: number;
    while ((end = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, end).trim();
      buffer = buffer.slice(end + 1);
      if (!line) continue;
      const frame = JSON.parse(line) as { p?: number; of?: number; images?: ChatImage[]; error?: string };
      if (frame.error) throw new ApiError(502, frame.error);
      if (frame.images) return frame.images;
      if (frame.p != null && frame.of != null) request.onProgress?.(frame.p, frame.of);
    }
    if (done) break;
  }
  throw new ApiError(502, "the workflow ended without images");
}

/** The image's bytes from this node, as an object URL; `null` when the node no longer has it. */
export async function loadChatImage(hash: string): Promise<string | null> {
  const response = await withKey(`/api/ui/chat/image/${hash}`, { method: "GET" });
  if (response.status === 404) return null;
  if (!response.ok) throw new ApiError(response.status, `HTTP ${response.status}`);
  return URL.createObjectURL(await response.blob());
}
