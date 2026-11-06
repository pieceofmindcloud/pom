import { ArrowLeft, Image as ImageIcon, Power } from "lucide-react";
import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { Confirm } from "../components/Confirm";
import { Badge, Button, Empty, FullBleedHeader, Input, Mono, Select } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { api } from "../lib/api";
import { useCluster, useClusterCatalog, useImageDeployments } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { ImageDeployment, ImageModelFile } from "../lib/types";

const COMPONENTS = ["diffusion", "text_encoder", "vae"] as const;
type Component = (typeof COMPONENTS)[number];

/** One machine's copy of a file. */
interface Holder {
  nodeId: string;
  hostname: string;
  path: string;
}

/** A file as the network has it: the same name and size on one or more machines. */
interface NetworkFile {
  key: string;
  fileName: string;
  folder: string;
  category: string;
  holders: Holder[];
}

interface Choice {
  file: string;
  nodeId: string;
}

const baseName = (name: string) => name.split("/").pop() ?? name;
/** `<org>/<repo>` of a catalog name, the folder a model's files share. */
const folderOf = (name: string) => name.split("/").slice(0, -1).join("/");
const fits = (component: Component, category: string) => category === component || category === "checkpoint";

export function ImageDeploy() {
  const { t } = useI18n();
  const toast = useToast();
  const catalog = useClusterCatalog();
  const cluster = useCluster();
  const deployments = useImageDeployments();
  const [name, setName] = useState("");
  const [choices, setChoices] = useState<Record<Component, Choice>>({
    diffusion: { file: "", nodeId: "" },
    text_encoder: { file: "", nodeId: "" },
    vae: { file: "", nodeId: "" },
  });
  const [busy, setBusy] = useState(false);
  const [undeploying, setUndeploying] = useState<ImageDeployment | null>(null);

  // Machines whose ComfyUI publishes the image service.
  const servingNodes = useMemo(() => {
    const ids = new Set<string>();
    for (const peer of cluster.data?.peers ?? []) {
      if (peer.node?.plugin_services?.some((service) => service.kind === "image")) ids.add(peer.node_id);
    }
    return ids;
  }, [cluster.data]);

  const files = useMemo(() => {
    const byKey = new Map<string, NetworkFile>();
    for (const peer of catalog.data?.peers ?? []) {
      for (const file of (peer.image_files ?? []) as ImageModelFile[]) {
        if (file.metadata_error) continue;
        const fileName = baseName(file.name);
        const key = `${fileName}|${file.size_bytes}`;
        const entry = byKey.get(key) ?? { key, fileName, folder: folderOf(file.name), category: file.category, holders: [] };
        if (servingNodes.has(peer.node_id)) {
          entry.holders.push({ nodeId: peer.node_id, hostname: peer.hostname ?? peer.node_id, path: file.path });
        }
        byKey.set(key, entry);
      }
    }
    return [...byKey.values()].sort((a, b) => a.fileName.localeCompare(b.fileName));
  }, [catalog.data, servingNodes]);

  const fileOf = (key: string) => files.find((file) => file.key === key);

  function choose(component: Component, fileKey: string) {
    const file = fileOf(fileKey);
    const next = { ...choices, [component]: { file: fileKey, nodeId: file?.holders[0]?.nodeId ?? "" } };
    if (component === "diffusion" && file) {
      // Pre-fill what sits in the same repository folder, when exactly one fits.
      for (const other of ["text_encoder", "vae"] as const) {
        const sameFolder = files.filter((f) => f.folder === file.folder && f.category === other);
        if (sameFolder.length === 1) {
          next[other] = { file: sameFolder[0].key, nodeId: sameFolder[0].holders[0]?.nodeId ?? "" };
        }
      }
      if (!name.trim()) setName(file.fileName.replace(/\.[^.]+$/, ""));
    }
    setChoices(next);
  }

  const components = COMPONENTS.flatMap((component) => {
    const choice = choices[component];
    const holder = fileOf(choice.file)?.holders.find((h) => h.nodeId === choice.nodeId);
    return holder ? [{ component, node_id: holder.nodeId, path: holder.path }] : [];
  });
  const canDeploy = !busy && name.trim() !== "" && components.some((c) => c.component === "diffusion");

  async function deploy() {
    setBusy(true);
    try {
      await api.post("/api/ui/image-deploy", { name: name.trim(), components });
      toast.success(t("imageDeploy.deployed", { name: name.trim() }));
      void deployments.refetch();
    } catch (error) {
      toast.fromError(t("imageDeploy.failed"), error);
    } finally {
      setBusy(false);
    }
  }

  async function undeploy(deployment: ImageDeployment) {
    setUndeploying(null);
    try {
      await api.post("/api/ui/image-undeploy", { deploy_id: deployment.deploy_id });
      toast.success(t("imageDeploy.undeployed", { name: deployment.name }));
      void deployments.refetch();
    } catch (error) {
      toast.fromError(t("imageDeploy.failed"), error);
    }
  }

  const served = deployments.data?.deployments ?? [];

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader
        title={t("imageDeploy.title")}
        subtitle={t("imageDeploy.subtitle")}
        actions={
          <Link to="/deploy">
            <Button variant="ghost" icon={<ArrowLeft className="h-3.5 w-3.5" />}>
              {t("imageDeploy.back")}
            </Button>
          </Link>
        }
      />
      <div className="min-h-0 flex-1 overflow-auto p-5">
        <div className="mx-auto flex max-w-4xl flex-col gap-4">
          <label className="flex flex-col gap-1 text-sm">
            <span className="text-faint">{t("imageDeploy.name")}</span>
            <Input aria-label={t("imageDeploy.name")} value={name} onChange={(event) => setName(event.target.value)} />
          </label>

          {COMPONENTS.map((component) => {
            const choice = choices[component];
            const file = fileOf(choice.file);
            return (
              <div key={component} data-testid={`image-component-${component}`} className="panel grid gap-3 p-4 sm:grid-cols-[10rem_1fr_1fr] sm:items-center">
                <span className="font-medium text-fg">{t(`imageDeploy.component.${component}`)}</span>
                <Select aria-label={t("imageDeploy.file")} value={choice.file} onChange={(event) => choose(component, event.target.value)}>
                  <option value="">{t("imageDeploy.chooseFile")}</option>
                  {files
                    .filter((f) => fits(component, f.category))
                    .map((f) => (
                      <option key={f.key} value={f.key}>
                        {f.folder ? `${f.folder}/${f.fileName}` : f.fileName}
                      </option>
                    ))}
                </Select>
                <Select
                  aria-label={t("imageDeploy.machine")}
                  value={choice.nodeId}
                  disabled={!file}
                  onChange={(event) => setChoices({ ...choices, [component]: { ...choice, nodeId: event.target.value } })}
                >
                  <option value="">{file && file.holders.length === 0 ? t("imageDeploy.noHolder") : t("imageDeploy.chooseMachine")}</option>
                  {(file?.holders ?? []).map((holder) => (
                    <option key={holder.nodeId} value={holder.nodeId}>
                      {holder.hostname}
                    </option>
                  ))}
                </Select>
              </div>
            );
          })}

          <div className="flex justify-end">
            <Button variant="primary" icon={<ImageIcon className="h-3.5 w-3.5" />} disabled={!canDeploy} loading={busy} onClick={() => void deploy()}>
              {t("imageDeploy.deploy")}
            </Button>
          </div>

          <h2 className="mt-4 text-sm font-semibold text-fg">{t("imageDeploy.served")}</h2>
          {served.length === 0 ? (
            <Empty icon={<ImageIcon className="h-5 w-5" />} title={t("imageDeploy.none")} />
          ) : (
            served.map((deployment) => (
              <div key={deployment.deploy_id} data-testid={`image-deployment-${deployment.deploy_id}`} className="panel p-4">
                <div className="flex items-center justify-between gap-2">
                  <span className="font-medium text-fg">{deployment.name}</span>
                  <Button size="sm" variant="ghost" icon={<Power className="h-3.5 w-3.5" />} onClick={() => setUndeploying(deployment)}>
                    {t("imageDeploy.undeploy")}
                  </Button>
                </div>
                <ul className="mt-2 space-y-1 text-[0.8rem]">
                  {deployment.components.map((c) => (
                    <li key={c.component} className="flex flex-wrap items-center gap-2">
                      <span className="w-32 text-muted">{t(`imageDeploy.component.${c.component}`)}</span>
                      <Mono>{baseName(c.path)}</Mono>
                      <span className="text-faint">{c.hostname}</span>
                      <Badge tone={c.status === "ready" ? "lime" : c.status === "loading" ? "amber" : "rose"}>
                        {t(`imageDeploy.status.${["ready", "loading", "error"].includes(c.status) ? c.status : "missing"}`)}
                      </Badge>
                    </li>
                  ))}
                </ul>
              </div>
            ))
          )}
        </div>
      </div>
      <Confirm
        open={undeploying !== null}
        title={t("imageDeploy.undeploy")}
        description={undeploying ? t("imageDeploy.undeployConfirm", { name: undeploying.name }) : ""}
        confirmLabel={undeploying ? t("imageDeploy.undeployNamed", { name: undeploying.name }) : ""}
        tone="danger"
        onConfirm={() => undeploying && void undeploy(undeploying)}
        onCancel={() => setUndeploying(null)}
      />
    </div>
  );
}
