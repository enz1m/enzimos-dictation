# enzimOS Dictation

Local speech-to-text for a compatible Wayland desktop.
Hold **Super + H**, speak, let go: the words are copied and appear where you are typing. Tap
**Super + H** instead to listen hands-free: the take ends when you pause for
a second and a half after speaking, or at the next press. **Dictation** in the
app launcher starts a hands-free take the same way. Speech is recognised by the
local AI service (`enzimos-inference-service`: Whisper base on whisper.cpp by
default, or Parakeet TDT or Moonshine on sherpa-onnx when chosen) through
`enzimos-ai-gateway`; nothing is sent to the cloud.

```sh
enzimos-dictation daemon        # one per session (enzimos-dictation.service)
enzimos-dictation press|release # the dictation key, as the desktop binds it
enzimos-dictation start|stop|toggle|cancel|status  # start and toggle listen hands-free
```

The enzimOS desktop starts the service through `enzui-session-ready.target`. A command
starts the daemon when none runs. `$XDG_CONFIG_HOME/enzimos/dictation.json`
may set `model` (default: the speech model chosen in the inference service, with
`enzimos-inference-models default ID`, else `whisper-base`), `gateway_socket`,
`inference_socket` and `max_seconds` (at most 60, the speech service's limit).

How it works:

- Consent: the Shell asks whether Dictation may use the microphone on the first take. **Allow microphone** is remembered in
  `$XDG_CONFIG_HOME/enzimos/dictation-consent` and starts the take; **Not now**
  records nothing. Delete that file to be asked again. Cancelling a take while
  it is being recognised discards the transcript.

- Recording: PipeWire `pw-record`, 16 kHz mono 16-bit, into the private runtime
  directory; a take that is too short or too quiet is dropped without being sent.
- Typing: the daemon asks the compositor to commit text into the focused field
  with `enzui_text_commit_v1`. The compositor admits the installed Dictation
  executable for this request. Fcitx5 and other input methods keep their seat.
  The compositor adds a space when needed. If the focused field uses an
  unsupported input protocol or an IME has an active composition, the user can
  paste the transcript from the clipboard.
- Feedback: a dedicated Shell indicator presents listening, the live microphone
  level, transcription and completion.
  `deploy/enzimos-dictation.desktop` provides the Dictation launcher and icon.

## Shell feedback

Connect to `$XDG_RUNTIME_DIR/enzimos-dictation/control.sock` and write
`subscribe\n`. The first line reports the current state. Subsequent lines
report state changes and `level N` samples (0–100, at most 25 per second).
States are `idle`, `permission`, `listening`, `transcribing`, `inserted`,
`copied`, `no-microphone`, `no-model`, `nothing-heard`, `disabled`, and
`failed`. A result older than 1.5 seconds is reported as idle to new
subscribers. The Shell replies to a pending permission request over a
separate connection with `allow` or `deny`.

## Build and test

Requires Linux, Rust 1.95 or newer, PipeWire's `pw-record`, a compatible Shell,
and a configured local speech model through the inference service and AI gateway.
Text insertion needs the compositor's `enzui_text_commit_v1` support. Clipboard
delivery uses wlr data-control or `wl-copy`. Keep the protocol XML in sync
with `protocols/enzui-text-commit-v1.xml` in the compositor fork. A global key
binding must be configured by the desktop; building alone does not bind Super+H.

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets --locked
cargo fmt --check
```

Tests use generated audio and fixture sockets. They do not open a microphone,
record your desktop or contact cloud services. Installed key handling, microphone
consent and text delivery require a disposable compatible Wayland session.
`deploy/` contains the user service and desktop entry.

## License

MIT; see [LICENSE](LICENSE).
