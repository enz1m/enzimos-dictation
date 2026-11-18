//! Speech to text through the enzimOS AI gateway, over its private Unix
//! socket: `POST /v1/audio/transcriptions` with `{model, wav_base64}`.

use base64::Engine as _;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// The speech model: the configured one, else the one chosen in the local
/// inference service (marked `default`), else Whisper base, else any
/// installed one.
pub(crate) fn model(configured: Option<&str>, inference: &Path) -> Result<String, String> {
    if let Some(model) = configured {
        return Ok(model.to_owned());
    }
    let (status, body) = request(inference, "GET", "/v1/models", None, Duration::from_secs(5))
        .map_err(|_| "the local AI service is not running".to_owned())?;
    if status != 200 {
        return Err("the local AI service did not list its models".to_owned());
    }
    let list: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| "the local AI service answered garbage".to_owned())?;
    let speech: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["kind"] == "speech")
        .collect();
    speech
        .iter()
        .find(|entry| entry["default"] == true)
        .or_else(|| speech.iter().find(|entry| entry["id"] == "whisper-base"))
        .or_else(|| speech.first())
        .and_then(|entry| entry["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| "no speech model is installed".to_owned())
}

/// The words in `wav` (a canonical 16 kHz mono WAV).
pub(crate) fn transcribe(gateway: &Path, model: &str, wav: &[u8]) -> Result<String, String> {
    let body = serde_json::json!({
        "model": model,
        "wav_base64": base64::engine::general_purpose::STANDARD.encode(wav),
    })
    .to_string();
    let (status, reply) = request(
        gateway,
        "POST",
        "/v1/audio/transcriptions",
        Some(body.as_bytes()),
        Duration::from_secs(120),
    )
    .map_err(|_| "the AI gateway is not running".to_owned())?;
    let value: serde_json::Value = serde_json::from_slice(&reply).unwrap_or_default();
    if status != 200 {
        return Err(value["error"].as_str().map_or_else(
            || format!("speech recognition failed ({status})"),
            str::to_owned,
        ));
    }
    value["text"]
        .as_str()
        .map(|text| text.trim().to_owned())
        .ok_or_else(|| "speech recognition returned no text".to_owned())
}

/// One HTTP/1.1 request over a Unix socket; the status and the body.
fn request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<(u16, Vec<u8>)> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let body = body.unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    parse(&reply)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad HTTP reply"))
}

fn parse(reply: &[u8]) -> Option<(u16, Vec<u8>)> {
    let end = reply.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(reply.get(..end)?).ok()?;
    let body = reply.get(end + 4..)?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    let chunked = head.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    });
    Some((
        status,
        if chunked {
            dechunk(body)?
        } else {
            body.to_vec()
        },
    ))
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line = body.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(body.get(..line)?)
                .ok()?
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(line + 2..line + 2 + size)?);
        body = body.get(line + 4 + size..)?;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::BufRead as _;
    use std::os::unix::net::UnixListener;

    /// A gateway stand-in: answers each request on `socket` with `reply`
    /// and hands back what it was sent.
    pub(crate) fn serve(socket: &Path, replies: Vec<String>) -> std::sync::mpsc::Receiver<String> {
        let listener = UnixListener::bind(socket).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for reply in replies {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(&stream);
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                tx.send(head + &String::from_utf8(body).unwrap()).unwrap();
                (&stream).write_all(reply.as_bytes()).unwrap();
            }
        });
        rx
    }

    /// The installed speech model is found and the audio reaches the gateway
    /// as base64 PCM; a chunked reply is read whole; a refusal says why.
    #[test]
    fn speech_goes_to_the_gateway_and_text_comes_back() {
        let directory = std::env::temp_dir().join(format!("dictation-http-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let inference = directory.join("inference.sock");
        let gateway = directory.join("gateway.sock");
        let _models = serve(
            &inference,
            vec![
                http(
                    200,
                    r#"{"object":"list","data":[{"id":"minicpm","kind":"llm","default":true},{"id":"moonshine-base","kind":"speech"},{"id":"whisper-base","kind":"speech"}]}"#,
                    false,
                ),
                http(
                    200,
                    r#"{"object":"list","data":[{"id":"minicpm","kind":"llm","default":true},{"id":"moonshine-base","kind":"speech"},{"id":"parakeet-tdt-0.6b-v3","kind":"speech","default":true},{"id":"whisper-base","kind":"speech"}]}"#,
                    false,
                ),
            ],
        );
        assert_eq!(
            model(None, &inference).unwrap(),
            "whisper-base",
            "Whisper base when nothing is chosen, though listed later"
        );
        assert_eq!(
            model(None, &inference).unwrap(),
            "parakeet-tdt-0.6b-v3",
            "the speech model chosen in the service"
        );
        assert_eq!(
            model(Some("mine"), &inference).unwrap(),
            "mine",
            "a chosen model is not looked up"
        );
        let sent = serve(
            &gateway,
            vec![
                http(
                    200,
                    r#"{"text":"  hello there  ","model":"whisper-base"}"#,
                    true,
                ),
                http(503, r#"{"error":"speech model busy"}"#, false),
            ],
        );
        assert_eq!(
            transcribe(&gateway, "whisper-base", b"RIFFwav").unwrap(),
            "hello there"
        );
        let request = sent.recv().unwrap();
        assert!(
            request.starts_with("POST /v1/audio/transcriptions HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains(r#""wav_base64":"UklGRndhdg==""#),
            "{request}"
        );
        assert_eq!(
            transcribe(&gateway, "whisper-base", b"x").unwrap_err(),
            "speech model busy"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn http(status: u16, body: &str, chunked: bool) -> String {
        if chunked {
            let (a, b) = body.split_at(body.len() / 2);
            format!(
                "HTTP/1.1 {status} OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                a.len(),
                b.len()
            )
        } else {
            format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        }
    }
}
