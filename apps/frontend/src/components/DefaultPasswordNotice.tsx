import { ShieldAlert } from "lucide-react";
import { Link } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { useAdminAccess } from "../lib/queries";

/**
 * Faixa fixa enquanto a senha administrativa for a padrão do primeiro acesso.
 * O wizard não pergunta senha; é aqui que a pessoa é levada a trocá-la.
 */
export function DefaultPasswordNotice() {
  const { t } = useI18n();
  const access = useAdminAccess();
  if (!access.data?.default_password) return null;
  return (
    <div
      role="status"
      data-testid="default-password-notice"
      className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-amber/30 bg-amber/10 px-4 py-2 text-xs text-fg"
    >
      <ShieldAlert className="h-4 w-4 shrink-0 text-amber" />
      <span className="min-w-0 flex-1">{t("firstRun.defaultPasswordNotice")}</span>
      <Link to="/config/preferencias" className="shrink-0 font-semibold text-amber underline-offset-2 hover:underline">
        {t("firstRun.defaultPasswordChange")}
      </Link>
    </div>
  );
}
