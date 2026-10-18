import {
  ArrowRight,
  ArrowRightLeft,
  Crown,
  History,
  ShieldCheck,
  UserMinus,
  Users,
} from "lucide-react";
import { useState } from "react";
import { useI18n } from "../i18n/provider";
import type { AdminAuthorityMember, AdminAuthorityView, AdminNode } from "../lib/types";
import { Confirm } from "./Confirm";
import {
  Badge,
  Button,
  Empty,
  Footnote,
  KeyValue,
  Mono,
  Panel,
  PanelHeader,
  Select,
} from "./ui";

export type AdminAuthorityAction = "promote" | "resign" | `demote:${string}` | null;

export interface AdminAuthorityPanelProps {
  authority: AdminAuthorityView;
  candidates: AdminNode[];
  busyAction: AdminAuthorityAction;
  actionError?: string | null;
  onPromote: (nodeId: string) => void;
  onResign: () => void;
  onDemote: (nodeId: string) => void;
}

function memberLabel(member: AdminAuthorityMember): string {
  return member.hostname || member.short_id;
}

function MemberStatus({ active }: { active: boolean }) {
  const { t } = useI18n();
  return (
    <Badge tone={active ? "aqua" : "neutral"}>
      {active ? t("adminAuthority.active") : t("adminAuthority.inactive")}
    </Badge>
  );
}

function MemberRow({
  member,
  label,
  action,
}: {
  member: AdminAuthorityMember;
  label: string;
  action?: React.ReactNode;
}) {
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2 px-5 py-3.5">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-iris">
        <Users className="h-4 w-4" aria-hidden="true" />
      </div>
      <div className="min-w-[10rem] flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-medium text-fg">{memberLabel(member)}</span>
          <Badge tone="neutral">{label}</Badge>
          <MemberStatus active={member.active} />
        </div>
        <Mono className="mt-0.5 block text-faint">{member.short_id}</Mono>
      </div>
      {action}
    </div>
  );
}

export function AdminAuthorityPanel({
  authority,
  candidates,
  busyAction,
  actionError,
  onPromote,
  onResign,
  onDemote,
}: AdminAuthorityPanelProps) {
  const { t } = useI18n();
  const [promotionTarget, setPromotionTarget] = useState("");
  const [promotionConfirm, setPromotionConfirm] = useState<AdminNode | null>(null);
  const [demotionConfirm, setDemotionConfirm] = useState<AdminAuthorityMember | null>(null);
  const [resignationConfirm, setResignationConfirm] = useState(false);

  const isPrincipal = authority.viewer === "principal";
  const canChangeAdmins = isPrincipal && authority.permissions.promote;
  const canResign = isPrincipal && authority.permissions.resign && !authority.pending_transfer;
  const visibleCandidates = candidates.filter(
    (candidate) =>
      candidate.connected &&
      !candidate.blocked &&
      // An inference node's role is set at admission; the node refuses too.
      candidate.profile !== "visitor" &&
      candidate.node_id !== authority.principal.node_id &&
      !authority.secondary.some((admin) => admin.node_id === candidate.node_id),
  );
  const selectedCandidate = visibleCandidates.find(
    (candidate) => candidate.node_id === promotionTarget,
  );
  return (
    <div className="space-y-5">
      <Panel>
        <PanelHeader
          eyebrow={t("adminAuthority.eyebrow")}
          title={t("adminAuthority.title")}
          subtitle={t("adminAuthority.subtitle")}
        />
        <div className="border-b border-line">
          <MemberRow
            member={authority.principal}
            label={t("adminAuthority.principal")}
            action={
              <Badge tone="iris" className="ml-auto">
                <Crown className="mr-1 h-3 w-3" aria-hidden="true" />
                {t("adminAuthority.finalAuthority")}
              </Badge>
            }
          />
        </div>

        {authority.secondary.length === 0 ? (
          <Empty icon={<Users className="h-5 w-5" />} title={t("adminAuthority.noSecondary")}>
            {t("adminAuthority.noSecondaryDetail")}
          </Empty>
        ) : (
          <div className="divide-y divide-line">
            {authority.secondary.map((admin) => (
              <MemberRow
                key={admin.node_id}
                member={admin}
                label={t("adminAuthority.secondary")}
                action={
                  authority.permissions.demote && isPrincipal ? (
                    <Button
                      size="sm"
                      variant="danger"
                      icon={<UserMinus className="h-3.5 w-3.5" aria-hidden="true" />}
                      onClick={() => setDemotionConfirm(admin)}
                      loading={busyAction === `demote:${admin.node_id}`}
                      aria-label={t("adminAuthority.demoteAction", {
                        hostname: memberLabel(admin),
                      })}
                    >
                      {t("adminAuthority.demote")}
                    </Button>
                  ) : undefined
                }
              />
            ))}
          </div>
        )}

        <Footnote className="border-t border-line px-5 py-3" icon={<ShieldCheck className="h-3.5 w-3.5" />}>
          {isPrincipal
            ? t("adminAuthority.principalCanChange")
            : authority.viewer === "admin"
              ? t("adminAuthority.secondaryReadOnly")
              : t("adminAuthority.nodeReadOnly")}
        </Footnote>
      </Panel>

      <Panel>
        <PanelHeader
          title={t("adminAuthority.chainTitle")}
          subtitle={t("adminAuthority.chainSubtitle")}
        />
        {authority.grants.length === 0 ? (
          <Empty icon={<History className="h-5 w-5" />} title={t("adminAuthority.chainEmpty")} />
        ) : (
          <div className="divide-y divide-line">
            {authority.grants.map((grant) => (
              <div
                key={`${grant.sequence}-${grant.node_id}`}
                className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2 px-5 py-3.5"
              >
                <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full bg-iris/12 font-mono text-xs text-iris">
                  {grant.sequence}
                </span>
                <div className="min-w-[12rem] flex-1">
                  <p className="text-sm text-fg">
                    {grant.promoted_by_hostname}
                    <ArrowRight className="mx-1.5 inline h-3.5 w-3.5 text-faint" aria-hidden="true" />
                    {grant.node_hostname}
                  </p>
                  <p className="mt-0.5 text-xs text-muted">
                    {t("adminAuthority.grantBy", { hostname: grant.promoted_by_hostname })}
                  </p>
                </div>
                <Mono className="text-faint">{grant.node_id.slice(0, 12)}</Mono>
              </div>
            ))}
          </div>
        )}
        <Footnote className="border-t border-line px-5 py-3" icon={<History className="h-3.5 w-3.5" />}>
          {t("adminAuthority.chainNote")}
        </Footnote>
      </Panel>

      {authority.pending_transfer && (
        <Panel>
          <PanelHeader
            title={t("adminAuthority.pendingTitle")}
            subtitle={t("adminAuthority.pendingSubtitle")}
          />
          <div className="flex items-start gap-3 px-5 py-4">
            <ArrowRightLeft className="mt-0.5 h-4 w-4 shrink-0 text-amber" aria-hidden="true" />
            <div className="min-w-0">
              <p className="font-medium text-fg">
                {t("adminAuthority.pendingTo", { hostname: authority.pending_transfer.target_host })}
              </p>
              <p className="mt-1 text-sm leading-relaxed text-muted">
                {t("adminAuthority.pendingNoChange", { hostname: memberLabel(authority.principal) })}
              </p>
            </div>
          </div>
        </Panel>
      )}

      {isPrincipal && (
        <Panel>
          <PanelHeader
            title={t("adminAuthority.actionsTitle")}
            subtitle={t("adminAuthority.actionsSubtitle")}
          />
          <div className="space-y-6 px-5 py-5">
            {canChangeAdmins && (
              <div className="space-y-3">
                <div>
                  <h3 className="text-sm font-semibold text-fg">{t("adminAuthority.promoteTitle")}</h3>
                  <p className="mt-1 text-sm leading-relaxed text-muted">
                    {t("adminAuthority.promoteSubtitle")}
                  </p>
                </div>
                {visibleCandidates.length === 0 ? (
                  <p className="rounded-lg border border-line bg-raised px-3 py-2.5 text-sm text-muted">
                    {t("adminAuthority.noPromotionCandidates")}
                  </p>
                ) : (
                  <div className="flex flex-col gap-2 sm:flex-row sm:items-end">
                    <label className="min-w-0 flex-1 text-sm text-muted">
                      <span className="mb-1.5 block">{t("adminAuthority.promotionTarget")}</span>
                      <Select
                        aria-label={t("adminAuthority.promotionTarget")}
                        value={promotionTarget}
                        onChange={(event) => setPromotionTarget(event.target.value)}
                      >
                        <option value="">{t("adminAuthority.chooseNode")}</option>
                        {visibleCandidates.map((candidate) => (
                          <option key={candidate.node_id} value={candidate.node_id}>
                            {candidate.hostname || candidate.short_id}
                          </option>
                        ))}
                      </Select>
                    </label>
                    <Button
                      variant="primary"
                      icon={<ShieldCheck className="h-3.5 w-3.5" aria-hidden="true" />}
                      disabled={!selectedCandidate}
                      loading={busyAction === "promote"}
                      onClick={() => selectedCandidate && setPromotionConfirm(selectedCandidate)}
                    >
                      {t("adminAuthority.promote")}
                    </Button>
                  </div>
                )}
              </div>
            )}

            {canResign && (
              <div className="border-t border-line pt-5">
                <div>
                  <h3 className="text-sm font-semibold text-fg">{t("adminAuthority.resignTitle")}</h3>
                  <p className="mt-1 text-sm leading-relaxed text-muted">
                    {t("adminAuthority.resignSubtitle")}
                  </p>
                </div>
                {authority.resignation.kind === "eligible" ? (
                  <div className="mt-3 rounded-lg border border-amber/30 bg-amber/10 px-3.5 py-3">
                    <KeyValue label={t("adminAuthority.successorLabel")}>
                      <span className="font-medium text-fg">
                        {memberLabel(authority.resignation.successor)}
                      </span>
                    </KeyValue>
                    <p className="mt-2 text-sm leading-relaxed text-muted">
                      {t("adminAuthority.successorRule")}
                    </p>
                    <Button
                      className="mt-3"
                      variant="danger"
                      icon={<ArrowRightLeft className="h-3.5 w-3.5" aria-hidden="true" />}
                      loading={busyAction === "resign"}
                      onClick={() => setResignationConfirm(true)}
                    >
                      {t("adminAuthority.resign")}
                    </Button>
                  </div>
                ) : (
                  <div role="alert" className="mt-3 rounded-lg border border-rose/30 bg-rose/10 px-3.5 py-3">
                    <p className="text-sm font-medium text-fg">{t("adminAuthority.resignRefused")}</p>
                    <p className="mt-1 text-sm leading-relaxed text-muted">
                      {authority.resignation.kind === "no_secondary"
                        ? t("adminAuthority.noSecondarySuccessor")
                        : t("adminAuthority.noActiveSuccessor")}
                    </p>
                  </div>
                )}
              </div>
            )}
          </div>
        </Panel>
      )}

      {actionError && (
        <p role="alert" className="break-words rounded-lg border border-rose/30 bg-rose/10 px-3.5 py-3 text-sm text-rose">
          {actionError}
        </p>
      )}

      <Confirm
        open={promotionConfirm !== null}
        title={t("adminAuthority.promoteConfirmTitle", {
          hostname: promotionConfirm?.hostname || promotionConfirm?.short_id || "",
        })}
        description={t("adminAuthority.promoteConfirmDescription")}
        confirmLabel={t("adminAuthority.promote")}
        busy={busyAction === "promote"}
        onConfirm={() => {
          if (promotionConfirm) onPromote(promotionConfirm.node_id);
          setPromotionConfirm(null);
        }}
        onCancel={() => setPromotionConfirm(null)}
      />
      <Confirm
        open={demotionConfirm !== null}
        title={t("adminAuthority.demoteConfirmTitle", {
          hostname: demotionConfirm ? memberLabel(demotionConfirm) : "",
        })}
        description={t("adminAuthority.demoteConfirmDescription")}
        confirmLabel={t("adminAuthority.demote")}
        tone="danger"
        busy={demotionConfirm ? busyAction === `demote:${demotionConfirm.node_id}` : false}
        onConfirm={() => {
          if (demotionConfirm) onDemote(demotionConfirm.node_id);
          setDemotionConfirm(null);
        }}
        onCancel={() => setDemotionConfirm(null)}
      />
      <Confirm
        open={resignationConfirm}
        title={t("adminAuthority.resignConfirmTitle", {
          hostname:
            authority.resignation.kind === "eligible"
              ? memberLabel(authority.resignation.successor)
              : "",
        })}
        description={t("adminAuthority.resignConfirmDescription", {
          hostname:
            authority.resignation.kind === "eligible"
              ? memberLabel(authority.resignation.successor)
              : "",
        })}
        confirmLabel={t("adminAuthority.resign")}
        tone="danger"
        busy={busyAction === "resign"}
        onConfirm={() => {
          onResign();
          setResignationConfirm(false);
        }}
        onCancel={() => setResignationConfirm(false)}
      />
    </div>
  );
}
