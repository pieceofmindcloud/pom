import clsx from "clsx";

/**
 * Operating system marks for the platforms a plugin ships builds for. lucide
 * has no brand icons, so these are small inline drawings in `currentColor`;
 * each carries its name as the accessible label and the hover title.
 */
export function PlatformIcon({ os, label, className }: { os: string; label: string; className?: string }) {
  const shared = {
    viewBox: "0 0 24 24",
    role: "img",
    "aria-label": label,
    fill: "currentColor",
    className: clsx("h-3.5 w-3.5", className),
  } as const;
  switch (os) {
    case "windows":
      return (
        <svg {...shared}>
          <title>{label}</title>
          <path d="M3 5.2 10.4 4.2v7.2H3zM11.4 4.1 21 2.7v8.7h-9.6zM3 12.5h7.4v7.3L3 18.8zM11.4 12.5H21v8.8l-9.6-1.4z" />
        </svg>
      );
    case "macos":
      return (
        <svg {...shared}>
          <title>{label}</title>
          <path d="M16.5 12.6c0-2.4 2-3.5 2-3.6-1.1-1.6-2.8-1.8-3.4-1.8-1.4-.1-2.8.9-3.5.9-.7 0-1.8-.8-3-.8-1.5 0-3 .9-3.8 2.3-1.6 2.8-.4 7 1.2 9.3.8 1.1 1.7 2.4 2.9 2.3 1.2 0 1.6-.7 3-.7s1.8.7 3 .7c1.3 0 2.1-1.1 2.8-2.3.9-1.3 1.3-2.6 1.3-2.7 0 0-2.5-1-2.5-3.6zM14.2 5.6c.6-.8 1.1-1.9 1-3-.9 0-2.1.6-2.7 1.4-.6.7-1.1 1.8-1 2.9 1 .1 2.1-.5 2.7-1.3z" />
        </svg>
      );
    case "linux":
      return (
        <svg {...shared}>
          <title>{label}</title>
          <path d="M12 2c-2.4 0-3.9 1.9-3.9 4.4 0 1.3.3 2.2-.6 3.6C6 12.2 4.8 14.3 5 16.6c.1 1.2.6 2.1 1.4 2.7L5 21h5l.6-1.2c.5.1.9.1 1.4.1s.9 0 1.4-.1L14 21h5l-1.4-1.7c.8-.6 1.3-1.5 1.4-2.7.2-2.3-1-4.4-2.5-6.6-.9-1.4-.6-2.3-.6-3.6C15.9 3.9 14.4 2 12 2z" />
          <ellipse cx="12" cy="15" rx="3.2" ry="4" fill="rgb(var(--surface))" />
          <circle cx="10.6" cy="6.4" r=".9" fill="rgb(var(--surface))" />
          <circle cx="13.4" cy="6.4" r=".9" fill="rgb(var(--surface))" />
          <path d="M10.6 8.4h2.8L12 10z" fill="rgb(var(--amber))" />
        </svg>
      );
    default:
      return null;
  }
}
