// ==================== THEME ====================
export function applyTheme(pref) {
  let theme = pref;
  if (pref === "system") {
    theme = window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  }
  document.documentElement.setAttribute("data-theme", theme);
}

// Apply immediately to prevent flash
applyTheme(localStorage.getItem("muninn-theme") || "dark");

// React to OS theme changes when set to "system"
window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
  const pref = localStorage.getItem("muninn-theme") || "dark";
  if (pref === "system") applyTheme("system");
});
