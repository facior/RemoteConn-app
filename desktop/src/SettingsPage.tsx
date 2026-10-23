import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, DefaultPermissions, QualityPreset, Settings, Theme } from "./api";
import { useSettings } from "./settings";
import { AccountTab, ServiceTab } from "./AccountSettings";

type Tab = "general" | "security" | "session" | "network" | "account" | "service";

const TABS: { id: Tab; label: string }[] = [
  { id: "general", label: "Ogólne" },
  { id: "security", label: "Bezpieczeństwo" },
  { id: "session", label: "Sesja" },
  { id: "network", label: "Sieć" },
  { id: "account", label: "Konto" },
  { id: "service", label: "Usługa" },
];

const PERMISSION_LABELS: { key: keyof DefaultPermissions; label: string }[] = [
  { key: "viewScreen", label: "Podgląd ekranu" },
  { key: "controlInput", label: "Sterowanie myszą i klawiaturą" },
  { key: "clipboard", label: "Wspólny schowek" },
  { key: "fileTransfer", label: "Przesyłanie plików" },
];

const dateFormat = new Intl.DateTimeFormat("pl-PL", { dateStyle: "medium" });

export function SettingsPage({ onClose }: { onClose: () => void }) {
  const { view, setView } = useSettings();
  const [tab, setTab] = useState<Tab>("general");
  const [draft, setDraft] = useState<Settings | null>(null);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);

  // Reload on open: the backend changes settings too (e.g. a device trusted
  // from the accept dialog), and the OS autostart entry can change outside
  // the app.
  useEffect(() => {
    Promise.all([api.getSettings(), api.autostartEnabled().catch(() => null)])
      .then(([fresh, autostart]) => {
        const settings = autostart === null ? fresh.settings : { ...fresh.settings, autostart };
        setView({ ...fresh, settings: fresh.settings });
        setDraft(settings);
      })
      .catch(console.error);
  }, [setView]);

  if (!view || !draft) return null;

  const dirty = JSON.stringify(draft) !== JSON.stringify(view.settings);
  const set = <K extends keyof Settings>(key: K, value: Settings[K]) => {
    setDraft({ ...draft, [key]: value });
    setMessage(null);
  };

  const save = async () => {
    setSaving(true);
    setMessage(null);
    try {
      const next = await api.setSettings(draft);
      setView(next);
      setDraft(next.settings);
      setMessage({ ok: true, text: "Zapisano." });
    } catch (e) {
      setMessage({ ok: false, text: String(e) });
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="settings-page">
      <header className="topbar">
        <button className="secondary" onClick={onClose}>
          ← Wróć
        </button>
        <h1>Ustawienia</h1>
        <span className="spacer" />
      </header>
      <div className="settings-layout">
        <nav className="tabs" role="tablist" aria-label="Kategorie ustawień">
          {TABS.map((t) => (
            <button
              key={t.id}
              role="tab"
              aria-selected={tab === t.id}
              className={tab === t.id ? "tab active" : "tab"}
              onClick={() => setTab(t.id)}
            >
              {t.label}
            </button>
          ))}
        </nav>
        <section className="settings-panel" role="tabpanel">
          {tab === "general" && <GeneralTab draft={draft} set={set} computerName={view.computerName} />}
          {tab === "security" && <SecurityTab draft={draft} set={set} />}
          {tab === "session" && <SessionTab draft={draft} set={set} defaultDir={view.defaultDownloadDir} />}
          {tab === "network" && (
            <NetworkTab
              draft={draft}
              set={set}
              patch={(changes) => {
                setDraft({ ...draft, ...changes });
                setMessage(null);
              }}
            />
          )}
          {tab === "account" && <AccountTab serverSet={!!view.settings.rendezvousServer} />}
          {tab === "service" && <ServiceTab />}
        </section>
      </div>
      <footer className="settings-footer" hidden={tab === "account" || tab === "service"}>
        {message && <span className={message.ok ? "status online" : "error"}>{message.text}</span>}
        <span className="spacer" />
        <button className="secondary" disabled={!dirty || saving} onClick={() => setDraft(view.settings)}>
          Cofnij zmiany
        </button>
        <button className="primary" disabled={!dirty || saving} onClick={save}>
          {saving ? "Zapisywanie…" : "Zapisz"}
        </button>
      </footer>
    </div>
  );
}

type SetFn = <K extends keyof Settings>(key: K, value: Settings[K]) => void;

function Toggle({
  checked,
  onChange,
  label,
  hint,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  hint?: string;
}) {
  return (
    <label className="setting toggle">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      <span>
        {label}
        {hint && <span className="hint">{hint}</span>}
      </span>
    </label>
  );
}

function GeneralTab({ draft, set, computerName }: { draft: Settings; set: SetFn; computerName: string }) {
  return (
    <>
      <h2>Ogólne</h2>
      <label className="setting">
        Nazwa urządzenia
        <input
          value={draft.deviceName}
          placeholder={computerName}
          maxLength={64}
          onChange={(e) => set("deviceName", e.target.value)}
        />
        <span className="hint">Widzi ją druga strona połączenia. Puste pole oznacza nazwę komputera.</span>
      </label>
      <fieldset className="setting">
        <legend>Motyw</legend>
        <div className="segmented">
          {(["system", "light", "dark"] as Theme[]).map((t) => (
            <label key={t} className={draft.theme === t ? "active" : ""}>
              <input type="radio" name="theme" checked={draft.theme === t} onChange={() => set("theme", t)} />
              {t === "system" ? "Systemowy" : t === "light" ? "Jasny" : "Ciemny"}
            </label>
          ))}
        </div>
      </fieldset>
      <Toggle
        checked={draft.autostart}
        onChange={(v) => set("autostart", v)}
        label="Uruchamiaj razem z systemem"
        hint="Aplikacja startuje w zasobniku, bez otwierania okna."
      />
      <Toggle
        checked={draft.shareOnStart}
        onChange={(v) => set("shareOnStart", v)}
        label="Włączaj udostępnianie przy starcie"
        hint="Razem ze stałym hasłem daje dostęp bez obecności użytkownika."
      />
      <Toggle
        checked={draft.closeToTray}
        onChange={(v) => set("closeToTray", v)}
        label="Zamknięcie okna chowa aplikację do zasobnika"
        hint="Udostępnianie działa dalej w tle. Zakończ przez ikonę w zasobniku."
      />
    </>
  );
}

function SecurityTab({ draft, set }: { draft: Settings; set: SetFn }) {
  const { view, setView } = useSettings();
  const [password, setPassword] = useState("");
  const [repeat, setRepeat] = useState("");
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null);

  const savePassword = async (value: string | null) => {
    setStatus(null);
    if (value !== null && value !== repeat) {
      setStatus({ ok: false, text: "Hasła nie są takie same." });
      return;
    }
    try {
      setView(await api.setPermanentPassword(value));
      setPassword("");
      setRepeat("");
      setStatus({ ok: true, text: value === null ? "Usunięto stałe hasło." : "Zapisano stałe hasło." });
    } catch (e) {
      setStatus({ ok: false, text: String(e) });
    }
  };

  const removeTrusted = async (fingerprint: string) => {
    const next = await api.removeTrustedDevice(fingerprint);
    setView(next);
    set(
      "trustedDevices",
      draft.trustedDevices.filter((t) => t.fingerprint !== fingerprint),
    );
  };

  return (
    <>
      <h2>Bezpieczeństwo</h2>
      <div className="setting group">
        <strong>Stałe hasło</strong>
        <span className="hint">
          Działa obok hasła jednorazowego i pozwala łączyć się bez obecności użytkownika. Jest przechowywane w postaci
          zaszyfrowanej (Windows DPAPI). Stan: {view?.permanentPasswordSet ? "ustawione" : "nieustawione"}.
        </span>
        <div className="row">
          <input
            type="password"
            placeholder="Nowe stałe hasło (min. 8 znaków)"
            value={password}
            autoComplete="new-password"
            onChange={(e) => setPassword(e.target.value)}
          />
          <input
            type="password"
            placeholder="Powtórz hasło"
            value={repeat}
            autoComplete="new-password"
            onChange={(e) => setRepeat(e.target.value)}
          />
        </div>
        <div className="row">
          <button className="secondary" disabled={password.length === 0} onClick={() => savePassword(password)}>
            {view?.permanentPasswordSet ? "Zmień stałe hasło" : "Ustaw stałe hasło"}
          </button>
          {view?.permanentPasswordSet && (
            <button className="link danger-link" onClick={() => savePassword(null)}>
              Usuń stałe hasło
            </button>
          )}
        </div>
        {status && <span className={status.ok ? "status online" : "error small"}>{status.text}</span>}
      </div>
      <Toggle
        checked={draft.confirmPermanentPassword}
        onChange={(v) => set("confirmPermanentPassword", v)}
        label="Pytaj o zgodę także przy stałym haśle"
        hint="Wyłączone: poprawne stałe hasło łączy od razu, z domyślnymi uprawnieniami."
      />
      <label className="setting">
        Długość hasła jednorazowego: {draft.passwordLength} znaków
        <input
          type="range"
          min={6}
          max={16}
          value={draft.passwordLength}
          onChange={(e) => set("passwordLength", Number(e.target.value))}
        />
      </label>
      <Toggle
        checked={draft.rotatePasswordAfterSession}
        onChange={(v) => set("rotatePasswordAfterSession", v)}
        label="Nowe hasło jednorazowe po każdej sesji"
      />
      <fieldset className="setting">
        <legend>Domyślne uprawnienia</legend>
        <span className="hint">Zaznaczone w oknie akceptacji i nadawane połączeniom zaakceptowanym automatycznie.</span>
        {PERMISSION_LABELS.map(({ key, label }) => (
          <label key={key} className="toggle">
            <input
              type="checkbox"
              checked={draft.defaultPermissions[key]}
              onChange={(e) => set("defaultPermissions", { ...draft.defaultPermissions, [key]: e.target.checked })}
            />
            {label}
          </label>
        ))}
      </fieldset>
      <div className="setting group">
        <strong>Zaufane urządzenia</strong>
        <span className="hint">
          Łączą się bez okna akceptacji, ale nadal muszą podać hasło. Dodasz je zaznaczając „Ufaj temu urządzeniu” przy
          akceptacji połączenia.
        </span>
        {draft.trustedDevices.length === 0 ? (
          <span className="muted small">Brak zaufanych urządzeń.</span>
        ) : (
          <ul className="trusted">
            {draft.trustedDevices.map((t) => (
              <li key={t.fingerprint}>
                <span>
                  <strong>{t.name || "Bez nazwy"}</strong>
                  <code className="muted small">{t.fingerprint}</code>
                  <span className="muted small">dodano {dateFormat.format(t.addedUnix * 1000)}</span>
                </span>
                <button className="link danger-link" onClick={() => removeTrusted(t.fingerprint)}>
                  Usuń
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </>
  );
}

function SessionTab({ draft, set, defaultDir }: { draft: Settings; set: SetFn; defaultDir: string }) {
  const pickDir = async () => {
    const dir = await open({ directory: true, title: "Folder na odebrane pliki" });
    if (typeof dir === "string") set("downloadDir", dir);
  };
  return (
    <>
      <h2>Sesja</h2>
      <div className="row">
        <label className="setting">
          Domyślna jakość obrazu
          <select value={draft.defaultQuality} onChange={(e) => set("defaultQuality", e.target.value as QualityPreset)}>
            <option value="balanced">Zrównoważona</option>
            <option value="quality">Najlepsza jakość</option>
            <option value="speed">Najszybsza</option>
          </select>
        </label>
        <label className="setting">
          Domyślna liczba klatek
          <select value={draft.defaultFps} onChange={(e) => set("defaultFps", Number(e.target.value))}>
            <option value={15}>15 fps</option>
            <option value={30}>30 fps</option>
            <option value={60}>60 fps</option>
          </select>
        </label>
      </div>
      <label className="setting">
        Folder na odebrane pliki
        <div className="row">
          <input value={draft.downloadDir} placeholder={defaultDir} readOnly />
          <button className="secondary" onClick={pickDir}>
            Wybierz…
          </button>
          {draft.downloadDir && (
            <button className="link" onClick={() => set("downloadDir", "")}>
              Domyślny
            </button>
          )}
        </div>
      </label>
      <Toggle
        checked={draft.showRemoteCursor}
        onChange={(v) => set("showRemoteCursor", v)}
        label="Pokazuj zdalny kursor"
        hint="Przy sterowaniu jego kształt zastępuje lokalny kursor; w podglądzie jest rysowany na obrazie."
      />
      <Toggle
        checked={draft.soundNotifications}
        onChange={(v) => set("soundNotifications", v)}
        label="Dźwięk przy połączeniu i nowej wiadomości"
      />
    </>
  );
}

function NetworkTab({
  draft,
  set,
  patch,
}: {
  draft: Settings;
  set: SetFn;
  patch: (changes: Partial<Settings>) => void;
}) {
  const { view, setView } = useSettings();
  const [test, setTest] = useState<{ ok: boolean; text: string } | null>(null);
  const [testing, setTesting] = useState(false);
  const [codeOpen, setCodeOpen] = useState(false);
  const [code, setCode] = useState("");
  const [codeError, setCodeError] = useState("");

  const applyCode = async () => {
    setCodeError("");
    try {
      const imported = await api.importConfigCode(code);
      patch({ rendezvousServer: imported.server, serverAccessKey: imported.accessKey });
      setCode("");
      setCodeOpen(false);
      setTest({
        ok: true,
        text: `Wczytano serwer ${imported.server}, klucz serwera ${imported.fingerprint} jest zaufany. Kliknij „Zapisz”, a potem możesz „Testuj”.`,
      });
      setView(await api.getSettings());
    } catch (e) {
      setCodeError(String(e));
    }
  };
  // The pin shown belongs to the saved server, not to an unsaved edit.
  const savedServer = view?.settings.rendezvousServer ?? "";
  const pinShown = view?.serverFingerprint && savedServer === draft.rendezvousServer.trim();

  const runTest = async () => {
    setTesting(true);
    setTest(null);
    try {
      const result = await api.testServer(draft.rendezvousServer, draft.serverAccessKey);
      setTest({
        ok: true,
        text: `Serwer odpowiada (${result.latencyMs} ms). Klucz serwera: ${result.fingerprint}${
          result.newlyPinned ? " (zapamiętany jako zaufany)" : " (zgodny z zapamiętanym)"
        }.`,
      });
      setView(await api.getSettings());
    } catch (e) {
      setTest({ ok: false, text: String(e) });
    } finally {
      setTesting(false);
    }
  };

  const forgetPin = async () => {
    setView(await api.forgetServerPin(savedServer));
    setTest(null);
  };

  return (
    <>
      <h2>Sieć</h2>
      <div className="setting group">
        <div className="row">
          <span>
            <strong>Kod konfiguracji serwera</strong>
            <span className="hint">
              Kod RC1-… od administratora serwera (panel WWW albo <code>rc-admin app</code>) wpisuje adres, klucz
              dostępu i zaufany klucz serwera naraz.
            </span>
          </span>
          {!codeOpen && (
            <button className="secondary" onClick={() => setCodeOpen(true)}>
              Wklej kod
            </button>
          )}
        </div>
        {codeOpen && (
          <>
            <textarea
              className="config-code"
              rows={3}
              autoFocus
              spellCheck={false}
              value={code}
              placeholder="RC1-…"
              onChange={(e) => {
                setCode(e.target.value);
                setCodeError("");
              }}
            />
            {codeError && <span className="error small">{codeError}</span>}
            <div className="row">
              <button className="primary" disabled={!code.trim()} onClick={applyCode}>
                Wczytaj
              </button>
              <button
                className="link"
                onClick={() => {
                  setCodeOpen(false);
                  setCode("");
                  setCodeError("");
                }}
              >
                Anuluj
              </button>
            </div>
          </>
        )}
      </div>
      <label className="setting">
        Serwer ID (rendezvous i relay)
        <div className="row">
          <input
            value={draft.rendezvousServer}
            placeholder="np. rc.twojafirma.pl lub 203.0.113.10:21116"
            onChange={(e) => {
              set("rendezvousServer", e.target.value);
              setTest(null);
            }}
          />
          <button className="secondary" disabled={!draft.rendezvousServer.trim() || testing} onClick={runTest}>
            {testing ? "Sprawdzam…" : "Testuj"}
          </button>
        </div>
        <span className="hint">Bez serwera połączenia działają tylko po adresie IP (np. w sieci lokalnej).</span>
        {test && <span className={test.ok ? "status online" : "error small"}>{test.text}</span>}
      </label>
      <label className="setting">
        Klucz dostępu do serwera
        <input
          type="password"
          value={draft.serverAccessKey}
          autoComplete="off"
          placeholder="pusty, jeśli serwer go nie wymaga"
          onChange={(e) => {
            set("serverAccessKey", e.target.value);
            setTest(null);
          }}
        />
        <span className="hint">Ustawiony na serwerze zmienną RC_ACCESS_KEY; bez niego obcy nie mogą używać Twojego serwera.</span>
      </label>
      {pinShown && (
        <div className="setting group">
          <strong>Zaufany klucz serwera</strong>
          <span className="hint">
            Zapamiętany przy pierwszym połączeniu. Jeśli serwer przedstawi inny klucz, aplikacja odmówi połączenia
            (ochrona przed podszyciem się pod serwer).
          </span>
          <div className="row">
            <code>{view?.serverFingerprint}</code>
            <button className="link danger-link" onClick={forgetPin}>
              Zapomnij klucz serwera
            </button>
          </div>
        </div>
      )}
      <label className="setting">
        Port nasłuchu (UDP)
        <input
          type="number"
          min={1024}
          max={65535}
          value={draft.listenPort}
          onChange={(e) => set("listenPort", Number(e.target.value))}
        />
        <span className="hint">Używany przy połączeniach po adresie IP i przekierowaniu portów. Domyślnie 21118.</span>
      </label>
      <Toggle
        checked={draft.forceRelay}
        onChange={(v) => set("forceRelay", v)}
        label="Zawsze łącz przez relay"
        hint="Pomija połączenie bezpośrednie (przydatne w restrykcyjnych sieciach). Dotyczy łączenia po ID."
      />
      <p className="muted small">
        Zmiana serwera, portu, nazwy urządzenia albo folderu plików uruchamia udostępnianie ponownie; trwające sesje
        zostaną przerwane.
      </p>
    </>
  );
}
