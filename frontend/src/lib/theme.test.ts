import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  applyThemePreference,
  initTheme,
  readStoredThemePreference,
  setTheme,
  storeThemePreference,
} from "./theme";

describe("theme preference", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute("data-theme");
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("defaults to system when nothing is stored", () => {
    expect(readStoredThemePreference()).toBe("system");
  });

  it("round-trips a stored preference", () => {
    storeThemePreference("dark");
    expect(readStoredThemePreference()).toBe("dark");
  });

  it("falls back to system on an invalid stored value", () => {
    localStorage.setItem("flyco.theme", "purple");
    expect(readStoredThemePreference()).toBe("system");
    expect(initTheme()).toBe("system");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
  });

  it("falls back to system when localStorage access throws", () => {
    const blockedGet = {
      getItem() {
        throw new DOMException("blocked", "SecurityError");
      },
    };
    const blockedSet = {
      setItem() {
        throw new DOMException("blocked", "SecurityError");
      },
    };
    expect(readStoredThemePreference(blockedGet)).toBe("system");
    expect(() => storeThemePreference("dark", blockedSet)).not.toThrow();

    vi.spyOn(window, "localStorage", "get").mockImplementation(() => {
      throw new DOMException("blocked", "SecurityError");
    });
    expect(initTheme()).toBe("system");
    expect(() => setTheme("dark")).not.toThrow();
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
  });

  it("applies dark and light by setting data-theme", () => {
    applyThemePreference("dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    applyThemePreference("light");
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
  });

  it("applies system by clearing data-theme", () => {
    applyThemePreference("dark");
    applyThemePreference("system");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
  });

  it("setTheme persists and applies together", () => {
    setTheme("light");
    expect(localStorage.getItem("flyco.theme")).toBe("light");
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
  });
});
