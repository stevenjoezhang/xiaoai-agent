# DODO XiaoAI Agent

Standalone on-device XiaoAI agent for the OH2P speaker.

This repository is split out from the broader `dodo-edge` workspace so it can
carry only the device-side agent and the deployment materials needed to prepare
and flash a compatible speaker.

## Layout

- `xiaoai-agent/` - Rust on-device agent. It uses the speaker firmware's native
  `libvpm.so` / `libflexkws.so` path for always-on KWS and VPM ASR audio.
- `deploy/client-patch/` - vendored Open-XiaoAI patch tooling used to build a
  firmware image with SSH and boot-time hooks. Generated firmware files are not
  copied into this repo.
- `deploy/flash-tool/` - vendored Open-XiaoAI macOS flash helper.
- `deploy/docs/` - local notes for OH2P firmware/flash work.
- `deploy/upstream-open-xiaoai/` - upstream snapshot metadata and license for
  vendored Open-XiaoAI materials.

## Local Config

The real runtime config can contain API keys, Home Assistant tokens, phone
numbers, and device credentials. It is intentionally not committed.

Start from:

```bash
cp xiaoai-agent/examples/agent.yaml xiaoai-agent/agent.yaml
```

Then edit the copied `agent.yaml` for the target device.

## Build

For the OH2P firmware glibc baseline, use the pinned Rust toolchain plus
`cargo-zigbuild`:

```bash
cd xiaoai-agent
cargo +1.88.0 zigbuild --release --target armv7-unknown-linux-gnueabihf.2.25
```

See `xiaoai-agent/AGENTS.md` for the current low-level audio, native VPM KWS,
and cross-compilation notes.

## Deploy

The current test workflow copies the agent binary and config to the speaker over
SSH:

```bash
scp -O target/armv7-unknown-linux-gnueabihf/release/dodo-xiaoai-agent \
  root@<speaker-ip>:/tmp/dodo-xiaoai-agent-kws

scp -O agent.yaml root@<speaker-ip>:/data/open-xiaoai/agent.yaml
```

Run on the device:

```sh
RUST_LOG=debug /tmp/dodo-xiaoai-agent-kws -c /data/open-xiaoai/agent.yaml
```

For firmware patching/flashing, start from `deploy/client-patch/README.md` and
`deploy/flash-tool/README.md`.
