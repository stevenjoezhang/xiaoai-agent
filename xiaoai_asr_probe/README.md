# xiaoai_asr_probe

`xiaoai_asr_probe` 是用于验证原生 `mipns-*` 语音前端能否通过 `speech.usock` 接入 XiaoAI Agent 的实验工具。它不再调用 `mico_aivs_lab` 或小米云端 ASR，而是作为本地服务端绑定：

```text
/tmp/mico_aivs_lab/usock/speech.usock
```

原生 `mipns-xiaomi`、`mipns-sai` 等进程继续负责设备匹配的麦克风阵列处理、唤醒词、AEC、波束成形和 VAD，Probe 接收其唤醒及语音数据并返回最小必要响应：

```text
真实麦克风 -> 原生 mipns-* -> xiaoai_asr_probe
                                (不连接小米云端)
```

## 当前能力

- 接收并解析 `REGISTER_REQUEST`，显示 `speech_vendor` 和 `speech_codec`
- 返回 `REGISTER_RESPONSE`
- 将 `STREAM_PREPARE_REQUEST` 识别为一次新的唤醒/采集会话
- 返回 `STREAM_PREPARE_RESPONSE(connected=true)`
- 区分并保存 `WAKEUP=0`、`WAKEUP_END=1`、`ASR=2` 数据包
- 在 `STREAM_END` 或 `STREAM_CANCEL` 后返回 `DIALOG_FINISH`，使原生前端回到待唤醒状态
- 检测到 `mico_aivs_lab` 仍在运行时拒绝占用原生 socket，并在运行期间持续检查

正式 Agent 已使用同一套 `speech.usock` 协议接收原生语音前端的数据。这个程序只保留为独立协议探针，不包含 ASR、LLM、TTS 或连续对话逻辑。

## 构建

本机测试：

```bash
(cd xiaoai_asr_probe && cargo test && cargo build --release)
```

为 ARMv7 音箱交叉编译：

```bash
(cd xiaoai_asr_probe && cargo +1.96.0 zigbuild \
  --release \
  --target armv7-unknown-linux-gnueabihf.2.25)
```

生成文件位于：

```text
xiaoai_asr_probe/target/armv7-unknown-linux-gnueabihf/release/xiaoai_asr_probe
```

## 真机测试

> [!CAUTION]
> 测试前必须停止 `mico_aivs_lab`。Probe 不会调用小米云端，但如果系统服务在测试期间重新启动，Probe 会报错退出。测试期间不要同时运行正式的 `xiaoai-agent`，也不要用于日常语音交互。

先上传程序：

```bash
scp -O xiaoai_asr_probe/target/armv7-unknown-linux-gnueabihf/release/xiaoai_asr_probe \
  root@<speaker-ip>:/data/open-xiaoai/xiaoai_asr_probe
ssh root@<speaker-ip> 'chmod +x /data/open-xiaoai/xiaoai_asr_probe'
```

在音箱上依次停止原生语音进程和云端服务：

```sh
killall xiaoai-agent 2>/dev/null || true
/etc/init.d/pns stop
/etc/init.d/mico_aivs_lab stop
ps | grep '[m]ico_aivs_lab'
```

最后一条命令必须没有输出。随后先启动 Probe：

```sh
/data/open-xiaoai/xiaoai_asr_probe --replace --dump
```

保持 Probe 运行，在另一个 SSH 会话中重新启动原生本地语音前端：

```sh
/etc/init.d/pns start
```

说出“小爱同学”和一条测试语句。成功时应依次看到类似输出：

```text
register vendor=xiaomi codec=opus32 peer=/tmp/mipns/xiaomi
sent register_response
stream_prepare activate=wakeup(0) interact=noncontinuous(0) session=1
sent stream_prepare_response connected=true
stream type=wakeup(0) ...
stream type=wakeup_end(1) ...
stream type=asr(2) ...
stream_end
sent dialog_finish
```

结束测试前先停止 `pns`，再终止 Probe：

```sh
/etc/init.d/pns stop
```

> [!IMPORTANT]
> 当前 OH2P 补丁固件会保留 `mipns` 的真实麦克风输入，并由 Agent 接管 `speech.usock`。运行 Probe 时仍须停止正式 Agent 和 `mico_aivs_lab`，避免多个服务同时占用同一路径。

## 输出文件

每次运行会在 `/tmp/xiaoai_asr_probe/run-<timestamp>-<pid>/` 创建目录，每个会话可能包含：

```text
session-0001-wakeup.packets
session-0001-wakeup-end.packets
session-0001-asr.packets
session-0001-unknown.packets
```

文件保留 Unix datagram 的包边界，每个包使用以下格式顺序写入：

```text
4 字节小端无符号长度 + 原始 payload
```

`speech_codec=opus32` 时 payload 不是裸 PCM，需要保留包边界后进一步解析或解码。若原生 `mipns` 不启用 `-r opus32`，需要在真机上确认注册的 codec 和 payload 格式。

## 参数

```text
--socket PATH        服务端 socket，默认 /tmp/mico_aivs_lab/usock/speech.usock
--output-dir PATH    数据包输出根目录，默认 /tmp/xiaoai_asr_probe
--replace            删除已有 socket 后绑定
--dump               输出 protobuf wire 字段
--no-reply           只观察消息，不发送握手和状态响应
--no-auto-finish     流结束后不发送 DIALOG_FINISH
--timeout-ms N       N 毫秒后退出；0 表示不限制
```
