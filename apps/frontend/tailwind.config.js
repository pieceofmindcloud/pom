/**
 * CODAI-SERVER design tokens.
 *
 * Every colour is a CSS variable so one token table drives both themes; the
 * values live in `src/index.css`. Tailwind only names them here.
 */
/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  darkMode: ["class", '[data-theme="dark"]'],
  theme: {
    extend: {
      colors: {
        ink: "rgb(var(--ink) / <alpha-value>)",
        surface: "rgb(var(--surface) / <alpha-value>)",
        raised: "rgb(var(--raised) / <alpha-value>)",
        line: "rgb(var(--line) / <alpha-value>)",
        "line-strong": "rgb(var(--line-strong) / <alpha-value>)",
        fg: "rgb(var(--fg) / <alpha-value>)",
        muted: "rgb(var(--muted) / <alpha-value>)",
        faint: "rgb(var(--faint) / <alpha-value>)",
        iris: "rgb(var(--iris) / <alpha-value>)",
        aqua: "rgb(var(--aqua) / <alpha-value>)",
        amber: "rgb(var(--amber) / <alpha-value>)",
        relay: "rgb(var(--relay) / <alpha-value>)",
        rose: "rgb(var(--rose) / <alpha-value>)",
        sky: "rgb(var(--sky) / <alpha-value>)",
        lime: "rgb(var(--lime) / <alpha-value>)",
      },
      fontFamily: {
        display: ["Space Grotesk Variable", "Space Grotesk", "system-ui", "sans-serif"],
        sans: ["IBM Plex Sans", "system-ui", "sans-serif"],
        mono: ["JetBrains Mono Variable", "JetBrains Mono", "ui-monospace", "monospace"],
      },
      fontSize: {
        "2xs": ["0.6875rem", { lineHeight: "1rem", letterSpacing: "0.04em" }],
      },
      borderRadius: { xl: "0.875rem", "2xl": "1.125rem" },
      boxShadow: {
        panel: "0 1px 2px rgb(0 0 0 / 0.04), 0 8px 32px -16px rgb(0 0 0 / 0.28)",
        lift: "0 2px 4px rgb(0 0 0 / 0.06), 0 18px 48px -20px rgb(0 0 0 / 0.42)",
      },
      transitionTimingFunction: {
        swift: "cubic-bezier(0.22, 1, 0.36, 1)",
      },
      keyframes: {
        "flow-dash": { to: { strokeDashoffset: "-24" } },
        shimmer: { "100%": { transform: "translateX(100%)" } },
        "pulse-soft": {
          "0%, 100%": { opacity: "1" },
          "50%": { opacity: "0.45" },
        },
      },
      animation: {
        "flow-dash": "flow-dash 1.1s linear infinite",
        shimmer: "shimmer 1.6s infinite",
        "pulse-soft": "pulse-soft 2s ease-in-out infinite",
      },
    },
  },
  plugins: [],
};
