//! The recording, checked and rewritten as the speech service expects it:
//! one canonical 16 kHz mono 16-bit PCM WAV, no longer than the limit.

pub(crate) const RATE: u32 = 16_000;

/// What a recording holds once checked.
#[derive(Debug, PartialEq)]
pub(crate) enum Take {
    /// Speech-length audio, as a canonical WAV.
    Audio(Vec<u8>),
    /// Too short or too quiet to hold any words.
    Silent,
}

/// Checks `bytes` (a WAV from the recorder) and returns the audio to send.
/// A header the recorder could not finish (a size of zero or past the end)
/// is read as running to the end of the file.
pub(crate) fn prepare(bytes: &[u8], max_seconds: u32) -> Result<Take, String> {
    let samples = pcm(bytes)?;
    let limit = (RATE * max_seconds) as usize * 2;
    let samples = &samples[..samples.len().min(limit) & !1];
    // A quarter of a second, or a level below that of a quiet room.
    if samples.len() < (RATE / 4) as usize * 2 || peak(samples) < 600 {
        return Ok(Take::Silent);
    }
    Ok(Take::Audio(canonical(samples)))
}

fn pcm(bytes: &[u8]) -> Result<&[u8], String> {
    let bad = || "the recording is not a WAV file".to_owned();
    if bytes.len() < 12 || bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err(bad());
    }
    let mut at = 12;
    let mut format = None;
    while at + 8 <= bytes.len() {
        let id = bytes.get(at..at + 4).ok_or_else(bad)?;
        let size = u32_at(bytes, at + 4).ok_or_else(bad)? as usize;
        let body = at + 8;
        if id == b"fmt " {
            let tag = u16_at(bytes, body).ok_or_else(bad)?;
            let channels = u16_at(bytes, body + 2).ok_or_else(bad)?;
            let rate = u32_at(bytes, body + 4).ok_or_else(bad)?;
            let bits = u16_at(bytes, body + 14).ok_or_else(bad)?;
            format = Some((tag, channels, rate, bits));
        } else if id == b"data" {
            if format != Some((1, 1, RATE, 16)) {
                return Err("the recording is not 16 kHz mono 16-bit PCM".to_owned());
            }
            let end = if size == 0 || body + size > bytes.len() {
                bytes.len()
            } else {
                body + size
            };
            return bytes.get(body..end).ok_or_else(bad);
        }
        at = body + size + (size & 1);
    }
    Err(bad())
}

fn peak(samples: &[u8]) -> u16 {
    samples
        .chunks_exact(2)
        .map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs())
        .max()
        .unwrap_or(0)
}

fn canonical(samples: &[u8]) -> Vec<u8> {
    let data = u32::try_from(samples.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(44 + samples.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    out.extend_from_slice(samples);
    out
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The loudness that counts as speech, and the quiet that ends it.
const SPEECH_PEAK: u16 = 1500;
const QUIET_PEAK: u16 = 600;

/// The end of a hands-free take: speech was heard, then quiet lasted `hold`.
#[derive(Debug)]
pub(crate) struct Pause {
    hold: std::time::Duration,
    heard: bool,
    quiet_since: Option<std::time::Instant>,
}

impl Pause {
    pub(crate) fn new(hold: std::time::Duration) -> Self {
        Self {
            hold,
            heard: false,
            quiet_since: None,
        }
    }

    /// Takes the loudness of the latest audio at `now`; true once the
    /// speaker has paused.
    pub(crate) fn observe(&mut self, peak: u16, now: std::time::Instant) -> bool {
        if peak >= SPEECH_PEAK {
            self.heard = true;
            self.quiet_since = None;
        } else if peak < QUIET_PEAK && self.heard {
            let since = *self.quiet_since.get_or_insert(now);
            return now.duration_since(since) >= self.hold;
        } else {
            self.quiet_since = None;
        }
        false
    }
}

/// How loud `peak` sounds, 0 to 100: the square root of its share of full
/// scale, so speech fills most of a level bar.
pub(crate) fn level(peak: u16) -> u8 {
    ((f32::from(peak) / f32::from(i16::MAX)).sqrt() * 100.0).min(100.0) as u8
}

/// The loudest sample in little-endian 16-bit PCM.
pub(crate) fn peak_of(samples: &[u8]) -> u16 {
    peak(samples)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A recorder's file: a list chunk before the data, and the data size
    /// left unfinished when the recorder was stopped hard.
    pub(crate) fn recorded(seconds: f32, amplitude: i16, finished: bool) -> Vec<u8> {
        let count = (RATE as f32 * seconds) as usize;
        let samples: Vec<u8> = (0..count)
            .flat_map(|i| {
                let phase = i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32;
                ((phase.sin() * f32::from(amplitude)) as i16).to_le_bytes()
            })
            .collect();
        let mut wav = canonical(&samples);
        let list = b"LIST\x04\0\0\0INFO";
        wav.splice(36..36, list.iter().copied());
        if !finished {
            wav[40 + list.len()..44 + list.len()].copy_from_slice(&0_u32.to_le_bytes());
        }
        wav
    }

    // Speech is sent as one canonical WAV, cut to the limit; silence and
    // a stray tap are not sent at all; other formats are refused.
    #[test]
    fn a_take_is_sent_canonical_and_bounded_or_not_at_all() {
        let Take::Audio(audio) = prepare(&recorded(2.0, 8000, false), 1).unwrap() else {
            panic!("speech is sent");
        };
        assert_eq!(audio.len(), 44 + RATE as usize * 2, "cut to one second");
        assert_eq!(&audio[36..40], b"data");
        assert_eq!(u32_at(&audio, 40), Some(RATE * 2));
        assert_eq!(
            prepare(&recorded(2.0, 200, true), 60).unwrap(),
            Take::Silent,
            "a quiet room"
        );
        assert_eq!(
            prepare(&recorded(0.1, 8000, true), 60).unwrap(),
            Take::Silent,
            "a tap"
        );
        let mut stereo = recorded(1.0, 8000, true);
        stereo[22] = 2;
        assert!(prepare(&stereo, 60).is_err());
        assert!(prepare(b"not audio", 60).is_err());
    }

    // Hands-free takes end on a pause after speech, not on the quiet
    // before it, and a word in the middle of the pause restarts it.
    #[test]
    fn a_pause_after_speech_ends_a_hands_free_take() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut pause = Pause::new(Duration::from_millis(1500));
        assert!(!pause.observe(100, at(0)), "quiet before speech waits");
        assert!(!pause.observe(100, at(3000)), "however long");
        assert!(!pause.observe(9000, at(3200)), "speech");
        assert!(!pause.observe(100, at(3400)));
        assert!(!pause.observe(9000, at(4500)), "a word restarts the pause");
        assert!(!pause.observe(100, at(4700)));
        assert!(!pause.observe(100, at(6000)));
        assert!(pause.observe(100, at(6300)), "1.5 s of quiet after speech");
    }
}
