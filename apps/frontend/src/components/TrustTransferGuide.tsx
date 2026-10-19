/**
 * How a network travels from one machine to another, told once for both ends.
 *
 * "Share" (on the network's own machine) and "Add network" (on the machine that
 * wants it) are the two halves of the same trip: the trust document is
 * downloaded on one side, carried by the operator and chosen on the other. The
 * same drawing explains both, with "you are here" on the side the operator is
 * standing, so whoever learned one half already knows the other.
 *
 * The file holds no secret - it is the network's signed public anchor - but it
 * must arrive unaltered, which is why the text asks for a channel the operator
 * trusts. The node checks the signature when the file is added.
 */
import clsx from "clsx";
import { FileKey2, Monitor, Network } from "lucide-react";
import type { ReactNode } from "react";
import { Modal } from "./Confirm";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";

export type TrustTransferDirection = "share" | "add";

function MachineBox({ label, icon, here, hereLabel }: { label: string; icon: ReactNode; here: boolean; hereLabel: string }) {
  return (
    <div
      className={clsx(
        "relative flex w-28 shrink-0 flex-col items-center gap-1.5 rounded-xl border px-2 py-3 text-center sm:w-36",
        here ? "border-iris/60 bg-iris/10" : "border-line bg-raised/60",
      )}
    >
      {here && (
        <span className="absolute -top-2.5 rounded-full bg-iris px-2 py-0.5 text-[0.58rem] font-semibold uppercase tracking-[0.08em] text-white">
          {hereLabel}
        </span>
      )}
      <span className={clsx("flex h-9 w-9 items-center justify-center rounded-lg border", here ? "border-iris/50 text-iris" : "border-line text-muted")}>{icon}</span>
      <span className="text-[0.7rem] font-medium leading-tight text-fg">{label}</span>
    </div>
  );
}

/** The two machines with the file moving from the network's side to the other. */
export function TrustTransferDiagram({ direction }: { direction: TrustTransferDirection }) {
  const { t } = useI18n();
  return (
    <div className="flex items-center gap-2 rounded-xl border border-line bg-surface/60 px-3 py-5 sm:gap-3 sm:px-5" data-testid="trust-transfer-diagram">
      <MachineBox
        label={t(direction === "share" ? "trustGuide.thisMachine" : "trustGuide.networkMachine")}
        icon={<Network className="h-4 w-4" />}
        here={direction === "share"}
        hereLabel={t("trustGuide.youAreHere")}
      />
      <div className="relative h-10 min-w-0 flex-1" aria-hidden="true">
        <span className="absolute inset-x-0 top-1/2 border-t-2 border-dashed border-line-strong" />
        <span className="absolute right-0 top-1/2 h-2 w-2 -translate-y-1/2 rotate-45 border-r-2 border-t-2 border-line-strong" />
        <span className="trust-travel absolute top-1/2 -translate-x-1/2 -translate-y-1/2">
          <span className="flex items-center gap-1 rounded-md border border-aqua/50 bg-ink px-1.5 py-1 text-[0.6rem] font-medium text-aqua shadow-panel">
            <FileKey2 className="h-3.5 w-3.5" />
            <span className="hidden sm:inline">{t("trustGuide.file")}</span>
          </span>
        </span>
      </div>
      <MachineBox
        label={t(direction === "share" ? "trustGuide.otherMachine" : "trustGuide.thisMachine")}
        icon={<Monitor className="h-4 w-4" />}
        here={direction === "add"}
        hereLabel={t("trustGuide.youAreHere")}
      />
    </div>
  );
}

/**
 * The dialog behind "Share" and "Add network". `onAction` is the step the
 * operator takes here: download the file (share) or choose it (add).
 */
export function TrustTransferGuide({
  direction,
  open,
  onClose,
  onAction,
  busy = false,
}: {
  direction: TrustTransferDirection | null;
  open: boolean;
  onClose: () => void;
  onAction: () => void;
  busy?: boolean;
}) {
  const { t } = useI18n();
  const share = direction === "share";
  const steps = share
    ? [t("trustGuide.share1"), t("trustGuide.share2"), t("trustGuide.share3")]
    : [t("trustGuide.add1"), t("trustGuide.add2"), t("trustGuide.add3")];
  return (
    <Modal
      open={open && direction !== null}
      size="lg"
      title={t(share ? "trustGuide.shareTitle" : "trustGuide.addTitle")}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>
          <Button variant="primary" loading={busy} onClick={onAction}>
            {t(share ? "trustGuide.download" : "trustGuide.choose")}
          </Button>
        </>
      }
    >
      {direction && (
        <div className="space-y-4 text-fg">
          <p className="text-sm leading-relaxed text-muted">{t(share ? "trustGuide.shareIntro" : "trustGuide.addIntro")}</p>
          <TrustTransferDiagram direction={direction} />
          <ol className="space-y-2.5">
            {steps.map((step, index) => (
              <li key={index} className="flex gap-3 text-sm leading-relaxed">
                <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-iris/50 bg-iris/10 font-mono text-[0.7rem] text-iris">{index + 1}</span>
                <span className="pt-0.5">{step}</span>
              </li>
            ))}
          </ol>
        </div>
      )}
    </Modal>
  );
}
