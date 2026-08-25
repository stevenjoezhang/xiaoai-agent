# XiaoAI Agent Notes

This file complements [README.md](README.md). The README is the user-facing
entry point; this document records engineering notes and invariants for future
coding agents working on this repository.

## Native Speech Frontend Invariant

The agent does not open an ALSA capture device and does not load `libvpm.so` or
`libaivs_sdk.so`. The firmware's device-specific `mipns-*` process remains the
sole owner of the real microphone path and continues to provide the matched
microphone-array processing, wake word, AEC, beamforming, and VAD. It sends wake
events and 16 kHz mono `S16_LE` PCM to the agent over the native
`speech.usock` datagram protocol.

The agent binds the standard path:

```text
/tmp/mico_aivs_lab/usock/speech.usock
```

The firmware patch starts `mipns-xiaomi` without `-r opus32` so that this socket
receives PCM rather than Opus. Do not restore `-r opus32` unless the agent also
gains a packet-preserving Opus decoder.

`mico_aivs_lab` must remain running because its `common.usock` services are used
by the system TTS path. The agent isolates only its speech endpoint by moving the
native socket to `speech.native.usock`, then takes over the standard speech
socket before `mipns` reconnects. Do not stop the complete service or remove the
socket-isolation logic merely because the agent no longer uses native AIVS ASR.

The native `/data/mipns/dialog_continuous` mode is deliberately disabled. After
each answer, the agent finishes the current native dialog and requests the next
capture through `pnshelper`'s public `ubus` event. This segmented sequence is
required for reliable follow-up turns; restoring the native full-duplex marker
can make the LED turn off and prevent the next utterance from reaching the
agent.

Keep this ownership boundary intact:

```text
real microphone -> native mipns-* -> speech.usock -> xiaoai-agent -> external ASR
```

Supporting another model should normally reuse that model's own `mipns-*`
frontend instead of linking the agent directly to model-specific shared
libraries. Compatibility still depends on the firmware providing a compatible
`mipns` executable, socket protocol, PCM mode, and `pnshelper` control path.

## Cross Compilation

Do not assume this project cannot be cross-compiled locally.

The local workspace already has a working Rust cross-build path for the speaker
target, and `xiaoai-agent` has successfully produced an ARMv7 hard-float Linux
binary:

```bash
cargo +1.96.0 zigbuild --release --target armv7-unknown-linux-gnueabihf.2.25
```

This host uses `cargo-zigbuild` plus `zig` for the ARMv7 Linux native build and
link steps. A plain `cargo +1.96.0 build --release --target
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
xiaoai-agent/target/armv7-unknown-linux-gnueabihf/release/xiaoai-agent
```

The resulting binary is expected to be:

```text
ELF 32-bit LSB pie executable, ARM, EABI5, dynamically linked,
interpreter /lib/ld-linux-armhf.so.3
```

That ABI matches the speaker userland. The agent has no direct dependency on
model-specific Xiaomi shared libraries; its expected ELF dependencies are the
standard glibc runtime libraries `libc`, `libm`, `libpthread`, and `libdl`. If a
future shell cannot find a bare `arm-linux-gnueabihf-gcc`, inspect the existing
Rust target/build setup and the `cargo-zigbuild` glibc 2.25 path before rewriting
the toolchain assumptions.

## Runtime Safety

- Avoid NAND writes on the speaker whenever possible; prefer `/tmp` for logs,
  probes, PID files, and temporary audio.
- Do not commit real `agent.yaml` secrets. Use sanitized examples for committed
  config.
- Do not reconnect `mipns` to the native AIVS speech socket; that can bring back
  double answers and cloud-side control actions.
- Do not stop `mico_aivs_lab` as part of normal Agent startup; doing so also
  removes services required by system TTS.
- When testing `speech.usock` protocol changes, prefer `xiaoai_asr_probe` first,
  then wire the validated behavior into the main agent.
