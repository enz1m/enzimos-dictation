//! The daemon: one take at a time, from the key press to the words placed.

use crate::config::Config;
use crate::consent;
use crate::control::{self, Command, Request};
use crate::deliver::{Deliverer, Outcome};
use crate::feedback::{Feedback, State};
use crate::record::Recording;
use crate::{streaming, transcribe, wav};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const HOLD: Duration = Duration::from_millis(400);
const PAUSE: Duration = Duration::from_millis(1500);
const LEVEL_INTERVAL: Duration = Duration::from_millis(40);
const PAUSE_INTERVAL: Duration = Duration::from_millis(200);
const MIC_TIMEOUT: Duration = Duration::from_secs(2);
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(120);

enum Phase {
    Idle,
    Asking(Instant),
    Listening(Recording, bool),
    Streaming(StreamingTake),
    Recognising { discard: bool },
}

struct StreamingTake {
    id: u64,
    started: Instant,
    hands_free: bool,
    finishing: bool,
    control: streaming::Control,
}

struct Failure {
    state: State,
    detail: String,
}

impl Failure {
    fn report(self, feedback: &mut Feedback) {
        eprintln!("enzimos-dictation: {}", self.detail);
        feedback.show(self.state);
    }
}

enum Event {
    Request(Request),
    Heard(Result<Option<String>, Failure>),
    StreamProgress(u64, streaming::Progress),
    StreamHeard(u64, Result<Option<String>, String>),
}

pub(crate) fn run(config: &Config) -> Result<(), String> {
    std::fs::create_dir_all(&config.runtime).map_err(|error| error.to_string())?;
    std::fs::set_permissions(&config.runtime, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
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
        pause: wav::Pause::new(PAUSE),
        next_pause: Instant::now(),
        feedback: Feedback::default(),
        deliverer,
        events,
        stream_id: 0,
    };
    daemon.serve(&rx);
    drop(lock);
    Ok(())
}

struct Daemon {
    config: Config,
    phase: Phase,
    pause: wav::Pause,
    next_pause: Instant,
    feedback: Feedback,
    deliverer: Deliverer,
    events: Sender<Event>,
    stream_id: u64,
}

impl Daemon {
    fn serve(&mut self, rx: &Receiver<Event>) {
        loop {
            let wait = match &self.phase {
                Phase::Listening(recording, _) => (recording.started
                    + Duration::from_secs(u64::from(self.config.max_seconds)))
                .saturating_duration_since(Instant::now())
                .min(LEVEL_INTERVAL),
                Phase::Asking(deadline) => deadline.saturating_duration_since(Instant::now()),
                _ => Duration::from_secs(3600),
            };
            match rx.recv_timeout(wait) {
                Ok(Event::Request(Request::Command(command, reply))) => {
                    let answer = self.command(command);
                    let _ = reply.send(answer);
                }
                Ok(Event::Request(Request::Subscribe(stream))) => self.feedback.subscribe(stream),
                Ok(Event::Heard(result)) => self.heard(result),
                Ok(Event::StreamProgress(id, progress)) => {
                    if let Phase::Streaming(take) = &mut self.phase
                        && take.id == id
                        && !take.finishing
                    {
                        match progress {
                            streaming::Progress::Peak(peak) => {
                                self.feedback.level(wav::level(peak));
                            }
                            streaming::Progress::Finalizing => {
                                take.finishing = true;
                                self.feedback.show(State::Transcribing);
                            }
                        }
                    }
                }
                Ok(Event::StreamHeard(id, result)) => {
                    if matches!(&self.phase, Phase::Streaming(take) if take.id == id) {
                        self.heard(result.map_err(|detail| Failure {
                            state: State::Failed,
                            detail,
                        }));
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.tick(),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn tick(&mut self) {
        if matches!(self.phase, Phase::Asking(_)) {
            let _ = self.command(Command::Deny);
            return;
        }
        let Phase::Listening(recording, hands_free) = &mut self.phase else {
            return;
        };
        let now = Instant::now();
        let peak = recording.recent_peak(Duration::from_millis(65));
        if recording.exited() || (peak.is_none() && recording.started.elapsed() >= MIC_TIMEOUT) {
            self.discard_recording();
            self.feedback.show(State::NoMicrophone);
            return;
        }
        let mut paused = false;
        if *hands_free && now >= self.next_pause {
            self.next_pause = now + PAUSE_INTERVAL;
            paused = recording
                .recent_peak(Duration::from_millis(300))
                .is_some_and(|peak| self.pause.observe(peak, now));
        }
        let limit = Duration::from_secs(u64::from(self.config.max_seconds));
        if paused || recording.started.elapsed() >= limit {
            let _ = self.stop();
        } else if let Some(peak) = peak {
            self.feedback.level(wav::level(peak));
        }
    }

    fn command(&mut self, command: Command) -> String {
        match (command, &self.phase) {
            (_, Phase::Streaming(_)) => self.streaming_command(command),
            (Command::Status, Phase::Idle) => "idle".into(),
            (Command::Status, Phase::Listening(..)) => "listening".into(),
            (Command::Allow, Phase::Asking(_)) => {
                self.phase = Phase::Idle;
                if let Err(error) = consent::grant(self.config.consent.as_deref()) {
                    Failure {
                        state: State::Failed,
                        detail: error.to_string(),
                    }
                    .report(&mut self.feedback);
                    "idle".into()
                } else {
                    self.listen(true)
                }
            }
            (Command::Deny | Command::Cancel | Command::Stop, Phase::Asking(_)) => {
                self.phase = Phase::Idle;
                self.feedback.show(State::Idle);
                "idle".into()
            }
            (_, Phase::Asking(_)) => "asking".into(),
            (Command::Cancel, Phase::Recognising { .. }) => {
                self.phase = Phase::Recognising { discard: true };
                self.feedback.show(State::Idle);
                "cancelling".into()
            }
            (_, Phase::Recognising { discard: true }) => "cancelling".into(),
            (_, Phase::Recognising { discard: false }) => "recognising".into(),
            (Command::Press, Phase::Idle) => self.listen(false),
            (Command::Start | Command::Toggle, Phase::Idle) => self.listen(true),
            (Command::Release, Phase::Listening(recording, false)) => {
                if recording.started.elapsed() >= HOLD {
                    self.stop()
                } else {
                    if let Phase::Listening(_, hands_free) = &mut self.phase {
                        *hands_free = true;
                    }
                    "listening".into()
                }
            }
            (Command::Stop | Command::Toggle | Command::Press, Phase::Listening(..)) => self.stop(),
            (Command::Cancel, Phase::Listening(..)) => {
                self.discard_recording();
                self.feedback.show(State::Idle);
                "idle".into()
            }
            (
                Command::Start | Command::Release | Command::Allow | Command::Deny,
                Phase::Listening(..),
            ) => "listening".into(),
            (_, Phase::Idle) => "idle".into(),
        }
    }

    fn streaming_command(&mut self, command: Command) -> String {
        let Phase::Streaming(take) = &mut self.phase else {
            return "idle".into();
        };
        if matches!(command, Command::Cancel) {
            self.phase = Phase::Idle;
            self.feedback.show(State::Idle);
            return "idle".into();
        }
        if take.finishing {
            return "recognising".into();
        }
        let result = match command {
            Command::Release if take.hands_free => return "listening".into(),
            Command::Release if take.started.elapsed() < HOLD => {
                take.hands_free = true;
                take.control.hands_free()
            }
            Command::Release | Command::Stop | Command::Toggle | Command::Press => {
                take.finishing = true;
                self.feedback.show(State::Transcribing);
                take.control.finish()
            }
            _ => return "listening".into(),
        };
        match result {
            Ok(()) if take.finishing => "recognising".into(),
            Ok(()) => "listening".into(),
            // The worker may have finished on a pause before the command arrived.
            // Its reader reports the final text or a failure.
            Err(_) => "recognising".into(),
        }
    }

    fn discard_recording(&mut self) {
        if let Phase::Listening(recording, _) = std::mem::replace(&mut self.phase, Phase::Idle) {
            recording.cancel();
        }
    }

    fn listen(&mut self, hands_free: bool) -> String {
        if self
            .config
            .disabled
            .as_ref()
            .is_some_and(|marker| marker.exists())
        {
            self.feedback.show(State::Disabled);
            return "disabled".into();
        }
        if !consent::given(self.config.consent.as_deref()) {
            self.phase = Phase::Asking(Instant::now() + PERMISSION_TIMEOUT);
            self.feedback.show(State::Permission);
            return "asking".into();
        }
        if self.config.model.is_none()
            && self
                .config
                .use_inference
                .as_ref()
                .is_none_or(|marker| !marker.exists())
            && let Some(runtime) = self.config.runtime.parent()
        {
            match streaming::Control::connect(runtime, hands_free, self.config.max_seconds as u8) {
                Ok(Some((control, reader))) => {
                    self.stream_id = self.stream_id.wrapping_add(1);
                    let id = self.stream_id;
                    self.phase = Phase::Streaming(StreamingTake {
                        id,
                        started: Instant::now(),
                        hands_free,
                        finishing: false,
                        control,
                    });
                    self.feedback.show(State::Listening);
                    let events = self.events.clone();
                    std::thread::spawn(move || {
                        let result = streaming::read(reader, |progress| {
                            let _ = events.send(Event::StreamProgress(id, progress));
                        });
                        let _ = events.send(Event::StreamHeard(id, result));
                    });
                    return "listening".into();
                }
                Ok(None) => {}
                Err(error) => {
                    Failure {
                        state: State::Failed,
                        detail: error.to_string(),
                    }
                    .report(&mut self.feedback);
                    return format!("error: {error}");
                }
            }
        }
        match Recording::start(&self.config.runtime) {
            Ok(recording) => {
                self.phase = Phase::Listening(recording, hands_free);
                self.pause = wav::Pause::new(PAUSE);
                self.next_pause = Instant::now();
                self.feedback.show(State::Listening);
                "listening".into()
            }
            Err(error) => {
                Failure {
                    state: State::NoMicrophone,
                    detail: error.clone(),
                }
                .report(&mut self.feedback);
                format!("error: {error}")
            }
        }
    }

    fn stop(&mut self) -> String {
        let Phase::Listening(recording, _) =
            std::mem::replace(&mut self.phase, Phase::Recognising { discard: false })
        else {
            return "idle".into();
        };
        self.feedback.show(State::Transcribing);
        let config = self.config.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            let _ = events.send(Event::Heard(recognise(recording, &config)));
        });
        "recognising".into()
    }

    fn heard(&mut self, result: Result<Option<String>, Failure>) {
        let discard = matches!(self.phase, Phase::Recognising { discard: true });
        self.phase = Phase::Idle;
        if discard {
            return;
        }
        let state = match result {
            Ok(Some(text)) if !text.is_empty() => match self.deliverer.deliver(&text) {
                Outcome::Typed => State::Inserted,
                Outcome::Copied => State::Copied,
                Outcome::Failed(detail) => {
                    Failure {
                        state: State::Failed,
                        detail,
                    }
                    .report(&mut self.feedback);
                    return;
                }
            },
            Ok(_) => State::NothingHeard,
            Err(error) => {
                error.report(&mut self.feedback);
                return;
            }
        };
        self.feedback.show(state);
    }
}

fn recognise(recording: Recording, config: &Config) -> Result<Option<String>, Failure> {
    let bytes = recording.finish().map_err(|detail| Failure {
        state: State::NoMicrophone,
        detail,
    })?;
    let wav::Take::Audio(audio) =
        wav::prepare(&bytes, config.max_seconds).map_err(|detail| Failure {
            state: State::NoMicrophone,
            detail,
        })?
    else {
        return Ok(None);
    };
    let model =
        transcribe::model(config.model.as_deref(), &config.inference).map_err(|detail| {
            Failure {
                state: State::NoModel,
                detail,
            }
        })?;
    transcribe::transcribe(&config.gateway, &model, &audio)
        .map(Some)
        .map_err(|error| Failure {
            state: if error.model_unavailable {
                State::NoModel
            } else {
                State::Failed
            },
            detail: error.detail,
        })
}
