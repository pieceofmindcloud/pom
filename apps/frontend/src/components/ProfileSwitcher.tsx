import clsx from "clsx";
import { FlaskConical } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { PROFILES, PROFILE_SWITCHER_ENABLED, useProfile } from "../lib/profile";

/**
 * Trocar de perfil sem sair da tela - andaime de desenvolvimento.
 *
 * Não é um controle do produto: existe para conferir, lado a lado, o que cada
 * perfil vê. Por isso a borda tracejada e o âmbar do resto da interface reserva
 * para "atenção": um controle que muda o que a tela mostra e **não** é uma
 * decisão de operação precisa parecer diferente de um que é, ou alguém vai
 * usá-lo achando que trocou de permissão de verdade. Não troca: ver
 * `lib/profile.tsx`.
 *
 * `PROFILE_SWITCHER_ENABLED` false apaga o controle. Quando o perfil passar a
 * chegar do nó, este arquivo inteiro sai.
 */
export function ProfileSwitcher() {
  const { t } = useI18n();
  const { profile, setProfile } = useProfile();

  if (!PROFILE_SWITCHER_ENABLED) return null;

  return (
    <div
      role="group"
      aria-label={t("profile.switcher")}
      title={t("profile.switcherHint")}
      className="flex h-9 shrink-0 items-center gap-1 rounded-lg border border-dashed border-amber/45 bg-amber/[0.06] px-1"
    >
      <FlaskConical
        className="ml-1 h-3.5 w-3.5 shrink-0 text-amber"
        aria-hidden="true"
      />
      {PROFILES.map((id) => (
        <button
          key={id}
          type="button"
          aria-pressed={profile === id}
          onClick={() => setProfile(id)}
          className={clsx(
            "rounded-md px-2 py-1 text-[0.78rem] transition-colors duration-150",
            profile === id
              ? "bg-iris/15 text-fg ring-1 ring-inset ring-iris/30"
              : "text-muted hover:text-fg",
          )}
        >
          {t(`profile.${id}`)}
        </button>
      ))}
    </div>
  );
}
