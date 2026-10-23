import { createContext, ReactNode, useCallback, useContext, useEffect, useState } from "react";
import { getCurrentWindow, UserAttentionType } from "@tauri-apps/api/window";
import { api, SettingsView, Theme } from "./api";

interface SettingsContextValue {
  view: SettingsView | null;
  setView: (view: SettingsView) => void;
}

const SettingsContext = createContext<SettingsContextValue>({ view: null, setView: () => {} });

/** Loads settings once and applies the theme whenever it changes. */
export function SettingsProvider({ children }: { children: ReactNode }) {
  const [view, setView] = useState<SettingsView | null>(null);

  useEffect(() => {
    api.getSettings().then(setView).catch(console.error);
  }, []);

  useEffect(() => {
    if (view) applyTheme(view.settings.theme);
  }, [view?.settings.theme]);

  return <SettingsContext.Provider value={{ view, setView }}>{children}</SettingsContext.Provider>;
}

export function useSettings() {
  return useContext(SettingsContext);
}

function applyTheme(theme: Theme) {
  const root = document.documentElement;
  if (theme === "system") delete root.dataset.theme;
  else root.dataset.theme = theme;
  // Title bar follows too.
  getCurrentWindow()
    .setTheme(theme === "system" ? null : theme)
    .catch(() => {});
}

let audio: AudioContext | null = null;

/** Short two-tone chime plus a taskbar flash when the window is not focused. */
export function useNotify() {
  const { view } = useSettings();
  const enabled = view?.settings.soundNotifications ?? true;
  return useCallback(() => {
    if (!enabled) return;
    try {
      audio ??= new AudioContext();
      const now = audio.currentTime;
      [880, 1320].forEach((freq, i) => {
        const osc = audio!.createOscillator();
        const gain = audio!.createGain();
        osc.frequency.value = freq;
        gain.gain.setValueAtTime(0.0001, now + i * 0.12);
        gain.gain.exponentialRampToValueAtTime(0.15, now + i * 0.12 + 0.02);
        gain.gain.exponentialRampToValueAtTime(0.0001, now + i * 0.12 + 0.25);
        osc.connect(gain).connect(audio!.destination);
        osc.start(now + i * 0.12);
        osc.stop(now + i * 0.12 + 0.3);
      });
    } catch {
      // audio unavailable
    }
    if (!document.hasFocus()) {
      getCurrentWindow()
        .requestUserAttention(UserAttentionType.Informational)
        .catch(() => {});
    }
  }, [enabled]);
}
