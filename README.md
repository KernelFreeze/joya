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
