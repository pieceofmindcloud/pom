/**
 * How a transfer presents itself on screen.
 *
 * A slice's name on disk is its **sha256**, and a hash does not identify
 * anything readable: the notification showed `df48d094c3d1023c0dab9e…` and the
 * operator had no way of knowing which model it was or which layers it carried.
 * What knows both is the `.pom`, which travels with the offer and is written before the
 * first weight byte - the backend reads it and sends `model_label` and the range
 * along with the job.
 *
 * The hash does not disappear: it is still what identifies the part between
 * machines and what names the file on disk. It just stops being the title.
 */
import { layerRange } from "./format";

/** What is enough to name a slice - job or offer. */
export interface SliceNamed {
  name: string;
  model_label?: string | null;
  start_layer?: number | null;
  end_layer?: number | null;
}

/**
 * The title: model and layer range, with the file name as a last
 * resort. A whole model has no range and keeps being called by its name.
 */
export function transferTitle(item: SliceNamed): string {
  const range =
    item.start_layer != null && item.end_layer != null
      ? layerRange(item.start_layer, item.end_layer)
      : null;
  if (item.model_label && range) return `${item.model_label} · ${range}`;
  if (item.model_label) return item.model_label;
  if (range) return `${range} · ${item.name}`;
  return item.name;
}

/**
 * The short hash, when the file name is a sha256 - the secondary identification
 * that sits alongside, discreet. `null` when the name is already readable,
 * because then repeating it adds nothing.
 */
export function transferHash(name: string): string | null {
  const stem = name.replace(/\.gguf$/i, "");
  return /^[0-9a-f]{64}$/i.test(stem) ? stem.slice(0, 12) : null;
}
