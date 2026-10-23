import { useEffect, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import {
  api,
  ChatLine,
  Monitor,
  parseFrame,
  Peer,
  QualityPreset,
  RemoteMouse,
  SessionEvent,
  Transfer,
} from "./api";

interface RemoteCursor {
  url: string;
  width: number;
  height: number;
  hotX: number;
  hotY: number;
}

/** Browsers ignore CSS cursor images larger than this. */
const MAX_CSS_CURSOR = 128;
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { ChatPanel } from "./ChatPanel";
import { useNotify, useSettings } from "./settings";
import { pickFiles, TransferList, upsertTransfer, isActive } from "./Transfers";
import { CanvasDecoder } from "./decoder";
import { hidUsage } from "./keymap";

/**
 * Buffers channel messages until the session view mounts; the backend may
 * deliver the first keyframe before React renders the canvas.
 */
export class SessionChannels {
  private frameQueue: ArrayBuffer[] = [];
  private eventQueue: SessionEvent[] = [];
  private frameHandler: ((buf: ArrayBuffer) => void) | null = null;
  private eventHandler: ((ev: SessionEvent) => void) | null = null;

  constructor(frames: Channel<ArrayBuffer>, events: Channel<SessionEvent>) {
    frames.onmessage = (buf) => (this.frameHandler ? this.frameHandler(buf) : this.frameQueue.push(buf));
    events.onmessage = (ev) => (this.eventHandler ? this.eventHandler(ev) : this.eventQueue.push(ev));
  }

  attach(onFrame: (buf: ArrayBuffer) => void, onEvent: (ev: SessionEvent) => void) {
    this.frameHandler = onFrame;
    this.eventHandler = onEvent;
    this.eventQueue.splice(0).forEach(onEvent);
    this.frameQueue.splice(0).forEach(onFrame);
  }

  detach() {
    this.frameHandler = null;
    this.eventHandler = null;
  }
}

interface Stats {
  rttMs: number;
  lossPermille: number;
  receivedKbps: number;
  fps: number;
}

export function SessionView({
  peer,
  channels,
  onClose,
}: {
  peer: Peer;
  channels: SessionChannels;
  onClose: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [monitors, setMonitors] = useState<Monitor[]>([]);
  const [monitorId, setMonitorId] = useState(0);
  const [canControl, setCanControl] = useState(false);
  const [stats, setStats] = useState<Stats | null>(null);
  const [closedReason, setClosedReason] = useState("");
  const [hasVideo, setHasVideo] = useState(false);
  const [chat, setChat] = useState<ChatLine[]>([]);
  const [chatOpen, setChatOpen] = useState(false);
  const [unread, setUnread] = useState(0);
  const [cursor, setCursor] = useState<RemoteCursor | null>(null);
  const [cursorPos, setCursorPos] = useState<{ x: number; y: number; visible: boolean } | null>(null);
  const { view } = useSettings();
  const defaults = view?.settings;
  const showRemoteCursor = defaults?.showRemoteCursor ?? true;
  const notify = useNotify();
  const notifyRef = useRef(notify);
  notifyRef.current = notify;
  const [quality, setQuality] = useState<QualityPreset>(defaults?.defaultQuality ?? "balanced");
  const [fps, setFps] = useState(defaults?.defaultFps ?? 30);
  const [targetKbps, setTargetKbps] = useState(0);
  const [canSendFiles, setCanSendFiles] = useState(false);
  const [transfers, setTransfers] = useState<Transfer[]>([]);
  const [dragOver, setDragOver] = useState(false);
  const canSendFilesRef = useRef(false);
  canSendFilesRef.current = canSendFiles && !closedReason;
  const chatOpenRef = useRef(false);
  chatOpenRef.current = chatOpen;
  const monitorRef = useRef(0);
  monitorRef.current = monitorId;

  // Video + session events.
  useEffect(() => {
    const canvas = canvasRef.current!;
    const decoder = new CanvasDecoder(
      canvas,
      () => api.viewerRequestKeyframe().catch(() => {}),
      () => setHasVideo(true),
    );
    channels.attach(
      (buf) => {
        const frame = parseFrame(buf);
        if (frame) decoder.push(frame);
      },
      (ev) => {
        switch (ev.type) {
          case "monitors":
            setMonitors(ev.monitors);
            break;
          case "videoConfig":
            setMonitorId(ev.monitorId);
            setTargetKbps(ev.bitrateKbps);
            break;
          case "cursorShape":
            setCursor(ev);
            break;
          case "cursorPosition":
            if (ev.monitorId === monitorRef.current) setCursorPos(ev);
            break;
          case "permissions":
            setCanControl(ev.controlInput);
            setCanSendFiles(ev.fileTransfer);
            break;
          case "transfer":
            setTransfers((list) => upsertTransfer(list, ev.transfer));
            break;
          case "stats":
            setStats(ev);
            break;
          case "chat":
            setChat((c) => [...c, { mine: false, text: ev.text, at: Date.now() }]);
            if (!chatOpenRef.current) setUnread((n) => n + 1);
            notifyRef.current();
            break;
          case "closed":
            setClosedReason(ev.reason);
            break;
        }
      },
    );
    return () => {
      channels.detach();
      decoder.close();
    };
  }, [channels]);

  // Mouse: moves are coalesced to one per animation frame.
  useEffect(() => {
    if (!canControl || closedReason) return;
    const canvas = canvasRef.current!;
    let pendingMove: RemoteMouse | null = null;
    let raf = 0;

    const toRemote = (e: MouseEvent) => {
      const r = canvas.getBoundingClientRect();
      return {
        monitorId: monitorRef.current,
        x: Math.round(((e.clientX - r.left) * canvas.width) / r.width),
        y: Math.round(((e.clientY - r.top) * canvas.height) / r.height),
      };
    };
    const send = (ev: RemoteMouse) => api.viewerMouse(ev).catch(() => {});

    const onMove = (e: MouseEvent) => {
      pendingMove = { kind: "move", ...toRemote(e) };
      if (!raf)
        raf = requestAnimationFrame(() => {
          raf = 0;
          if (pendingMove) send(pendingMove);
          pendingMove = null;
        });
    };
    const onButton = (kind: "down" | "up") => (e: MouseEvent) => {
      e.preventDefault();
      if (kind === "down") canvas.focus();
      send({ kind, button: e.button, ...toRemote(e) });
    };
    const onDown = onButton("down");
    const onUp = onButton("up");
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      // Windows wheel units: 120 per notch, positive = away from the user.
      const scale = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 120 * 3 : 1.2;
      send({
        kind: "wheel",
        wheelDx: Math.round(e.deltaX * scale),
        wheelDy: Math.round(-e.deltaY * scale),
      });
    };
    const noMenu = (e: Event) => e.preventDefault();

    canvas.addEventListener("mousemove", onMove);
    canvas.addEventListener("mousedown", onDown);
    canvas.addEventListener("mouseup", onUp);
    canvas.addEventListener("wheel", onWheel, { passive: false });
    canvas.addEventListener("contextmenu", noMenu);
    return () => {
      cancelAnimationFrame(raf);
      canvas.removeEventListener("mousemove", onMove);
      canvas.removeEventListener("mousedown", onDown);
      canvas.removeEventListener("mouseup", onUp);
      canvas.removeEventListener("wheel", onWheel);
      canvas.removeEventListener("contextmenu", noMenu);
    };
  }, [canControl, closedReason]);

  // Keyboard: physical keys while the canvas has focus; released on blur.
  useEffect(() => {
    if (!canControl || closedReason) return;
    const canvas = canvasRef.current!;
    const held = new Set<number>();
    const onKey = (down: boolean) => (e: KeyboardEvent) => {
      const hid = hidUsage(e.code);
      if (hid === undefined) return;
      e.preventDefault();
      if (down) held.add(hid);
      else held.delete(hid);
      api.viewerKey(hid, down).catch(() => {});
    };
    const onDown = onKey(true);
    const onUp = onKey(false);
    const onBlur = () => {
      held.forEach((hid) => api.viewerKey(hid, false).catch(() => {}));
      held.clear();
    };
    canvas.addEventListener("keydown", onDown);
    canvas.addEventListener("keyup", onUp);
    canvas.addEventListener("blur", onBlur);
    canvas.focus();
    return () => {
      onBlur();
      canvas.removeEventListener("keydown", onDown);
      canvas.removeEventListener("keyup", onUp);
      canvas.removeEventListener("blur", onBlur);
    };
  }, [canControl, closedReason]);

  // Files dropped anywhere on the window are sent to the remote computer.
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((e) => {
      const p = e.payload;
      if (p.type === "enter" || p.type === "over") {
        setDragOver(canSendFilesRef.current);
      } else if (p.type === "leave") {
        setDragOver(false);
      } else if (p.type === "drop") {
        setDragOver(false);
        if (canSendFilesRef.current && p.paths.length > 0) api.viewerSendFiles(p.paths).catch(console.error);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  const sendFiles = async () => {
    const paths = await pickFiles();
    if (paths.length > 0) await api.viewerSendFiles(paths);
  };

  // Apply the default quality from settings when the session starts.
  useEffect(() => {
    api.viewerSetQuality(quality, fps).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const changeQuality = (preset: QualityPreset, maxFps: number) => {
    setQuality(preset);
    setFps(maxFps);
    api.viewerSetQuality(preset, maxFps).catch(console.error);
  };

  // While controlling, the remote pointer image becomes the local cursor:
  // correct shape with zero added latency.
  const cssCursor =
    showRemoteCursor && canControl && cursor && cursor.width <= MAX_CSS_CURSOR && cursor.height <= MAX_CSS_CURSOR
      ? `url(${cursor.url}) ${cursor.hotX} ${cursor.hotY}, default`
      : undefined;

  // View-only: draw the remote pointer where the remote user moves it.
  let pointerStyle: React.CSSProperties | null = null;
  const canvas = canvasRef.current;
  if (showRemoteCursor && !canControl && cursor && cursorPos?.visible && canvas && hasVideo && canvas.width > 0) {
    const scale = canvas.clientWidth / canvas.width;
    pointerStyle = {
      left: canvas.offsetLeft + (cursorPos.x - cursor.hotX) * scale,
      top: canvas.offsetTop + (cursorPos.y - cursor.hotY) * scale,
      width: cursor.width * scale,
      height: cursor.height * scale,
    };
  }

  const selectMonitor = (id: number) => {
    setMonitorId(id);
    setHasVideo(false);
    api.viewerSelectMonitor(id).catch(console.error);
  };

  return (
    <div className="session">
      <div className="toolbar">
        <span className="peer" title={`Adres: ${peer.addr}`}>
          <span className={closedReason ? "dot off" : "dot"} />
          <span className="peer-name">{peer.deviceName}</span>
        </span>
        {monitors.length > 1 && (
          <select value={monitorId} onChange={(e) => selectMonitor(Number(e.target.value))} aria-label="Monitor">
            {monitors.map((m, i) => (
              <option key={m.id} value={m.id}>
                Monitor {i + 1}{m.primary ? " (główny)" : ""} · {m.width}×{m.height}
              </option>
            ))}
          </select>
        )}
        <span
          className="badge"
          title={
            peer.relayed
              ? "Ruch idzie przez serwer relay (zaszyfrowany end-to-end)"
              : "Bezpośrednie połączenie peer-to-peer"
          }
        >
          {peer.relayed ? "Relay" : "P2P"}
        </span>
        {!canControl && !closedReason && <span className="badge">Podgląd</span>}
        <select
          value={quality}
          onChange={(e) => changeQuality(e.target.value as QualityPreset, fps)}
          aria-label="Jakość obrazu"
          disabled={!!closedReason}
        >
          <option value="balanced">Zrównoważona</option>
          <option value="quality">Najlepsza jakość</option>
          <option value="speed">Najszybsza</option>
        </select>
        <select
          value={fps}
          onChange={(e) => changeQuality(quality, Number(e.target.value))}
          aria-label="Klatki na sekundę"
          disabled={!!closedReason}
        >
          <option value={15}>15 fps</option>
          <option value={30}>30 fps</option>
          <option value={60}>60 fps</option>
        </select>
        <span className="spacer" />
        {stats && (
          <span
            className="stats"
            title={`Opóźnienie · utrata pakietów · przepływność · klatki/s${targetKbps ? ` (cel: ${(targetKbps / 1000).toFixed(1)} Mb/s)` : ""}`}
          >
            {stats.rttMs} ms · {(stats.lossPermille / 10).toFixed(1)}% · {(stats.receivedKbps / 1000).toFixed(1)} Mb/s ·{" "}
            {stats.fps} fps
          </span>
        )}
        {canControl && !closedReason && (
          <button
            className="secondary"
            onClick={() => api.viewerSpecialKey("ctrlAltDel").catch(console.error)}
            title="Ekran logowania / blokady na komputerze zdalnym (wymaga tam usługi RemoteConn)"
          >
            Ctrl+Alt+Del
          </button>
        )}
        <button
          className="secondary"
          onClick={() => sendFiles().catch(console.error)}
          disabled={!canSendFiles || !!closedReason}
          title={canSendFiles ? "Możesz też przeciągnąć pliki na okno" : "Komputer zdalny nie zezwolił na przesyłanie plików"}
        >
          Wyślij pliki
        </button>
        <button
          className={chatOpen ? "secondary active-toggle" : "secondary"}
          onClick={() => {
            setChatOpen((o) => !o);
            setUnread(0);
          }}
          aria-pressed={chatOpen}
        >
          Czat{unread > 0 ? ` (${unread})` : ""}
        </button>
        <button className="secondary" onClick={() => canvasRef.current?.requestFullscreen()}>
          Pełny ekran
        </button>
        <button className="danger" onClick={onClose}>
          Rozłącz
        </button>
      </div>
      <div className="session-body">
        <div className="stage">
          <canvas
            ref={canvasRef}
            tabIndex={0}
            className={hasVideo ? "" : "hidden"}
            style={cssCursor ? { cursor: cssCursor } : undefined}
          />
          {pointerStyle && cursor && <img className="remote-pointer" src={cursor.url} style={pointerStyle} alt="" />}
          {!hasVideo && !closedReason && <div className="overlay">Oczekiwanie na obraz…</div>}
          {dragOver && <div className="overlay dropzone">Upuść pliki, aby wysłać je na komputer zdalny</div>}
          <div className="transfers-dock">
            <TransferList
              transfers={transfers}
              onCancel={(id) => api.viewerCancelTransfer(id).catch(console.error)}
              onClear={() => setTransfers((list) => list.filter(isActive))}
            />
          </div>
          {closedReason && (
            <div className="overlay">
              <p>Sesja zakończona: {closedReason}</p>
              <button className="primary" onClick={onClose}>
                Wróć
              </button>
            </div>
          )}
        </div>
        {chatOpen && (
          <aside className="drawer" aria-label="Czat">
            <ChatPanel
              lines={chat}
              peerName={peer.deviceName}
              onSend={async (text) => {
                await api.viewerSendChat(text);
                setChat((c) => [...c, { mine: true, text, at: Date.now() }]);
              }}
            />
          </aside>
        )}
      </div>
    </div>
  );
}
