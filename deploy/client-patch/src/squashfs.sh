#!/usr/bin/env bash

set -e

BASE_DIR=$(pwd)
WORK_DIR=$BASE_DIR/temp

FIRMWARE=$(basename $(ls $BASE_DIR/assets/*.bin 2>/dev/null | head -n 1) .bin)

cd $WORK_DIR

if [ ! -f "$BASE_DIR/assets/$FIRMWARE.bin" ]; then
    echo "❌ 固件文件不存在，请先下载固件到：$BASE_DIR/assets/"
    exit 1
fi

if [ ! -d "$FIRMWARE" ]; then
    echo "❌ 解压后的固件文件夹不存在，请先提取固件"
    exit 1
fi

SQUASHFS_INFO=$(file $FIRMWARE/root.squashfs)
echo "🚗 原始固件信息: $SQUASHFS_INFO"

COMPRESSION=$(echo "$SQUASHFS_INFO" | grep -o "xz\|gzip\|lzo\|lz4\|zstd compressed" | cut -d' ' -f1)
BLOCKSIZE=$(echo "$SQUASHFS_INFO" | grep -o "blocksize: [0-9]* bytes" | cut -d' ' -f2)

echo "🔥 使用原始参数重新打包固件..."
mksquashfs squashfs-root $FIRMWARE/root-patched.squashfs \
    -comp $COMPRESSION -b $BLOCKSIZE \
    -noappend -all-root -always-use-fragments -no-xattrs -no-exports \
    -p "dev/console c 600 0 0 5 1"


# 校验固件大小上限
MODEL=$(cat $BASE_DIR/assets/.model)
IMAGE_MAX_SIZE=0
if [ "$MODEL" = "OH2P" ]; then
    IMAGE_MAX_SIZE=$((0x02800000))
elif [ "$MODEL" = "LX06" ]; then
    IMAGE_MAX_SIZE=$((0x02800000))
fi

if command -v gstat >/dev/null 2>&1; then
    SIZE=$(gstat -L -c %s "$FIRMWARE/root-patched.squashfs")
else
    SIZE=$(stat -f %z "$FIRMWARE/root-patched.squashfs")
fi
SIZE_MB=$((SIZE / 1024 / 1024))
IMAGE_MAX_SIZE_MB=$((IMAGE_MAX_SIZE / 1024 / 1024))

echo "📊 当前固件大小: $SIZE 字节 ($SIZE_MB MB)"

if [ "$SIZE" -ge "$IMAGE_MAX_SIZE" ]; then
    echo "❌ 固件大小超过允许的最大值：$IMAGE_MAX_SIZE 字节 ($IMAGE_MAX_SIZE_MB MB)"
    exit 1
fi

REMAINING_BYTES=$((IMAGE_MAX_SIZE - SIZE))
REMAINING_MB=$((REMAINING_BYTES / 1024 / 1024))
echo "✅ 固件大小检查通过，剩余可用空间: $REMAINING_BYTES 字节 ($REMAINING_MB MB)"

rm -rf $BASE_DIR/assets/$FIRMWARE
cp -rf $FIRMWARE $BASE_DIR/assets/$FIRMWARE

echo "✅ 打包完成，固件文件已复制到 assets 目录..."
echo $BASE_DIR/assets/$FIRMWARE/root-patched.squashfs
