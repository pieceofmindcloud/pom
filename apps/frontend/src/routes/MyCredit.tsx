/**
 * A visitor's credit on the network it visits (`/config/credit`): its home
 * network's balance there, the prices, what this machine spent, and a request
 * for more credit to the network's admin. Reads `GET /api/ui/payments/balance`
 * and posts `POST /api/ui/payments/request` (`crates/node/src/payments/runtime.rs`).
 */
import { Send, Wallet } from "lucide-react";
import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Badge, Button, Empty, ErrorState, Field, Input, PageHeader, Panel, PanelHeader, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";

type Balance = {
  available: boolean;
  reason?: string;
  charging?: boolean;
  account?: string;
  balance_micro?: number | null;
  prices?: Record<string, { in_micro_per_1k: number; out_micro_per_1k: number }>;
  spent?: { model: string | null; charged_micro: number; balance_micro: number | null; at: string }[];
};

const credits = (micro: number) => (micro / 1_000_000).toLocaleString(undefined, { maximumFractionDigits: 6 });

export function MyCredit() {
  const { t } = useI18n();
  const toast = useToast();
  const balance = useQuery<Balance>({
    queryKey: ["my-credit"],
    queryFn: () => api.get<Balance>("/api/ui/payments/balance"),
    refetchInterval: 15_000,
  });
  const [amount, setAmount] = useState("");
  const [note, setNote] = useState("");
  const [sending, setSending] = useState(false);

  async function request() {
    setSending(true);
    try {
      await api.post("/api/ui/payments/request", { amount_micro: Math.round(Number(amount) * 1_000_000), note: note.trim() || undefined });
      toast.success(t("credit.requested"));
      setAmount("");
      setNote("");
    } catch (error) {
      toast.fromError(t("credit.requestFailed"), error);
    } finally {
      setSending(false);
    }
  }

  if (balance.isLoading) return <Skeleton className="h-40" />;
  if (balance.isError) return <ErrorState title={t("credit.loadFailed")} error={balance.error} onRetry={() => void balance.refetch()} />;
  const data = balance.data!;
  if (!data.available) {
    return (
      <div className="grid gap-4">
        <PageHeader title={t("credit.title")} subtitle={t("credit.subtitle")} />
        <Empty icon={<Wallet className="h-5 w-5" />} title={t("credit.unavailable")}>{data.reason ?? ""}</Empty>
      </div>
    );
  }
  const low = (data.balance_micro ?? 0) <= 0;
  return (
    <div className="grid gap-4">
      <PageHeader
        title={t("credit.title")}
        subtitle={t("credit.subtitle")}
        actions={<Badge tone={data.charging ? (low ? "rose" : "aqua") : "neutral"}>{data.charging ? t("credit.charging") : t("credit.free")}</Badge>}
      />
      <div className="grid gap-3 md:grid-cols-2">
        <Panel>
          <PanelHeader title={t("credit.balance")} subtitle={data.account} />
          <div className="px-4 pb-4">
            <div className={`text-3xl font-semibold ${low ? "text-rose" : ""}`}>{credits(data.balance_micro ?? 0)}</div>
            {low && data.charging && <p className="mt-2 text-sm text-muted">{t("credit.lowHint")}</p>}
          </div>
        </Panel>
        <Panel>
          <PanelHeader title={t("credit.ask")} subtitle={t("credit.askHint")} />
          <div className="grid gap-2 px-4 pb-4">
            <Field label={t("payments.amount")}>
              <Input inputMode="decimal" value={amount} onChange={(e) => setAmount(e.target.value)} />
            </Field>
            <Field label={t("payments.note")}>
              <Input value={note} onChange={(e) => setNote(e.target.value)} />
            </Field>
            <div>
              <Button variant="primary" icon={<Send className="h-4 w-4" />} loading={sending} disabled={!(Number(amount) > 0)} onClick={() => void request()}>
                {t("credit.send")}
              </Button>
            </div>
          </div>
        </Panel>
      </div>
      {data.prices && Object.keys(data.prices).length > 0 && (
        <Panel>
          <PanelHeader title={t("payments.prices")} />
          <div className="px-4 pb-4">
            <table className="w-full text-sm">
              <thead className="text-left text-xs text-muted">
                <tr><th>{t("payments.model")}</th><th>{t("payments.inputPrice")}</th><th>{t("payments.outputPrice")}</th></tr>
              </thead>
              <tbody>
                {Object.entries(data.prices).map(([model, p]) => (
                  <tr key={model} className="border-t border-line">
                    <td className="py-1.5">{model}</td>
                    <td>{credits(p.in_micro_per_1k)}</td>
                    <td>{credits(p.out_micro_per_1k)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Panel>
      )}
      <Panel>
        <PanelHeader title={t("credit.spent")} />
        <div className="px-4 pb-4">
          {!data.spent?.length ? (
            <div className="text-sm text-muted">{t("credit.noSpend")}</div>
          ) : (
            <table className="w-full text-sm">
              <thead className="text-left text-xs text-muted">
                <tr><th>{t("payments.when")}</th><th>{t("payments.model")}</th><th>{t("credit.charged")}</th><th>{t("payments.balance")}</th></tr>
              </thead>
              <tbody>
                {data.spent.map((row, i) => (
                  <tr key={i} className="border-t border-line">
                    <td className="py-1.5 text-xs">{new Date(row.at).toLocaleString()}</td>
                    <td>{row.model ?? "-"}</td>
                    <td>{credits(row.charged_micro)}</td>
                    <td>{row.balance_micro == null ? "-" : credits(row.balance_micro)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </Panel>
    </div>
  );
}
