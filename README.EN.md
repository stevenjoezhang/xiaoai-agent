# XiaoAI Agent

> **中文文档：[README.md](README.md)**

![](https://forthebadge.com/images/badges/built-with-love.svg)
![](https://forthebadge.com/images/badges/made-with-rust.svg)
![](https://forthebadge.com/images/badges/powered-by-electricity.svg)
![](https://forthebadge.com/images/badges/makes-people-smile.svg)

An independent voice Agent that runs directly on XiaoAI speakers. With ASR and LLM service API configuration, the speaker can handle wake word detection, ASR, LLM conversation, tool calls, and TTS replies on device.
Unlike Open-XiaoAI and [MiGPT](https://github.com/idootop/mi-gpt), XiaoAI Agent does not require a dedicated server for running the Agent, and it does not compete with the native XiaoAI assistant for microphone input, answers, or Xiaomi cloud-side device control.
At the moment, it has only been successfully tested on Xiaomi Smart Speaker Pro (OH2P) firmware `1.62.2`. Other models and firmware versions require your own adaptation and are used at your own risk.

https://github.com/user-attachments/assets/b12d71b7-6734-4166-a2fe-959f82273702

## Features

- Fully takes over the voice conversation flow: to avoid competing with the native XiaoAI assistant for microphone input, answers, or Xiaomi cloud-side control, this project mutes the native XiaoAI microphone input. Real microphone audio is handled by `xiaoai-agent`, and replies are spoken through the speaker's system TTS command.
- No separate server required: the Agent runs directly on the speaker and no longer depends on an independent WebSocket message bridge.
- Reuses native device audio capabilities: it uses the firmware's built-in resident wake word and VPM audio callback mechanism for a polished audio experience; continuous conversation, VAD, interruption, echo cancellation, and recording during playback are supported.
- Supports tools and device control: it uses a modern Agent framework and includes built-in time, weather, web search, and Navidrome music playback tools, and can control smart home devices through Home Assistant MCP.
- Supports AirPlay audio output: the speaker can act as an AirPlay audio receiver and play audio streams from iPhone, iPad, Mac, and other devices.
- Preserves other system capabilities of the speaker: microphone input is taken over by `xiaoai-agent`, but non-voice-conversation services such as the Bluetooth gateway are not affected, and LED indicator animations can be customized.

## Repository Structure

```text
.
├── xiaoai-agent/              # Speaker-side Agent written in Rust
├── deploy/client-patch/       # Patch firmware files for SSH and startup hooks
├── deploy/flash-tool/         # macOS flashing helper tool
├── deploy/OH2P_1.62.2_BUILD_NOTES.md # OH2P build notes and pitfalls
├── upstream-open-xiaoai/      # Upstream Open-XiaoAI snapshot notes and licenses
└── AGENTS.md                  # Engineering supplement to the README
```

`deploy/client-patch/`, `deploy/flash-tool/`, and `upstream-open-xiaoai/` mainly come from other open-source projects.

## Usage Flow

### 1. Clone the Repository

```bash
git clone https://github.com/stevenjoezhang/xiaoai-agent.git
cd xiaoai-agent
```

### 2. Repack the Patched Firmware

To run XiaoAI Agent on the speaker, you need to use this repository to repack the patched firmware yourself, then flash a rootfs with SSH, startup scripts, and audio path adjustments. Do not directly use the upstream Open-XiaoAI prebuilt patched firmware; it does not include this project's patch for muting the native XiaoAI microphone input.

- Building the patched firmware and flashing: see [deploy/README.md](deploy/README.md)
- The author's OH2P 1.62.2 build notes: see [deploy/OH2P_1.62.2_BUILD_NOTES.md](deploy/OH2P_1.62.2_BUILD_NOTES.md)

The patched firmware provides SSH and the `/data/init.sh` startup hook, and it mutes the native XiaoAI microphone input to avoid conflicts with `xiaoai-agent`.

### 3. Build the Speaker-Side Agent

You can use the `xiaoai-agent` binary automatically built by GitHub Actions and download it from [Releases](https://github.com/stevenjoezhang/xiaoai-agent/releases).

You can also build it locally. Since the speaker is ARMv7 Linux, cross-compilation is usually required. First install the build toolchain:

```bash
rustup toolchain install 1.96.0
rustup target add armv7-unknown-linux-gnueabihf --toolchain 1.96.0
cargo install cargo-zigbuild
```

`cargo-zigbuild` also requires Zig. On macOS, install it with Homebrew:

```bash
brew install zig
```

When building the ARMv7 Linux binary for OH2P, use the fixed Rust version and the glibc 2.25 target:

```bash
(cd xiaoai-agent && cargo +1.96.0 zigbuild --release --target armv7-unknown-linux-gnueabihf.2.25)
```

For more cross-compilation and ABI notes, see [AGENTS.md](AGENTS.md).

### 4. Create the Runtime Configuration

To use the project normally, prepare API keys for the ASR service and the LLM service. For smart home control, HA-MCP is recommended; you do not need to create a separate Home Assistant long-lived token for the speaker.

```bash
cp xiaoai-agent/agent.example.yaml xiaoai-agent/agent.yaml
```

Then edit `xiaoai-agent/agent.yaml`:

- `asr.provider`: ASR backend. Available values are `open_ai`, `openai_realtime`, and `xiaomi_aivs`. `open_ai` uses OpenAI-compatible HTTP ASR configuration; `openai_realtime` uses the OpenAI Realtime transcription WebSocket event protocol; `xiaomi_aivs` reuses the speaker's native AIVS ASR and sends ASR-only `Execution.RequestControl` by default to avoid cloud-side NLP, TTS, and device-control side effects.
- `asr.open_ai.base_url`, `asr.open_ai.api_key`, `asr.open_ai.model`: OpenAI-compatible ASR service configuration
- `asr.openai_realtime.base_url`, `asr.openai_realtime.api_key`, `asr.openai_realtime.model`: OpenAI Realtime transcription service configuration. When `openai_realtime` is selected, `server_vad` is enabled by default: the client continuously sends enhanced VPM PCM, and the server's `speech_started` / `speech_stopped` events determine user utterance boundaries and automatically commit the final text. For FunASR, `target_sample_rate` must remain `16000`; compatible services that do not support server-side VAD can set `asr.openai_realtime.server_vad.enabled` to `false` to fall back to local energy endpoint detection.
- `llm.protocol`, `llm.base_url`, `llm.api_key`, `llm.model`: LLM service configuration. `protocol` can be `open_ai_chat` or `anthropic`; Anthropic-compatible services can control reasoning through `thinking.mode`.
- `mcp.servers`: generic MCP server list. Each server can independently configure URL, token, timeout, and tool allowlist.
- `music`: music service configuration. Navidrome is recommended; keep `music.enabled: false` if music is not needed.
- `runtime` / `capture`: wake word and recording parameters. Usually start with the example values.
- `airplay`: AirPlay audio output configuration. Disabled by default.

#### Home Assistant MCP (HA-MCP Recommended)

It is recommended to use the HA-MCP Custom Component from [homeassistant-ai/ha-mcp](https://github.com/homeassistant-ai/ha-mcp), rather than Home Assistant's built-in MCP Server. HA-MCP provides structured tools such as `ha_search`, `ha_get_state`, and `ha_call_service`: the Agent can first find the real `entity_id` by room and natural-language name, then call services according to the actual `domain`, avoiding cases where device display names or space-separated words are mistakenly treated as control parameters.

In Home Assistant, add the custom repository `https://github.com/homeassistant-ai/ha-mcp-integration` through HACS (category: Integration), install it, then restart Home Assistant. After that, go to "Settings -> Devices & services" and add HA-MCP Custom Component, choose HA-MCP Server, and copy the webhook connection URL from its "Configure" page. For a speaker on the same network, the URL looks like `http://<ha-host>:8123/api/webhook/<webhook-id>`.

In `agent.yaml`, configure only this generic MCP server:

```yaml
mcp:
  servers:
    - name: home_assistant
      enabled: true
      url: http://<ha-host>:8123/api/webhook/<webhook-id>
      token: ""
      timeout_s: 10
      tools:
        - ha_search
        - ha_get_state
        - ha_call_service
        - ha_bulk_control
        - ha_get_operation_status
```

The webhook URL itself is an access credential. Do not commit it to the repository. `mcp.home_assistant` is the old Home Assistant built-in MCP Server configuration and is kept only for compatibility; do not enable it at the same time as the HA-MCP server above.

### 5. Install on the Speaker

After flashing and confirming SSH works, install the `xiaoai-agent` binary and configuration to a persistent directory:

```bash
ssh root@<speaker-ip> 'mkdir -p /data/open-xiaoai'

scp -O xiaoai-agent/target/armv7-unknown-linux-gnueabihf/release/xiaoai-agent \
  root@<speaker-ip>:/data/open-xiaoai/xiaoai-agent

scp -O xiaoai-agent/agent.yaml \
  root@<speaker-ip>:/data/open-xiaoai/agent.yaml

ssh root@<speaker-ip> 'chmod +x /data/open-xiaoai/xiaoai-agent'
```

After logging into the speaker through SSH, run it manually first and confirm that wake word detection, recording, ASR, LLM replies, and TTS all work correctly:

```sh
RUST_LOG=debug /data/open-xiaoai/xiaoai-agent -c /data/open-xiaoai/agent.yaml
```

After confirmation, write `/data/init.sh` on the speaker for startup:

```sh
cat >/data/init.sh <<'EOF'
#!/bin/sh
RUST_LOG=info /data/open-xiaoai/xiaoai-agent -c /data/open-xiaoai/agent.yaml >>/data/open-xiaoai/xiaoai-agent.log 2>&1 &
EOF
chmod +x /data/init.sh
```

## How It Works

After startup, the Agent runs as a resident process:

1. It uses the firmware's native VPM/FlexKWS to listen for the wake word.
2. Each wake-up interrupts the current voice output or music playback and resets the current conversation turn.
3. It captures 16 kHz mono audio from the VPM ASR callback stream.
4. It recognizes text with the configured ASR backend, optionally OpenAI-compatible HTTP ASR, OpenAI Realtime transcription, or native Xiaomi AIVS ASR.
5. It passes the recognized text to the on-device Rig Agent and calls MCP, weather, music, and other tools as needed.
6. It speaks the reply using the XiaoAI speaker system TTS command.

## TODO

- [ ] Support speaker button control

## Disclaimer

This project is an unofficial technical research project and has no affiliation, cooperation, authorization, approval, or endorsement from Xiaomi or any of its affiliates.

Users are responsible for confirming that their use complies with applicable laws and regulations, platform rules, device vendor policies, and relevant service agreements, and they assume all risks and responsibilities arising from downloading, installing, configuring, modifying, distributing, or using this project.

For the full disclaimer, see [DISCLAIMER.md](./DISCLAIMER.md). The license and distribution terms are governed by the [LICENSE](./LICENSE) file in this repository.

## License and Sources

This repository contains this project's self-developed `xiaoai-agent/`, as well as deployment helper materials from projects such as Open-XiaoAI. The sources and licenses of upstream materials are documented in [upstream-open-xiaoai/](upstream-open-xiaoai/).
