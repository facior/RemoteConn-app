import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { AccountDevice, AccountView, api, ServiceStatus } from "./api";

const dateFormat = new Intl.DateTimeFormat("pl-PL", { dateStyle: "medium" });

type Status = { ok: boolean; text: string } | null;

/** Settings → Konto. Actions apply at once (no "Zapisz"). */
export function AccountTab({ serverSet }: { serverSet: boolean }) {
  const [account, setAccount] = useState<AccountView | null>(null);
  const [mode, setMode] = useState<"login" | "register">("login");
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [repeat, setRepeat] = useState("");
  const [invite, setInvite] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<Status>(null);

  useEffect(() => {
    api.accountStatus().then(setAccount).catch(console.error);
  }, []);

  const run = async (action: () => Promise<AccountView>, done: string) => {
    setBusy(true);
    setStatus(null);
    try {
      setAccount(await action());
      setPassword("");
      setRepeat("");
      setStatus({ ok: true, text: done });
    } catch (e) {
      setStatus({ ok: false, text: String(e) });
    } finally {
      setBusy(false);
    }
  };

  if (!account) return null;

  if (account.loggedIn) {
    return (
      <>
        <h2>Konto</h2>
        <div className="setting group">
          <strong>
            {account.displayName} <span className="muted">({account.name})</span>
          </strong>
          <span className="hint">Serwer: {account.server}</span>
          {account.disabled && <span className="error small">Administrator serwera zablokował to konto.</span>}
          <label className="toggle">
            <input
              type="checkbox"
              checked={account.hostTrust}
              disabled={busy}
              onChange={(e) =>
                run(
                  () => api.accountSetHostTrust(e.target.checked),
                  e.target.checked ? "Urządzenia konta łączą się z tym komputerem bez hasła." : "Wyłączono.",
                )
              }
            />
            <span>
              Wpuszczaj moje urządzenia bez hasła
              <span className="hint">
                Inne komputery zalogowane na to konto łączą się z tym komputerem bez hasła i bez okna akceptacji.
              </span>
            </span>
          </label>
          <div className="row">
            <button className="secondary" disabled={busy} onClick={() => run(api.accountLogout, "Wylogowano.")}>
              Wyloguj
            </button>
          </div>
          {status && <span className={status.ok ? "status online" : "error small"}>{status.text}</span>}
        </div>
        <DeviceList />
        <ChangePassword onChanged={setAccount} />
      </>
    );
  }

  const register = mode === "register";
  const submit = () => {
    if (register && password !== repeat) {
      setStatus({ ok: false, text: "Hasła nie są takie same." });
      return;
    }
    run(
      () => (register ? api.accountRegister(name, password, invite, displayName) : api.accountLogin(name, password)),
      register ? "Konto założone, jesteś zalogowany." : "Zalogowano.",
    );
  };

  return (
    <>
      <h2>Konto</h2>
      <p className="hint">
        Zaloguj się na to samo konto na swoich komputerach, a będziesz się między nimi łączyć bez haseł. Hasło konta nie
        opuszcza tego komputera: powstaje z niego klucz, a serwer zna tylko jego część publiczną.
      </p>
      {!serverSet && <p className="error small">Najpierw ustaw serwer ID w zakładce Sieć.</p>}
      <div className="segmented">
        <label className={!register ? "active" : ""}>
          <input type="radio" checked={!register} onChange={() => setMode("login")} />
          Logowanie
        </label>
        <label className={register ? "active" : ""}>
          <input type="radio" checked={register} onChange={() => setMode("register")} />
          Nowe konto
        </label>
      </div>
      <form
        className="setting group"
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <label className="setting">
          Nazwa konta (np. e-mail)
          <input value={name} autoComplete="username" onChange={(e) => setName(e.target.value)} required />
        </label>
        {register && (
          <label className="setting">
            Wyświetlana nazwa
            <input value={displayName} placeholder="np. Anna Kowalska" onChange={(e) => setDisplayName(e.target.value)} />
          </label>
        )}
        <label className="setting">
          Hasło konta
          <input
            type="password"
            value={password}
            autoComplete={register ? "new-password" : "current-password"}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
        </label>
        {register && (
          <>
            <label className="setting">
              Powtórz hasło
              <input type="password" value={repeat} autoComplete="new-password" onChange={(e) => setRepeat(e.target.value)} />
            </label>
            <label className="setting">
              Kod zaproszenia
              <input value={invite} placeholder="od administratora serwera" onChange={(e) => setInvite(e.target.value)} />
              <span className="hint">Administrator tworzy go w panelu serwera (zakładka Konta).</span>
            </label>
          </>
        )}
        <div className="row">
          <button className="primary" disabled={busy || !serverSet}>
            {busy ? "Chwileczkę…" : register ? "Załóż konto" : "Zaloguj"}
          </button>
        </div>
        {status && <span className={status.ok ? "status online" : "error small"}>{status.text}</span>}
      </form>
    </>
  );
}

function DeviceList() {
  const [devices, setDevices] = useState<AccountDevice[] | null>(null);
  const [error, setError] = useState("");

  const load = useCallback(() => {
    api
      .accountDevices()
      .then((d) => {
        setDevices(d);
        setError("");
      })
      .catch((e) => setError(String(e)));
  }, []);
  useEffect(load, [load]);

  const revoke = async (d: AccountDevice) => {
    if (!confirm(`Odebrać dostęp urządzeniu „${d.name}”? Nie połączy się już z komputerami tego konta bez hasła.`)) return;
    try {
      await api.accountRevoke(d.publicKey);
      load();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="setting group">
      <strong>Urządzenia na koncie</strong>
      <span className="hint">
        „Odbierz dostęp” przy zgubionym lub sprzedanym urządzeniu: komputery konta przestaną je wpuszczać.
      </span>
      {error && <span className="error small">{error}</span>}
      {devices && devices.length === 0 && <span className="muted small">Brak urządzeń.</span>}
      {devices && devices.length > 0 && (
        <ul className="trusted">
          {devices.map((d) => (
            <li key={d.publicKey}>
              <span>
                <strong>
                  {d.name || "Bez nazwy"}
                  {d.thisDevice && <span className="muted small"> (to urządzenie)</span>}
                </strong>
                <span className={d.online ? "status online small" : "muted small"}>
                  {d.deviceId ? (d.online ? `online · ${d.deviceId}` : `offline · ${d.deviceId}`) : "bez ID"}
                </span>
                <code className="muted small">{d.fingerprint}</code>
                <span className="muted small">dodano {dateFormat.format(d.addedUnix * 1000)}</span>
              </span>
              {!d.thisDevice && (
                <button className="link danger-link" onClick={() => revoke(d)}>
                  Odbierz dostęp
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
      <div className="row">
        <button className="link" onClick={load}>
          Odśwież
        </button>
      </div>
    </div>
  );
}

function ChangePassword({ onChanged }: { onChanged: (a: AccountView) => void }) {
  const [oldPassword, setOld] = useState("");
  const [newPassword, setNew] = useState("");
  const [repeat, setRepeat] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<Status>(null);

  const change = async () => {
    if (newPassword !== repeat) {
      setStatus({ ok: false, text: "Nowe hasła nie są takie same." });
      return;
    }
    setBusy(true);
    setStatus(null);
    try {
      onChanged(await api.accountChangePassword(oldPassword, newPassword));
      setOld("");
      setNew("");
      setRepeat("");
      setStatus({
        ok: true,
        text: "Hasło zmienione. Komputery konta przyjmą nowy klucz same; na innych urządzeniach, z których się łączysz, zaloguj się ponownie.",
      });
    } catch (e) {
      setStatus({ ok: false, text: String(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="setting group">
      <strong>Zmiana hasła konta</strong>
      <div className="row">
        <input type="password" placeholder="Obecne hasło" value={oldPassword} onChange={(e) => setOld(e.target.value)} />
      </div>
      <div className="row">
        <input type="password" placeholder="Nowe hasło (min. 10 znaków)" value={newPassword} onChange={(e) => setNew(e.target.value)} />
        <input type="password" placeholder="Powtórz nowe hasło" value={repeat} onChange={(e) => setRepeat(e.target.value)} />
      </div>
      <div className="row">
        <button className="secondary" disabled={busy || !oldPassword || !newPassword} onClick={change}>
          {busy ? "Zmieniam…" : "Zmień hasło"}
        </button>
      </div>
      {status && <span className={status.ok ? "status online" : "error small"}>{status.text}</span>}
    </div>
  );
}

/** Settings → Usługa (unattended access). */
export function ServiceTab() {
  const [service, setService] = useState<ServiceStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<Status>(null);

  useEffect(() => {
    const refresh = () => api.serviceStatus().then(setService).catch(console.error);
    refresh();
    const unlisten = listen("service://state", refresh);
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  const run = async (action: () => Promise<ServiceStatus | void>, done: string) => {
    setBusy(true);
    setStatus(null);
    try {
      const next = await action();
      if (next) setService(next);
      setStatus({ ok: true, text: done });
    } catch (e) {
      setStatus({ ok: false, text: String(e) });
    } finally {
      setBusy(false);
    }
  };

  if (!service) return null;

  return (
    <>
      <h2>Usługa (dostęp bez nadzoru)</h2>
      <p className="hint">
        Usługa systemowa udostępnia ten komputer zawsze po jego włączeniu: także przed zalogowaniem, na ekranie
        blokady i przy oknach „Kontrola konta użytkownika” (UAC). Działa z ustawieniami i stałym hasłem z tej
        aplikacji. Instalacja i każda zmiana jej ustawień wymagają zgody administratora.
      </p>
      {!service.supported ? (
        <p className="muted">Na tym systemie usługa nie jest jeszcze dostępna.</p>
      ) : (
        <div className="setting group">
          <strong>
            Stan:{" "}
            {!service.installed
              ? "nie zainstalowana"
              : service.running
                ? service.connected
                  ? "działa"
                  : "działa (łączenie z oknem…)"
                : "zatrzymana"}
          </strong>
          <span className="hint">
            Bez stałego hasła albo konta połączenia wymagają akceptacji w oknie aplikacji, więc przed zalogowaniem nikt
            ich nie zaakceptuje. Ustaw stałe hasło (Bezpieczeństwo) albo zaloguj się na konto.
          </span>
          <div className="row">
            {!service.installed ? (
              <button
                className="primary"
                disabled={busy}
                onClick={() => run(api.serviceInstall, "Usługa zainstalowana i uruchomiona.")}
              >
                {busy ? "Czekam na zgodę administratora…" : "Zainstaluj usługę"}
              </button>
            ) : (
              <>
                <button
                  className="secondary"
                  disabled={busy}
                  onClick={() => run(api.serviceSync, "Przekazano bieżące ustawienia do usługi.")}
                >
                  Przekaż ustawienia do usługi
                </button>
                <button
                  className="secondary"
                  disabled={busy}
                  title="Usługa to osobna kopia programu: po aktualizacji aplikacji zastąp ją nową wersją"
                  onClick={() => run(api.serviceInstall, "Usługa zaktualizowana do wersji tej aplikacji.")}
                >
                  {busy ? "Czekam na zgodę administratora…" : "Zaktualizuj usługę"}
                </button>
                <button
                  className="link danger-link"
                  disabled={busy}
                  onClick={() => {
                    if (confirm("Odinstalować usługę? Komputer przestanie być dostępny bez uruchomionej aplikacji.")) {
                      run(api.serviceUninstall, "Usługa odinstalowana.");
                    }
                  }}
                >
                  Odinstaluj usługę
                </button>
              </>
            )}
          </div>
          {status && <span className={status.ok ? "status online" : "error small"}>{status.text}</span>}
        </div>
      )}
    </>
  );
}
