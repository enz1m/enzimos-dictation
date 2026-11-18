//! Where the words go: typed into the focused text field through the
//! input-method protocol, or, with no text field focused (or another input
//! method in charge), put on the clipboard through data-control.

use std::os::fd::AsRawFd as _;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, event_created_child};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// What happened to the words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Typed into the focused text field.
    Typed,
    /// Put on the clipboard: no text field had focus.
    Copied,
    Failed(String),
}

/// Text to place, and where to say what happened.
type Job = (String, Sender<Outcome>);

/// The delivery thread's handle.
pub(crate) struct Deliverer {
    jobs: Sender<Job>,
}

impl Deliverer {
    /// Connects to the compositor and starts the delivery thread.
    pub(crate) fn connect() -> Result<Self, String> {
        let connection = Connection::connect_to_env()
            .map_err(|_| "no Wayland session to type into".to_owned())?;
        let (jobs, rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || run(&connection, &rx, &ready_tx));
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "the compositor did not answer".to_owned())??;
        Ok(Self { jobs })
    }

    /// Types or copies `text`, waiting for the outcome.
    pub(crate) fn deliver(&self, text: &str) -> Outcome {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.jobs.send((text.to_owned(), tx)).is_err() {
            return Outcome::Failed("the compositor connection closed".to_owned());
        }
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| Outcome::Failed("the compositor did not answer".to_owned()))
    }
}

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    input_manager: Option<ZwpInputMethodManagerV2>,
    clipboard_manager: Option<ZwlrDataControlManagerV1>,
    input: Option<ZwpInputMethodV2>,
    /// Another input method holds the seat: this one can never type.
    unavailable: bool,
    /// A text field is focused, as of the last `done`.
    active: bool,
    pending_active: bool,
    /// `done` events received: the serial a commit refers to.
    serial: u32,
    /// The text before the cursor, when the field reports it.
    before: Option<String>,
    pending_before: Option<String>,
    /// Dictation typed into this field last, since it was focused.
    typed_here: bool,
    device: Option<ZwlrDataControlDeviceV1>,
    /// The clipboard's text while this owns the selection.
    selection: Option<(ZwlrDataControlSourceV1, String)>,
}

fn run(connection: &Connection, jobs: &Receiver<Job>, ready: &Sender<Result<(), String>>) {
    let mut queue: EventQueue<State> = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut state = State::default();
    if queue.roundtrip(&mut state).is_err() {
        let _ = ready.send(Err("the compositor connection failed".to_owned()));
        return;
    }
    let Some(seat) = state.seat.clone() else {
        let _ = ready.send(Err("the compositor has no seat".to_owned()));
        return;
    };
    if let Some(manager) = &state.input_manager {
        state.input = Some(manager.get_input_method(&seat, &qh, ()));
    }
    if let Some(manager) = &state.clipboard_manager {
        state.device = Some(manager.get_data_device(&seat, &qh, ()));
    }
    let _ = queue.roundtrip(&mut state);
    let _ = ready.send(Ok(()));
    loop {
        if dispatch(connection, &mut queue, &mut state).is_err() {
            return;
        }
        loop {
            match jobs.try_recv() {
                Ok((text, reply)) => {
                    let outcome = place(&mut state, &qh, &text);
                    let _ = connection.flush();
                    let _ = reply.send(outcome);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
    }
}

/// Reads and dispatches what the compositor sent, waiting at most 50 ms.
fn dispatch(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
) -> Result<(), ()> {
    connection.flush().map_err(|_| ())?;
    if let Some(guard) = queue.prepare_read() {
        let fd = guard.connection_fd().as_raw_fd();
        if readable(fd, 50) {
            guard.read().map_err(|_| ())?;
        }
    }
    queue.dispatch_pending(state).map_err(|_| ())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::joined;

    /// Dictation into a sentence keeps words apart, and does not put a
    /// space after an opening bracket or before punctuation.
    #[test]
    fn dictated_words_do_not_run_into_the_text_before_them() {
        assert_eq!(joined(Some("Hello"), false, "there"), " there");
        assert_eq!(joined(Some("Hello "), false, "there"), "there");
        assert_eq!(joined(Some("call ("), false, "me"), "me");
        assert_eq!(joined(Some("Hello"), false, ", there"), ", there");
        assert_eq!(
            joined(None, true, "again"),
            " again",
            "the field does not say: after our own words"
        );
        assert_eq!(joined(None, false, "first"), "first");
    }
}

#[allow(unsafe_code, reason = "poll(2) on the compositor connection")]
fn readable(fd: std::os::fd::RawFd, timeout_ms: i32) -> bool {
    let mut entry = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd, borrowed for the call; `fd` stays open while
    // the read guard holding the connection lives.
    unsafe { libc::poll(&raw mut entry, 1, timeout_ms) > 0 }
}

fn place(state: &mut State, qh: &QueueHandle<State>, text: &str) -> Outcome {
    if state.active
        && !state.unavailable
        && let Some(input) = &state.input
    {
        let text = joined(state.before.as_deref(), state.typed_here, text);
        input.commit_string(text);
        input.commit(state.serial);
        state.typed_here = true;
        return Outcome::Typed;
    }
    copy(state, qh, text)
}

/// `text` with a space before it when it would otherwise run into a word:
/// after a letter, digit or closing punctuation before the cursor (or, when
/// the field does not say, after earlier dictation into the same field).
fn joined(before: Option<&str>, typed_here: bool, text: &str) -> String {
    let after_word = match before {
        Some(before) => before
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace() && !"([{\"'«¿¡".contains(c)),
        None => typed_here,
    };
    let starts_word = text
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace() && !".,;:!?)]}%…".contains(c));
    if after_word && starts_word {
        format!(" {text}")
    } else {
        text.to_owned()
    }
}

fn copy(state: &mut State, qh: &QueueHandle<State>, text: &str) -> Outcome {
    let (Some(manager), Some(device)) = (&state.clipboard_manager, &state.device) else {
        return wl_copy(text);
    };
    let source = manager.create_data_source(qh, ());
    for mime in [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "TEXT",
        "STRING",
    ] {
        source.offer(mime.to_owned());
    }
    device.set_selection(Some(&source));
    if let Some((old, _)) = state.selection.replace((source, text.to_owned())) {
        old.destroy();
    }
    Outcome::Copied
}

/// The clipboard through `wl-copy`, for a compositor without data-control.
fn wl_copy(text: &str) -> Outcome {
    use std::io::Write as _;
    let child = std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return Outcome::Failed("there is no clipboard to put the words on".to_owned());
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    if written && child.wait().is_ok_and(|status| status.success()) {
        Outcome::Copied
    } else {
        Outcome::Failed("the clipboard did not take the words".to_owned())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, version.min(7), qh, ()));
            }
            "zwp_input_method_manager_v2" => {
                state.input_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "zwlr_data_control_manager_v1" => {
                state.clipboard_manager = Some(registry.bind(name, version.min(2), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => {
                state.pending_active = true;
                state.pending_before = None;
            }
            zwp_input_method_v2::Event::Deactivate => state.pending_active = false,
            zwp_input_method_v2::Event::SurroundingText { text, cursor, .. } => {
                let cursor = usize::try_from(cursor).unwrap_or(0).min(text.len());
                state.pending_before = text.get(..cursor).map(str::to_owned);
            }
            zwp_input_method_v2::Event::Done => {
                if state.pending_active && !state.active {
                    state.typed_here = false;
                }
                state.active = state.pending_active;
                state.before = state.pending_before.clone();
                state.serial = state.serial.wrapping_add(1);
            }
            zwp_input_method_v2::Event::Unavailable => {
                state.unavailable = true;
                state.active = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                use std::io::Write as _;
                if let Some((_, text)) = state.selection.as_ref().filter(|(own, _)| own == source) {
                    let mut file = std::fs::File::from(fd);
                    let _ = file.write_all(text.as_bytes());
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                if state
                    .selection
                    .as_ref()
                    .is_some_and(|(own, _)| own == source)
                {
                    state.selection = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Offers of what others copied are not read.
            zwlr_data_control_device_v1::Event::Selection { id: Some(offer) }
            | zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                offer.destroy();
            }
            zwlr_data_control_device_v1::Event::Finished => state.device = None,
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(_: &mut Self, _: &$proxy, _: <$proxy as wayland_client::Proxy>::Event, (): &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    wl_seat::WlSeat,
    ZwpInputMethodManagerV2,
    ZwlrDataControlManagerV1,
    ZwlrDataControlOfferV1
);
