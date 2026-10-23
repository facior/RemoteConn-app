import type { VideoFrame as RemoteFrame } from "./api";

// Matches rc_codec::H264_CODEC_STRING (constrained baseline, level 5.1).
const H264_CODEC = "avc1.42E033";

/**
 * Hardware-accelerated H.264 decoding via WebCodecs, drawn onto a canvas.
 * Without a `description` the decoder expects Annex B, which is what the host
 * sends. After any decoder error we wait for the next keyframe.
 */
export class CanvasDecoder {
  private decoder: VideoDecoder | null = null;
  private ctx: CanvasRenderingContext2D;
  private waitingForKey = true;

  constructor(
    private canvas: HTMLCanvasElement,
    private onNeedKeyframe: () => void,
    private onResize: (width: number, height: number) => void,
  ) {
    const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
    if (!ctx) throw new Error("2D canvas unavailable");
    this.ctx = ctx;
  }

  static supported(): boolean {
    return typeof VideoDecoder !== "undefined";
  }

  private create(): VideoDecoder {
    const decoder = new VideoDecoder({
      output: (frame) => {
        const { displayWidth: w, displayHeight: h } = frame;
        if (this.canvas.width !== w || this.canvas.height !== h) {
          this.canvas.width = w;
          this.canvas.height = h;
          this.onResize(w, h);
        }
        this.ctx.drawImage(frame, 0, 0);
        frame.close();
      },
      error: (e) => {
        console.warn("decoder error", e);
        this.reset();
      },
    });
    decoder.configure({ codec: H264_CODEC, optimizeForLatency: true, hardwareAcceleration: "no-preference" });
    return decoder;
  }

  private reset() {
    try {
      this.decoder?.close();
    } catch {
      // already closed
    }
    this.decoder = null;
    this.waitingForKey = true;
    this.onNeedKeyframe();
  }

  push(frame: RemoteFrame) {
    if (this.waitingForKey && !frame.keyframe) return;
    if (!this.decoder || this.decoder.state === "closed") this.decoder = this.create();
    this.waitingForKey = false;
    try {
      this.decoder.decode(
        new EncodedVideoChunk({
          type: frame.keyframe ? "key" : "delta",
          timestamp: frame.timestampUs,
          data: frame.data,
        }),
      );
    } catch (e) {
      console.warn("decode failed", e);
      this.reset();
    }
  }

  close() {
    try {
      this.decoder?.close();
    } catch {
      // already closed
    }
    this.decoder = null;
  }
}
