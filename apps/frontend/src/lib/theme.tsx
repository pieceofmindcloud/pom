import { createContext, useCallback, useContext, useEffect, useState } from "react";
import { storageKey } from "./brand";

type Theme = "dark" | "light";

const ThemeContext = createContext<{ theme: Theme; toggle: () => void }>({
  theme: "dark",
  toggle: () => {},
});

function current(): Theme {
  return document.documentElement.dataset.theme === "light" ? "light" : "dark";
}

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  // index.html already stamped the root before first paint; read it rather
  // than deciding again, so the first render matches what is on screen.
  const [theme, setTheme] = useState<Theme>(current);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem(storageKey("theme"), theme);
  }, [theme]);

  const toggle = useCallback(
    () => setTheme((t) => (t === "dark" ? "light" : "dark")),
    [],
  );

  return (
    <ThemeContext.Provider value={{ theme, toggle }}>{children}</ThemeContext.Provider>
  );
}

export const useTheme = () => useContext(ThemeContext);
