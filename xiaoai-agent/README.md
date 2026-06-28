# dodo-xiaoai-agent

Standalone XiaoAI on-device agent experiment.

This crate removes the previous client/server WebSocket boundary. The agent is
intended to run directly on the speaker:

1. run the native VPM/FlexKWS monitor for wake-word events
2. interrupt the current turn on every new wake event
3. capture one utterance from the VPM ASR callback stream
4. send the utterance to an OpenAI-compatible ASR endpoint
5. run the Rig agent with optional MCP and music tools
6. speak the reply with XiaoAI's local TTS command

It does not contain any WebSocket client/server code.

## Build

```bash
cargo check
cargo build --release
```

The default config path on device is:

```text
/data/open-xiaoai/agent.yaml
```

See [examples/agent.yaml](examples/agent.yaml).

## Current Notes

- KWS is driven directly through the firmware `libvpm.so`/`libflexkws.so`
  stack; do not reintroduce tailing native XiaoAI logs for wake detection.
- When `capture.pcm` is `vpm_asr`, utterance capture uses VPM's 16 kHz mono
  ASR callback stream instead of a separate raw `arecord` process.
- ASR uses `POST /v1/audio/transcriptions` with multipart `file` and `model`.
- ASR WAV packaging is done in memory; no temporary audio file is written.
- NetEase login cookies are kept in memory. `music.netease.cookie_file` is only
  read as an optional seed and is never written back by this program.
- The release build measured on macOS arm64 is a host reference only; a Linux
  target build is still needed before deploying to the speaker.
