# OH2P 1.62.2 Patch Build Notes

This note records the successful local build path for a Xiaomi Smart Speaker
Pro (`OH2P`) running firmware `1.62.2`.

No Xiaomi account credentials, passToken, device DID, serial number, or MAC
address are required in this document. Those values are only used at OTA
download time and must stay out of git.

## Result

Generated patched firmware:

```text
xiaoai-client/packages/client-patch/assets/mico_all_616cd9d93_1.62.2/root-patched.squashfs
```

Original rootfs kept for recovery/reference:

```text
xiaoai-client/packages/client-patch/assets/mico_all_616cd9d93_1.62.2/root.squashfs
```

Observed build metadata:

```text
model: OH2P
version: 1.62.2
original OTA: mico_all_616cd9d93_1.62.2.bin
original OTA MD5: bc81f5b40f3db5e9a9d5943616cd9d93
patched rootfs size: 32059392 bytes
patched rootfs MD5: feb5ae8b9f2c587973f81301552cae21
remaining partition space reported by script: 9883648 bytes
```

## Required Local Tools

On macOS, the successful run used:

```bash
brew install squashfs
```

The upstream Docker path is still preferable when Docker is available. This
local path exists because Docker was not installed on the machine used for this
build.

## Credential Handling

Create a temporary `.env` under:

```text
xiaoai-client/packages/client-patch/.env
```

Use the Xiaomi numeric account ID for `MI_USER`, not the phone number. Using a
phone number caused MiNA `401 Unauthorized` even when `passToken` was accepted.

Template:

```env
MI_USER=<xiaomi_numeric_user_id>
MI_PASS=<xiaomi_password>
MI_TOKEN=<account.xiaomi.com passToken>
MI_DID=<device name or miot DID>
MI_DEBUG=true
SSH_PASSWORD=open-xiaoai
```

After the build, delete:

```bash
rm -f .env .mi.json
rm -rf node_modules temp
```

The `client-patch/.gitignore` now ignores `.env`.

## Build Steps

From:

```bash
cd xiaoai-client/packages/client-patch
```

Install dependencies:

```bash
npm install
```

Download the current OTA for the real device:

```bash
npm run ota
```

Expected output should identify:

```text
hardware: OH2P
romVersion: 1.62.2
```

It should download:

```text
assets/mico_all_616cd9d93_1.62.2.bin
```

Then run the remaining steps without re-querying Xiaomi:

```bash
npm run extract
npm run patch
npm run squashfs
```

## macOS Fixes Applied

The following local compatibility fixes were needed.

### OpenSSL md5crypt Salt Length

macOS OpenSSL rejected the upstream salt `open-xiaoai`:

```text
Assertion failed: (salt_len <= 8), function md5crypt
```

The script now uses an 8-character salt:

```bash
openssl passwd -1 -salt "openxiao" "$PASSWORD"
```

This only affects the generated SSH root password hash. It does not embed Xiaomi
credentials.

### macOS stat

The upstream script used Linux `stat -c`. The local script now prefers `gstat`
when available, otherwise falls back to macOS `stat -f %z`.

### `/dev/console` Device Node

Non-root `unsquashfs` on macOS cannot create the original character device:

```text
create_inode: could not create character device squashfs-root/dev/console, because you're not superuser
```

The repack step now adds it back with a squashfs pseudo entry:

```bash
-p "dev/console c 600 0 0 5 1"
```

The final `mksquashfs` summary should include:

```text
Number of device nodes 1
```

### Directory Copy on macOS

Repeated `cp -rf source dest` created nested output directories when `dest`
already existed. The script now removes the output directory before copying:

```bash
rm -rf "$BASE_DIR/assets/$FIRMWARE"
cp -rf "$FIRMWARE" "$BASE_DIR/assets/$FIRMWARE"
```

## What Is Patched

The successful patch application changed:

- Dropbear SSH startup behavior
- root SSH password hash in `/etc/shadow`
- serial console login behavior
- PAM auth path
- OTA/update paths to prevent automatic updates
- `/etc/rc.local` to run `/data/init.sh` at boot

The patch does not write Xiaomi account credentials, passToken, device DID,
serial number, or MAC address into the firmware.

## Safety Notes

Do not flash a patched rootfs for a different firmware version. This build is
for `OH2P` firmware `1.62.2`.

Before flashing, keep both files:

```text
root-patched.squashfs
root.squashfs
```

The original `root.squashfs` is useful as a reference and possible recovery
input.
