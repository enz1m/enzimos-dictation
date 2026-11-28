//! A live take from the account's resident speech worker.

use std::io::{self, BufRead as _, BufReader, Write as _};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub(crate) struct Control(UnixStream);

pub(crate) enum Progress {
    Peak(u16),
    Finalizing,
}

impl Control {
    pub(crate) fn connect(
        runtime: &Path,
        automatic: bool,
        seconds: u8,
    ) -> io::Result<Option<(Self, UnixStream)>> {
        let socket = runtime.join("enzimos-streaming-speech/service.sock");
        let mut stream = match UnixStream::connect(socket) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.set_read_timeout(Some(Duration::from_secs(125)))?;
        stream.write_all(&[if automatic { b'a' } else { b'm' }, seconds])?;
        let reader = stream.try_clone()?;
        Ok(Some((Self(stream), reader)))
    }

    pub(crate) fn hands_free(&mut self) -> io::Result<()> {
        self.0.write_all(b"a")
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        self.0.write_all(b"f")
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

pub(crate) fn read(
    reader: UnixStream,
    mut progress: impl FnMut(Progress),
) -> Result<Option<String>, String> {
    for line in BufReader::new(reader).lines() {
        let line = line.map_err(|error| error.to_string())?;
        let (kind, words) = line
            .split_once('\t')
            .ok_or_else(|| "Invalid streaming speech response".to_owned())?;
        let words = words.trim();
        match kind {
            "ready" | "partial" => {}
            "level" => {
                let peak = words
                    .parse()
                    .ok()
                    .filter(|&peak| peak <= 32768)
                    .ok_or_else(|| "Invalid streaming speech level".to_owned())?;
                progress(Progress::Peak(peak));
            }
            "transcribing" => progress(Progress::Finalizing),
            "final" => return Ok((!words.is_empty()).then(|| words.to_owned())),
            "error" => return Err(words.to_owned()),
            _ => return Err("Invalid streaming speech response".to_owned()),
        }
    }
    Err("The streaming speech worker stopped".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_capture_progress_and_returns_only_the_final_transcript() {
        let (reader, mut worker) = UnixStream::pair().unwrap();
        worker
            .write_all(
                b"ready\t\nlevel\t2500\npartial\tRough draft\ntranscribing\t\nfinal\tFinal words\n",
            )
            .unwrap();
        let mut progress = Vec::new();
        let text = read(reader, |event| progress.push(event)).unwrap();
        assert!(matches!(
            progress.as_slice(),
            [Progress::Peak(2500), Progress::Finalizing]
        ));
        assert_eq!(text.as_deref(), Some("Final words"));
    }
}
