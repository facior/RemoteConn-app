import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ChatPanel } from "./ChatPanel";
import { isActive, pickFiles, TransferList, upsertTransfer } from "./Transfers";
import {
  AccountDevice,
  api,
  ChatLine,
  DeviceInfo,
  HostChat,
  HostTransfer,
  Transfer,
  HostSessionEvent,
  HostStatus,
  IncomingRequest,
  Peer,
  AutoAccepted,
  RendezvousEvent,
  SessionEvent,
} from "./api";
import { useNotify } from "./settings";
import { SettingsPage } from "./SettingsPage";
import { UpdateBanner } from "./Updater";
import { CanvasDecoder } from "./decoder";
import { IncomingDialog } from "./IncomingDialog";
import { SessionView, SessionChannels } from "./SessionView";

export default function App() {
  const [device, setDevice] = useState<DeviceInfo | null>(null);
  const [session, setSession] = useState<{ peer: Peer; channels: SessionChannels } | null>(null);
  const [page, setPage] = useState<"home" | "settings">("home");

  useEffect(() => {
    // Refreshed after leaving settings (the device name may have changed).
    if (page === "home") api.deviceInfo().then(setDevice).catch(console.error);
  }, [page]);

  // A remoteconn:// link (e.g. from the server panel) brings up the connect form.
  useEffect(() => {
    const unlisten = listen<string>("app://connect-to", () => setPage("home"));
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // The home screen stays mounted (only hidden) so sharing keeps receiving
  // requests, chat and transfer events while a session or settings is open.
  return (
    <>
    <div className="home" hidden={!!session || page !== "home"}>
      <header className="topbar">
        <div className="brand">
          <span className="logo" aria-hidden>
            ▣
          </span>
          RemoteConn
        </div>
        <span className="spacer" />
        {device && (
          <div className="device" title="Odcisk klucza tego urządzenia">
            {device.name} · <code>{device.fingerprint}</code>
          </div>
        )}
        <button className="secondary" onClick={() => setPage("settings")}>
          Ustawienia
        </button>
      </header>
      <UpdateBanner />
      <main className="cards">
        <HostCard />
        <ConnectCard onConnected={(peer, channels) => setSession({ peer, channels })} />
      </main>
      {!CanvasDecoder.supported() && (
        <p className="warning">Ten WebView nie obsługuje WebCodecs — podgląd zdalnego ekranu nie zadziała.</p>
      )}
    </div>
    {page === "settings" && !session && <SettingsPage onClose={() => setPage("home")} />}
    {session && (
      <SessionView
        peer={session.peer}
        channels={session.channels}
        onClose={() => {
          api.viewerDisconnect().catch(console.error);
          setSession(null);
        }}
      />
    )}
    </>
  );
}

function HostCard() {
  const [status, setStatus] = useState<HostStatus | null>(null);
  const [error, setError] = useState("");
  const [requests, setRequests] = useState<IncomingRequest[]>([]);
  const [active, setActive] = useState<Peer[]>([]);
  const [lastEnded, setLastEnded] = useState("");
  const [rendezvous, setRendezvous] = useState<RendezvousEvent | null>(null);
  const [chats, setChats] = useState<Record<number, ChatLine[]>>({});
  const [transfers, setTransfers] = useState<Record<number, Transfer[]>>({});
  const [autoAccepted, setAutoAccepted] = useState("");
  const notify = useNotify();
  const [appVersion, setAppVersion] = useState("");
  const [serviceUpdate, setServiceUpdate] = useState("");

  useEffect(() => {
    api
      .deviceInfo()
      .then((d) => setAppVersion(d.version))
      .catch(console.error);
  }, []);

  // The status carries the last rendezvous state: the service's host may
  // have gone online before this window was there to get the event.
  const showStatus = (s: HostStatus | null) => {
    setStatus(s);
    if (s?.rendezvous) setRendezvous(s.rendezvous);
  };

  // After an app update the service still runs the old program (it is a copy
  // in Program Files); replacing it needs the administrator's consent.
  const updateService = async () => {
    setServiceUpdate("Czekam na zgodę administratora…");
    try {
      await api.serviceInstall();
      setServiceUpdate("");
      showStatus(await api.hostStatus());
    } catch (e) {
      setServiceUpdate(String(e));
    }
  };

  // Sharing may already run (started on app launch).
  useEffect(() => {
    api
      .hostStatus()
      .then((s) => s && showStatus(s))
      .catch(console.error);
  }, []);

  useEffect(() => {
    const unlisteners = [
      listen<RendezvousEvent>("host://rendezvous", (e) => setRendezvous(e.payload)),
      listen<HostStatus>("host://restarted", (e) => {
        setActive([]);
        setRequests([]);
        setRendezvous(null);
        showStatus(e.payload);
      }),
      listen<string>("host://password", (e) => setStatus((s) => (s ? { ...s, password: e.payload } : s))),
      listen("service://state", () => {
        api
          .hostStatus()
          .then((s) => showStatus(s))
          .catch(() => setStatus(null));
      }),
      listen<{ ok: boolean; error: string }>("service://sync", (e) => {
        if (!e.payload.ok) setError(`Usługa nie dostała nowych ustawień: ${e.payload.error}`);
      }),
      listen<AutoAccepted>("host://auto-accepted", (e) => {
        const why =
          e.payload.reason === "account"
            ? `konto ${e.payload.peer.account ?? ""}`.trim()
            : e.payload.reason === "permanentPassword"
              ? "stałe hasło"
              : "zaufane urządzenie";
        setAutoAccepted(`${e.payload.peer.deviceName} połączył się automatycznie (${why}).`);
        notify();
      }),
      listen<IncomingRequest>("host://request", (e) => {
        setRequests((r) => [...r, e.payload]);
        notify();
      }),
      listen<HostChat>("host://chat", (e) => {
        notify();
        const { sessionId, text } = e.payload;
        setChats((c) => ({ ...c, [sessionId]: [...(c[sessionId] ?? []), { mine: false, text, at: Date.now() }] }));
      }),
      listen<HostTransfer>("host://transfer", (e) => {
        const { sessionId, transfer } = e.payload;
        setTransfers((t) => ({ ...t, [sessionId]: upsertTransfer(t[sessionId] ?? [], transfer) }));
      }),
      listen<HostSessionEvent>("host://session", (e) => {
        const ev = e.payload;
        if (ev.state === "started") {
          setActive((a) => [...a, ev.peer]);
        } else {
          setActive((a) => a.filter((p) => p.sessionId !== ev.peer.sessionId));
          setLastEnded(`${ev.peer.deviceName}: ${ev.reason}`);
        }
      }),
    ];
    return () => unlisteners.forEach((u) => u.then((f) => f()));
  }, [notify]);

  const start = async () => {
    setError("");
    setRendezvous(null);
    try {
      showStatus(await api.hostStart());
    } catch (e) {
      setError(String(e));
    }
  };
  const stop = async () => {
    await api.hostStop();
    setStatus(null);
    setActive([]);
    setRequests([]);
    setRendezvous(null);
  };
  const deviceId = rendezvous?.deviceId || status?.deviceId || "";
  const rotate = async () => {
    const password = await api.hostNewPassword();
    setStatus((s) => (s ? { ...s, password } : s));
  };

  return (
    <section className="card">
      <h2>Ten komputer</h2>
      <p className="muted">Udostępnij ekran osobie, której podasz ID (lub adres) i hasło.</p>
      {status ? (
        <>
          <dl className="credentials">
            {status.usesRendezvous && (
              <>
                <dt>ID</dt>
                <dd>
                  <code className="big">{deviceId || "—"}</code>
                  <span
                    className={rendezvous?.online ? "status online" : "status"}
                    title={rendezvous && !rendezvous.online ? rendezvous.reason : undefined}
                  >
                    <span className={rendezvous?.online ? "dot" : "dot off"} />
                    {rendezvous === null ? "łączenie z serwerem…" : rendezvous.online ? "online" : "offline"}
                  </span>
                </dd>
              </>
            )}
            <dt>Adres</dt>
            <dd>
              {status.addresses.map((a) => (
                <code key={a} className="big">
                  {a}:{status.port}
                </code>
              ))}
            </dd>
            <dt>Hasło jednorazowe</dt>
            <dd>
              <code className="big">{status.password}</code>
              <button className="link" onClick={rotate}>
                Nowe hasło
              </button>
            </dd>
            {status.permanentPasswordSet && (
              <>
                <dt>Stały dostęp</dt>
                <dd>
                  <span className="status online">
                    <span className="dot" /> włączony (stałe hasło)
                  </span>
                </dd>
              </>
            )}
          </dl>
          {autoAccepted && active.length > 0 && <p className="muted small">{autoAccepted}</p>}
          {active.map((peer) => (
            <ActiveSession
              key={peer.sessionId}
              peer={peer}
              lines={chats[peer.sessionId] ?? []}
              transfers={transfers[peer.sessionId] ?? []}
              onClearTransfers={() =>
                setTransfers((t) => ({ ...t, [peer.sessionId]: (t[peer.sessionId] ?? []).filter(isActive) }))
              }
              onSent={(text) =>
                setChats((c) => ({
                  ...c,
                  [peer.sessionId]: [...(c[peer.sessionId] ?? []), { mine: true, text, at: Date.now() }],
                }))
              }
            />
          ))}
          {lastEnded && active.length === 0 && <p className="muted small">Ostatnia sesja: {lastEnded}</p>}
          {/* A service from before 0.2.0 does not report its version at all. */}
          {status.service && appVersion && status.version !== appVersion && (
            <div className="notice">
              <span>
                Usługa ma starszą wersję ({status.version || "sprzed 0.2.0"}) niż aplikacja ({appVersion}).
                {serviceUpdate && <span className="muted small"> {serviceUpdate}</span>}
              </span>
              <button className="secondary" onClick={updateService}>
                Zaktualizuj usługę
              </button>
            </div>
          )}
          {status.service ? (
            <p className="muted small">
              Udostępnia usługa systemowa: komputer jest dostępny zawsze po włączeniu, także przed zalogowaniem.
            </p>
          ) : (
            <button className="secondary" onClick={stop}>
              Zatrzymaj udostępnianie
            </button>
          )}
        </>
      ) : (
        <button className="primary" onClick={start}>
          Zezwól na połączenia
        </button>
      )}
      {error && <p className="error">{error}</p>}
      {/* Portal: visible on every screen, also while viewing or in settings. */}
      {requests[0] &&
        createPortal(
          <IncomingDialog
            key={requests[0].id}
            request={requests[0]}
            onDone={() => setRequests((r) => r.slice(1))}
          />,
          document.body,
        )}
    </section>
  );
}

/** A viewer connected to this computer: chat and a way to end the session. */
function ActiveSession({
  peer,
  lines,
  transfers,
  onSent,
  onClearTransfers,
}: {
  peer: Peer;
  lines: ChatLine[];
  transfers: Transfer[];
  onSent: (text: string) => void;
  onClearTransfers: () => void;
}) {
  const [chatOpen, setChatOpen] = useState(false);
  const [seen, setSeen] = useState(0);
  const unread = chatOpen ? 0 : lines.filter((l) => !l.mine).length - seen;

  useEffect(() => {
    if (chatOpen) setSeen(lines.filter((l) => !l.mine).length);
  }, [chatOpen, lines]);

  return (
    <div className="active-session">
      <div className="active-head">
        <span className="active">
          <span className="dot" /> {peer.deviceName} {peer.relayed ? "(relay)" : ""}
        </span>
        <span className="spacer" />
        <button
          className="secondary"
          onClick={async () => {
            const paths = await pickFiles();
            if (paths.length > 0) await api.hostSendFiles(peer.sessionId, paths).catch(console.error);
          }}
        >
          Wyślij plik
        </button>
        <button className="secondary" onClick={() => setChatOpen((o) => !o)}>
          Czat{unread > 0 ? ` (${unread})` : ""}
        </button>
        <button className="danger" onClick={() => api.hostDisconnect(peer.sessionId).catch(console.error)}>
          Rozłącz
        </button>
      </div>
      <TransferList
        transfers={transfers}
        onCancel={(id) => api.hostCancelTransfer(peer.sessionId, id).catch(console.error)}
        onClear={onClearTransfers}
      />
      {chatOpen && (
        <ChatPanel
          lines={lines}
          peerName={peer.deviceName}
          onSend={async (text) => {
            await api.hostSendChat(peer.sessionId, text);
            onSent(text);
          }}
        />
      )}
    </div>
  );
}

function ConnectCard({ onConnected }: { onConnected: (peer: Peer, channels: SessionChannels) => void }) {
  const [address, setAddress] = useState(() => localStorageGet("lastAddress"));
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [fromLink, setFromLink] = useState(false);
  const passwordRef = useRef<HTMLInputElement>(null);

  // A link only fills in the ID; the user still types the password.
  useEffect(() => {
    const fill = (id: string | null) => {
      if (!id) return;
      api.takePendingLink().catch(() => null);
      setAddress(id.replace(/(\d{3})(\d{3})(\d{3})/, "$1 $2 $3"));
      setPassword("");
      setError("");
      setFromLink(true);
      setTimeout(() => passwordRef.current?.focus(), 50);
    };
    api.takePendingLink().then(fill).catch(console.error);
    const unlisten = listen<string>("app://connect-to", (e) => fill(e.payload));
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  const [account, setAccount] = useState(false);
  useEffect(() => {
    api
      .accountStatus()
      .then((a) => setAccount(a.loggedIn))
      .catch(() => setAccount(false));
    // Logging in or out happens in the settings while this stays mounted.
    const unlisten = listen<{ loggedIn: boolean }>("account://changed", (e) => setAccount(e.payload.loggedIn));
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  const connect = (e: React.FormEvent) => {
    e.preventDefault();
    connectTo(address, password);
  };

  const connectTo = async (address: string, password: string) => {
    setBusy(true);
    setError("");
    // Channels are created before connecting so no early frame or event is lost.
    const frames = new Channel<ArrayBuffer>();
    const events = new Channel<SessionEvent>();
    const channels = new SessionChannels(frames, events);
    try {
      const { peer } = await api.viewerConnect(address, password, frames, events);
      localStorageSet("lastAddress", address);
      onConnected(peer, channels);
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card">
      <h2>Połącz z komputerem</h2>
      <p className="muted">Wpisz ID (lub adres) i hasło widoczne na komputerze zdalnym.</p>
      <form onSubmit={connect} className="form">
        <label>
          ID lub adres
          <input
            value={address}
            onChange={(e) => {
              setAddress(e.target.value);
              setFromLink(false);
            }}
            placeholder="123 456 789 albo 192.168.1.20"
            required
            autoFocus
          />
        </label>
        {fromLink && <p className="muted small">Wpisano ID z linku. Podaj hasło widoczne na komputerze zdalnym.</p>}
        <label>
          Hasło
          <input
            ref={passwordRef}
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required={!account}
            placeholder={account ? "puste: wejdź kontem (Twoje komputery)" : undefined}
            autoComplete="off"
          />
        </label>
        <button className="primary" disabled={busy}>
          {busy ? "Czekam na akceptację…" : "Połącz"}
        </button>
      </form>
      {error && <p className="error">{error}</p>}
      {account && <MyComputers busy={busy} onConnect={(id) => connectTo(id, "")} />}
    </section>
  );
}

/** Computers on the account: one click, no password. */
function MyComputers({ busy, onConnect }: { busy: boolean; onConnect: (id: string) => void }) {
  const [devices, setDevices] = useState<AccountDevice[] | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    const load = () =>
      api
        .accountDevices()
        .then((d) => {
          setDevices(d.filter((x) => x.deviceId && !x.thisDevice));
          setError("");
        })
        .catch((e) => setError(String(e)));
    load();
    const timer = setInterval(load, 30_000);
    return () => clearInterval(timer);
  }, []);

  return (
    <div className="my-computers">
      <h3>Moje komputery</h3>
      {error && <p className="error small">{error}</p>}
      {devices && devices.length === 0 && (
        <p className="muted small">Zaloguj się na to konto na innych komputerach, a pojawią się tutaj.</p>
      )}
      {devices && devices.length > 0 && (
        <ul className="trusted">
          {devices.map((d) => (
            <li key={d.publicKey}>
              <span>
                <strong>{d.name || d.deviceId}</strong>
                <span className={d.online ? "status online small" : "muted small"}>
                  {d.online ? "online" : "offline"} · {d.deviceId}
                </span>
              </span>
              <button className="secondary" disabled={busy || !d.online} onClick={() => onConnect(d.deviceId)}>
                Połącz
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function localStorageGet(key: string): string {
  try {
    return localStorage.getItem(key) ?? "";
  } catch {
    return "";
  }
}

function localStorageSet(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // storage unavailable; not important
  }
}
