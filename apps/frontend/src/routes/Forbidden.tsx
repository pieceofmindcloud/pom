import { ShieldOff } from "lucide-react";
import { Link } from "react-router-dom";
import { Button, Empty, Panel } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { useProfile } from "../lib/profile";

/**
 * A tela que este perfil não abre.
 *
 * Não é a mesma coisa que `NotFound`: o endereço existe e funciona, o que falta
 * é o perfil. Dizer "não encontrado" mandaria o operador procurar um defeito de
 * rota que não há - então a tela diz qual perfil está ativo e que o caminho é
 * trocar de perfil, não corrigir o endereço.
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
