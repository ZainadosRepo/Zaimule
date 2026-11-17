/**
 * Native 60 FPS Canvas Video Capture (`.webm` VP9/VP8 via `canvas.captureStream(60)` + `MediaRecorder`)
 * and deterministic clip filename/duration formatting helpers.
 */

export function formatRecordingDuration(ms) {
  const safeMs = Math.max(0, Number(ms) || 0);
  const totalSec = Math.floor(safeMs / 1000);
  const mins = String(Math.floor(totalSec / 60)).padStart(2, "0");
  const secs = String(totalSec % 60).padStart(2, "0");
  const tenths = Math.floor((safeMs % 1000) / 100);
  return `${mins}:${secs}.${tenths}`;
}

export function buildClipFileName(gameId, ext = "webm") {
  const slug = String(gameId || "session")
    .toLowerCase()
    .replace(/[^a-z0-9-_]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return `zaimule-clip-${slug || "session"}.${ext}`;
}

export class ViewportRecorder {
  constructor() {
    this.mediaRecorder = null;
    this.chunks = [];
    this.isRecording = false;
    this.startTimeMs = 0;
    this.activeGameId = "session";
  }

  start(canvasEl, gameId = "session") {
    if (!canvasEl || typeof canvasEl.captureStream !== "function" || typeof MediaRecorder === "undefined") {
      this.isRecording = true;
      this.startTimeMs = Date.now();
      this.activeGameId = gameId;
      return true;
    }

    const stream = canvasEl.captureStream(60);
    const mimeType = MediaRecorder.isTypeSupported("video/webm;codecs=vp9")
      ? "video/webm;codecs=vp9"
      : "video/webm";

    this.chunks = [];
    this.activeGameId = gameId;
    this.startTimeMs = Date.now();
    this.mediaRecorder = new MediaRecorder(stream, { mimeType });
    this.mediaRecorder.ondataavailable = (ev) => {
      if (ev.data && ev.data.size > 0) {
        this.chunks.push(ev.data);
      }
    };
    this.mediaRecorder.onstop = () => {
      if (this.chunks.length > 0 && typeof Blob !== "undefined" && typeof URL !== "undefined") {
        const blob = new Blob(this.chunks, { type: "video/webm" });
        const url = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = buildClipFileName(this.activeGameId, "webm");
        document.body.appendChild(a);
        a.click();
        a.remove();
        setTimeout(() => URL.revokeObjectURL(url), 5000);
      }
    };
    this.mediaRecorder.start(250);
    this.isRecording = true;
    return true;
  }

  stop() {
    const elapsedMs = Date.now() - this.startTimeMs;
    if (this.mediaRecorder && this.mediaRecorder.state !== "inactive") {
      this.mediaRecorder.stop();
    }
    this.mediaRecorder = null;
    this.isRecording = false;
    return {
      fileName: buildClipFileName(this.activeGameId, "webm"),
      durationText: formatRecordingDuration(elapsedMs)
    };
  }
}
