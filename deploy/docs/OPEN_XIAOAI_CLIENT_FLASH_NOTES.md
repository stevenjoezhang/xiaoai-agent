# Open XiaoAI Client Flash Notes

This note records practical lessons from a successful open-xiaoai client setup.
It is intentionally generic to the open-xiaoai client flow and does not depend
on any specific downstream server implementation.

## Scope

The open-xiaoai patch does not replace the whole XiaoAI firmware. The native
system still boots, joins Wi-Fi, keeps the Xiaomi pairing/account state, and
continues to provide wake word, ASR, TTS, and native device services.

The patch primarily does these things:

- enables SSH through Dropbear
- sets a known root SSH password hash
- disables or blocks native OTA update paths
- makes `/etc/rc.local` run `/data/init.sh` on boot

The client program itself is not baked into the patched rootfs. After flashing,
copy or download the client program and runtime config under `/data/open-xiaoai`.

## Files On The Speaker

The common runtime layout is:

```text
/data/open-xiaoai/client
/data/open-xiaoai/server.txt
/data/open-xiaoai/token.txt    # optional
/data/init.sh
```

`server.txt` contains the WebSocket server URL, for example:

```text
ws://192.168.1.10:4399
```

`token.txt` is only needed when the server requires an authorization token.

`/data/init.sh` is the boot script. The patch makes `/etc/rc.local` run it, so
the client starts automatically after the speaker boots and the network is up.

## Firmware Build Notes

Build the patched rootfs for the exact device model and firmware version. Do not
reuse a patched rootfs from another model or version.

Successful example:

```text
model: OH2P
firmware: 1.62.2
output: packages/client-patch/assets/mico_all_616cd9d93_1.62.2/root-patched.squashfs
```

For macOS local builds, these compatibility fixes were needed:

- OpenSSL md5crypt salt must be 8 characters or shorter.
- Linux `stat -c` needs a macOS fallback such as `stat -f %z`.
- `/dev/console` may need to be restored with an `mksquashfs -p` pseudo entry.
- Repeated output copies should remove the old destination first to avoid nested
  directories.

The generated firmware patch should not contain Xiaomi account credentials,
passToken, device DID, serial number, or MAC address. Those values are only used
to discover/download the correct OTA package.

## Flashing Notes

On macOS, the upstream flash tool can be used from:

```text
packages/flash-tool/flash
```

The successful sequence was:

```bash
./flash connect
./flash delay 15
./flash switch boot0
./flash system system0 ../client-patch/assets/<firmware>/root-patched.squashfs
```

If the first `system` write fails with an USB I/O error, reconnect the speaker
in flashing mode and retry the sequence. In the successful run, repeating
`delay`, `switch boot0`, and `system system0 ...` completed the write.

After flashing, reboot the speaker normally. The device should still connect to
the same Wi-Fi and keep its account/pairing state.

## SSH After Flashing

The patched firmware starts Dropbear SSH on port 22. The default password is the
value used when building the patch, commonly:

```text
open-xiaoai
```

Modern OpenSSH clients may need compatibility options for the older Dropbear
server:

```bash
ssh -4 -o IPQoS=none \
  -o HostKeyAlgorithms=ssh-rsa \
  -o PubkeyAcceptedAlgorithms=ssh-rsa \
  -o KexAlgorithms=curve25519-sha256@libssh.org,diffie-hellman-group14-sha1,diffie-hellman-group1-sha1 \
  root@<speaker-ip>
```

If `ssh` times out but `nc` can read the `SSH-2.0-dropbear` banner, `IPQoS=none`
is worth trying. Some Wi-Fi/router combinations appear sensitive to OpenSSH's
default QoS/TOS setting.

## Installing The Client

After SSH login:

```sh
mkdir -p /data/open-xiaoai
echo 'ws://<server-ip>:4399' > /data/open-xiaoai/server.txt
rm -f /data/open-xiaoai/token.txt
curl -sSfL https://gitee.com/coderzc/open-xiaoai/raw/main/packages/client-rust/init.sh \
  -o /data/open-xiaoai/init.sh
chmod +x /data/open-xiaoai/init.sh
sh /data/open-xiaoai/init.sh --update
curl -L -o /data/init.sh \
  https://gitee.com/coderzc/open-xiaoai/raw/main/packages/client-rust/boot.sh
chmod +x /data/init.sh /data/open-xiaoai/client
```

The speaker image may not include `nohup`. Starting the client manually can be
done with a plain background process:

```sh
/data/open-xiaoai/client "$(cat /data/open-xiaoai/server.txt)" \
  >/data/open-xiaoai/client.log 2>&1 &
```

Check status:

```sh
ps | grep open-xiaoai | grep -v grep
tail -80 /data/open-xiaoai/client.log
```

Expected client log:

```text
已启动
已连接: "ws://<server-ip>:4399"
```

## Boot Behavior

The upstream boot script waits for external network connectivity before starting
the client, then reads:

```text
/data/open-xiaoai/server.txt
/data/open-xiaoai/token.txt
```

Because the server address is static text, keep it updated if the server host's
LAN IP changes. A DHCP reservation for the server machine is recommended.

## Useful Checks

From the server machine:

```bash
ping <speaker-ip>
nc -vz <speaker-ip> 22
printf '' | nc -v -w 5 <speaker-ip> 22
```

From the speaker:

```sh
cat /data/open-xiaoai/server.txt
ls -l /data/init.sh /data/open-xiaoai/client
grep -n 'data/init.sh\|open-xiaoai' /etc/rc.local /etc/init.d/* 2>/dev/null
```

The patched `/etc/rc.local` should contain a line equivalent to:

```sh
[ -f "/data/init.sh" ] && sh /data/init.sh >/dev/null 2>&1 &
```

## Recovery Mindset

Keep the original rootfs next to the patched rootfs:

```text
root.squashfs
root-patched.squashfs
```

Avoid native OTA after patching. If the speaker unexpectedly updates itself, SSH
and `/data/init.sh` autostart behavior may disappear and the rootfs may need to
be patched again for the new version.
