/**
 * The authority state of the network and the actions this viewer may perform.
 *
 * This page deliberately consumes a presentation contract. Grant storage,
 * signatures and the initial trust root belong to the node, not to the
 * browser.
 */
import { ClusterVersion } from "../components/ClusterVersion";
import { ShieldCheck } from "lucide-react";
import { useState } from "react";
import { AdminAuthorityPanel, type AdminAuthorityAction } from "../components/AdminAuthorityPanel";
import { ErrorState, PageHeader, Skeleton } from "../components/ui";
import { adminAuthorityApi } from "../lib/adminAuthority";
import { useAdminAuthority, useAdminNodes, useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";

function errorMessage(error: unknown, fallback: string): string {
  return error instanceof Error && error.message ? error.message : fallback;
}

export function AdminAuthority() {
  const { t } = useI18n();
  const authority = useAdminAuthority();
  const isPrincipal = authority.data?.viewer === "principal";
  const nodes = useAdminNodes(isPrincipal);
  const invalidate = useInvalidate();
  const toast = useToast();
  const [busyAction, setBusyAction] = useState<AdminAuthorityAction>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  async function runAction(
    action: Exclude<AdminAuthorityAction, null>,
    request: () => Promise<unknown>,
    success: () => void,
  ) {
    setBusyAction(action);
    setActionError(null);
    try {
      await request();
      success();
      invalidate();
      void authority.refetch();
      void nodes.refetch();
    } catch (error) {
      setActionError(errorMessage(error, t("adminAuthority.actionFailed")));
    } finally {
      setBusyAction(null);
    }
  }

  if (authority.isLoading) {
    return (
      <>
        <PageHeader title={t("adminAuthority.pageTitle")} subtitle={t("adminAuthority.pageSubtitle")} />
        <div className="space-y-5" aria-label={t("adminAuthority.pageLoading")}>
          <Skeleton className="h-56 w-full" />
          <Skeleton className="h-44 w-full" />
        </div>
      </>
    );
  }

  if (authority.isError || !authority.data) {
    return (
      <>
        <PageHeader title={t("adminAuthority.pageTitle")} subtitle={t("adminAuthority.pageSubtitle")} />
        <ErrorState
          title={t("adminAuthority.pageFailed")}
          error={authority.error}
          onRetry={() => void authority.refetch()}
        />
      </>
    );
  }

  const authorityState = authority.data;
  return (
    <>
      <PageHeader
        title={t("adminAuthority.pageTitle")}
        subtitle={t("adminAuthority.pageSubtitle")}
        actions={
          <div className="hidden items-center gap-2 text-xs text-faint sm:flex">
            <ShieldCheck className="h-3.5 w-3.5 text-iris" aria-hidden="true" />
            {t(`profile.${authorityState.viewer === "member" ? "worker" : "admin"}`)}
          </div>
        }
      />
      <div className="mb-5">
        <ClusterVersion />
      </div>
      {nodes.isError && isPrincipal && (
        <div className="mb-5">
          <ErrorState error={nodes.error} onRetry={() => void nodes.refetch()} />
        </div>
      )}
      <AdminAuthorityPanel
        authority={authorityState}
        candidates={nodes.data ?? []}
        busyAction={busyAction}
        actionError={actionError}
        onPromote={(nodeId) =>
          void runAction(
            "promote",
            () => adminAuthorityApi.promoteNode(nodeId),
            () => toast.success(t("network.nodePromoted"), t("network.nodePromotedDetail")),
          )
        }
        onResign={() =>
          void runAction(
            "resign",
            () => adminAuthorityApi.resign(),
            () => toast.success(t("adminAuthority.resignationSent"), t("adminAuthority.resignationSentDetail")),
          )
        }
        onDemote={(nodeId) =>
          void runAction(
            `demote:${nodeId}`,
            () => adminAuthorityApi.demoteSecondary(nodeId),
            () => toast.success(t("adminAuthority.demoted"), t("adminAuthority.demotedDetail")),
          )
        }
      />
    </>
  );
}
