//! The microphone, through PipeWire's `pw-record`: 16 kHz mono 16-bit PCM,
//! the format the speech service takes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) struct Recording {
    child: Child,
    path: PathBuf,
    pub(crate) started: Instant,
}

impl Recording {
    /// Starts recording into a new file in `directory`.
    pub(crate) fn start(directory: &Path) -> Result<Self, String> {
        let path = directory.join(format!("take-{}.wav", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let child = Command::new("pw-record")
            .args([
                "--rate",
                "16000",
                "--channels",
                "1",
                "--format",
                "s16",
                "--media-role",
                "Communication",
            ])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "the microphone recorder (pw-record) is not available".to_owned())?;
        Ok(Self {
            child,
            path,
            started: Instant::now(),
        })
    }

    /// The loudest sample of roughly the last `window` of audio, or none
    /// before the recorder has written any.
    pub(crate) fn recent_peak(&self, window: Duration) -> Option<u16> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut file = std::fs::File::open(&self.path).ok()?;
        let length = file.metadata().ok()?.len();
        // 16 kHz, 2 bytes a sample, after the 44-byte header.
        let wanted = (u64::from(crate::wav::RATE) * 2 * window.as_millis() as u64 / 1000) & !1;
        let available = length.saturating_sub(44) & !1;
        let take = wanted.min(available);
        if take == 0 {
            return None;
        }
        file.seek(SeekFrom::Start(44 + (available - take))).ok()?;
        let mut tail = vec![0; usize::try_from(take).ok()?];
        file.read_exact(&mut tail).ok()?;
        Some(crate::wav::peak_of(&tail))
    }

    /// Stops recording and returns the file's bytes; the file is removed.
    pub(crate) fn finish(mut self) -> Result<Vec<u8>, String> {
        // pw-record finishes the file's header when interrupted.
        interrupt(&self.child);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let bytes = std::fs::read(&self.path).map_err(|_| "nothing was recorded".to_owned());
        let _ = std::fs::remove_file(&self.path);
        bytes
    }

    /// Stops recording and throws it away.
    pub(crate) fn cancel(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[allow(unsafe_code, reason = "sending SIGINT to our own child process")]
fn interrupt(child: &Child) {
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        return;
    };
    // SAFETY: `pid` names our own child, which has not been waited for, so
    // the id cannot have been reused; kill(2) has no memory effects.
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }
}
