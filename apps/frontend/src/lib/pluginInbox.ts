/**
 * Caixa de entrada dos plugins no sino da barra superior
 * (`components/Notifications.tsx`). Guarda dois tipos de item:
 *
 * - perguntas (`notification.confirm`): esperam Aceitar ou Recusar, como uma
 *   oferta de transferência, e a resposta volta à janela do plugin. Vivem só
 *   nesta aba, porque é aqui que o plugin espera a resposta;
 * - avisos (`notification.notify`, locais ou recebidos da rede): só uma
 *   mensagem, sem botões. Entram como não lidos e ficam lidos quando a pessoa
 *   abre o sino. Os ids lidos ficam no `localStorage`, para um aviso de rede
 *   que continua no snapshot do nó não voltar como novo depois de recarregar.
 */
import { useSyncExternalStore } from "react";
import { storageKey } from "./brand";
import type { ConfirmRequest, NotificationLevel } from "./pluginEvents";

export interface PendingPluginConfirm {
  id: string;
  code: string;
  request: ConfirmRequest;
  createdAt: string;
}

export interface PluginNotice {
  id: string;
  code: string;
  title: string;
  body?: string;
  level: NotificationLevel;
  createdAt: string;
  /** `network` quando veio de um nó pelo Chat da rede. */
  source: "local" | "network";
  senderNodeId?: string;
  read: boolean;
}

type Entry = PendingPluginConfirm & { resolve: (action: "accepted" | "cancelled") => void };

/** Quantos avisos o sino mantém; os mais antigos saem primeiro. */
const MAX_NOTICES = 30;
const MAX_READ_IDS = 200;
const READ_KEY = storageKey("plugin-notices-read");

let confirms: Entry[] = [];
let notices: PluginNotice[] = [];
let snapshot: { confirms: readonly PendingPluginConfirm[]; notices: readonly PluginNotice[] } = {
  confirms: [],
  notices: [],
};
const listeners = new Set<() => void>();

function readIds(): string[] {
  try {
    const value = JSON.parse(localStorage.getItem(READ_KEY) ?? "[]");
    return Array.isArray(value) ? value.filter((id): id is string => typeof id === "string") : [];
  } catch {
    return [];
  }
}

function rememberRead(ids: string[]) {
  if (ids.length === 0) return;
  try {
    const next = [...new Set([...readIds(), ...ids])].slice(-MAX_READ_IDS);
    localStorage.setItem(READ_KEY, JSON.stringify(next));
  } catch {
    // Sem storage (aba privada): o estado de lido vale só até recarregar.
  }
}

function emit() {
  snapshot = {
    confirms: confirms.map(({ resolve: _resolve, ...entry }) => entry),
    notices: [...notices],
  };
  for (const listener of listeners) listener();
}

export function addPendingPluginConfirm(
  entry: PendingPluginConfirm,
  resolve: (action: "accepted" | "cancelled") => void,
) {
  confirms = [...confirms, { ...entry, resolve }];
  emit();
}

/** Responde e retira a pergunta. Um id desconhecido (já respondido) é ignorado. */
export function answerPendingPluginConfirm(id: string, action: "accepted" | "cancelled") {
  const entry = confirms.find((item) => item.id === id);
  if (!entry) return;
  confirms = confirms.filter((item) => item.id !== id);
  emit();
  entry.resolve(action);
}

/**
 * Acrescenta um aviso ao sino. Um id já presente é ignorado; um id já lido
 * antes (outra visita à página) entra como lido. Devolve `true` quando o aviso
 * é novo e ainda não lido.
 */
export function addPluginNotice(notice: Omit<PluginNotice, "read">): boolean {
  if (notices.some((item) => item.id === notice.id)) return false;
  const read = readIds().includes(notice.id);
  notices = [{ ...notice, read }, ...notices].slice(0, MAX_NOTICES);
  emit();
  return !read;
}

/** Marca todos os avisos como lidos (o sino foi aberto e eles foram vistos). */
export function markPluginNoticesRead() {
  const unread = notices.filter((notice) => !notice.read);
  if (unread.length === 0) return;
  rememberRead(unread.map((notice) => notice.id));
  notices = notices.map((notice) => (notice.read ? notice : { ...notice, read: true }));
  emit();
}

/** Tira um aviso do sino. */
export function dismissPluginNotice(id: string) {
  const notice = notices.find((item) => item.id === id);
  if (!notice) return;
  rememberRead([id]);
  notices = notices.filter((item) => item.id !== id);
  emit();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function current() {
  return snapshot;
}

export function pendingPluginConfirms(): readonly PendingPluginConfirm[] {
  return snapshot.confirms;
}

export function pluginNotices(): readonly PluginNotice[] {
  return snapshot.notices;
}

export function usePendingPluginConfirms(): readonly PendingPluginConfirm[] {
  return useSyncExternalStore(subscribe, current, current).confirms;
}

export function usePluginNotices(): readonly PluginNotice[] {
  return useSyncExternalStore(subscribe, current, current).notices;
}

/** Só para testes. */
export function resetPluginInboxForTest() {
  confirms = [];
  notices = [];
  try {
    localStorage.removeItem(READ_KEY);
  } catch {
    // ignore
  }
  emit();
}
