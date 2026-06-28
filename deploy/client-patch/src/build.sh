#!/usr/bin/env bash

set -e

# 下载固件
if [ -z "$CI" ]; then
  npm run ota
else
  npx tsx src/ota.ts
fi

# 提取固件
npm run extract

# 打补丁
npm run patch

# 打包固件
npm run squashfs