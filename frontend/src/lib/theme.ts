/**
 * Light/dark theme preference.
 *
 * Resolution order (see src/styles/tokens.css): `prefers-color-scheme`
 * picks a default, and an explicit preference stored here overrides it via
 * `data-theme` on the document root. "system" means "no override" — it
 * removes the attribute so the media query decides.
 */

export type ThemePreference = "light" | "dark" | "system";

const STORAGE_KEY = "flyco.theme";

function isThemePreference(value: string): value is ThemePreference {
  return value === "light" || value === "dark" || value === "system";
}

/**
 * Reads the stored preference. Defaults to "system" when nothing is stored,
 * storage is blocked, or the stored value isn't one of the three valid
 * preferences.
 */
export function readStoredThemePreference(
  storage?: Pick<Storage, "getItem">,
): ThemePreference {
  try {
    const target = storage ?? localStorage;
    const raw = target.getItem(STORAGE_KEY);
    if (raw === null || !isThemePreference(raw)) {
      return "system";
    }
    return raw;
  } catch {
    return "system";
  }
}

export function storeThemePreference(
  preference: ThemePreference,
  storage?: Pick<Storage, "setItem">,
): void {
  try {
    const target = storage ?? localStorage;
    target.setItem(STORAGE_KEY, preference);
  } catch {
    // Blocked or full storage must not crash startup or preference changes.
  }
}

/** Applies a preference to the document by setting or clearing `data-theme`. */
export function applyThemePreference(
  preference: ThemePreference,
  root: HTMLElement = document.documentElement,
): void {
  if (preference === "system") {
    root.removeAttribute("data-theme");
  } else {
    root.setAttribute("data-theme", preference);
  }
}

/** Reads, applies, and returns the current preference — the module's one entry point for startup. */
export function initTheme(): ThemePreference {
  const preference = readStoredThemePreference();
  applyThemePreference(preference);
  return preference;
}

/** Persists a new preference and applies it immediately. */
export function setTheme(preference: ThemePreference): void {
  storeThemePreference(preference);
  applyThemePreference(preference);
}
