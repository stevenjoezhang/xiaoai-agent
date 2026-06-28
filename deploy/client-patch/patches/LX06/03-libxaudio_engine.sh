#!/usr/bin/env bash

set -e

# 动态获取偏移量
OFFSET=$(strings -t d usr/lib/libxaudio_engine.so | grep hw:0,3 | awk '{print $1}')

# 检查偏移量是否存在
if [ -z "$OFFSET" ]; then
    echo "未找到 \"hw:0,3\" 字符串偏移量，取消 patch usr/lib/libxaudio_engine.so"
    exit -1
fi

echo -n -e "noop\x00\x00" | dd of=usr/lib/libxaudio_engine.so bs=1 count=6 seek=$OFFSET conv=notrunc

echo "patched file usr/lib/libxaudio_engine.so"