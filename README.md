# Joya

`Joya` is a real-time voice-to-voice translator built in Rust with [GPUI](https://www.gpui.rs/) and [GPUI Component](https://longbridge.github.io/gpui-component/). It listens to the other side of a call, translates what's said, and speaks it back in your language fast enough to hold a conversation.

Built for the Cerebras and Google DeepMind Gemma 4 Hackathon.

## Name

Joya means "gem" in Spanish, and it's a synonym of "Gema", which sounds a lot like Gemma 4.

## How it works

Joya runs a four-stage pipeline on live audio: it captures the other party's voice, transcribes it ([Voxtral Realtime](https://mistral.ai/news/voxtral-transcribe-2/) over WebSocket), translates it ([Cerebras](https://inference-docs.cerebras.ai/capabilities/reasoning) Gemma 4 31B), and voices the result back to your headphones ([Voxtral TTS](https://mistral.ai/news/voxtral-tts/)). Cerebras inference is what keeps the round trip fast enough to feel like a conversation.

There are two directions, toggleable in `config.yaml` and runnable at once for a two-way interpreter:

- **`relay`** (gold, default) — the other party's speech → your language, played to your headphones.
- **`self`** (teal) — your speech → their language, played into the call's virtual mic.

In dual mode the two are color-coded in the overlay, and `self` translates the opposite way from `relay`, so the pair forms a matched interpreter.

## Setup

### 1. API keys

Joya reads `MISTRAL_API_KEY` and `CEREBRAS_API_KEY` from the environment (or set them in `config.yaml`):

```sh
with-bws-secrets cargo run --release
```

### 2. Audio routing

`scripts/setup-virtual-mic.sh` sets up the PipeWire routing Joya needs to feed the call without listening to its own output. It creates two null sinks and a loopback:

- **`joya_mic`** — Joya plays `self` TTS here; its monitor is the mic you select in your call app.
- **`call_remote`** — the call app plays the other party's audio here (set the call's output to **Call_Remote**). Joya captures `call_remote.monitor` for `relay`, so `relay` hears only the other party.
- **loopback** — `call_remote.monitor` → your headphones, so you still hear the call.

Joya's TTS goes to your headphones but never to `call_remote`, so there's no path from its output back into `relay`'s capture — no re-translating its own voice.

```sh
scripts/setup-virtual-mic.sh           # load (run once per session)
scripts/setup-virtual-mic.sh teardown  # remove
```

PipeWire modules don't survive a reboot, so run it each session. Set `JOYA_LISTEN_SINK=<sink>` (from `pactl list short sinks`) before running if your default output isn't where you want to hear the call.

### 3. Configure

A default `config.yaml` is written on first run under your platform config dir (`~/.config/joya/config.yaml` on Linux). Print the schema with `cargo run -- schema`. Key fields:

- `audio.relay.enabled` / `audio.self.enabled` — which directions run; both can run at once. Set `audio.relay.capture_device` to `call_remote` so `relay` hears only the other party (`null` falls back to the default output's monitor, which also picks up Joya's TTS and loops). `audio.self.capture_device` is your mic (`null` = default input).
- `audio.output.mic_sink` / `playback_device` / `monitor_self` — `mic_sink` feeds the call (set it to `joya_mic`); `playback_device` is your headphones; `monitor_self` also plays `self` output to your headphones.
- `languages.source` / `target` — from your point of view: `source` is what you speak, `target` is what the other party speaks. `source: null` auto-detects. `self` translates `source → target`; `relay` translates the swapped pair.
- `mistral.tts_voice` — default Voxtral voice id. `mistral.tts_voices` — per-language ids keyed by language name (e.g. `English`, `Spanish`); falls back to `tts_voice`.

## Run

```sh
cargo run -- list-devices   # enumerate audio devices
cargo run -- schema         # print the config JSON schema
cargo run --release         # launch the overlay
```

## Troubleshooting: "no input" / mic not capturing

Joya logs at `info` by default (override with `RUST_LOG=debug` or `RUST_LOG=joya=trace`). On launch it prints the resolved device per direction, then logs the input level every 0.5s. Read the messages as:

- **`no audio samples in 0.5s`** — the device opened but cpal delivers no callbacks. Wrong device, paused, or in exclusive use.
- **`input appears dead: rms=0.00000 peak=0.00000`** — samples arrive but are all zero. Usually a muted source, the monitor of a silent sink, or the wrong PipeWire profile. For a Scarlett interface, set the card to `HiFi` (`pactl set-card-profile alsa_card.usb-Focusrite_… HiFi`); the `Direct` profile only exposes the silent monitor.
- **`(quiet — below typical speech)`** — signal present but low; raise input gain or move closer.
- **`peak` above ~0.1 but no `Speech started`** — the VAD (threshold 0.5) isn't classifying it as speech; rare, usually noise or the wrong device.

### Picking the right device when names collide

On PipeWire several nodes can share a friendly name (every Scarlett input reads `"Scarlett Solo USB"`). `list-devices` prints a unique `id:` under each. `capture_device` matches either the friendly name or the `id`, so use the full `id:` string to target an exact node.

### Confirm the device carries signal outside Joya

```sh
pw-record --target '<id from list-devices>' --format f32 --container wav /tmp/test.wav
ffmpeg -i /tmp/test.wav -af volumedetect -f null -   # check mean/max volume
```

If that records silence too, the problem is the device or profile, not Joya.
