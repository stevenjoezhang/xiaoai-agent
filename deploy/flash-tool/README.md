# Open-XiaoAI Flash Tool

小爱音箱刷机工具（macOS 专用版）

> [!NOTE]
> 本工具基于 [@khadas/utils](https://github.com/khadas/utils/tree/master/aml-flash-tool)，macOS 二进制文件来自 [@numbqq](https://github.com/numbqq)，特此鸣谢！

> [!CAUTION]
> 刷机有风险，操作需谨慎。刷机可能会造成设备失去保修资格，变砖无法运行等。请自行评估相关风险，一切后果自负！🚨


## 安装依赖

请先在 macOS 上使用 [Homebrew](https://brew.sh/zh-cn/) 安装 `libusb-0.1.4.dylib` 依赖。

```shell
brew install libusb-compat
```

## 使用方法

```shell
# 克隆代码
git clone https://github.com/idootop/open-xiaoai.git

# 进入当前项目根目录
cd packages/flash-tool

# 授予可运行权限
chmod +x ./flash

# 查看使用说明
./flash help

# 小爱音箱刷机工具 v1.0.0  by https://del.wang
#
# 使用方法:
#   ./flash connect                   # 连接设备
#   ./flash delay <秒数>              # 设置启动延时，如 5 秒
#   ./flash switch <boot分区>         # 设置启动分区，如 boot0
#   ./flash system <分区> <固件路径>  # 把固件刷到指定分区，如 system0

# 第 1 步：连接设备
# 执行命令后拔掉小爱音箱的电源线，重新插上电源，等待设备连接
./flash connect

# 第 2 步：设置启动延时（15 秒）
./flash delay 15

# 第 3 步：切换启动分区
./flash switch boot0

# 第 4 步：刷写固件（注意替换固件文件的实际路径）
./flash system system0 root-patched.squashfs

# PS: 如果提示刷写错误，可以多试几次，不一定是真的无法刷机
```

> [!TIP]
> 如果你卡在第一步，连接不上设备，可以按照[此教程](https://github.com/idootop/open-xiaoai/issues/6#issuecomment-2815632879)排查问题。

> [!NOTE]
> 提示刷机成功之后，拔掉数据线和电源，重新插电重启小爱音箱即可。
> 如果重启之后小爱音箱没有反应，可以拔掉电源等几分钟再重新上电开机。
> 如果还是没反应，可以重新刷机试试看，或者将启动分区设置成 `boot1` 恢复原系统启动。
