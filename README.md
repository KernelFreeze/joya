# Joya

`Joya` is a real-time voice-to-voice translation, built in Rust with [GPUI](https://www.gpui.rs/) and [GPUI Component](https://longbridge.github.io/gpui-component/). Joya listens to the other side of a call or conference, translates what's said, and speaks it back in your language with low enough latency to hold a conversation.

Built for the [Cerebras and Google DeepMind Gemma 4 Hackathon](docs/Gemma%204%20Hackathon.md).

## Name

Joya means Gem in Spanish and it's also a synonym of "Gema" (Gem in spanish, sounds a lot like Gemma 4 😜).

## How it works

Joya runs a four-stage pipeline on live audio:

1. It captures audio frames from an output device (headphones, a virtual sink, and so on).
2. A speech-to-text model transcribes the speech.
3. The transcript goes to Gemma 4, running on Cerebras for fast inference, which translates it.
4. A text-to-speech model voices the translation and feeds it back as microphone input, so the other person hears it in their language.

The Cerebras inference step is what keeps the round trip fast enough to feel like a real conversation rather than a relay.

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

### 2. Virtual microphone

To feed the translation back into a call, create a virtual mic. Joya plays the
synthesized speech into a null sink whose *monitor* you select as the microphone
in your call app:

```sh
scripts/setup-virtual-mic.sh           # load (run once per session)
scripts/setup-virtual-mic.sh teardown  # remove
```

Then in your call app, choose **Monitor of Joya_Mic** as the microphone.

### 3. Configure

A default `config.yaml` is written on first run under your platform config dir
(`~/.config/joya/config.yaml` on Linux). Print the schema with:

```sh
cargo run -- schema
```

Key fields:

- `audio.capture_device` — the output/monitor device to listen to (run
  `cargo run -- list-devices`); `null` uses the default output.
- `audio.mic_sink` — set to `joya_mic` to feed the call; the translation is also
  played on your own speakers when `audio.monitor_self` is `true`.
- `languages.source` / `languages.target` — `source: null` auto-detects.
- `mistral.tts_voice` — optional Voxtral voice id.

## Run

```sh
cargo run -- list-devices   # enumerate audio devices
cargo run -- schema         # print the config JSON schema
cargo run --release         # launch the overlay
```
