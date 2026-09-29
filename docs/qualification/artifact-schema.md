# Verification artifact schema

All evidence is UTF-8 JSON unless noted. Unknown fields are retained for provenance but do not satisfy a required field. No artifact can set product certification; generated reports always contain `certified: false`.

## Captured audio

`loopback` and `click-test` accept a headerless mono `f32le` stream supplied by the capture operator. For a multichannel capture, the operator must extract one documented channel before invoking the command. The operator records channel mapping, sample rate, capture device, stimulus hash, and start/end wall-clock time alongside the binary capture. Loopback also requires the exact emitted stimulus as `f32le`; its stimulus must be 1–4,096 samples. The command writes capture/stimulus lengths, best lag in samples, and normalized correlation. Click analysis writes sample count, peak adjacent-sample derivative, RMS derivative, and the supplied threshold.

## MIDI trace

`midi-timing --events` consumes an array of objects:

```json
[{"source":"coremidi","sent_micros":1000,"received_micros":1080}]
```

Both timestamps are unsigned monotonic microseconds and `received_micros >= sent_micros`. The trace must contain at least 100 records, and a live CoreMIDI input must be attached during evaluation. The resulting report includes count, min, mean, p50, p95, p99, and max microseconds.

## Supervised soak metrics

`soak --metrics` consumes an object containing at least:

```json
{
  "supervised": true,
  "hardware_attached": true,
  "operator_acknowledged": true,
  "duration_seconds": 28800,
  "faults": {"unexpected": 0, "contained": 0},
  "processes": [{
    "pid": 1234,
    "started": true,
    "exit_code": null,
    "resident_bytes_warmup": 104857600,
    "resident_bytes_end": 125829120
  }]
}
```

Each process needs all listed fields. `resident_bytes_warmup` is sampled after the defined warm-up interval, and `exit_code` must remain `null`. The tool derives memory growth from warm-up to end and rejects any process exceeding `--max-memory-growth-percent` (5% by default) or an optional absolute `--max-memory-growth-bytes` cap. The requested duration, hardware attachment, supervision, acknowledgement, zero unexpected faults, and every process metric are mandatory.

## Manual visual and accessibility evidence

Store a separate JSON record per review with `schema_version: 1`, `review_type` (`visual` or `accessibility`), `build_identifier`, `operator`, `started_at`, `completed_at`, `environment`, `checks`, and `verdict`. Each check has `id`, `result` (`pass` or `fail`), `evidence_path`, and `notes`; `verdict` is `pass` only when every required check passes. Required visual checks cover 1x/2x show-screen captures, every state token, and the fault line. Required accessibility checks cover keyboard traversal, visible focus, VoiceOver labels/roles/values, dynamic state announcements, and reduced-motion behavior. Missing records or a non-pass verdict are incomplete evidence, never an implicit pass.
