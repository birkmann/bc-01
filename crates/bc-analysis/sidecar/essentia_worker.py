#!/usr/bin/env python
"""Persistent essentia worker for bc-analysis (fallback analyser for tracks without BPM/key).

Protocol: one JSON object per stdin line {"id": <any>, "path": "<file>"}; one JSON object per
stdout line {"id", "bpm", "bpm_confidence", "beat_offset_ms", "bpm_candidates", "key_root",
"key_mode", "key_confidence", "duration_s", "error"}. Same rules as the legacy
services/analysis/backends.py: 22.05 kHz mono, 120 s excerpt starting 25 % in,
RhythmExtractor2013(multifeature) with confidence/5.32, KeyExtractor(edma).
No DB access; the parent writes results.
"""
import json
import sys

PITCH = {"C": 0, "C#": 1, "Db": 1, "D": 2, "D#": 3, "Eb": 3, "E": 4, "F": 5, "F#": 6, "Gb": 6,
         "G": 7, "G#": 8, "Ab": 8, "A": 9, "A#": 10, "Bb": 10, "B": 11}
SR = 22050
EXCERPT_S = 120
START_PCT = 0.25


def analyse(es, path):
    audio = es.MonoLoader(filename=path, sampleRate=SR)()
    dur = len(audio) / SR
    if dur > EXCERPT_S:
        start = int(min(dur * START_PCT, dur - EXCERPT_S) * SR)
        window = audio[start:start + EXCERPT_S * SR]
    else:
        window = audio
    out = {"duration_s": round(dur, 2)}
    bpm, beats, conf, _, _ = es.RhythmExtractor2013(method="multifeature")(window)
    bpm = float(bpm)
    out["bpm"] = round(bpm, 2) if bpm else None
    out["bpm_confidence"] = round(min(1.0, float(conf) / 5.32), 3) if conf else None
    out["beat_offset_ms"] = round(float(beats[0]) * 1000, 1) if len(beats) else None
    out["bpm_candidates"] = [c for c in (round(bpm * m, 2) for m in (0.5, 2.0)) if 40 <= c <= 250] if bpm else []
    key, scale, strength = es.KeyExtractor(profileType="edma")(window)
    out["key_root"] = PITCH.get(str(key))
    out["key_mode"] = str(scale).lower()
    out["key_confidence"] = round(float(strength), 3) if strength is not None else None
    return out


def main():
    import essentia
    import essentia.standard as es
    essentia.log.infoActive = False
    essentia.log.warningActive = False
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        req = json.loads(line)
        try:
            res = analyse(es, req["path"])
            res["error"] = None
        except Exception as exc:  # a bad file must not kill the worker
            res = {"error": str(exc)[:300]}
        res["id"] = req.get("id")
        sys.stdout.write(json.dumps(res) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
