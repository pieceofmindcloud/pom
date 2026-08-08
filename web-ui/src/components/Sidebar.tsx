import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { ChevronDown } from "lucide-react";
import { useState } from "react";
import { NavLink, useLocation } from "react-router-dom";
import { Logo } from "./Logo";
import { NAV, type NavSection } from "./nav";

function sectionHoldsPath(section: NavSection, pathname: string) {
  return (section.children ?? []).some((c) =>
    c.end ? pathname === c.to : pathname.startsWith(c.to),
  );
}

function Leaf({
  to,
  label,
  end,
  onNavigate,
}: {
  to: string;
  label: string;
  end?: boolean;
  onNavigate?: () => void;
}) {
  return (
    <NavLink to={to} end={end} onClick={onNavigate}>
      {({ isActive }) => (
        <span
          className={clsx(
            "relative flex items-center rounded-lg py-1.5 pl-3 pr-2.5 text-[0.83rem]",
            "transition-colors duration-150",
            isActive ? "text-fg" : "text-muted hover:bg-line/40 hover:text-fg",
          )}
        >
          {isActive && (
            <motion.span
              layoutId="nav-leaf"
              className="absolute inset-0 rounded-lg bg-iris/12 ring-1 ring-inset ring-iris/25"
              transition={{ type: "spring", stiffness: 520, damping: 42 }}
            />
          )}
          <span className="relative truncate">{label}</span>
        </span>
      )}
    </NavLink>
  );
}

function Section({
  section,
  onNavigate,
}: {
  section: NavSection;
  onNavigate?: () => void;
}) {
  const { pathname } = useLocation();
  const holdsPath = sectionHoldsPath(section, pathname);
  // A section the operator opened stays open; one holding the current page
  // opens itself. Collapsing the section you are inside of would hide where
  // you are.
  const [open, setOpen] = useState(holdsPath);
  const expanded = open || holdsPath;
  const Icon = section.icon;

  if (section.to) {
    return (
      <NavLink to={section.to} end onClick={onNavigate}>
        {({ isActive }) => (
          <span
            className={clsx(
              "relative flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm font-medium",
              "transition-colors duration-150",
              isActive ? "text-fg" : "text-muted hover:bg-line/40 hover:text-fg",
            )}
          >
            {isActive && (
              <motion.span
                layoutId="nav-section"
                className="absolute inset-0 rounded-lg bg-iris/12 ring-1 ring-inset ring-iris/25"
                transition={{ type: "spring", stiffness: 520, damping: 42 }}
              />
            )}
            <Icon
              className={clsx("relative h-[1.05rem] w-[1.05rem] shrink-0", isActive && "text-iris")}
            />
            <span className="relative truncate">{section.label}</span>
          </span>
        )}
      </NavLink>
    );
  }

  return (
    <div>
      <button
        onClick={() => setOpen((o) => !o)}
        aria-expanded={expanded}
        className={clsx(
          "flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm font-medium",
          "transition-colors duration-150",
          holdsPath ? "text-fg" : "text-muted hover:bg-line/40 hover:text-fg",
        )}
      >
        <Icon
          className={clsx("h-[1.05rem] w-[1.05rem] shrink-0", holdsPath && "text-iris")}
        />
        <span className="flex-1 truncate text-left">{section.label}</span>
        <ChevronDown
          className={clsx(
            "h-3.5 w-3.5 shrink-0 text-faint transition-transform duration-200 ease-swift",
            expanded && "rotate-180",
          )}
        />
      </button>
      <AnimatePresence initial={false}>
        {expanded && (
          <motion.div
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
            className="overflow-hidden"
          >
            {/* The rail makes the children read as belonging to the section. */}
            <div className="ml-[1.32rem] mt-0.5 flex flex-col gap-0.5 border-l border-line pl-2">
              {section.children?.map((c) => (
                <Leaf key={c.to} {...c} onNavigate={onNavigate} />
              ))}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

export function SidebarContent({ onNavigate }: { onNavigate?: () => void }) {
  return (
    <>
      <div className="flex h-16 shrink-0 items-center px-5">
        <Logo size={30} />
      </div>
      <nav className="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto px-3 pb-4">
        {NAV.map((s) => (
          <Section key={s.label} section={s} onNavigate={onNavigate} />
        ))}
      </nav>
    </>
  );
}

export function Sidebar() {
  return (
    <aside className="hidden w-[248px] shrink-0 flex-col border-r border-line bg-surface lg:flex">
      <SidebarContent />
    </aside>
  );
}
