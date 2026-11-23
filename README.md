# enzimOS Dictation

Local speech-to-text for a compatible Wayland desktop.
Hold **Super + H**, speak, let go: the words appear where you are typing. Tap
**Super + H** instead to listen hands-free: the take ends when you pause for
a second and a half after speaking, or at the next press. **Dictation** in the
app launcher starts a hands-free take the same way. With no
text field focused, the words go to the clipboard. Speech is recognised by the
local AI service (`enzimos-inference-service`: Whisper base on whisper.cpp by
default, or Parakeet TDT or Moonshine on sherpa-onnx when chosen) through
`enzimos-ai-gateway`; nothing is sent to the cloud.

```sh
enzimos-dictation daemon        # one per session (enzimos-dictation.service)
enzimos-dictation press|release # the dictation key, as the desktop binds it
enzimos-dictation start|stop|toggle|cancel|status  # start and toggle listen hands-free
```

A command starts the daemon when none runs. `$XDG_CONFIG_HOME/enzimos/dictation.json`
may set `model` (default: the speech model chosen in the inference service, with
`enzimos-inference-models default ID`, else `whisper-base`), `gateway_socket`,
`inference_socket` and `max_seconds` (at most 60, the speech service's limit).

How it works:

- Consent: the first take asks, in a notification, whether Dictation may use
  the microphone. **Allow microphone** is remembered in
  `$XDG_CONFIG_HOME/enzimos/dictation-consent` and starts the take; **Not now**
  records nothing. Delete that file to be asked again. Cancelling a take while
  it is being recognised discards the transcript.

- Recording: PipeWire `pw-record`, 16 kHz mono 16-bit, into the private runtime
  directory; a take that is too short or too quiet is dropped without being sent.
- Typing: the daemon is the session's input method (`zwp_input_method_v2`), so
  the text is committed into the focused field as if typed, with a space before
  it when it would run into a word (from the field's surrounding text, which
  enzUI fields report, else from what dictation typed there). When Fcitx5 (CJK input) holds the input
  method, or nothing is focused, the text goes to the clipboard instead.
- Feedback: the bar shows listening, transcription and completion. Desktop
  notifications also show the microphone level when listening and confirm when
  text was inserted or copied. Transcripts stay out of the notification body.
  `deploy/enzimos-dictation.desktop` provides the Dictation launcher and icon.

## Build and test

Requires Linux, Rust 1.95 or newer, PipeWire's `pw-record`, a notification server,
and a configured local speech model through the inference service and AI gateway.
Text insertion needs the compositor's `zwp_input_method_v2` support; clipboard
fallback needs wlr data-control. Desktop protocol support varies. A global key
binding must be configured by the desktop; building alone does not bind Super+H.

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets --locked
cargo fmt --check
```

Tests use generated audio and fixture sockets. They do not open a microphone,
record your desktop or contact cloud services. Installed key handling, notification
consent and text delivery require a disposable compatible Wayland session.
`deploy/` contains the opt-in user service and desktop entry.

## License

MIT; see [LICENSE](LICENSE).
