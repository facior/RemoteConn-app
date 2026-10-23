import { useEffect, useState } from "react";
import { api, GrantedPermissions, IncomingRequest } from "./api";

// Host-side accept dialog; the backend rejects automatically after 60 s.
const TIMEOUT_S = 60;

const PERMISSIONS: { key: keyof GrantedPermissions; label: string }[] = [
  { key: "viewScreen", label: "Podgląd ekranu" },
  { key: "controlInput", label: "Sterowanie myszą i klawiaturą" },
  { key: "clipboard", label: "Wspólny schowek" },
  { key: "fileTransfer", label: "Przesyłanie plików" },
];

export function IncomingDialog({ request, onDone }: { request: IncomingRequest; onDone: () => void }) {
  const [granted, setGranted] = useState<GrantedPermissions>(request.permissions);
  const [trust, setTrust] = useState(false);
  const [left, setLeft] = useState(TIMEOUT_S);

  useEffect(() => {
    const t = setInterval(() => setLeft((s) => s - 1), 1000);
    return () => clearInterval(t);
  }, [request.id]);

  useEffect(() => {
    if (left <= 0) onDone();
  }, [left, onDone]);

  const respond = async (accept: boolean) => {
    try {
      await api.hostRespond(request.id, accept, granted, accept && trust);
    } catch (e) {
      console.warn(e);
    }
    onDone();
  };

  return (
    <div className="backdrop" role="dialog" aria-modal="true" aria-labelledby="incoming-title">
      <div className="dialog">
        <h3 id="incoming-title">Przychodzące połączenie</h3>
        <p>
          <strong>{request.peer.deviceName || "Nieznane urządzenie"}</strong> ({request.peer.addr}
          {request.peer.relayed ? ", przez relay" : ""}) chce się połączyć z tym komputerem.
          {request.peer.auth === "permanent" && " Użyto stałego hasła."}
        </p>
        <p className="muted small">
          Odcisk klucza: <code>{request.peer.fingerprint}</code>
        </p>
        <fieldset className="perms">
          <legend>Uprawnienia</legend>
          {PERMISSIONS.map(({ key, label }) => (
            <label key={key}>
              <input
                type="checkbox"
                checked={granted[key]}
                onChange={(e) => setGranted((g) => ({ ...g, [key]: e.target.checked }))}
              />
              {label}
            </label>
          ))}
        </fieldset>
        <label className="toggle">
          <input type="checkbox" checked={trust} onChange={(e) => setTrust(e.target.checked)} />
          Ufaj temu urządzeniu (następnym razem bez pytania, hasło nadal wymagane)
        </label>
        <div className="actions">
          <button className="secondary" onClick={() => respond(false)}>
            Odrzuć
          </button>
          <button className="primary" onClick={() => respond(true)} autoFocus>
            Akceptuj ({left}s)
          </button>
        </div>
      </div>
    </div>
  );
}
