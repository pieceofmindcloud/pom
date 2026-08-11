import { Compass } from "lucide-react";
import { Link } from "react-router-dom";
import { Button, Empty, Panel } from "../components/ui";
import { useI18n } from "../i18n/provider";

export function NotFound() {
  const { t } = useI18n();
  return (
    <Panel>
      <Empty
        icon={<Compass className="h-5 w-5" />}
        title={t("notFound.title")}
        action={
          <Link to="/">
            <Button variant="primary">{t("notFound.back")}</Button>
          </Link>
        }
      >
        {t("notFound.detail")}
      </Empty>
    </Panel>
  );
}
