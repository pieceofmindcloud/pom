/**
 * Charging visitors (the principal's screen): the switch and the access
 * policy it needs, the price of each model, the visitor networks' accounts
 * with their balances, granting credit, and the ledger.
 *
 * The switch and the prices are network settings (`NetworkSettings.payments`,
 * `inference_access`), saved through `/api/ui/network/settings` like the rest
 * of the network; the accounts and the ledger come from `/api/ui/payments`
 * (`crates/node/src/payments/runtime.rs`). See
 * `docs/superpowers/specs/2026-11-09-visitor-charging-design.md`.
 */
import { Coins, Plus, Save, Trash2, Wallet } from "lucide-react";
import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Badge, Button, Empty, ErrorState, Field, Input, PageHeader, Panel, PanelHeader, Select, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import type { NetworkInfoView } from "../lib/types";

type Price = { in_micro_per_1k: number; out_micro_per_1k: number };
type Overview = {
  enabled: boolean;
  inference_access: string;
  prices: Record<string, Price>;
  principal: boolean;
  accounts: { account: string; balance_micro: number; reserved_micro: number }[];
  ledger: { account: string; kind: string; amount_micro: number; detail: string | null; at: string }[];
};

const MICRO = 1_000_000;
const credits = (micro: number) => (micro / MICRO).toLocaleString(undefined, { maximumFractionDigits: 6 });
const short = (account: string) => (account.length > 22 ? `${account.slice(0, 18)}…` : account);

export function Payments() {
  const { t } = useI18n();
  const toast = useToast();
  const client = useQueryClient();
  const overview = useQuery<Overview>({
    queryKey: ["payments"],
    queryFn: () => api.get<Overview>("/api/ui/payments"),
    refetchInterval: 15_000,
  });
  const info = useQuery<NetworkInfoView>({
    queryKey: ["payments-network"],
    queryFn: () => api.get<NetworkInfoView>("/api/ui/network/info"),
  });
  const [enabled, setEnabled] = useState(false);
  const [prices, setPrices] = useState<{ model: string; input: string; output: string }[]>([]);
  const [saving, setSaving] = useState(false);
  const [grantAccount, setGrantAccount] = useState("");
  const [grantAmount, setGrantAmount] = useState("");
  const [granting, setGranting] = useState(false);

  useEffect(() => {
    const data = overview.data;
    if (!data) return;
    setEnabled(data.enabled);
    setPrices(
      Object.entries(data.prices).map(([model, p]) => ({
        model,
        input: String(p.in_micro_per_1k / MICRO),
        output: String(p.out_micro_per_1k / MICRO),
      })),
    );
  }, [overview.data]);

  async function save() {
    const network = info.data?.network as unknown as Record<string, unknown> | undefined;
    if (!network) return;
    setSaving(true);
    try {
      const priceMap: Record<string, Price> = {};
      for (const row of prices) {
        const model = row.model.trim();
        if (!model) continue;
        priceMap[model] = {
          in_micro_per_1k: Math.round(Number(row.input || 0) * MICRO),
          out_micro_per_1k: Math.round(Number(row.output || 0) * MICRO),
        };
      }
      await api.post("/api/ui/network/settings", {
        ...network,
        // Charging needs every visitor to go through an entry node.
        inference_access: enabled ? "member_gateway_only" : network.inference_access ?? "direct",
        payments: { enabled, prices: priceMap },
      });
      toast.success(t("payments.saved"));
      await Promise.all([client.invalidateQueries({ queryKey: ["payments"] }), client.invalidateQueries({ queryKey: ["payments-network"] })]);
    } catch (error) {
      toast.fromError(t("payments.saveFailed"), error);
    } finally {
      setSaving(false);
    }
  }

  async function grant() {
    setGranting(true);
    try {
      await api.post("/api/ui/payments/grant", {
        account: grantAccount.trim(),
        amount_micro: Math.round(Number(grantAmount) * MICRO),
      });
      toast.success(t("payments.granted"));
      setGrantAmount("");
      await client.invalidateQueries({ queryKey: ["payments"] });
    } catch (error) {
      toast.fromError(t("payments.grantFailed"), error);
    } finally {
      setGranting(false);
    }
  }

  if (overview.isLoading || info.isLoading) return <Skeleton className="h-60" />;
  if (overview.isError) {
    return <ErrorState title={t("payments.loadFailed")} error={overview.error} onRetry={() => void overview.refetch()} />;
  }
  const data = overview.data!;
  return (
    <div className="grid gap-4">
      <PageHeader
        title={t("payments.title")}
        subtitle={t("payments.subtitle")}
        actions={<Badge tone={data.enabled ? "aqua" : "neutral"}>{data.enabled ? t("payments.on") : t("payments.off")}</Badge>}
      />
      <Panel>
        <PanelHeader title={t("payments.settings")} subtitle={t("payments.settingsHint")} />
        <div className="grid gap-3 px-4 pb-4">
          <Field label={t("payments.charge")} hint={t("payments.chargeHint")}>
            <Select value={enabled ? "on" : "off"} onChange={(e) => setEnabled(e.target.value === "on")} disabled={!data.principal}>
              <option value="off">{t("payments.off")}</option>
              <option value="on">{t("payments.on")}</option>
            </Select>
          </Field>
          <div className="grid gap-2">
            <div className="text-sm font-medium">{t("payments.prices")}</div>
            <div className="text-xs text-muted">{t("payments.pricesHint")}</div>
            {prices.map((row, i) => (
              <div key={i} className="grid grid-cols-[2fr_1fr_1fr_auto] items-center gap-2">
                <Input aria-label={t("payments.model")} placeholder={t("payments.model")} value={row.model}
                  onChange={(e) => setPrices(prices.map((r, j) => (j === i ? { ...r, model: e.target.value } : r)))} />
                <Input aria-label={t("payments.inputPrice")} placeholder={t("payments.inputPrice")} inputMode="decimal" value={row.input}
                  onChange={(e) => setPrices(prices.map((r, j) => (j === i ? { ...r, input: e.target.value } : r)))} />
                <Input aria-label={t("payments.outputPrice")} placeholder={t("payments.outputPrice")} inputMode="decimal" value={row.output}
                  onChange={(e) => setPrices(prices.map((r, j) => (j === i ? { ...r, output: e.target.value } : r)))} />
                <Button variant="ghost" size="sm" icon={<Trash2 className="h-4 w-4" />} aria-label={t("payments.removePrice")}
                  onClick={() => setPrices(prices.filter((_, j) => j !== i))} />
              </div>
            ))}
            <div>
              <Button size="sm" icon={<Plus className="h-4 w-4" />} onClick={() => setPrices([...prices, { model: "", input: "", output: "" }])}>
                {t("payments.addPrice")}
              </Button>
            </div>
          </div>
          <div>
            <Button variant="primary" icon={<Save className="h-4 w-4" />} loading={saving} disabled={!data.principal} onClick={() => void save()}>
              {t("payments.save")}
            </Button>
          </div>
        </div>
      </Panel>
      <Panel>
        <PanelHeader title={t("payments.accounts")} subtitle={t("payments.accountsHint")} />
        <div className="grid gap-3 px-4 pb-4">
          {data.accounts.length === 0 ? (
            <Empty icon={<Wallet className="h-5 w-5" />} title={t("payments.noAccounts")}>{t("payments.noAccountsHint")}</Empty>
          ) : (
            <table className="w-full text-sm">
              <thead className="text-left text-xs text-muted">
                <tr><th>{t("payments.account")}</th><th>{t("payments.balance")}</th><th>{t("payments.reserved")}</th><th /></tr>
              </thead>
              <tbody>
                {data.accounts.map((a) => (
                  <tr key={a.account} className="border-t border-line">
                    <td className="py-1.5 font-mono text-xs" title={a.account}>{short(a.account)}</td>
                    <td className={a.balance_micro < 0 ? "text-rose" : undefined}>{credits(a.balance_micro)}</td>
                    <td>{credits(a.reserved_micro)}</td>
                    <td><Button size="sm" variant="ghost" onClick={() => setGrantAccount(a.account)}>{t("payments.grantTo")}</Button></td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          <div className="grid grid-cols-[2fr_1fr_auto] items-end gap-2">
            <Field label={t("payments.account")}>
              <Input placeholder="net:…" value={grantAccount} onChange={(e) => setGrantAccount(e.target.value)} />
            </Field>
            <Field label={t("payments.amount")}>
              <Input inputMode="decimal" value={grantAmount} onChange={(e) => setGrantAmount(e.target.value)} />
            </Field>
            <Button variant="primary" icon={<Coins className="h-4 w-4" />} loading={granting}
              disabled={!data.principal || !grantAccount.trim() || !Number(grantAmount)} onClick={() => void grant()}>
              {t("payments.grant")}
            </Button>
          </div>
        </div>
      </Panel>
      <Panel>
        <PanelHeader title={t("payments.ledger")} />
        <div className="px-4 pb-4">
          {data.ledger.length === 0 ? (
            <div className="text-sm text-muted">{t("payments.noLedger")}</div>
          ) : (
            <table className="w-full text-sm">
              <thead className="text-left text-xs text-muted">
                <tr><th>{t("payments.when")}</th><th>{t("payments.account")}</th><th>{t("payments.kind")}</th><th>{t("payments.amount")}</th></tr>
              </thead>
              <tbody>
                {data.ledger.map((row, i) => (
                  <tr key={i} className="border-t border-line">
                    <td className="py-1.5 text-xs">{new Date(row.at).toLocaleString()}</td>
                    <td className="font-mono text-xs" title={row.account}>{short(row.account)}</td>
                    <td>{t(`payments.kind.${row.kind}`)}</td>
                    <td>{row.kind === "grant" ? "+" : "−"}{credits(Math.abs(row.amount_micro))}</td>
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
