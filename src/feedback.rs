use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) enum State {
    Idle,
    Permission,
    Listening,
    Transcribing,
    Inserted,
    Copied,
    NoMicrophone,
    NoModel,
    NothingHeard,
    Disabled,
    Failed,
}

impl State {
    fn line(self) -> &'static [u8] {
        match self {
            Self::Idle => b"idle\n",
            Self::Permission => b"permission\n",
            Self::Listening => b"listening\n",
            Self::Transcribing => b"transcribing\n",
            Self::Inserted => b"inserted\n",
            Self::Copied => b"copied\n",
            Self::NoMicrophone => b"no-microphone\n",
            Self::NoModel => b"no-model\n",
            Self::NothingHeard => b"nothing-heard\n",
            Self::Disabled => b"disabled\n",
            Self::Failed => b"failed\n",
        }
    }

    fn transient(self) -> bool {
        !matches!(
            self,
            Self::Idle | Self::Permission | Self::Listening | Self::Transcribing
        )
    }
}

pub(crate) struct Feedback {
    observers: Vec<UnixStream>,
    state: State,
    changed: Instant,
}

impl Default for Feedback {
    fn default() -> Self {
        Self {
            observers: Vec::new(),
            state: State::Idle,
            changed: Instant::now(),
        }
    }
}

impl Feedback {
    pub(crate) fn subscribe(&mut self, mut stream: UnixStream) {
        self.observers.retain_mut(|observer| {
            observer
                .read(&mut [0])
                .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
        });
        let state =
            if self.state.transient() && self.changed.elapsed() >= Duration::from_millis(1500) {
                State::Idle
            } else {
                self.state
            };
        if self.observers.len() < 8
            && stream.set_nonblocking(true).is_ok()
            && stream.write_all(state.line()).is_ok()
        {
            self.observers.push(stream);
        }
    }

    pub(crate) fn show(&mut self, state: State) {
        self.state = state;
        self.changed = Instant::now();
        self.publish(state.line());
    }

    pub(crate) fn level(&mut self, level: u8) {
        if !self.observers.is_empty() {
            self.publish(format!("level {level}\n").as_bytes());
        }
    }

    fn publish(&mut self, line: &[u8]) {
        // A stalled observer cannot delay capture or keep a partial line in the stream.
        self.observers
            .retain_mut(|stream| stream.write_all(line).is_ok());
    }
}
