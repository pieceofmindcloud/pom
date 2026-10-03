import { useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { useI18n } from "../i18n/provider";
import { api } from "../lib/api";
import { useProfile } from "../lib/profile";
import { keys, useSidecarLinks } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { PeerCatalog, SidecarKind, SidecarLink } from "../lib/types";
import { SidecarPicker, sidecarOptions, type SidecarPick } from "./SidecarPicker";

function pickOf(link: SidecarLink | undefined): SidecarPick {
  if (!link) return { mode: "none" };
  if (link.mode === "file" && link.file_name && link.size_bytes != null) {
    return { mode: "file", file: { file_name: link.file_name, size_bytes: link.size_bytes } };
  }
  return { mode: link.mode };
}

/**
 * The vision projector and MTP draft linked to one model, edited in place on
 * the Shards and Models screens. The link is saved on the principal and named
 * by file name + size, so it holds whichever machine keeps the file; the
 * deploy screen starts from it.
 */
export function SidecarLinkEditor({
  modelKey,
  model,
  embeddedMtp,
  peers,
}: {
  /** The model label: the slice set's `model_label` or the whole model's name. */
  modelKey: string;
  model: { architecture?: string | null; embedding_length?: number | null } | null | undefined;
  embeddedMtp: boolean;
  peers: PeerCatalog[] | undefined;
}) {
  const { t } = useI18n();
  const toast = useToast();
  const queryClient = useQueryClient();
  const { profile } = useProfile();
  const links = useSidecarLinks();
  const [saving, setSaving] = useState<SidecarKind | null>(null);
  const isAdmin = profile === "admin";
  const visionOptions = useMemo(() => sidecarOptions(peers, "mmproj", model), [peers, model]);
  const mtpOptions = useMemo(() => sidecarOptions(peers, "mtp", model), [peers, model]);
  const linkOf = (kind: SidecarKind) =>
    links.data?.links.find((link) => link.model_key === modelKey && link.kind === kind);

  async function save(kind: SidecarKind, pick: SidecarPick) {
    if (pick.mode === "link") return;
    setSaving(kind);
    try {
      const body: SidecarLink = {
        model_key: modelKey,
        kind,
        mode: pick.mode,
        file_name: pick.mode === "file" ? pick.file?.file_name ?? null : null,
        size_bytes: pick.mode === "file" ? pick.file?.size_bytes ?? null : null,
      };
      const saved = await api.post<{ links: SidecarLink[]; stale: boolean }>("/api/ui/model-sidecar-links", body);
      queryClient.setQueryData(keys.sidecarLinks, saved);
      toast.success(t("sidecars.saved"));
    } catch (error) {
      toast.fromError(t("sidecars.saveFailed"), error);
    } finally {
      setSaving(null);
    }
  }

  return (
    <section className="space-y-2" aria-label={t("sidecars.linkTitle")} data-testid="sidecar-link-editor">
      <p className="text-[0.66rem] leading-relaxed text-faint">{t("sidecars.linkHint")}</p>
      {links.data?.stale && <p className="text-[0.66rem] text-amber">{t("sidecars.stale")}</p>}
      <div className="grid gap-3 md:grid-cols-2">
        <label className="min-w-0 space-y-1">
          <span className="text-[0.68rem] font-medium text-muted">{t("sidecars.vision")}</span>
          <SidecarPicker
            kind="mmproj"
            label={t("sidecars.vision")}
            options={visionOptions}
            value={pickOf(linkOf("mmproj"))}
            disabled={!isAdmin || saving === "mmproj"}
            onChange={(pick) => void save("mmproj", pick)}
          />
        </label>
        <label className="min-w-0 space-y-1">
          <span className="text-[0.68rem] font-medium text-muted">{t("sidecars.mtp")}</span>
          <SidecarPicker
            kind="mtp"
            label={t("sidecars.mtp")}
            options={mtpOptions}
            value={linkOf("mtp") ? pickOf(linkOf("mtp")) : embeddedMtp ? { mode: "embedded" } : { mode: "none" }}
            allowEmbedded={embeddedMtp}
            disabled={!isAdmin || saving === "mtp"}
            onChange={(pick) => void save("mtp", pick)}
          />
        </label>
      </div>
      {!isAdmin && <p className="text-[0.62rem] text-faint">{t("sidecars.readOnly")}</p>}
    </section>
  );
}
