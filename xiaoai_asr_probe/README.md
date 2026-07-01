# xiaoai_asr_probe

Standalone probe for the Xiaomi `mico_aivs_lab` speech usock protocol.

It sends protobuf-c compatible datagrams to:

```text
/tmp/mico_aivs_lab/usock/speech.usock
```

and binds its own client socket, by default:

```text
/tmp/xiaoai_asr_probe/speech.usock
```

## Protocol Notes

The protocol was reconstructed from `mico_aivs_lab` and `mipns-xiaomi`.

Client to service:

```text
SpeechMessage(type=0/upward, upward=UpwardMessage)

UpwardMessage:
  type=0 register, body register_request
  type=1 stream_prepare, body stream_prepare_request
  type=2 cancel, no body
  type=3 stream_transmitting, body stream_transmitting
  type=4 stream finished / wakeup latency, body optional
  type=5 voip, body voip
  type=7 wuw upload, no body

StreamPrepareRequest:
  activate_mode: bool
  capture_mode: int32

StreamTransmitting:
  type: int32
  data: bytes
```

Service to client:

```text
SpeechMessage(type=1/downward, downward=DownwardMessage)

DownwardMessage:
  type=0 register_response
  type=1 stream_prepare_response
  type=2 stop_capture
  type=3 expect_speech
  type=4 truncation_notification
  type=5 dialog_finish
  type=6 asr_timeout
  type=7 asr_partial
  type=8 disable_voice_wakeup
  type=9 enable_voice_wakeup
  type=10 disconnected

AsrPartialMessage:
  is_final: bool
  length: int32
```

Important: the downlink `asr_partial` path used by `mipns-xiaomi` appears to carry only final/length/dialog IDs, not the transcript text itself. This probe prints every string field it can see in downlink messages and can dump raw protobuf fields so we can confirm that on device without depending on log parsing.

For ASR-only cloud behavior, keep `/data/pns.lab` enabled or otherwise ensure `mico_aivs_lab` adds `Execution.RequestControl` disabling NLP/TTS.

## Usage

Raw 16 kHz mono signed 16-bit little-endian PCM:

```sh
cargo run --release -- --pcm sample.s16le
```

On the device:

```sh
./xiaoai_asr_probe --pcm /tmp/query.s16le --dump
```

Read audio from stdin:

```sh
cat query.s16le | ./xiaoai_asr_probe --stdin
```

Useful options:

```text
--server PATH          service socket path
--bind PATH            client socket path
--capture-mode N       default 1; original client also uses 3 and 5
--activate-mode BOOL   default true
--chunk-ms N           default 100
--sample-rate N        default 16000
--timeout-ms N         default 12000
--no-throttle          send chunks as fast as possible
--dump                 print decoded field dumps
--listen-only          only bind and decode incoming messages
```
