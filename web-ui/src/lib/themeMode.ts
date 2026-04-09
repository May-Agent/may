import { useEffect, useState } from "react";

export type ThemeMode = "light" | "dark";

function readThemeMode(): ThemeMode {
  if (typeof document === "undefined") {
    return "light";
  }
  return document.documentElement.classList.contains("dark") ? "dark" : "light";
}

export function useThemeMode(): ThemeMode {
  const [themeMode, setThemeMode] = useState<ThemeMode>(() => readThemeMode());

  useEffect(() => {
    if (typeof document === "undefined") {
      return;
    }
    const root = document.documentElement;
    const observer = new MutationObserver(() => {
      setThemeMode(readThemeMode());
    });
    observer.observe(root, {
      attributeFilter: ["class"],
      attributes: true,
    });
    return () => observer.disconnect();
  }, []);

  return themeMode;
}
