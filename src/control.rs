//! The daemon's control socket serves commands and streams indicator updates.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// The dictation key went down: start, or stop a hands-free take.
    Press,
    /// The dictation key came up: stop a take held down, keep a tapped one.
    Release,
    Start,
    Stop,
    Toggle,
    Cancel,
    Status,
    Allow,
    Deny,
}

impl Command {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text.trim() {
            "press" => Self::Press,
            "release" => Self::Release,
            "start" => Self::Start,
            "stop" => Self::Stop,
            "toggle" => Self::Toggle,
            "cancel" => Self::Cancel,
            "status" => Self::Status,
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            _ => return None,
        })
    }
}

/// A command and where its reply goes.
pub(crate) enum Request {
    Command(Command, Sender<String>),
    Subscribe(UnixStream),
}

/// Sends `command` to the daemon and returns its reply, starting the daemon
/// first when none runs (a key press must not depend on a service unit).
pub(crate) fn send(socket: &Path, command: &str) -> Result<String, String> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(_) if command == "status" => return Ok("stopped".to_owned()),
        Err(_) => start_daemon(socket)?,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    writeln!(stream, "{command}").map_err(|error| error.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|error| error.to_string())?;
    Ok(reply.trim().to_owned())
}

fn start_daemon(socket: &Path) -> Result<UnixStream, String> {
    use std::os::unix::process::CommandExt as _;
    let program = std::env::current_exe().map_err(|error| error.to_string())?;
    std::process::Command::new(program)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| "the dictation service could not start".to_owned())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("the dictation service did not start".to_owned())
}

/// Binds the control socket (owner only) and forwards each command to the
/// daemon's loop from a thread of its own.
pub(crate) fn listen(socket: &Path, requests: Sender<Request>) -> Result<(), String> {
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", socket.display())),
    }
    let listener =
        UnixListener::bind(socket).map_err(|error| format!("{}: {error}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = serve(stream, &requests);
        }
    });
    Ok(())
}

fn serve(stream: UnixStream, requests: &Sender<Request>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    BufReader::new(&stream).take_line(&mut line)?;
    if line.trim() == "subscribe" {
        let _ = requests.send(Request::Subscribe(stream));
        return Ok(());
    }
    let reply = match Command::parse(&line) {
        Some(command) => {
            let (tx, rx) = std::sync::mpsc::channel();
            if requests.send(Request::Command(command, tx)).is_err() {
                "stopping".to_owned()
            } else {
                rx.recv_timeout(Duration::from_secs(5))
                    .unwrap_or_else(|_| "busy".to_owned())
            }
        }
        None => "unknown command".to_owned(),
    };
    let mut stream = stream;
    writeln!(stream, "{reply}")
}

/// Reads one line of at most 64 bytes.
trait TakeLine {
    fn take_line(&mut self, line: &mut String) -> std::io::Result<()>;
}

impl<R: std::io::Read> TakeLine for BufReader<R> {
    fn take_line(&mut self, line: &mut String) -> std::io::Result<()> {
        use std::io::Read as _;
        self.by_ref().take(64).read_line(line).map(|_| ())
    }
}
