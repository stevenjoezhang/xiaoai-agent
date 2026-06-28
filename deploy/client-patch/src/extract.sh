#!/usr/bin/env bash

set -e

BASE_DIR=$(pwd)
WORK_DIR=$BASE_DIR/temp

FIRMWARE=$(basename $(ls $BASE_DIR/assets/*.bin 2>/dev/null | head -n 1) .bin)

if [ ! -f "$BASE_DIR/assets/$FIRMWARE.bin" ]; then
    echo "❌ 固件文件不存在，请先下载固件到：$BASE_DIR/assets/"
    exit 1
fi

rm -rf "$WORK_DIR" && mkdir -pv "$WORK_DIR" && cd $WORK_DIR

python3 $BASE_DIR/src/extract.py -e "$BASE_DIR/assets/$FIRMWARE.bin" -d "$WORK_DIR/$FIRMWARE"

ln -sf $WORK_DIR/$FIRMWARE/root.squashfs $WORK_DIR/root.squashfs 

unsquashfs $WORK_DIR/root.squashfs