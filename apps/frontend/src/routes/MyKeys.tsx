/**
 * A signed-in person's own API keys (`/api/ui/my-keys`), outside demo mode.
 * Requests with these keys carry the person's id, so they count against their
 * quota and appear in their own logs. The secret shows once, at creation.
 */
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Badge, Button, Empty, ErrorState, Input, PageHeader, Panel, PanelHeader, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { useI18n } from "../i18n/provider";

interface MyKey {
  id: string;
  label: string;
  key_hint: string;
  active: boolean;
  created_at: string;
}

const MY_KEYS = ["my-keys"] as const;

export function MyKeys() {
  const { t } = useI18n();
  const client = useQueryClient();
  const [label, setLabel] = useState("");
  const [secret, setSecret] = useState<string | null>(null);
  const list = useQuery({
    queryKey: MY_KEYS,
    queryFn: () => api.get<{ keys: MyKey[] }>("/api/ui/my-keys"),
  });
  const create = useMutation({
    mutationFn: () => api.post<{ key: string }>("/api/ui/my-keys", { label }),
    onSuccess: (answer) => {
      setSecret(answer.key);
      setLabel("");
      void client.invalidateQueries({ queryKey: MY_KEYS });
    },
  });
  const revoke = useMutation({
    mutationFn: (id: string) => api.del(`/api/ui/my-keys/${encodeURIComponent(id)}`),
    onSuccess: () => void client.invalidateQueries({ queryKey: MY_KEYS }),
  });

  return (
    <div className="mx-auto w-full max-w-3xl space-y-5 p-5">
      <PageHeader title={t("myKeys.title")} subtitle={t("myKeys.subtitle")} />
      <Panel>
        <PanelHeader title={t("myKeys.create")} />
        <form
          className="flex flex-wrap gap-2 p-5"
          onSubmit={(event) => {
            event.preventDefault();
            create.mutate();
          }}
        >
          <Input
            className="min-w-0 flex-1"
            value={label}
            maxLength={60}
            placeholder={t("myKeys.label")}
            onChange={(event) => setLabel(event.target.value)}
          />
          <Button type="submit" variant="primary" disabled={create.isPending}>
            {t("myKeys.create")}
          </Button>
        </form>
        {secret && (
          <div className="border-t border-line px-5 py-4 text-sm">
            <p className="mb-2 text-muted">{t("myKeys.copyOnce")}</p>
            <code className="break-path block rounded bg-raised px-3 py-2 font-mono text-fg">{secret}</code>
          </div>
        )}
        {(create.isError || revoke.isError) && (
          <p role="alert" className="px-5 pb-4 text-sm text-rose">{t("myKeys.failed")}</p>
        )}
      </Panel>
      <Panel>
        {list.isLoading ? (
          <div className="p-5"><Skeleton className="h-10" /></div>
        ) : list.isError ? (
          <ErrorState error={list.error} />
        ) : (list.data?.keys.length ?? 0) === 0 ? (
          <Empty title={t("myKeys.empty")} />
        ) : (
          <ul className="divide-y divide-line">
            {list.data!.keys.map((key) => (
              <li key={key.id} className="flex flex-wrap items-center justify-between gap-3 px-5 py-3 text-sm">
                <div className="min-w-0">
                  <p className="truncate font-medium text-fg">{key.label}</p>
                  <p className="font-mono text-xs text-faint">{key.key_hint} · {new Date(key.created_at).toLocaleString()}</p>
                </div>
                {key.active ? (
                  <Button variant="outline" disabled={revoke.isPending} onClick={() => revoke.mutate(key.id)}>
                    {t("myKeys.revoke")}
                  </Button>
                ) : (
                  <Badge>{t("myKeys.revoked")}</Badge>
                )}
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </div>
  );
}
