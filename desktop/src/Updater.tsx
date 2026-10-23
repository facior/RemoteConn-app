import { useEffect, useState } from "react";
import { check, Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;

/**
 * Checks GitHub releases for a newer, signed version and offers to install it.
 * Nothing is installed without the user's click.
 */
export function UpdateBanner() {
  const [update, setUpdate] = useState<Update | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [progress, setProgress] = useState<number | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    const run = () =>
      check()
        .then((found) => {
          if (found) setUpdate(found);
        })
        .catch((e) => console.warn("update check failed", e));
    run();
    const timer = setInterval(run, CHECK_INTERVAL_MS);
    return () => clearInterval(timer);
  }, []);

  if (!update || dismissed) return null;

  const install = async () => {
    if (!confirm(`Zainstalować RemoteConn ${update.version}? Aplikacja uruchomi się ponownie, a trwające połączenia zostaną przerwane.`)) {
      return;
    }
    setError("");
    setProgress(0);
    let total = 0;
    let done = 0;
    try {
      await update.downloadAndInstall((event) => {
        if (event.event === "Started") total = event.data.contentLength ?? 0;
        if (event.event === "Progress") {
          done += event.data.chunkLength;
          setProgress(total ? Math.min(100, Math.round((done * 100) / total)) : 0);
        }
      });
      await relaunch();
    } catch (e) {
      setProgress(null);
      setError(`Nie udało się zainstalować aktualizacji: ${e}`);
    }
  };

  return (
    <div className="update-banner" role="status">
      <span>
        <strong>Dostępna nowa wersja RemoteConn {update.version}</strong>
        <span className="muted small"> (masz {update.currentVersion})</span>
        {update.body && <span className="muted small update-notes">{update.body.split("\n")[0]}</span>}
        {error && <span className="error small">{error}</span>}
      </span>
      <span className="spacer" />
      {progress !== null ? (
        <span className="muted">Pobieranie… {progress}%</span>
      ) : (
        <>
          <button className="primary" onClick={install}>
            Zainstaluj
          </button>
          <button className="link" onClick={() => setDismissed(true)}>
            Później
          </button>
        </>
      )}
    </div>
  );
}
