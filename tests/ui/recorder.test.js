import test from "node:test";
import assert from "node:assert/strict";
import { formatRecordingDuration, buildClipFileName, ViewportRecorder } from "../../ui/recorder.js";

test("formatRecordingDuration formats minutes, seconds, and tenths accurately", () => {
  assert.equal(formatRecordingDuration(0), "00:00.0");
  assert.equal(formatRecordingDuration(12450), "00:12.4");
  assert.equal(formatRecordingDuration(65900), "01:05.9");
});

test("buildClipFileName sanitizes game IDs and appends extension", () => {
  assert.equal(
    buildClipFileName("gba-golden-horizon", "webm"),
    "zaimule-clip-gba-golden-horizon.webm"
  );
  assert.equal(
    buildClipFileName("Pokémon Émeraude!", "webm"),
    "zaimule-clip-pok-mon-meraude.webm"
  );
});

test("ViewportRecorder toggles recording state cleanly in headless environment", () => {
  const rec = new ViewportRecorder();
  assert.equal(rec.isRecording, false);
  rec.start(null, "gb-pocket-arena");
  assert.equal(rec.isRecording, true);
  const summary = rec.stop();
  assert.equal(rec.isRecording, false);
  assert.equal(summary.fileName, "zaimule-clip-gb-pocket-arena.webm");
});
