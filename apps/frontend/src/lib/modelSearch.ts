import { resolvePluginRoute } from "./pluginUi";

/**
 * Searching and downloading models is the Hugging Face plugin's screen
 * (`pom-plugin-hugging-face`, `plugin_code` `huggingface`, route `/models`
 * normalized under its code). The node keeps the downloads and the token; only
 * the screen moved.
 */
export const MODEL_SEARCH_ROUTE = "/huggingface/models";

/** The Search Models route while the plugin serves it, or `null` without it. */
export function modelSearchRoute(): string | null {
  return resolvePluginRoute(MODEL_SEARCH_ROUTE) ? MODEL_SEARCH_ROUTE : null;
}
