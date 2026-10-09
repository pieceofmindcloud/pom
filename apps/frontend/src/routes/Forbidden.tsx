import { ShieldOff } from "lucide-react";
import { Link } from "react-router-dom";
import { Button, Empty, Panel } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { useProfile } from "../lib/profile";

/**
 * The screen this profile does not open.
 *
 * Not the same thing as `NotFound`: the address exists and works, what is missing
 * is the profile. Saying "not found" would send the operator looking for a route
 * defect that is not there - so the screen says which profile is active and that
 * the way forward is to switch profile, not to fix the address.
 */
export function Forbidden() {
  const { t } = useI18n();
  const { profile } = useProfile();
  return (
    <Panel>
      <Empty
        icon={<ShieldOff className="h-5 w-5" />}
        title={t("forbidden.title")}
        action={
          <Link to="/">
            <Button variant="primary">{t("forbidden.back")}</Button>
          </Link>
        }
      >
        {t("forbidden.detail", { profile: t(`profile.${profile}`) })}
      </Empty>
    </Panel>
  );
}
