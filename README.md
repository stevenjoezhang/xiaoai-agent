# XiaoAI Agent

![](https://forthebadge.com/images/badges/built-with-love.svg)
![](https://forthebadge.com/images/badges/made-with-rust.svg)
![](https://forthebadge.com/images/badges/powered-by-electricity.svg)
![](https://forthebadge.com/images/badges/makes-people-smile.svg)

运行在小爱音箱端侧的独立语音 Agent。仅需配置 ASR 与大模型服务 API，即可在音箱端侧完成唤醒、ASR、LLM 对话、工具调用和 TTS 回复。
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

为了在音箱上运行 XiaoAI Agent 程序，需要自行使用本仓库重新打包补丁固件，并刷入带 SSH、启动脚本和音频路径调整的 rootfs。不要直接使用上游 Open-XiaoAI 预构建的 patched 固件；它不包含本项目用于静音原生小爱麦克风输入的补丁。

- 生成补丁固件和刷机：见 [deploy/README.md](deploy/README.md)
- 作者自己 OH2P 1.62.2 构建踩坑记录：见 [deploy/OH2P_1.62.2_BUILD_NOTES.md](deploy/OH2P_1.62.2_BUILD_NOTES.md)

补丁固件会提供 SSH 和 `/data/init.sh` 启动钩子，并让原生小爱的麦克风输入静音，避免与 `xiaoai-agent` 冲突。

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

为了正常使用，需要准备 ASR 服务和大模型服务 API Key。可选配置包含 Home Assistant Token 等。

```bash
cp xiaoai-agent/agent.example.yaml xiaoai-agent/agent.yaml
```

然后编辑 `xiaoai-agent/agent.yaml`：

- `asr.provider`：ASR 后端，可选 `open_ai` 或 `xiaomi_aivs`。`open_ai` 使用
  OpenAI-compatible ASR 配置；`xiaomi_aivs` 复用音箱原生 AIVS ASR，并默认发送
  ASR-only `Execution.RequestControl`，避免云端 NLP/TTS/设备控制副作用。
- `asr.open_ai.base_url`、`asr.open_ai.api_key`、`asr.open_ai.model`：
  OpenAI-compatible ASR 服务配置
- `llm.base_url`、`llm.api_key`、`llm.model`：大模型服务配置
- `mcp.home_assistant`：Home Assistant MCP 配置
- `music`：音乐服务配置，推荐使用 Navidrome；不需要音乐功能时保持 `music.enabled: false`
- `runtime` / `capture`：唤醒和录音参数，通常先使用示例值
- `airplay`：AirPlay 音频输出配置，默认关闭

### 5. 安装到音箱

刷机并确认 SSH 可用后，将 `xiaoai-agent` 二进制程序和配置安装到持久化目录：

```bash
ssh root@<speaker-ip> 'mkdir -p /data/open-xiaoai'

scp -O xiaoai-agent/target/armv7-unknown-linux-gnueabihf/release/xiaoai-agent \
  root@<speaker-ip>:/data/open-xiaoai/xiaoai-agent

scp -O xiaoai-agent/agent.yaml \
  root@<speaker-ip>:/data/open-xiaoai/agent.yaml

ssh root@<speaker-ip> 'chmod +x /data/open-xiaoai/xiaoai-agent'
```

通过 SSH 登录音箱后，先手动运行，确认唤醒、录音、ASR、大模型回复和 TTS 都正常：

```sh
RUST_LOG=debug /data/open-xiaoai/xiaoai-agent -c /data/open-xiaoai/agent.yaml
```

确认后，在音箱上写入 `/data/init.sh` 开机自启：

```sh
cat >/data/init.sh <<'EOF'
#!/bin/sh
RUST_LOG=info /data/open-xiaoai/xiaoai-agent -c /data/open-xiaoai/agent.yaml >>/data/open-xiaoai/xiaoai-agent.log 2>&1 &
EOF
chmod +x /data/init.sh
```

## 运行原理

Agent 启动后会常驻运行：

1. 使用固件原生 VPM/FlexKWS 监听唤醒词。
2. 每次唤醒都会中断当前语音输出或音乐播放，并重置当前对话轮次。
3. 从 VPM ASR 回调流采集一段 16 kHz 单声道音频。
4. 使用配置的 ASR 后端识别文本，可选 OpenAI-compatible ASR 或原生 Xiaomi AIVS ASR。
5. 把识别文本交给端侧 Rig Agent，并按需调用 MCP、天气、音乐等工具。
6. 使用小爱音箱系统 TTS 命令朗读回复。

## TODO

- [ ] 支持音箱按键控制

## 免责声明

本项目为非官方技术研究项目，与小米及其关联公司不存在任何隶属、合作、授权、认可或背书关系。

使用者应自行确认其使用行为符合适用法律法规、平台规则、设备厂商政策及相关服务协议，并自行承担由下载、安装、配置、修改、传播或使用本项目所产生的全部风险与责任。

详细免责声明请见 [DISCLAIMER.md](./DISCLAIMER.md)。项目授权与分发条件以仓库中的 [LICENSE](./LICENSE) 文件为准。

## 许可证和来源

本仓库包含本项目自研的 `xiaoai-agent/`，也包含来自 Open-XiaoAI 等项目的部署辅助材料。上游材料的来源和许可证见 [upstream-open-xiaoai/](upstream-open-xiaoai/)。
