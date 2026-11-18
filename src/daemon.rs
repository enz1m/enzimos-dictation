//! The daemon: one take at a time, from the key press to the words placed.

use crate::config::Config;
use crate::consent;
use crate::control::{self, Command, Request};
use crate::deliver::{Deliverer, Outcome};
use crate::notify::Notifier;
use crate::record::Recording;
use crate::{transcribe, wav};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// A take held this long ends when the key comes up; a shorter press (a
/// tap) leaves it listening until the next press.
const HOLD: Duration = Duration::from_millis(400);

const LISTENING: &str = "Listening…";
const HOLDING: &str = "Let go of Super+H when you are done, or tap it to keep listening";
const HANDS_FREE: &str = "Stops when you pause, or press Super+H";

/// The quiet after speech that ends a hands-free take.
const PAUSE: Duration = Duration::from_millis(1500);
/// How often a take is listened to, for its level and a hands-free pause.
const LISTEN: Duration = Duration::from_millis(200);

enum Phase {
    Idle,
    Asking {
        id: u64,
        // The listener ends when this sender is dropped.
        _cancel: async_channel::Sender<()>,
    },
    /// The take, and whether it is hands-free (tapped, not held).
    Listening(Recording, bool),
    /// The take is with the speech service.
    Recognising {
        discard: bool,
    },
}

enum Event {
    Request(Request),
    Consent {
        id: u64,
        allowed: bool,
    },
    /// The speech service's answer for the take.
    Heard(Result<String, String>),
}

pub(crate) fn run(config: &Config) -> Result<(), String> {
    std::fs::create_dir_all(&config.runtime).map_err(|error| error.to_string())?;
    std::fs::set_permissions(&config.runtime, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    // One daemon per session: the lock lives as long as the process.
    let lock = std::fs::File::create(config.runtime.join("daemon.lock"))
        .map_err(|error| error.to_string())?;
    if lock.try_lock().is_err() {
        return Err("the dictation service is already running".to_owned());
    }
    let deliverer = Deliverer::connect()?;
    let (events, rx) = std::sync::mpsc::channel::<Event>();
    let (requests, incoming) = std::sync::mpsc::channel::<Request>();
    control::listen(&config.control, requests)?;
    {
        let events = events.clone();
        std::thread::spawn(move || {
            for request in incoming {
                if events.send(Event::Request(request)).is_err() {
                    return;
                }
            }
        });
    }
    let mut daemon = Daemon {
        config: config.clone(),
        phase: Phase::Idle,
        prompt_id: 0,
        pause: wav::Pause::new(PAUSE),
        notifier: Notifier::default(),
        deliverer,
        events,
    };
    daemon.serve(&rx);
    drop(lock);
    Ok(())
}

struct Daemon {
    config: Config,
    phase: Phase,
    prompt_id: u64,
    /// When a hands-free take has paused for good.
    pause: wav::Pause,
    notifier: Notifier,
    deliverer: Deliverer,
    events: Sender<Event>,
}

impl Daemon {
    fn serve(&mut self, rx: &Receiver<Event>) {
        loop {
            // A take stops itself at the speech service's limit.
            let wait = match &self.phase {
                Phase::Listening(recording, _) => (recording.started
                    + Duration::from_secs(u64::from(self.config.max_seconds)))
                .saturating_duration_since(Instant::now())
                .min(LISTEN),
                _ => Duration::from_secs(3600),
            };
            match rx.recv_timeout(wait) {
                Ok(Event::Request((command, reply))) => {
                    let answer = self.command(command);
                    let _ = reply.send(answer);
                }
                Ok(Event::Consent { id, allowed }) => self.consent_answer(id, allowed),
                Ok(Event::Heard(result)) => self.heard(result),
                Err(RecvTimeoutError::Timeout) => self.tick(),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// A take at its length limit stops; a hands-free one also stops once
    /// the speaker pauses.
    fn tick(&mut self) {
        let Phase::Listening(recording, hands_free) = &self.phase else {
            return;
        };
        let limit = Duration::from_secs(u64::from(self.config.max_seconds));
        let peak = recording.recent_peak(Duration::from_millis(300));
        let hands_free = *hands_free;
        let paused =
            hands_free && peak.is_some_and(|peak| self.pause.observe(peak, Instant::now()));
        if paused || recording.started.elapsed() >= limit {
            let _ = self.command(Command::Stop);
        } else if let Some(peak) = peak {
            let body = if hands_free { HANDS_FREE } else { HOLDING };
            self.notifier.level(LISTENING, body, wav::level(peak));
        }
    }

    fn command(&mut self, command: Command) -> String {
        match (command, &self.phase) {
            (Command::Status, Phase::Idle) => "idle".into(),
            (Command::Status, Phase::Listening(..)) => "listening".into(),
            (Command::Cancel | Command::Stop, Phase::Asking { .. }) => {
                self.phase = Phase::Idle;
                self.notifier.close();
                "idle".into()
            }
            (_, Phase::Asking { .. }) => "asking".into(),
            (Command::Cancel, Phase::Recognising { .. }) => {
                self.phase = Phase::Recognising { discard: true };
                self.notifier.show("Dictation cancelled", "", 2);
                "cancelling".into()
            }
            (_, Phase::Recognising { discard: true }) => "cancelling".into(),
            (_, Phase::Recognising { discard: false }) => "recognising".into(),
            (Command::Press, Phase::Idle) => self.listen(false),
            // Without a key held, the take ends on a pause.
            (Command::Start | Command::Toggle, Phase::Idle) => self.listen(true),
            (Command::Release, Phase::Listening(recording, false)) => {
                if recording.started.elapsed() >= HOLD {
                    self.stop()
                } else {
                    if let Phase::Listening(_, hands_free) = &mut self.phase {
                        *hands_free = true;
                    }
                    self.notifier.show(LISTENING, HANDS_FREE, 0);
                    "listening".into()
                }
            }
            (Command::Stop | Command::Toggle | Command::Press, Phase::Listening(..)) => self.stop(),
            (Command::Cancel, Phase::Listening(..)) => {
                if let Phase::Listening(recording, _) =
                    std::mem::replace(&mut self.phase, Phase::Idle)
                {
                    recording.cancel();
                }
                self.notifier.show("Dictation cancelled", "", 2);
                "idle".into()
            }
            (Command::Start | Command::Release, Phase::Listening(..)) => "listening".into(),
            (Command::Stop | Command::Cancel | Command::Release, Phase::Idle) => "idle".into(),
        }
    }

    fn listen(&mut self, hands_free: bool) -> String {
        if !consent::given(self.config.consent.as_deref()) {
            return self.ask_consent();
        }
        match Recording::start(&self.config.runtime) {
            Ok(recording) => {
                self.phase = Phase::Listening(recording, hands_free);
                self.pause = wav::Pause::new(PAUSE);
                let body = if hands_free { HANDS_FREE } else { HOLDING };
                self.notifier.show(LISTENING, body, 0);
                "listening".into()
            }
            Err(error) => {
                self.notifier.show("Dictation is not available", &error, 5);
                format!("error: {error}")
            }
        }
    }

    fn ask_consent(&mut self) -> String {
        let Some(prompt) = self.notifier.ask(
            "Allow Dictation to use the microphone?",
            "Speech is recognised on this computer and is not sent anywhere.",
            &["allow", "Allow microphone", "deny", "Not now"],
        ) else {
            return "error: no notification service".into();
        };
        let (cancel, cancelled) = async_channel::bounded(1);
        self.prompt_id = self.prompt_id.wrapping_add(1);
        let id = self.prompt_id;
        self.phase = Phase::Asking {
            id,
            _cancel: cancel,
        };
        let events = self.events.clone();
        std::thread::spawn(move || {
            let allowed = prompt.allowed(cancelled);
            let _ = events.send(Event::Consent { id, allowed });
        });
        "asking".into()
    }

    fn consent_answer(&mut self, id: u64, allowed: bool) {
        if !matches!(self.phase, Phase::Asking { id: pending, .. } if id == pending) {
            return;
        }
        self.phase = Phase::Idle;
        if !allowed {
            self.notifier.close();
        } else if let Err(error) = consent::grant(self.config.consent.as_deref()) {
            self.notifier.show(
                "Microphone permission could not be saved",
                &error.to_string(),
                5,
            );
        } else {
            let _ = self.listen(true);
        }
    }

    fn stop(&mut self) -> String {
        let Phase::Listening(recording, _) =
            std::mem::replace(&mut self.phase, Phase::Recognising { discard: false })
        else {
            return "idle".into();
        };
        let take = recording
            .finish()
            .and_then(|bytes| wav::prepare(&bytes, self.config.max_seconds));
        let audio = match take {
            Ok(wav::Take::Audio(audio)) => audio,
            Ok(wav::Take::Silent) => {
                self.phase = Phase::Idle;
                self.notifier.show("Nothing was heard", "", 2);
                return "idle".into();
            }
            Err(error) => {
                self.phase = Phase::Idle;
                self.notifier.show("Dictation failed", &error, 5);
                return format!("error: {error}");
            }
        };
        self.notifier.show("Writing down what you said…", "", 0);
        let config = self.config.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            let heard = transcribe::model(config.model.as_deref(), &config.inference)
                .and_then(|model| transcribe::transcribe(&config.gateway, &model, &audio));
            let _ = events.send(Event::Heard(heard));
        });
        "recognising".into()
    }

    fn heard(&mut self, result: Result<String, String>) {
        let discard = matches!(self.phase, Phase::Recognising { discard: true });
        self.phase = Phase::Idle;
        if discard {
            return;
        }
        match result {
            Ok(text) if text.is_empty() => self.notifier.show("Nothing was heard", "", 2),
            Ok(text) => match self.deliverer.deliver(&text) {
                Outcome::Typed => self.notifier.show("Typed", &text, 2),
                Outcome::Copied => self.notifier.show(
                    "Copied to the clipboard",
                    &format!("No text field had focus. {text}"),
                    5,
                ),
                Outcome::Failed(error) => self.notifier.show("Dictation failed", &error, 5),
            },
            Err(error) => self.notifier.show("Dictation failed", &error, 5),
        }
    }
}
