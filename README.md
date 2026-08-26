# XiaoAI Agent

> **English documentation: [README.EN.md](README.EN.md)**

![](https://forthebadge.com/images/badges/built-with-love.svg)
![](https://forthebadge.com/images/badges/made-with-rust.svg)
![](https://forthebadge.com/images/badges/powered-by-electricity.svg)
![](https://forthebadge.com/images/badges/makes-people-smile.svg)

运行在小爱音箱端侧的独立语音 Agent。仅需配置外部 ASR 与大模型服务 API，即可在音箱端侧完成唤醒、ASR、LLM 对话、工具调用和 TTS 回复。
与 Open-XiaoAI 和 [MiGPT](https://github.com/idootop/mi-gpt) 项目不同，XiaoAI Agent 无需部署专门的服务端运行 Agent，也不会与原生小爱同学抢麦、抢答或触发小米云端控制。
目前仅在 Xiaomi 智能音箱 Pro（OH2P）固件 `1.62.2` 上测试成功，其他型号和固件版本需要自行适配并承担风险。

https://github.com/user-attachments/assets/b12d71b7-6734-4166-a2fe-959f82273702

## 特性

- 完全接管语音对话流程：为了避免和原生小爱同学抢麦、抢答或触发小米云端控制，本项目会将原生小爱的麦克风输入静音，真实麦克风音频由 `xiaoai-agent` 接管，使用音箱系统 TTS 命令播报回复。
- 无需单独搭建服务器：Agent 直接运行在音箱上，不再依赖独立的 WebSocket 消息桥接层。
- 复用设备原生音频能力：使用固件内置的常驻唤醒和 VPM 音频回调机制，音频体验完美；支持连续对话、VAD、中途打断、回声消除、播放时录音。
- 支持工具和设备控制：使用现代 Agent 框架支撑，内置时间、天气、网络搜索、Navidrome 音乐播放工具，并可通过 Home Assistant MCP 控制智能家居。
- 支持 AirPlay 音频输出：音箱可以作为 AirPlay 音频接收端，播放来自 iPhone、iPad、Mac 等设备的音频流。
- 保留音箱其它系统能力：麦克风输入会被 `xiaoai-agent` 接管，但蓝牙网关等非语音对话服务不受到影响，且 LED 指示灯动态可以自定义控制。

## 代码结构

```text
.
├── xiaoai-agent/              # Rust 编写的音箱端 Agent
├── deploy/client-patch/       # 用于制作带 SSH 和启动钩子的补丁固件
├── deploy/flash-tool/         # macOS 刷机辅助工具
├── deploy/OH2P_1.62.2_BUILD_NOTES.md # OH2P 构建踩坑记录
├── upstream-open-xiaoai/      # 上游 Open-XiaoAI 快照说明和许可证
└── AGENTS.md                  # README 的工程补充说明
```

`deploy/client-patch/`、`deploy/flash-tool/` 和 `upstream-open-xiaoai/` 主要来自其它开源项目。

## 使用流程

### 1. 克隆仓库

```bash
git clone https://github.com/stevenjoezhang/xiaoai-agent.git
cd xiaoai-agent
```

### 2. 重新打包补丁固件

为了在音箱上运行 XiaoAI Agent 程序，需要自行使用本仓库重新打包补丁固件，并刷入带 SSH 和启动脚本的 rootfs。不要直接使用上游 Open-XiaoAI 预构建的 patched 固件；语音链路调整由 Agent 启动脚本在运行时完成。

- 生成补丁固件和刷机：见 [deploy/README.md](deploy/README.md)
- 作者自己 OH2P 1.62.2 构建踩坑记录：见 [deploy/OH2P_1.62.2_BUILD_NOTES.md](deploy/OH2P_1.62.2_BUILD_NOTES.md)

补丁固件会提供 SSH 和 `/data/init.sh` 启动钩子；语音链路接管不写入固件。

### 3. 构建音箱端 Agent

可以直接使用 GitHub Actions 自动构建的 `xiaoai-agent` 程序，从 [Releases](https://github.com/stevenjoezhang/xiaoai-agent/releases) 下载即可。

也可以自行在本地构建。由于音箱端是 ARMv7 Linux，通常需交叉编译。先安装构建工具链：

```bash
rustup toolchain install 1.96.0
rustup target add armv7-unknown-linux-gnueabihf --toolchain 1.96.0
cargo install cargo-zigbuild
```

`cargo-zigbuild` 还需要 Zig。macOS 可以使用 Homebrew 安装：

```bash
brew install zig
```

构建给 OH2P 使用的 ARMv7 Linux 二进制时，使用固定 Rust 版本和 glibc 2.25 目标：

```bash
(cd xiaoai-agent && cargo +1.96.0 zigbuild --release --target armv7-unknown-linux-gnueabihf.2.25)
```

更多交叉编译和 ABI 注意事项见 [AGENTS.md](AGENTS.md)。

### 4. 创建运行配置

为了正常使用，需要准备 ASR 服务和大模型服务 API Key。智能家居控制推荐连接
HA-MCP，不需要为音箱单独创建 Home Assistant long-lived token。

```bash
cp xiaoai-agent/agent.example.yaml xiaoai-agent/agent.yaml
```

然后编辑 `xiaoai-agent/agent.yaml`：

- `asr.provider`：外部 ASR 后端，可选 `open_ai` 或 `openai_realtime`。
  `open_ai` 使用 OpenAI-compatible HTTP ASR 配置；`openai_realtime` 使用
  OpenAI Realtime transcription WebSocket 事件协议。
- `asr.open_ai.base_url`、`asr.open_ai.api_key`、`asr.open_ai.model`：
  OpenAI-compatible ASR 服务配置
- `asr.openai_realtime.base_url`、`asr.openai_realtime.api_key`、
  `asr.openai_realtime.model`：OpenAI Realtime transcription 服务配置；选择
  `openai_realtime` 时默认启用 `server_vad`：客户端持续发送增强后的 VPM PCM，
  服务端的 `speech_started` / `speech_stopped` 决定用户语句边界并自动提交最终文本。
  对 FunASR，`target_sample_rate` 必须保持 `16000`；不支持服务端 VAD 的兼容服务可将
  `asr.openai_realtime.server_vad.enabled` 设为 `false`，退回本地 energy 端点检测
- `llm.protocol`、`llm.base_url`、`llm.api_key`、`llm.model`：大模型服务配置。
  `protocol` 可选 `open_ai_chat` 或 `anthropic`；Anthropic-compatible 服务可通过
  `thinking.mode` 控制推理。
- `mcp.servers`：通用 MCP server 列表；每个 server 可独立配置 URL、token、timeout 和工具 allowlist
- `music`：音乐服务配置，推荐使用 Navidrome；不需要音乐功能时保持 `music.enabled: false`
- `runtime` / `capture`：唤醒和录音参数，通常先使用示例值
- `airplay`：AirPlay 音频输出配置，默认关闭

#### Home Assistant MCP（推荐 HA-MCP）

推荐使用 [homeassistant-ai/ha-mcp](https://github.com/homeassistant-ai/ha-mcp) 的
HA-MCP Custom Component，而不是 Home Assistant 内置 MCP Server。HA-MCP 提供
`ha_search`、`ha_get_state`、`ha_call_service` 等结构化工具：Agent 能先按房间和
自然语言名称找到真实 `entity_id`，再按实际 `domain` 调用服务，避免把设备显示名称
或空格分词误当成控制参数。

在 Home Assistant 中通过 HACS 添加自定义仓库
`https://github.com/homeassistant-ai/ha-mcp-integration`（类别选 Integration），安装后
重启 Home Assistant。随后在“设置 → 设备与服务”添加 HA-MCP Custom Component，选择
HA-MCP Server，并从其“配置”页面复制 webhook 连接地址。对同一网络内的音箱，地址形如
`http://<ha-host>:8123/api/webhook/<webhook-id>`。

在 `agent.yaml` 中仅配置这个通用 MCP server：

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

webhook URL 本身是访问凭据，勿提交到仓库。`mcp.home_assistant` 是旧版的 Home
Assistant 内置 MCP Server 配置，仅为兼容保留；不要与上述 HA-MCP server 同时启用。

### 5. 安装到音箱

刷机并确认 SSH 可用后，将 `xiaoai-agent` 二进制程序和配置安装到持久化目录：

```bash
ssh root@<speaker-ip> 'mkdir -p /data/open-xiaoai'

scp -O xiaoai-agent/target/armv7-unknown-linux-gnueabihf/release/xiaoai-agent \
  root@<speaker-ip>:/data/open-xiaoai/xiaoai-agent

scp -O xiaoai-agent/agent.yaml \
  root@<speaker-ip>:/data/open-xiaoai/agent.yaml

scp -O xiaoai-agent/start-agent.sh \
  root@<speaker-ip>:/data/open-xiaoai/start-agent.sh

ssh root@<speaker-ip> \
  'chmod +x /data/open-xiaoai/xiaoai-agent /data/open-xiaoai/start-agent.sh'
```

通过 SSH 登录音箱后，先手动运行，确认唤醒、录音、ASR、大模型回复和 TTS 都正常：

```sh
RUST_LOG=debug /data/open-xiaoai/start-agent.sh start
```

确认后，在音箱上写入 `/data/init.sh` 开机自启：

```sh
cat >/data/init.sh <<'EOF'
#!/bin/sh
exec /data/open-xiaoai/start-agent.sh start
EOF
chmod +x /data/init.sh
```

## 运行原理

Agent 启动后会常驻运行：

1. 使用固件原生 `mipns-*` 语音前端监听唤醒词。
2. 每次唤醒都会中断当前语音输出或音乐播放，并重置当前对话轮次。
3. 通过原生 `speech.usock` 协议接收 16 kHz 单声道 PCM 音频。
4. 使用配置的外部 ASR 后端识别文本，可选 OpenAI-compatible HTTP ASR 或 OpenAI
   Realtime transcription。
5. 把识别文本交给端侧 Rig Agent，并按需调用 MCP、天气、音乐等工具。
6. 使用小爱音箱系统 TTS 命令朗读回复。

## 分层设计

项目将开机入口、固件适配和 Agent 运行逻辑分为三层：

1. `/data/init.sh` 是统一的开机入口。补丁固件通过 `rc.local` 调用它，它本身只负责启动 `start-agent.sh`，不包含型号或业务逻辑。
2. `xiaoai-agent/start-agent.sh` 是系统和固件适配层，负责管理原生服务、设置 PNS PCM 模式、保护 `speech.usock`，以及启动检查、故障回滚和原生链路恢复。不同固件的进程名称、路径或启动方式存在差异时，应优先在这一层适配。
3. Rust 主程序是统一的 Agent 运行层，负责音频协议、对话状态、ASR、LLM、MCP、音乐和 TTS 等功能；固件服务和运行环境的编排原则上由启动脚本承担。

因此，只要目标固件提供兼容的 `mipns`/`speech.usock` 音频接口，同一套 Rust 程序就有机会直接复用，设备差异主要由启动脚本吸收。如果底层音频协议本身不同，则仍需为 Rust 主程序增加相应的输入适配。目前这种复用范围指兼容接口的小爱音箱 Linux 固件，并不表示程序可以在任意系统上原样运行。

## TODO

- [ ] 支持音箱按键控制

## 免责声明

本项目为非官方技术研究项目，与小米及其关联公司不存在任何隶属、合作、授权、认可或背书关系。

使用者应自行确认其使用行为符合适用法律法规、平台规则、设备厂商政策及相关服务协议，并自行承担由下载、安装、配置、修改、传播或使用本项目所产生的全部风险与责任。

详细免责声明请见 [DISCLAIMER.md](./DISCLAIMER.md)。项目授权与分发条件以仓库中的 [LICENSE](./LICENSE) 文件为准。

## 许可证和来源

本仓库包含本项目自研的 `xiaoai-agent/`，也包含来自 Open-XiaoAI 等项目的部署辅助材料。上游材料的来源和许可证见 [upstream-open-xiaoai/](upstream-open-xiaoai/)。
