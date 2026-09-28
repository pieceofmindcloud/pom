/**
 * Ponte entre o shell do POM e os plugins (`pom-plugin-events/v1`).
 *
 * Publica o idioma e o tema atuais sempre que mudam, repassa às telas os
 * eventos que o nó publica (`GET /api/ui/plugins/events`: implantação
 * iniciada, modelo servindo ou parado, preferências salvas), executa os
 * comandos que os plugins pedem (aviso local, aviso para a rede, confirmação
 * Aceitar/Recusar no sino ou em diálogo) e transforma cada aviso de plugin que
 * chega pela rede em toast e evento `notification.received`. Não desenha nada
 * além do diálogo de confirmação. O protocolo em si está em
 * `lib/pluginEvents.ts`.
 */
import { useEffect, useRef, useState } from "react";
import { api, postNetworkChat } from "../lib/api";
import { addPendingPluginConfirm, addPluginNotice } from "../lib/pluginInbox";
import {
  forwardNodeEvent,
  installPluginCommandExecutor,
  listenForPluginCommands,
  publishPluginEvent,
  type ConfirmRequest,
  type NotifyRequest,
} from "../lib/pluginEvents";
import { pluginDisplayName, pluginUiSnapshot } from "../lib/pluginUi";
import { useNetworkChat, useNode } from "../lib/queries";
import { randomId } from "../lib/id";
import { useTheme } from "../lib/theme";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { Modal } from "./Confirm";
import { Button } from "./ui";

/** Intervalo da leitura dos eventos do nó para as telas de plugin. */
const NODE_EVENTS_POLL_MS = 3000;

interface PendingConfirm {
  code: string;
  request: ConfirmRequest;
  resolve: (action: "accepted" | "cancelled") => void;
}

export function PluginBridge() {
  const { t, locale } = useI18n();
  const { theme } = useTheme();
  const toast = useToast();
  const node = useNode();
  const chat = useNetworkChat();
  const [pending, setPending] = useState<PendingConfirm[]>([]);
  const seenNotices = useRef<Set<string> | null>(null);
  const nodeId = node.data?.node_id ?? null;

  useEffect(() => {
    publishPluginEvent("locale.changed", { locale });
  }, [locale]);

  useEffect(() => {
    publishPluginEvent("theme.changed", { theme });
  }, [theme]);

  // O executor lê sempre os valores mais recentes sem ser reinstalado.
  const latest = useRef({ toast, nodeId, locale, t });
  latest.current = { toast, nodeId, locale, t };

  useEffect(() => {
    installPluginCommandExecutor({
      isPlugin: (code) => pluginUiSnapshot().some((plugin) => plugin.code === code),
      notify: async (code: string, request: NotifyRequest) => {
        const { toast: show, nodeId: sender, locale: currentLocale, t: translate } = latest.current;
        const name = pluginDisplayName(code, currentLocale);
        if (request.scope === "network") {
          if (!sender) throw new Error("node_unavailable");
          await postNetworkChat({
            action: {
              kind: "plugin_notice",
              message_id: randomId(),
              room_id: "network",
              sender_node_id: sender,
              recipient_node_id: null,
              plugin_code: code,
              title: request.title,
              body: request.body ?? "",
              level: request.level ?? "info",
              sent_at: new Date().toISOString(),
            },
          });
          show.info(translate("pluginEvents.networkSent", { plugin: name }), request.title);
          return;
        }
        const level = request.level ?? "info";
        // O aviso também fica no sino como não lido, sem botões.
        addPluginNotice({
          id: randomId(),
          code,
          title: request.title,
          body: request.body,
          level,
          createdAt: new Date().toISOString(),
          source: "local",
        });
        show[level](`${name}: ${request.title}`, request.body);
      },
      confirm: (code: string, request: ConfirmRequest) =>
        new Promise((resolve) => {
          if (request.display === "dialog") {
            setPending((queue) => [...queue, { code, request, resolve }]);
            return;
          }
          // Como uma oferta de transferência: o pedido fica no sino até a
          // pessoa aceitar ou recusar; o toast só avisa que ele chegou.
          const { toast: show, locale: currentLocale, t: translate } = latest.current;
          addPendingPluginConfirm(
            { id: randomId(), code, request, createdAt: new Date().toISOString() },
            resolve,
          );
          show.info(translate("pluginEvents.confirmWaiting", { plugin: pluginDisplayName(code, currentLocale) }), request.title);
        }),
    });
    const stop = listenForPluginCommands();
    return () => {
      stop();
      installPluginCommandExecutor(null);
    };
  }, []);

  // Eventos do nó. A primeira leitura só aprende a sequência atual.
  useEffect(() => {
    let after: number | null = null;
    let stopped = false;
    const poll = async () => {
      try {
        const query = after === null ? "" : `?after=${after}`;
        const response = await api.get<{ latest?: unknown; events?: unknown }>(`/api/ui/plugins/events${query}`);
        if (stopped) return;
        if (Array.isArray(response.events)) {
          for (const item of response.events as Array<{ event?: unknown }>) forwardNodeEvent(item.event);
        }
        if (typeof response.latest === "number") after = response.latest;
      } catch {
        // Sem permissão ou nó indisponível: as telas seguem sem esses eventos.
      }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), NODE_EVENTS_POLL_MS);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, []);

  // Avisos de plugin recebidos da rede. Todos entram no sino (os já vistos
  // numa visita anterior entram como lidos); na primeira leitura não há toast
  // nem evento, para não reapresentar avisos antigos a cada recarga.
  const notices = chat.data?.notices;
  useEffect(() => {
    if (!notices) return;
    const firstRead = seenNotices.current === null;
    seenNotices.current ??= new Set();
    for (const notice of notices) {
      if (seenNotices.current.has(notice.messageId)) continue;
      seenNotices.current.add(notice.messageId);
      const code = notice.pluginCode ?? "*";
      const level = notice.level ?? "info";
      const fresh = addPluginNotice({
        id: notice.messageId,
        code,
        title: notice.title ?? "",
        body: notice.text ?? undefined,
        level,
        createdAt: notice.sentAt,
        source: "network",
        senderNodeId: notice.senderNodeId,
      });
      if (firstRead) continue;
      publishPluginEvent(
        "notification.received",
        {
          message_id: notice.messageId,
          plugin_code: code,
          title: notice.title ?? "",
          body: notice.text ?? "",
          level,
          sender_node_id: notice.senderNodeId,
          sent_at: notice.sentAt,
        },
        code,
      );
      if (!fresh || notice.senderNodeId === nodeId) continue;
      toast[level](`${pluginDisplayName(code, locale)}: ${notice.title ?? ""}`, notice.text ?? undefined);
    }
  }, [notices, nodeId, locale, toast]);

  const current = pending[0];
  const answer = (action: "accepted" | "cancelled") => {
    if (!current) return;
    current.resolve(action);
    setPending((queue) => queue.slice(1));
  };

  return (
    <Modal
      open={current !== undefined}
      title={current?.request.title ?? ""}
      onClose={() => answer("cancelled")}
      footer={
        <>
          <Button variant="ghost" onClick={() => answer("cancelled")}>
            {current?.request.cancelLabel || t("common.cancel")}
          </Button>
          <Button
            variant={current?.request.tone === "danger" ? "danger" : "primary"}
            onClick={() => answer("accepted")}
          >
            {current?.request.acceptLabel || t("pluginEvents.accept")}
          </Button>
        </>
      }
    >
      {current && (
        <>
          <p className="eyebrow mb-2">{pluginDisplayName(current.code, locale)}</p>
          {current.request.body && <p className="whitespace-pre-wrap">{current.request.body}</p>}
        </>
      )}
    </Modal>
  );
}
