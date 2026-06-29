# Joya

`Joya` is a real-time voice-to-voice translation, built in Rust with [GPUI](https://www.gpui.rs/) and [GPUI Component](https://longbridge.github.io/gpui-component/). Joya listens to the other side of a call or conference, translates what's said, and speaks it back in your language with low enough latency to hold a conversation.

Built for the [Cerebras and Google DeepMind Gemma 4 Hackathon](docs/Gemma%204%20Hackathon.md).

## Name

Joya means Gem in Spanish and it's also a synonym of "Gema" (Gem in spanish, sounds a lot like Gemma 4 😜).

## How it works

Joya runs a four-stage pipeline on live audio:

1. It captures the other party's audio from an output device (your headphones,
   or a monitor of the call's output).
2. A speech-to-text model transcribes the speech.
3. The transcript goes to Gemma 4, running on Cerebras for fast inference, which
   translates it into your language.
4. A text-to-speech model voices the translation and plays it to your
   headphones, so you hear it in your language.

The Cerebras inference step is what keeps the round trip fast enough to feel like a real conversation rather than a relay.

Joya supports two directions, independently toggleable in `config.yaml` and
both **runnable at once** for a two-way interpreter. They're color-coded in the
overlay:

- **`relay`** (gold, default) — headphones → STT → Gemma → TTS → headphones.
  The other party's speech, translated into your language, played to you.
- **`self`** (teal) — microphone → STT → Gemma → TTS → virtual mic (the call).
  Your speech, translated into their language, played to the other party (and
  to your headphones when `audio.output.monitor_self`). `self` translates the
  opposite way from `relay`, so the pair is a matched two-way interpreter.

## Backends

- **STT** — Mistral [Voxtral Realtime](https://mistral.ai/news/voxtral-transcribe-2/) over WebSocket.
- **Translation** — [Cerebras](https://inference-docs.cerebras.ai/capabilities/reasoning) Gemma 4 31B with reasoning.
- **TTS** — Mistral [Voxtral TTS](https://mistral.ai/news/voxtral-tts/).

## Setup

### 1. API keys

Joya reads `MISTRAL_API_KEY` and `CEREBRAS_API_KEY` from the environment (or you
can set them directly in `config.yaml`). For example:

```sh
with-bws-secrets cargo run --release
```

### 2. Audio routing

`scripts/setup-virtual-mic.sh` sets up the PipeWire routing Joya needs to feed
the call **and** to avoid listening to its own output. It creates two null sinks
plus a loopback:

- **`joya_mic`** — Joya plays the `self` direction's TTS here; its monitor is the
  microphone you select in your call app so the other party hears you.
- **`call_remote`** — the call app plays the *other party's* audio here (set the
  call's output device to **Call_Remote**). Joya captures `call_remote.monitor`
  for the `relay` direction, so `relay` hears only the other party.
- **loopback** — `call_remote.monitor` → your headphones, so you still hear the
  other party.

Joya's TTS goes to your headphones (`audio.output.playback_device`) but never to
`call_remote`, so there's no path from Joya's output back into `relay`'s capture
— no re-translating its own voice, and it stays full-duplex. Without this, the
default `relay` capture (the headphone monitor) would hear Joya's TTS too and
loop.

```sh
scripts/setup-virtual-mic.sh           # load (run once per session)
scripts/setup-virtual-mic.sh teardown  # remove
```

PipeWire modules don't persist across reboots, so run it once per session. Set
`JOYA_LISTEN_SINK=<sink>` (from `pactl list short sinks`) before running if your
default output isn't where you want to hear the other party.

### 3. Configure

A default `config.yaml` is written on first run under your platform config dir
(`~/.config/joya/config.yaml` on Linux). Print the schema with:

```sh
cargo run -- schema
```

Key fields:

- `audio.relay.enabled` / `audio.self.enabled` — which directions run. Joya
  supports two independent pipelines, and **both can run at once**:
  - **`relay`** (gold, enabled by default) captures the other party's voice from
    `audio.relay.capture_device` and plays the translation to your headphones.
    Set `capture_device` to `call_remote.monitor` (created by
    `scripts/setup-virtual-mic.sh`) so `relay` hears only the other party; `null`
    falls back to the default output's monitor, which also picks up Joya's own
    TTS and loops. Flow: **headphones → STT → Gemma → TTS → headphones**.
  - **`self`** (teal, disabled by default) captures your voice from
    `audio.self.capture_device` (`null` = default input) and plays the
    translation into the call's virtual mic (and your headphones when
    `audio.output.monitor_self`). Flow: **microphone → STT → Gemma → TTS → mic**.
    Watch for feedback if the mic can hear the speakers — use headphones.
  In dual mode (both `enabled: true`) the two flows are color-coded in the
  overlay and `self` automatically translates the opposite way from `relay`
  (your `source` language → the other party's `target` language), so the pair
  forms a matched two-way interpreter.
- `audio.output.mic_sink` / `audio.output.playback_device` /
  `audio.output.monitor_self` — shared output routing, independent of direction.
  `mic_sink` feeds the call (the `self` direction); `playback_device` is your
  headphones (the `relay` direction, and `self` when `monitor_self`). Set
  `mic_sink` to the virtual mic created by `scripts/setup-virtual-mic.sh`
  (e.g. `joya_mic`).
- `languages.source` / `languages.target` — framed from your point of view:
  `source` is the language you speak, `target` is the language the other party
  speaks. `source: null` auto-detects your speech. `self` translates
  `source → target`; `relay` translates the swapped pair.
- `mistral.tts_voice` — optional Voxtral voice id.

## Run

```sh
cargo run -- list-devices   # enumerate audio devices
cargo run -- schema         # print the config JSON schema
cargo run --release         # launch the overlay
```

## Troubleshooting: "no input" / mic not capturing

Joya logs at `info` level by default (override with `RUST_LOG=debug` or
`RUST_LOG=joya=trace`). On launch it prints, per direction, which device it
resolved and the sample rate/channels:

```
 INFO joya::audio: [self] Capture device: 'Scarlett Solo USB' [id=alsa_input.usb-…HiFi__Mic1__source] rate=44100, ch=1 (direction=SelfMode)
```

Then, every 0.5s per active direction, it logs the input level:

```
 INFO joya::audio: [self] input level: rms=0.0250 peak=0.1349
```

Read it as:

- **`no audio samples in 0.5s`** — the device opened but cpal is delivering no
  callbacks at all. The device is wrong, paused, or in use exclusively.
- **`input appears dead: rms=0.00000 peak=0.00000`** repeatedly — samples are
  arriving but they're all zero. Usually a muted source, a monitor of a silent
  sink, or (common on PipeWire) the **wrong device profile**. For a Scarlett
  interface, set the card profile to `HiFi` so the real mic source is exposed:
  `pactl set-card-profile alsa_card.usb-Focusrite_… HiFi` (the `Direct` profile
  only exposes the interface's monitor, which is silent unless you're also
  playing audio into it).
- **`(quiet — below typical speech)`** — signal is present but low; speak
  louder, raise the interface's input gain, or move the mic closer.
- **`peak` above ~0.1** but no `Speech started` log — the VAD (threshold 0.5)
  isn't classifying it as speech; very rare, indicates noise or the wrong device.

### Picking the right device when names collide

On PipeWire, several nodes on the same card can share a friendly name (every
Scarlett input reads `"Scarlett Solo USB"`). `list-devices` prints a unique `id:`
line under each name:

```
Input devices (default: input_default)
  Scarlett Solo USB
    id: alsa_input.usb-Focusrite_…HiFi__Mic1__source
  Scarlett Solo USB
    id: alsa_input.usb-Focusrite_…HiFi__Mic2__source
```

`capture_device` matches **either** the friendly name **or** the `id`, so set it
to the full `id:` string to target the exact node.

### Confirm the device carries signal outside Joya

```sh
pw-record --target '<id from list-devices>' --format f32 --container wav /tmp/test.wav
ffmpeg -i /tmp/test.wav -af volumedetect -f null -   # check mean/max volume
```

If that records silence too, the problem is the device/profile (PipeWire/ALSA
side), not Joya.
