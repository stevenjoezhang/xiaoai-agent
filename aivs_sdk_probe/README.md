# aivs_sdk_probe

Experimental Rust FFI probe for Xiaomi's proprietary `libaivs_sdk.so`.

This is intentionally separate from `xiaoai_asr_probe`: that probe talks to
`mico_aivs_lab` over its local speech Unix socket, while this one links the AIVS
SDK directly.

Current status:

- Rust can model the 32-bit GNU `std::__cxx11::string` layout used by the SDK.
- Rust can model `std::shared_ptr<T>` as two pointers for SDK call boundaries.
- `aivs::Event::build(json, shared_ptr<Event>&)` succeeds on the speaker and
  returns a non-null `Event`.
- `aivs::Engine::create(shared_ptr<AivsConfig>&, shared_ptr<ClientInfo>&, 0)`
  succeeds on the speaker when given a reconstructed minimal `ClientInfo`
  object. This uses the 32-bit object layout observed in `mico_aivs_lab`
  `sub_265C0`: object size `0x6c`, vtable pointer at `+0`, optional string
  pointers at offsets used by `EngineInit`, and the capabilities pointer at
  `+76`.
- `--asr-file` is a verified end-to-end ASR path: it creates the SDK engine,
  registers Rust-implemented capability vtables, injects `/data/TOKEN`, sends
  `SpeechRecognizer.Recognize`, streams a 16 kHz mono s16le PCM file with
  `Engine::postData`, sends `RecognizeStreamFinished`, and prints
  `SpeechRecognizer.RecognizeResult` text from the Rust instruction callback.
- By default, `SpeechRecognizer.Recognize` includes an
  `Execution.RequestControl` context with payload
  `{"disabled":["NLP","TTS"]}`. The field name must be singular `context`, not
  `contexts`; the plural form builds successfully but is ignored by the cloud
  path. With the singular `context`, device-control utterances such as
  "帮我打开客厅的灯", "帮我打开卧室的灯", and "帮我关闭空调" were observed to return
  only `SpeechRecognizer.RecognizeResult` plus `Dialog.Finish`, with no
  `Nlp.*`, `Execution.InstructionControl`, `Template.*`, or
  `SpeechSynthesizer.*` instructions.
- Pass `--allow-cloud-execution` only for comparison tests that intentionally
  allow normal XiaoAI NLP/TTS/device-control side effects.
- For AIVS mode `2`, the SDK uses the MIOT auth provider. The probe reads
  `app_id`, `device_id`, and `bind_id` from `/data/TOKEN`, then prefers the
  current `/data/miio/miio_token` and `/data/miio/miio_sessionid` for the
  MIOT auth callback. The older `xiaoai_token` in `/data/TOKEN` can be expired.
- The probe exits immediately after the final ASR result is printed unless
  `--keep-after-final` is set. This keeps normal tests focused on text
  recognition. Use `--keep-after-final` when verifying whether the cloud sends
  any downstream instructions after ASR.

This is still an ABI-oriented probe, not production glue. It does not register
playback or device-control capabilities, so test runs should not play audio or
execute cloud instructions.

Build for the speaker:

```sh
cargo +1.96.0 zigbuild --release --target armv7-unknown-linux-gnueabihf.2.25
```

Run on the speaker:

```sh
LD_LIBRARY_PATH=/usr/lib /tmp/aivs_sdk_probe --event-build-probe
```

Additional probes:

```sh
LD_LIBRARY_PATH=/usr/lib:/lib /tmp/aivs_sdk_probe --engine-create-fake-client-probe
```

End-to-end ASR from an audio file on the speaker:

```sh
LD_LIBRARY_PATH=/usr/lib:/lib /tmp/aivs_sdk_probe --asr-file /tmp/hello_test.s16le
```

The verified AIVS auth mode on OH2P 1.62.2 is:

```sh
LD_LIBRARY_PATH=/usr/lib:/lib /tmp/aivs_sdk_probe --asr-file /tmp/hello_test.s16le --engine-mode 2
```

Expected stdout includes SDK step return codes and, on success:

```text
ASR_TEXT final=true text=...
```
