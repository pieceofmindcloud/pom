/**
 * Sales page for the Statistics and Logs plugin (`enterprise`), shown at
 * `/logs` and `/statistics` while no active plugin provides those screens.
 *
 * It follows the pieceofmind.cloud look (dark floating hero, navy -> teal
 * gradient, numbered mono eyebrows, round cards, pill buttons) and reads what
 * the license server publishes in its public catalog: name, description,
 * icon, the official screenshots and the price, formatted in the interface's
 * language. "Buy" opens the site's purchase wizard (`/comprar/`, Paddle) in a
 * new browser tab with the plugin, the billing period and the language, so a
 * desktop shell hands it to the system browser and POM keeps running. One
 * purchase brings Request Logs, Statistics and Quota, and the plugin matters
 * most to the network's admins. Without the catalog, the page keeps working
 * from its own copy and drawn previews.
 */
import { ArrowRight, BarChart3, Check, ChevronLeft, ChevronRight, CreditCard, Gauge, KeyRound, ListTree, Lock, ShieldCheck, UserCog, X, Zap } from "lucide-react";
import { useEffect, useRef, useState, type CSSProperties } from "react";
import { Link } from "react-router-dom";
import { usePublicPluginCatalog } from "../lib/queries";
import type { PublicPluginPrice } from "../lib/pluginCatalog";
import { useI18n } from "../i18n/provider";
import { navigateToPluginPurchase, publicPluginPurchaseUrl } from "../lib/pluginPurchase";

const PLUGIN_CODE = "logs";

type ShowcaseKind = "logs" | "statistics";
type Interval = "month" | "year";

function LogsPreview() {
  const rows = [0, 1, 2, 3, 4, 5];
  return (
    <svg viewBox="0 0 320 180" className="h-full w-full" role="presentation" aria-hidden="true">
      <rect x="0" y="0" width="320" height="180" rx="10" fill="#0d1a2a" />
      <rect x="0" y="0" width="320" height="28" rx="10" fill="#132436" />
      {[0, 1, 2, 3, 4].map((index) => (
        <rect key={index} x={12 + index * 62} y="11" width="44" height="6" rx="3" fill="rgba(255,255,255,.28)" />
      ))}
      {rows.map((row) => (
        <g key={row}>
          <rect x="10" y={38 + row * 23} width="36" height="6" rx="3" fill="rgba(255,255,255,.45)" />
          <rect x="72" y={38 + row * 23} width="52" height="6" rx="3" fill="rgba(255,255,255,.3)" />
          <rect x="134" y={38 + row * 23} width="44" height="6" rx="3" fill="rgba(255,255,255,.3)" />
          <rect x="196" y={38 + row * 23} width="36" height="6" rx="3" fill="rgba(255,255,255,.3)" />
          <rect x="258" y={36 + row * 23} width="46" height="10" rx="5" fill={row === 2 ? "#ff7a8a" : "#2fdcc8"} opacity=".7" />
        </g>
      ))}
    </svg>
  );
}

function StatisticsPreview() {
  const bars = [38, 70, 48, 92, 60, 34, 80];
  return (
    <svg viewBox="0 0 320 180" className="h-full w-full" role="presentation" aria-hidden="true">
      <defs>
        <linearGradient id="pom-store-bar" x1="0" y1="1" x2="0" y2="0">
          <stop offset="0" stopColor="#4f9bff" />
          <stop offset="1" stopColor="#2fdcc8" />
        </linearGradient>
      </defs>
      <rect x="0" y="0" width="320" height="180" rx="10" fill="#0d1a2a" />
      {bars.map((height, index) => (
        <rect key={index} x={18 + index * 30} y={156 - height} width="18" height={height} rx="4" fill="url(#pom-store-bar)" opacity=".85" />
      ))}
      <line x1="10" y1="156" x2="232" y2="156" stroke="rgba(255,255,255,.18)" />
      <circle cx="278" cy="62" r="28" fill="none" stroke="rgba(255,255,255,.12)" strokeWidth="11" />
      <circle cx="278" cy="62" r="28" fill="none" stroke="#35c9f0" strokeWidth="11" strokeDasharray="120 176" transform="rotate(-90 278 62)" />
      <rect x="252" y="118" width="52" height="6" rx="3" fill="rgba(255,255,255,.4)" />
      <rect x="252" y="132" width="36" height="6" rx="3" fill="rgba(255,255,255,.25)" />
    </svg>
  );
}

function formatPrice(price: PublicPluginPrice | null | undefined, locale: string): string | null {
  if (!price || price.amount === null || price.amount === undefined || !price.currency) return null;
  try {
    return new Intl.NumberFormat(locale, { style: "currency", currency: price.currency.toUpperCase() }).format(price.amount / 100);
  } catch {
    return null;
  }
}

const stagger = (index: number) => ({ "--i": index }) as CSSProperties;

export function EnterpriseFeatureShowcase({ kind }: { kind: ShowcaseKind }) {
  const { t, locale } = useI18n();
  const catalog = usePublicPluginCatalog();
  const entry = catalog.data?.plugins.find((plugin) => plugin.code === PLUGIN_CODE) ?? null;
  const monthly = entry?.pricing.monthly ?? null;
  const annual = entry?.pricing.annual ?? null;
  const intervals = ([monthly ? "month" : null, annual ? "year" : null].filter(Boolean) as Interval[]);
  const [interval, setInterval] = useState<Interval>("month");
  useEffect(() => {
    if (intervals.length > 0 && !intervals.includes(interval)) setInterval(intervals[0]);
  }, [intervals.join(","), interval]);
  const price = interval === "year" ? annual : monthly;
  const priceLabel = formatPrice(price, locale);
  const name = entry?.display_name || t("enterprise.store.name");
  const description = locale === "pt-BR"
    ? entry?.description_pt_br || t("enterprise.store.description")
    : entry?.description || t("enterprise.store.description");
  const screenshots = entry?.screenshot_urls ?? [];

  const storeUrl = publicPluginPurchaseUrl(PLUGIN_CODE, { interval, locale });
  const buy = () => navigateToPluginPurchase(PLUGIN_CODE, { interval, locale });

  const [shot, setShot] = useState<number | null>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (shot !== null && !dialog.open) {
      if (typeof dialog.showModal === "function") dialog.showModal();
      else dialog.setAttribute("open", "");
    } else if (shot === null && dialog.open) {
      if (typeof dialog.close === "function") dialog.close();
      else dialog.removeAttribute("open");
    }
  }, [shot]);
  const move = (delta: number) => setShot((current) => current === null ? current : (current + delta + screenshots.length) % screenshots.length);

  const focus = kind === "logs" ? ["logs", "statistics", "quota"] as const : ["statistics", "logs", "quota"] as const;
  const buyButton = (className = "") => (
    <button type="button" className={`pom-store-buy ${className}`} onClick={buy}>
      <CreditCard className="h-4 w-4" aria-hidden="true" />
      {t("enterprise.store.buy")}
    </button>
  );

  return (
    <main className="pom-store" data-kind={kind}>
      <section className="pom-store-hero">
        <div className="pom-store-hero-copy">
          <p className="pom-store-eyebrow pom-store-eyebrow-hero"><span className="pom-store-tick" aria-hidden="true" />{t("enterprise.store.eyebrow")}</p>
          <h1 className="pom-store-title">
            <span>{t(`enterprise.store.${kind}.titleA`)}</span>
            <span className="pom-store-gradient">{t(`enterprise.store.${kind}.titleB`)}</span>
          </h1>
          <p className="pom-store-lede">{t(`enterprise.store.${kind}.lede`)}</p>
          <ul className="pom-store-badges">
            <li><Lock className="h-3.5 w-3.5" aria-hidden="true" />{t("enterprise.store.badgeLocal")}</li>
            <li><Zap className="h-3.5 w-3.5" aria-hidden="true" />{t("enterprise.store.badgeInstant")}</li>
            <li><ShieldCheck className="h-3.5 w-3.5" aria-hidden="true" />{t("enterprise.store.badgePaddle")}</li>
          </ul>
          <p className="pom-store-admins"><UserCog className="h-3.5 w-3.5" aria-hidden="true" />{t("enterprise.store.forAdmins")}</p>
        </div>

        <aside className="pom-store-price" aria-label={t("enterprise.store.priceLabel")}>
          <div className="pom-store-product">
            {entry?.icon_url ? (
              <img src={entry.icon_url} alt="" referrerPolicy="no-referrer" className="pom-store-icon" />
            ) : (
              <span className="pom-store-icon pom-store-icon-fallback" aria-hidden="true"><BarChart3 className="h-5 w-5" /></span>
            )}
            <div>
              <p className="pom-store-product-name">{name}</p>
              <p className="pom-store-product-code">plugin · {PLUGIN_CODE}</p>
            </div>
          </div>
          {intervals.length > 1 && (
            <div className="pom-store-intervals" role="group" aria-label={t("enterprise.store.interval")}>
              {intervals.map((value) => (
                <button key={value} type="button" aria-pressed={interval === value} onClick={() => setInterval(value)}>
                  {t(`enterprise.store.interval.${value}`)}
                </button>
              ))}
            </div>
          )}
          <p className="pom-store-amount">
            {priceLabel ?? t("enterprise.store.priceAtCheckout")}
            {priceLabel && <span>{t(`enterprise.store.per.${interval}`)}</span>}
          </p>
          {buyButton("pom-store-buy-wide")}
          <p className="pom-store-opens">{t("enterprise.store.opensInBrowser")}</p>
          <ul className="pom-store-included">
            {["included1", "included5", "included2", "included3", "included4"].map((key) => (
              <li key={key}><Check className="h-3.5 w-3.5" aria-hidden="true" />{t(`enterprise.store.${key}`)}</li>
            ))}
          </ul>
          <Link className="pom-store-have" to="/plugins">
            <KeyRound className="h-3.5 w-3.5" aria-hidden="true" />{t("enterprise.store.haveKey")}
          </Link>
        </aside>
      </section>

      <section className="pom-store-section" aria-labelledby="pom-store-shots">
        <p className="pom-store-eyebrow"><span className="pom-store-tick" aria-hidden="true" />01 / {t("enterprise.store.shotsLabel")}</p>
        <h2 id="pom-store-shots" className="pom-store-h2">{t("enterprise.store.shotsTitle")}</h2>
        {screenshots.length > 0 ? (
          <div className="pom-store-gallery">
            {screenshots.slice(0, 6).map((url, index) => (
              <button
                key={url}
                type="button"
                className={index === 0 ? "pom-store-shot pom-store-shot-main" : "pom-store-shot"}
                style={stagger(index)}
                onClick={() => setShot(index)}
                aria-label={t("enterprise.store.shotOpen", { number: index + 1 })}
              >
                <img src={url} alt={t("enterprise.store.shotAlt", { name, number: index + 1 })} loading="lazy" referrerPolicy="no-referrer" />
              </button>
            ))}
          </div>
        ) : (
          <div className="pom-store-gallery pom-store-gallery-drawn">
            <div className="pom-store-shot pom-store-shot-main" aria-label={t("enterprise.showcase.logs.screenshotAlt")} role="img"><LogsPreview /></div>
            <div className="pom-store-shot" aria-label={t("enterprise.showcase.statistics.screenshotAlt")} role="img"><StatisticsPreview /></div>
          </div>
        )}
      </section>

      <section className="pom-store-section" aria-labelledby="pom-store-features">
        <p className="pom-store-eyebrow"><span className="pom-store-tick" aria-hidden="true" />02 / {t("enterprise.store.featuresLabel")}</p>
        <h2 id="pom-store-features" className="pom-store-h2">{t("enterprise.store.featuresTitle")}</h2>
        <div className="pom-store-pair">
          {focus.map((part, partIndex) => (
            <article key={part} className={partIndex === 0 ? "pom-store-card pom-store-card-accent" : "pom-store-card"} style={stagger(partIndex)}>
              <span className="pom-store-card-icon" aria-hidden="true">{part === "logs" ? <ListTree className="h-5 w-5" /> : part === "quota" ? <Gauge className="h-5 w-5" /> : <BarChart3 className="h-5 w-5" />}</span>
              <h3 className="pom-store-h3">{t(`enterprise.showcase.${part}.title`)}</h3>
              <p>{t(`enterprise.store.${part}.summary`)}</p>
              <ul className="pom-store-list">
                {[1, 2, 3, 4, 5].map((index) => (
                  <li key={index}><Check className="h-4 w-4" aria-hidden="true" />{t(`enterprise.store.${part}.feature${index}`)}</li>
                ))}
              </ul>
            </article>
          ))}
        </div>
        <p className="pom-store-about">{description}</p>
      </section>

      <section className="pom-store-section" aria-labelledby="pom-store-steps">
        <p className="pom-store-eyebrow"><span className="pom-store-tick" aria-hidden="true" />03 / {t("enterprise.store.stepsLabel")}</p>
        <h2 id="pom-store-steps" className="pom-store-h2">{t("enterprise.store.stepsTitle")}</h2>
        <ol className="pom-store-steps">
          {[1, 2, 3].map((index) => (
            <li key={index} className="pom-store-step" style={stagger(index)}>
              <span className="pom-store-step-num" aria-hidden="true">{index}</span>
              <div>
                <h3 className="pom-store-h3">{t(`enterprise.store.step${index}.title`)}</h3>
                <p>{t(`enterprise.store.step${index}.body`)}</p>
              </div>
            </li>
          ))}
        </ol>
      </section>

      <section className="pom-store-section" aria-labelledby="pom-store-faq">
        <p className="pom-store-eyebrow"><span className="pom-store-tick" aria-hidden="true" />04 / {t("enterprise.store.faqLabel")}</p>
        <h2 id="pom-store-faq" className="pom-store-h2">{t("enterprise.store.faqTitle")}</h2>
        <div className="pom-store-faq">
          {[1, 2, 3, 4].map((index) => (
            <details key={index} className="pom-store-card">
              <summary>{t(`enterprise.store.faq${index}.q`)}</summary>
              <p>{t(`enterprise.store.faq${index}.a`)}</p>
            </details>
          ))}
        </div>
      </section>

      <section className="pom-store-cta">
        <div>
          <h2 className="pom-store-h2">{t("enterprise.store.ctaTitle")}</h2>
          <p>{priceLabel ? t("enterprise.store.ctaPrice", { price: `${priceLabel}${t(`enterprise.store.per.${interval}`)}` }) : t("enterprise.store.ctaBody")}</p>
        </div>
        {buyButton()}
        <a className="pom-store-site" href={storeUrl} target="_blank" rel="noopener noreferrer">
          {t("enterprise.store.moreOnSite")}<ArrowRight className="h-3.5 w-3.5" aria-hidden="true" />
        </a>
      </section>

      {screenshots.length > 0 && (
        <dialog
          ref={dialogRef}
          className="pom-store-lightbox"
          aria-label={name}
          onCancel={(event) => {
            event.preventDefault();
            setShot(null);
          }}
          onKeyDown={(event) => {
            if (event.key === "ArrowLeft") move(-1);
            if (event.key === "ArrowRight") move(1);
          }}
        >
          {shot !== null && (
            <img src={screenshots[shot]} alt={t("enterprise.store.shotAlt", { name, number: shot + 1 })} referrerPolicy="no-referrer" />
          )}
          <button type="button" className="pom-store-lightbox-close" aria-label={t("plugins.screenshotClose")} onClick={() => setShot(null)}>
            <X className="h-5 w-5" aria-hidden="true" />
          </button>
          {screenshots.length > 1 && (
            <>
              <button type="button" className="pom-store-lightbox-prev" aria-label={t("plugins.screenshotPrevious")} onClick={() => move(-1)}>
                <ChevronLeft className="h-5 w-5" aria-hidden="true" />
              </button>
              <button type="button" className="pom-store-lightbox-next" aria-label={t("plugins.screenshotNext")} onClick={() => move(1)}>
                <ChevronRight className="h-5 w-5" aria-hidden="true" />
              </button>
            </>
          )}
        </dialog>
      )}
    </main>
  );
}
