//! Commits text at keyboard focus or offers it on the Wayland clipboard.

use std::os::fd::AsRawFd as _;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

mod text_commit {
    #[allow(
        clippy::single_component_path_imports,
        reason = "the Wayland scanner imports the client crate from its parent module"
    )]
    use wayland_client;

    mod __interfaces {
        wayland_scanner::generate_interfaces!("protocols/enzui-text-commit-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/enzui-text-commit-v1.xml");
}

use text_commit::enzui_text_commit_v1::{self, EnzuiTextCommitV1};

/// What happened to the words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Typed into the focused text field.
    Typed,
    /// Put on the clipboard when direct insertion was unavailable.
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
    clipboard_manager: Option<ZwlrDataControlManagerV1>,
    text_commit: Option<EnzuiTextCommitV1>,
    text_sent: Option<bool>,
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
    if let Some(manager) = &state.clipboard_manager {
        state.device = Some(manager.get_data_device(&seat, &qh, ()));
    }
    if queue.roundtrip(&mut state).is_err() {
        let _ = ready.send(Err("the compositor connection failed".to_owned()));
        return;
    }
    let _ = ready.send(Ok(()));
    loop {
        if dispatch(connection, &mut queue, &mut state).is_err() {
            return;
        }
        loop {
            match jobs.try_recv() {
                Ok((text, reply)) => {
                    let outcome = place(&mut queue, &mut state, &text);
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

fn place(queue: &mut EventQueue<State>, state: &mut State, text: &str) -> Outcome {
    if let Some(commit) = &state.text_commit
        && !text.is_empty()
        && text.len() <= 4000
        && !text.contains('\0')
    {
        let space_before = text
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && !".,;:!?)]}%…".contains(c));
        commit.commit(text.to_owned(), u32::from(space_before));
        if queue.roundtrip(state).is_err() {
            return Outcome::Failed("the compositor connection closed".to_owned());
        }
        match state.text_sent.take() {
            Some(true) => return Outcome::Typed,
            Some(false) => {}
            None => {
                return Outcome::Failed("the compositor did not confirm text delivery".to_owned());
            }
        }
    }
    copy(state, &queue.handle(), text)
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
            "enzui_text_commit_v1" => {
                state.text_commit = Some(registry.bind(name, 1, qh, ()));
            }
            "zwlr_data_control_manager_v1" => {
                state.clipboard_manager = Some(registry.bind(name, version.min(2), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<EnzuiTextCommitV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &EnzuiTextCommitV1,
        event: enzui_text_commit_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let enzui_text_commit_v1::Event::Result { sent } = event;
        state.text_sent = Some(sent == 1);
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
    ZwlrDataControlManagerV1,
    ZwlrDataControlOfferV1
);
