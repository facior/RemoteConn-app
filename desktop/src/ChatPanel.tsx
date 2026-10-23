import { useEffect, useRef, useState } from "react";
import type { ChatLine } from "./api";

const timeFormat = new Intl.DateTimeFormat("pl-PL", { hour: "2-digit", minute: "2-digit" });

/** Message list with an input; shared by the host card and the session view. */
export function ChatPanel({
  lines,
  peerName,
  onSend,
}: {
  lines: ChatLine[];
  peerName: string;
  onSend: (text: string) => Promise<void>;
}) {
  const [draft, setDraft] = useState("");
  const [error, setError] = useState("");
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    listRef.current?.scrollTo({ top: listRef.current.scrollHeight });
  }, [lines.length]);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    const text = draft.trim();
    if (!text) return;
    setError("");
    try {
      await onSend(text);
      setDraft("");
    } catch (err) {
      setError(String(err));
    }
  };

  return (
    <div className="chat">
      <div className="chat-lines" ref={listRef} aria-live="polite">
        {lines.length === 0 && <p className="muted small">Brak wiadomości.</p>}
        {lines.map((l, i) => (
          <div key={i} className={l.mine ? "chat-line mine" : "chat-line"}>
            <span className="chat-meta">
              {l.mine ? "Ty" : peerName} · {timeFormat.format(l.at)}
            </span>
            <span className="chat-text">{l.text}</span>
          </div>
        ))}
      </div>
      <form className="chat-input" onSubmit={submit}>
        <input
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder="Napisz wiadomość…"
          aria-label="Wiadomość"
          maxLength={4000}
          // Keys typed here must not reach the remote computer.
          onKeyDown={(e) => e.stopPropagation()}
          onKeyUp={(e) => e.stopPropagation()}
        />
        <button className="primary" disabled={!draft.trim()}>
          Wyślij
        </button>
      </form>
      {error && <p className="error small">{error}</p>}
    </div>
  );
}
