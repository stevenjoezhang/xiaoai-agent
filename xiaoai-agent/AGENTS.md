# XiaoAI Agent Notes

This directory contains the standalone on-device XiaoAI agent. Treat this file
as working memory for future coding agents.

## Device Audio Invariant

The `hw:0,0` route is intentional.

On the OH2P speaker, native XiaoAI competes with this agent: it can answer first
and can also trigger Xiaomi cloud-side device actions. To avoid that
interference, the native XiaoAI capture path is deliberately hijacked by mapping
`pcm.mico_record` away from the real microphone capture stream and onto
`hw:0,0`, which behaves as an empty/ineffective audio source for the native
service in this setup.

Do not "fix" this by restoring `pcm.mico_record` to `Capture` unless the user
explicitly wants native XiaoAI ASR/control side effects back.

The useful microphone stream for this agent is the real capture path:

- `pcm.Capture`
- `pcm.noop`
- underlying hardware: `hw:0,3`
- format: 48 kHz, 4 channels, `S32_LE`
- frame size used by native flexkws config: 8 ms, 384 frames

This matches the firmware `flexkws.json5` `raw_stream` input. Keep native XiaoAI
muted/isolated while feeding the agent from the real capture path.

When `capture.pcm` is `vpm_asr`, utterance capture is no longer reading the raw
4-channel ALSA stream. It consumes `libvpm`'s ASR callback output, which is
16 kHz mono `S16_LE` after VPM/FlexKWS processing. RMS levels are therefore not
comparable with the older raw `arecord` path: raw capture could peak near 1.0,
while observed VPM ASR speech is commonly around `0.02` to `0.1` and room/noise
around `0.001` to `0.005`. A VPM ASR VAD threshold around `0.006` is intentional;
do not reuse raw-capture thresholds such as `0.16` without rechecking
`CAPTURE_LEVEL` on the actual backend.

## Cross Compilation

Do not assume this project cannot be cross-compiled locally.

The local workspace already has a working Rust cross-build path for the speaker
target, and `xiaoai-agent` has successfully produced an ARMv7 hard-float Linux
binary:

```bash
cargo +1.88.0 zigbuild --release --target armv7-unknown-linux-gnueabihf.2.25
```

This host uses `cargo-zigbuild` plus `zig` for the ARMv7 Linux native build and
link steps. A plain `cargo +1.88.0 build --release --target
armv7-unknown-linux-gnueabihf` may fail in shells where
`arm-linux-gnueabihf-gcc` is not present on `PATH`, because dependencies such
as `ring` and `aws-lc-sys` compile C/assembly during their `build.rs` scripts.
In that case, use the `zigbuild` command above rather than assuming cross
compilation is broken.

The speaker firmware uses glibc 2.25. Building with
`--target armv7-unknown-linux-gnueabihf` without the `.2.25` suffix can still
produce a valid ARMv7 hard-float ELF locally, but that binary may fail on the
speaker with missing symbols such as `GLIBC_2.28`, `GLIBC_2.29`,
`GLIBC_2.32`, `GLIBC_2.33`, or `GLIBC_2.34`. For binaries intended to run on
the speaker, keep the `armv7-unknown-linux-gnueabihf.2.25` target suffix.

Known-good output:

```text
xiaoai-agent/target/armv7-unknown-linux-gnueabihf/release/dodo-xiaoai-agent
```

The resulting binary is expected to be:

```text
ELF 32-bit LSB pie executable, ARM, EABI5, dynamically linked,
interpreter /lib/ld-linux-armhf.so.3
```

That ABI matches the speaker userland and the firmware `libflexkws.so`, which is
also 32-bit ARM hard-float. If a future shell cannot find a bare
`arm-linux-gnueabihf-gcc`, inspect the existing Rust target/build setup and the
`cargo-zigbuild` glibc 2.25 path before rewriting the toolchain assumptions.

## Runtime Safety

- Avoid NAND writes on the speaker whenever possible; prefer `/tmp` for logs,
  probes, PID files, and temporary audio.
- Do not commit real `agent.yaml` secrets. Use sanitized examples for committed
  config.
- Do not restart or restore native XiaoAI audio paths casually; that can bring
  back double answers and cloud-side control actions.
- When testing low-level audio or KWS changes, prefer a standalone probe first,
  then wire the validated behavior into the main agent.
