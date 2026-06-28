# OpenXiaoAI Patch

> [!CAUTION]
> 刷机有风险，操作需谨慎。请勿下载使用不明来历的固件！

Xiaomi 智能音箱 Pro 补丁固件制作流程：

- 固件提取（登录小米账号获取 OTA 链接）
- 开启固化 SSH（支持自定义登录密码）
- 禁用系统自动更新（系统更新后需要重新刷机打补丁）
- 添加开机启动脚本 `/data/init.sh`（方便执行一些初始化脚本）

## 获取原始固件

本项目必须使用当前仓库重新打包补丁固件。不要直接使用上游 Open-XiaoAI 预构建的 patched 固件；上游成品不包含本项目用于静音原生小爱麦克风输入的 OH2P 补丁。

本仓库目前在 Xiaomi 智能音箱 Pro（OH2P）固件 `1.62.2` 上测试成功。其它固件版本需要自行确认 rootfs 布局和补丁是否仍然适用。

原始固件由构建脚本通过小米 OTA 接口获取，生成补丁固件时会自动应用本仓库的 patch。

> [!NOTE]
> 默认 SSH 登录密码为 `open-xiaoai`，如需修改请自行制作固件。

> [!IMPORTANT]
> 请下载和你当前小爱音箱版本一致的固件，跨版本刷机可能会出现未知错误，导致设备变砖。
> 如果设备固件不是 `1.62.2`，请先自行评估补丁兼容性。

> [!CAUTION]
> 新版本固件可能存在变化，导致补丁失败、刷机失败或设备变砖，请自行评估风险。

## 制作固件

你可以按照下面的 2 种方法，制作自定义固件。

### 基础配置

修改 `.env.example` 文件里的配置，然后重命名为 `.env`。

```shell
# 你的小米账号/密码
MI_USER=23333333
MI_PASS=xxxxxxxxx

# 你的 Xiaomi 智能音箱 Pro 名称/DID
MI_DID=Xiaomi智能音箱Pro

# 你的 SSH 登录密码（默认为 open-xiaoai）
SSH_PASSWORD=open-xiaoai
```

### 1. 使用 Docker 打包固件（推荐）

[![Docker Image Version](https://img.shields.io/docker/v/idootop/open-xiaoai?color=%23086DCD&label=docker%20image)](https://hub.docker.com/r/idootop/open-xiaoai)

为了能够正常编译运行该项目，你需要安装以下依赖：

- Docker：https://www.docker.com/get-started/

> [!NOTE]
> Windows 系统请在 [Git Bash](https://git-scm.com/downloads) 终端中运行以下命令。

> [!TIP]
> 如果你是 Apple Silicon 芯片，请先在 Docker Desktop - Settings - General - Virtual Machine Options 中打开 Apple Virtual framework 选项，然后开启 `Use Rosetta for x86_64/amd64 emulation on Apple Silicon`

```shell
# 克隆代码
git clone https://github.com/stevenjoezhang/xiaoai-agent.git

# 进入当前项目根目录
cd xiaoai-agent
cd deploy/client-patch

# 使用 Docker 进行构建
docker run -it --rm \
    --platform linux/amd64 \
    --env-file $(pwd)/.env \
    -v $(pwd)/assets:/app/assets \
    -v $(pwd)/patches:/app/patches \
    idootop/open-xiaoai:latest

# ✅ 打包完成，固件文件已复制到 assets 目录...
# /app/assets/mico_all_92db90ed6_1.88.197/root-patched.squashfs
```

### 2. 本地构建（macOS、Linux）

为了能够正常编译运行该项目，你需要安装以下依赖：

- Python 3.x：https://www.python.org/downloads/
- Node.js 22.x: https://nodejs.org/zh-cn/download

```bash
# 克隆代码
git clone https://github.com/stevenjoezhang/xiaoai-agent.git

# 进入当前项目根目录
cd xiaoai-agent
cd deploy/client-patch

# 安装依赖
npm install

# 打包固件
npm run build

# ✅ 打包成功后，原始固件和补丁固件会保存在 assets 目录下
```

> [!TIP]
> 如果你想要更进一步的定制自己的固件，可以参考 `src/build.sh` 脚本里的构建流程：在提取固件后自行修改固件内的脚本、配置和应用程序，然后重新打包即可。

## 高级选项

### 1. 自定义启动脚本

默认修改后的补丁固件，会将 `/data/init.sh` 文件作为启动脚本，开机时自动运行。如果你需要自定义开机启动脚本，可自行创建和修改该文件。

示例：

```bash
#!/bin/sh

/usr/sbin/tts_play.sh '初始化成功'
```
