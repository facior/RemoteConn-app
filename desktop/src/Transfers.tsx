import { open } from "@tauri-apps/plugin-dialog";
import { api, Transfer } from "./api";

/** Inserts or updates a transfer, keeping first-seen order. */
export function upsertTransfer(list: Transfer[], t: Transfer): Transfer[] {
  const i = list.findIndex((x) => x.id === t.id);
  if (i === -1) return [...list, t];
  const next = list.slice();
  next[i] = t;
  return next;
}

export function isActive(t: Transfer) {
  return t.state === "preparing" || t.state === "running";
}

/** Opens the system file picker; resolves to the chosen paths (maybe none). */
export async function pickFiles(): Promise<string[]> {
  const chosen = await open({ multiple: true, directory: false, title: "Wybierz pliki do wysłania" });
  if (!chosen) return [];
  return Array.isArray(chosen) ? chosen : [chosen];
}

const sizeFormat = new Intl.NumberFormat("pl-PL", { maximumFractionDigits: 1 });

function formatSize(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${sizeFormat.format(value)} ${units[unit]}`;
}

function statusText(t: Transfer): string {
  switch (t.state) {
    case "preparing":
      return "Przygotowanie…";
    case "running":
      return `${formatSize(t.transferred)} z ${formatSize(t.total)}`;
    case "done":
      return t.incoming ? "Odebrano" : "Wysłano";
    case "failed":
      return `Błąd: ${t.error ?? "nieznany"}`;
    case "cancelled":
      return "Anulowano (można wznowić, wysyłając ponownie)";
  }
}

export function TransferList({
  transfers,
  onCancel,
  onClear,
}: {
  transfers: Transfer[];
  onCancel: (id: string) => void;
  onClear: () => void;
}) {
  if (transfers.length === 0) return null;
  const finished = transfers.some((t) => !isActive(t));
  return (
    <div className="transfers">
      <div className="transfers-head">
        <strong>Pliki</strong>
        <span className="spacer" />
        {finished && (
          <button className="link" onClick={onClear}>
            Wyczyść zakończone
          </button>
        )}
      </div>
      {transfers.map((t) => {
        const pct = t.total > 0 ? Math.min(100, (t.transferred / t.total) * 100) : 0;
        return (
          <div key={t.id} className={`transfer ${t.state}`}>
            <div className="transfer-row">
              <span className="transfer-dir" aria-label={t.incoming ? "odbierany" : "wysyłany"}>
                {t.incoming ? "↓" : "↑"}
              </span>
              <span className="transfer-name" title={t.name}>
                {t.name}
              </span>
              {isActive(t) && (
                <button className="link" onClick={() => onCancel(t.id)}>
                  Anuluj
                </button>
              )}
              {t.state === "done" && t.path && (
                <button className="link" onClick={() => api.revealPath(t.path!).catch(console.error)}>
                  Pokaż w folderze
                </button>
              )}
            </div>
            {isActive(t) && (
              <div
                className="progress"
                role="progressbar"
                aria-valuenow={Math.round(pct)}
                aria-valuemin={0}
                aria-valuemax={100}
              >
                <div style={{ width: `${t.state === "preparing" ? 0 : pct}%` }} />
              </div>
            )}
            <span className="muted small">{statusText(t)}</span>
          </div>
        );
      })}
    </div>
  );
}
