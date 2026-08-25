# OH2P 1.62.2 构建与排障补充

这份文档只保留主 README、`deploy/README.md`、`deploy/client-patch/README.md` 中没有覆盖的补充信息。完整使用流程以仓库根目录 README 和 `deploy/` 下文档为准。

## 已验证环境

本项目曾在 Xiaomi 智能音箱 Pro（OH2P）固件 `1.62.2` 上成功制作并刷入补丁 rootfs。原始 OTA 文件记录如下：

```text
model: OH2P
version: 1.62.2
original OTA: mico_all_616cd9d93_1.62.2.bin
original OTA MD5: bc81f5b40f3db5e9a9d5943616cd9d93
```

补丁内容会随仓库变化而变化，不要用旧的 `root-patched.squashfs` MD5 或文件大小判断当前构建是否正确。

## 小米账号与 OTA 下载

下载 OTA 时，`MI_USER` 建议使用小米数字账号 ID，而不是手机号。实际测试中，手机号可能导致 MiNA 返回 `401 Unauthorized`，即使 `passToken` 可用。

临时 `.env` 示例：

```env
MI_USER=<xiaomi_numeric_user_id>
MI_PASS=<xiaomi_password>
MI_TOKEN=<account.xiaomi.com passToken>
MI_DID=<device name or miot DID>
MI_DEBUG=true
SSH_PASSWORD=open-xiaoai
```

这些值只用于发现和下载正确 OTA，不应写入补丁固件，也不要提交到仓库。构建完成后可以清理：

```bash
rm -f .env .mi.json
rm -rf node_modules temp
```

## macOS 本地构建注意事项

本地构建曾使用：

```bash
brew install squashfs
```

仓库脚本已经吸收了几处 macOS 兼容修正；如果以后重新同步上游脚本，注意不要丢掉这些处理：

- OpenSSL `md5crypt` salt 不能超过 8 个字符，当前脚本使用 `openxiao`。
- Linux `stat -c` 在 macOS 上不可用，需要 `gstat` 或 `stat -f %z`。
- 非 root `unsquashfs` 可能无法创建原始 `/dev/console` 字符设备，重新打包时需要用 `mksquashfs -p` 补回。
- 覆盖复制构建产物前要先删除旧目录，避免 `cp -rf source dest` 产生嵌套目录。

## 刷写与 SSH 排障

macOS 刷机工具见 `deploy/flash-tool/README.md`。如果第一次写入 `system0` 时出现 USB I/O 错误，可以让音箱重新进入刷机模式后重试同一组 `delay`、`switch boot0`、`system system0 ...` 操作；实际成功记录里，重复刷写后完成了写入。

刷机后默认通过 Dropbear 开启 SSH。较新的 OpenSSH 客户端如果连接超时，但 `nc` 能读到 `SSH-2.0-dropbear` banner，可以尝试：

```bash
ssh -4 -o IPQoS=none \
  -o HostKeyAlgorithms=ssh-rsa \
  -o PubkeyAcceptedAlgorithms=ssh-rsa \
  -o KexAlgorithms=curve25519-sha256@libssh.org,diffie-hellman-group14-sha1,diffie-hellman-group1-sha1 \
  root@<speaker-ip>
```

其中 `IPQoS=none` 对部分 Wi-Fi / 路由器组合很有用。

## 恢复与安全

刷机前后都建议保留原始 rootfs 和补丁 rootfs：

```text
root.squashfs
root-patched.squashfs
```

`root.squashfs` 可作为参考或恢复输入。补丁后应避免原生 OTA；如果设备意外升级，SSH 和 `/data/init.sh` 自启动钩子可能消失，需要针对新固件重新制作补丁。
