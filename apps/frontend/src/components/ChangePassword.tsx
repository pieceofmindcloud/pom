import { useState, type FormEvent } from "react";
import { ArrowLeft, Check, Eye, EyeOff, X } from "lucide-react";
import clsx from "clsx";
import { useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { api, ApiError, setAdminAccessToken } from "../lib/api";
import { keys, useAdminAccess } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { BrandLanguageSwitch } from "./BrandLanguageSwitch";
import pomMark from "../assets/pom-mark-dark.png";

/** The node's own rule (`admin_password_weakness`), checked here as the person types. */
export const PASSWORD_MIN_CHARS = 10;

export function passwordChecks(password: string) {
  const classes = [/\p{Ll}/u, /\p{Lu}/u, /[0-9]/, /[^\p{L}\p{N}]/u].filter((rule) => rule.test(password)).length;
  return {
    length: [...password].length >= PASSWORD_MIN_CHARS,
    mixed: classes >= 3,
    notDefault: password !== "admin",
  };
}

/**
 * The panel password, on a screen of its own in the wizard's look, for every
 * profile. While the first-access default is in force (`/api/access`
 * `default_password`) it only asks the new password twice; afterwards it asks
 * the current one too. The exposure of the interface is a separate setting,
 * on Preferences. Saved through `POST /api/ui/access/password`; the tab then
 * goes on with the new password.
 */
export function ChangePassword() {
  const { t } = useI18n();
  const navigate = useNavigate();
  const toast = useToast();
  const client = useQueryClient();
  const access = useAdminAccess();
  const isDefault = access.data?.default_password === true;

  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [shown, setShown] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const checks = passwordChecks(next);
  const strong = checks.length && checks.mixed && checks.notDefault;
  const matches = confirm.length > 0 && confirm === next;
  const ready = strong && matches && (isDefault || current.length > 0) && !access.isLoading;

  function leave() {
    if (window.history.length > 1) navigate(-1);
    else navigate("/", { replace: true });
  }

  async function onSubmit(event: FormEvent) {
    event.preventDefault();
    if (!ready || busy) return;
    setBusy(true);
    setError(null);
    try {
      await api.post("/api/ui/access/password", {
        ...(isDefault ? {} : { current_password: current }),
        new_password: next,
      });
      // The node ended every session with the old password: this tab signs
      // in again with the new one on its next request.
      setAdminAccessToken(next);
      await client.invalidateQueries({ queryKey: keys.adminAccess });
      void client.invalidateQueries({ queryKey: keys.adminAccessInfo });
      toast.success(t("password.saved"), t("password.savedDetail"));
      leave();
    } catch (e) {
      const code = e instanceof ApiError ? (e.body as { code?: string } | undefined)?.code : undefined;
      setError(
        code === "current_password"
          ? t("password.currentWrong")
          : code && ["too_short", "too_simple", "default_password"].includes(code)
            ? t("password.weak")
            : t("password.failed", { error: e instanceof Error ? e.message : String(e) }),
      );
    } finally {
      setBusy(false);
    }
  }

  const rules: Array<[boolean, string]> = [
    [checks.length, t("password.ruleLength", { count: PASSWORD_MIN_CHARS })],
    [checks.mixed, t("password.ruleMixed")],
    [checks.notDefault, t("password.ruleNotDefault")],
  ];

  return (
    <main className="pom-login pom-setup" data-testid="change-password">
      <BrandLanguageSwitch />
      <section className="pom-login-panel pom-setup-panel pom-setup-scope">
        <header className="pom-setup-brand">
          <img className="pom-login-mark pom-setup-mark" src={pomMark} alt="POM" width={815} height={320} />
          <p className="pom-login-sub pom-setup-sub">PIECE OF MIND</p>
        </header>
        <form onSubmit={(event) => void onSubmit(event)}>
          <div className="min-h-[200px]">
            <h1 className="font-display text-xl font-semibold text-fg">
              {t(isDefault ? "password.titleDefault" : "password.title")}
            </h1>
            <p className="mt-2 text-sm leading-relaxed text-muted">
              {t(isDefault ? "password.detailDefault" : "password.detail")}
            </p>

            {!isDefault && (
              <PasswordField
                id="password-current"
                label={t("password.current")}
                value={current}
                onChange={setCurrent}
                shown={shown}
                autoComplete="current-password"
                autoFocus
              />
            )}
            <PasswordField
              id="password-new"
              label={t("password.new")}
              value={next}
              onChange={setNext}
              shown={shown}
              autoComplete="new-password"
              autoFocus={isDefault}
            />
            <ul className="mt-2 grid gap-1 text-xs" data-testid="password-rules">
              {rules.map(([ok, text]) => (
                <li key={text} className={clsx("flex items-center gap-1.5", ok ? "text-aqua" : "text-faint")}>
                  {ok ? <Check className="h-3.5 w-3.5" /> : <X className="h-3.5 w-3.5" />}
                  {text}
                </li>
              ))}
            </ul>
            <PasswordField
              id="password-confirm"
              label={t("password.confirm")}
              value={confirm}
              onChange={setConfirm}
              shown={shown}
              autoComplete="new-password"
            />
            {confirm.length > 0 && !matches && (
              <p className="mt-1.5 text-xs text-rose" data-testid="password-mismatch">{t("password.mismatch")}</p>
            )}
            <button
              type="button"
              className="mt-3 inline-flex items-center gap-1.5 text-xs text-muted hover:text-fg"
              onClick={() => setShown((s) => !s)}
            >
              {shown ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
              {t(shown ? "password.hide" : "password.show")}
            </button>
            {error && <p className="pom-login-error mt-3" role="alert">{error}</p>}
          </div>
          <footer className="pom-setup-footer">
            <button type="button" className="pom-setup-ghost" onClick={leave} disabled={busy}>
              <ArrowLeft className="h-4 w-4" />
              {t(isDefault ? "password.later" : "common.cancel")}
            </button>
            <button type="submit" className="pom-login-button" disabled={busy || !ready}>
              {busy && <span className="pom-login-spinner" aria-hidden="true" />}
              {t("password.save")}
            </button>
          </footer>
        </form>
      </section>
    </main>
  );
}

function PasswordField({ id, label, value, onChange, shown, autoComplete, autoFocus }: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  shown: boolean;
  autoComplete: string;
  autoFocus?: boolean;
}) {
  return (
    <>
      <label className="mt-5 block text-xs font-semibold uppercase tracking-wider text-muted" htmlFor={id}>
        {label}
      </label>
      <input
        id={id}
        data-testid={id}
        type={shown ? "text" : "password"}
        className="pom-login-input mt-2 !text-left"
        value={value}
        autoComplete={autoComplete}
        autoFocus={autoFocus}
        spellCheck={false}
        onChange={(event) => onChange(event.target.value)}
      />
    </>
  );
}
