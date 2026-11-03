import clsx from "clsx";
import { Info } from "lucide-react";
import { useState } from "react";
import { useI18n } from "../i18n/provider";
import { bytes } from "../lib/format";
import type {
  PeerCatalog,
  SidecarFile,
  SidecarKind,
  SidecarMode,
  SidecarOption,
  SidecarSlot,
} from "../lib/types";
import { Modal } from "./Confirm";
import { Button, Select } from "./ui";

/** What the picker holds: a mode, or "link" to leave the model's saved link in charge. */
export type SidecarPick = { mode: SidecarMode | "link"; file?: SidecarFile | null };

const fileKey = (file: SidecarFile) => `file:${file.file_name}:${file.size_bytes}`;

function keyOf(pick: SidecarPick): string {
  if (pick.mode === "file" && pick.file) return fileKey(pick.file);
  return pick.mode;
}

/**
 * Compatible sidecars of one kind across every machine of the network, merged
 * by name + size (the identity that crosses machines) with every holder and
 * the path on each. Mirrors the backend's `Sidecar::compatible_with`: a
 * projector matches the model's width; an MTP draft also its architecture
 * (a DSpark drafter, `dflash`, is DeepSeek-V4.1's).
 */
export function sidecarOptions(
  peers: PeerCatalog[] | undefined,
  kind: SidecarKind,
  model: { architecture?: string | null; embedding_length?: number | null } | null | undefined,
): SidecarOption[] {
  const width = model?.embedding_length ?? null;
  if (width == null) return [];
  const out: SidecarOption[] = [];
  for (const peer of peers ?? []) {
    for (const sidecar of peer.sidecars ?? []) {
      if (sidecar.kind !== kind || sidecar.dim !== width) continue;
      const draftFor = sidecar.architecture === "dflash" ? "deepseek41" : sidecar.architecture;
      if (kind === "mtp" && (!model?.architecture || draftFor !== model.architecture)) continue;
      const holder = {
        node_id: peer.node_id,
        hostname: peer.hostname || peer.node_id.slice(0, 8),
        path: sidecar.path,
      };
      const existing = out.find(
        (option) => option.file_name === sidecar.file_name && option.size_bytes === sidecar.size_bytes,
      );
      if (existing) {
        if (!existing.holders.some((item) => item.node_id === holder.node_id)) existing.holders.push(holder);
      } else {
        out.push({
          kind,
          file_name: sidecar.file_name,
          size_bytes: sidecar.size_bytes,
          general_name: sidecar.general_name,
          quant: sidecar.quant,
          holders: [holder],
        });
      }
    }
  }
  return out.sort((a, b) => a.file_name.localeCompare(b.file_name) || a.size_bytes - b.size_bytes);
}

/**
 * Choose the vision projector or the MTP draft of a model.
 *
 * The select names each file with its size and the machines that hold it.
 * Below it one line sums the chosen file up; the full name and the path on
 * every machine open in a window, because a list of paths per machine took
 * the whole inspector while saying nothing the operator needs at a glance.
 * On the deploy screen `slot` adds what the plan will do with the choice.
 */
export function SidecarPicker({
  kind,
  label,
  options,
  value,
  onChange,
  allowEmbedded = false,
  linkLabel,
  slot,
  disabled = false,
  compact = false,
}: {
  kind: SidecarKind;
  label: string;
  options: SidecarOption[];
  value: SidecarPick;
  onChange: (next: SidecarPick) => void;
  allowEmbedded?: boolean;
  /** Offers "use the model's link" when set; the text describes that link. */
  linkLabel?: string | null;
  slot?: SidecarSlot | null;
  disabled?: boolean;
  compact?: boolean;
}) {
  const { t } = useI18n();
  const [detailsOpen, setDetailsOpen] = useState(false);
  const selectedKey = keyOf(value);
  const selectedFile =
    value.mode === "file" && value.file
      ? options.find(
          (option) =>
            option.file_name === value.file?.file_name && option.size_bytes === value.file?.size_bytes,
        )
      : undefined;
  const shownFile = selectedFile ?? (slot?.file
    ? options.find((option) => option.file_name === slot.file?.file_name && option.size_bytes === slot.file?.size_bytes)
    : undefined);
  const missingSelected = value.mode === "file" && value.file && !selectedFile;

  return (
    <div className="min-w-0 space-y-1.5" data-testid={`sidecar-picker-${kind}`}>
      <Select
        aria-label={label}
        value={selectedKey}
        disabled={disabled}
        compact={compact}
        className={clsx(!compact && "h-8 text-[0.72rem]")}
        onChange={(event) => {
          const key = event.target.value;
          if (key === "link" || key === "none" || key === "embedded") {
            onChange({ mode: key });
            return;
          }
          const option = options.find((item) => fileKey(item) === key);
          if (option) onChange({ mode: "file", file: { file_name: option.file_name, size_bytes: option.size_bytes } });
        }}
      >
        {linkLabel !== undefined && (
          <option value="link">
            {linkLabel ? t("sidecars.useLink", { name: linkLabel }) : t("sidecars.useLinkNone")}
          </option>
        )}
        <option value="none">{kind === "mmproj" ? t("sidecars.noVision") : t("sidecars.noMtp")}</option>
        {kind === "mtp" && allowEmbedded && <option value="embedded">{t("sidecars.embedded")}</option>}
        {missingSelected && value.file && (
          <option value={fileKey(value.file)}>{value.file.file_name} · {t("sidecars.missing")}</option>
        )}
        {options.map((option) => (
          <option key={fileKey(option)} value={fileKey(option)}>
            {option.file_name} · {bytes(option.size_bytes)} · {option.holders.map((holder) => holder.hostname).join(", ")}
          </option>
        ))}
      </Select>
      {options.length === 0 && (
        <p className="text-[0.62rem] leading-relaxed text-faint">{t("sidecars.noneCompatible")}</p>
      )}
      {shownFile && (
        <div className="flex min-w-0 items-center justify-between gap-2 text-[0.62rem]">
          <span className="min-w-0 truncate text-faint">
            {bytes(shownFile.size_bytes)} · {t("sidecars.machineCount", { count: shownFile.holders.length })}
          </span>
          <button
            type="button"
            onClick={() => setDetailsOpen(true)}
            className="inline-flex shrink-0 items-center gap-1 rounded text-iris underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris"
          >
            <Info className="h-3 w-3" aria-hidden="true" />
            {t("sidecars.details")}
          </button>
        </div>
      )}
      {shownFile && (
        <Modal
          open={detailsOpen}
          title={kind === "mmproj" ? t("sidecars.vision") : t("sidecars.mtp")}
          onClose={() => setDetailsOpen(false)}
          footer={<Button variant="ghost" onClick={() => setDetailsOpen(false)}>{t("common.close")}</Button>}
        >
          <div className="space-y-3 text-xs leading-relaxed">
            <div>
              <p className="break-all font-mono text-fg">{shownFile.file_name}</p>
              <p className="text-faint">
                {bytes(shownFile.size_bytes)}
                {shownFile.general_name ? ` · ${shownFile.general_name}` : ""}
              </p>
            </div>
            <ul className="space-y-1">
              {shownFile.holders.map((holder) => (
                <li key={holder.node_id} className="break-all font-mono text-muted">
                  <span className="font-sans font-medium text-fg">{holder.hostname}</span>: {holder.path}
                </li>
              ))}
            </ul>
          </div>
        </Modal>
      )}
      {slot?.problem ? (
        <p role="alert" className="text-[0.62rem] leading-relaxed text-amber">{slot.problem}</p>
      ) : slot?.mode === "file" && slot.target_path ? (
        <p className="text-[0.62rem] leading-relaxed text-aqua">{t("sidecars.ready", { host: slot.target_host })}</p>
      ) : slot?.mode === "file" && slot.source_host ? (
        <p className="text-[0.62rem] leading-relaxed text-amber">
          {t("sidecars.copy", {
            host: slot.target_host,
            source: slot.source_host,
            size: bytes(slot.file?.size_bytes ?? 0),
          })}
        </p>
      ) : null}
    </div>
  );
}
