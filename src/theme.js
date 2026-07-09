// Theme registry + apply. CSS variables live in styles.css under [data-theme="…"].
// Preferences are stored in localStorage under "muninn-theme".
// "system" resolves to dark/light from the OS; named themes set data-theme directly.

export const THEMES = [
  { id: "dark",     label: "Dark",     base: "dark"  },
  { id: "light",    label: "Light",    base: "light" },
  { id: "terminal", label: "Terminal", base: "dark"  },
  { id: "abyss",    label: "Abyss",    base: "dark"  },
  { id: "slate",    label: "Slate",    base: "dark"  },
  { id: "ember",    label: "Ember",    base: "dark"  },
  { id: "paper",    label: "Paper",    base: "light" },
  { id: "ocean",    label: "Ocean",    base: "light" },
  { id: "system",   label: "System",   base: null    },
];

const THEME_IDS = new Set(THEMES.map((t) => t.id));

export function resolveThemeId(pref) {
  const id = THEME_IDS.has(pref) ? pref : "dark";
  if (id === "system") {
    return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  }
  return id;
}

/** Whether the active theme is dark-based (for Mapbox, etc.). */
export function isDarkTheme() {
  const pref = localStorage.getItem("muninn-theme") || "dark";
  const resolved = resolveThemeId(pref);
  const def = THEMES.find((t) => t.id === resolved);
  return !def || def.base !== "light";
}

export function applyTheme(pref) {
  const stored = THEME_IDS.has(pref) ? pref : "dark";
  const theme = resolveThemeId(stored);
  document.documentElement.setAttribute("data-theme", theme);
}

// Apply immediately to prevent flash
applyTheme(localStorage.getItem("muninn-theme") || "dark");

// React to OS theme changes when set to "system"
window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
  const pref = localStorage.getItem("muninn-theme") || "dark";
  if (pref === "system") applyTheme("system");
});
