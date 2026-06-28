#!/usr/bin/env bash

set -e

BASE_DIR=$(pwd)
WORK_DIR=$BASE_DIR/temp

PASSWORD=${SSH_PASSWORD:-"open-xiaoai"}
PASSWORD=$(openssl passwd -1 -salt "openxiao" "$PASSWORD")

# 应用指定目录下的补丁文件
apply_patches() {
    local patch_dir="$1"
    local message="$2"
    
    echo "🔥 $message"
    
    if [ -d "$patch_dir" ]; then
        for file in "$patch_dir"/*; do
            if [ -f "$file" ]; then
                if [[ "$file" == *.patch ]]; then
                    # 创建临时文件用于占位符替换
                    local temp_patch=$(mktemp)
                    # 将补丁文件中的 {SSH_PASSWORD} 替换为 PASSWORD
                    sed "s|{SSH_PASSWORD}|$PASSWORD|g" "$file" > "$temp_patch"
                    # 应用替换后的补丁
                    patch -p1 < "$temp_patch"
                    # 清理临时文件
                    rm "$temp_patch"
                elif [[ "$file" == *.sh ]]; then
                    bash "$file"
                fi
            fi
        done
    fi
}

if [ ! -f "$BASE_DIR/assets/.model" ]; then
    echo "❌ 固件信息不存在，请先下载固件到：$BASE_DIR/assets/"
    exit 1
fi

PATCH_DIR=$BASE_DIR/patches
MODEL=$(cat $BASE_DIR/assets/.model)

cd $WORK_DIR/squashfs-root

# 应用通用补丁
apply_patches "$PATCH_DIR" "正在应用通用补丁..."

# 应用特定型号补丁
apply_patches "$PATCH_DIR/$MODEL" "正在应用 $MODEL 补丁..."

echo "✅ 补丁应用完成"
