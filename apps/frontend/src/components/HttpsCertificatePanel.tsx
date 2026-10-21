/**
 * This machine's local CA, on the Preferences screen.
 *
 * The node serves `https://` with a certificate signed by a CA of its own
 * (`crates/node/src/network/web_ca.rs`). Once the system trusts that CA,
 * browsers, curl and the harnesses calling `/v1/chat/completions` accept the
 * address with no warning. The desktop app asks the system once at start; a
 * refusal is remembered, so this panel is where the person tries again
 * ("Install on this computer", only from the node's own machine) or downloads
 * the `.crt` to install by hand on this or any other machine.
 */
import { useState } from "react";
import { Download, ShieldCheck } from "lucide-react";
import { api } from "../lib/api";
import { useLocalCa } from "../lib/queries";
import type { LocalCaInstallReport } from "../lib/types";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { Badge, Button, KeyValue, Panel, PanelHeader } from "./ui";

function Command({ children }: { children: string }) {
  return (
    <pre className="mt-1 overflow-x-auto rounded-md border border-line bg-ink px-3 py-2 font-mono text-[0.72rem] text-fg">
      {children}
    </pre>
  );
}

export function HttpsCertificatePanel() {
  const { t } = useI18n();
  const toast = useToast();
  const [installing, setInstalling] = useState(false);
  const status = useLocalCa();

  async function download() {
    try {
      const pem = await api.getText("/api/ui/tls/ca.crt");
      const url = URL.createObjectURL(new Blob([pem], { type: "application/x-x509-ca-cert" }));
      const link = document.createElement("a");
      link.href = url;
      link.download = status.data?.filename ?? "pom-ca.crt";
      document.body.appendChild(link);
      link.click();
      link.remove();
      URL.revokeObjectURL(url);
    } catch (error) {
      toast.fromError(t("https.downloadFailed"), error);
    }
  }

  async function install() {
    setInstalling(true);
    try {
      const report = await api.post<LocalCaInstallReport>("/api/ui/tls/ca/install");
      if (report.outcome === "installed" || report.outcome === "already_trusted") {
        toast.success(t("https.installed"), t("https.installedDetail"));
      } else {
        toast.fromError(t(`https.outcome.${report.outcome}`), new Error(report.detail));
      }
    } catch (error) {
      toast.fromError(t("https.installFailed"), error);
    } finally {
      setInstalling(false);
      void status.refetch();
    }
  }

  if (status.isError) return null;
  const ca = status.data;
  const file = ca?.filename ?? "pom-ca.crt";
  const tone = ca?.trust === "trusted" ? "aqua" : ca?.trust === "not_trusted" ? "amber" : "neutral";

  return (
    <Panel>
      <PanelHeader
        eyebrow={t("https.eyebrow")}
        title={t("https.title")}
        subtitle={t("https.subtitle")}
        actions={
          <div className="flex flex-wrap gap-2">
            <Button icon={<Download className="h-4 w-4" />} onClick={() => void download()} disabled={!ca}>
              {t("https.download")}
            </Button>
            {ca?.can_install && ca.trust !== "trusted" && (
              <Button
                variant="primary"
                icon={<ShieldCheck className="h-4 w-4" />}
                loading={installing}
                onClick={() => void install()}
              >
                {t("https.install")}
              </Button>
            )}
          </div>
        }
      />
      <div data-testid="https-certificate" className="space-y-4 px-5 pb-5 text-sm text-muted">
        <KeyValue label={t("https.state")}>
          <Badge tone={tone}>{t(`https.trust.${ca?.trust ?? "unknown"}`)}</Badge>
        </KeyValue>
        {ca?.declined && ca.trust !== "trusted" && <p>{t("https.declined")}</p>}
        <KeyValue label={t("https.fingerprint")}>
          <span className="break-all font-mono text-[0.72rem] text-fg">{ca?.fingerprint ?? "-"}</span>
        </KeyValue>
        <details className="rounded-lg border border-line px-3 py-2">
          <summary className="cursor-pointer text-fg">{t("https.manualTitle")}</summary>
          <div className="mt-3 space-y-3">
            <p>{t("https.manualIntro")}</p>
            <div>
              <p className="font-medium text-fg">macOS</p>
              <p>{t("https.manualMac")}</p>
              <Command>{`security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db ${file}`}</Command>
            </div>
            <div>
              <p className="font-medium text-fg">Windows</p>
              <p>{t("https.manualWindows")}</p>
              <Command>{`certutil -user -addstore Root ${file}`}</Command>
            </div>
            <div>
              <p className="font-medium text-fg">Linux</p>
              <p>{t("https.manualLinux")}</p>
              <Command>{`sudo cp ${file} /usr/local/share/ca-certificates/ && sudo update-ca-certificates\ncertutil -d sql:$HOME/.pki/nssdb -A -t C,, -n POM -i ${file}`}</Command>
            </div>
            <div>
              <p className="font-medium text-fg">{t("https.harnessTitle")}</p>
              <p>{t("https.harnessIntro")}</p>
              <Command>{`# Node.js\nNODE_EXTRA_CA_CERTS=/path/to/${file}\n# Python (openai, httpx, requests) on Linux, after update-ca-certificates\nSSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt\nREQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt`}</Command>
            </div>
          </div>
        </details>
      </div>
    </Panel>
  );
}
