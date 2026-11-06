//! DAP wire framing: `Content-Length: N\r\n\r\n{json}`.

use std::io::{BufRead, Write};

/// Read one framed message. `Ok(None)` on clean EOF.
pub fn read_message(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }
    let Some(length) = content_length else {
        return Ok(None);
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

pub fn write_message(writer: &mut impl Write, body: &str) -> std::io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()
}

/// Serve DAP on stdio: read requests, dispatch through `handle`, write
/// responses and events. Ends on EOF.
pub fn serve_stdio(handle: &mut dyn FnMut(serde_json::Value) -> Vec<serde_json::Value>) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    while let Ok(Some(body)) = read_message(&mut reader) {
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        for message in handle(request) {
            let _ = write_message(&mut writer, &message.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let body = r#"{"seq":1}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut reader = std::io::BufReader::new(framed.as_bytes());
        assert_eq!(read_message(&mut reader).unwrap().as_deref(), Some(body));
        let mut out = Vec::new();
        write_message(&mut out, body).unwrap();
        let expected = format!("Content-Length: {}\r\n\r\n", body.len());
        assert!(out.starts_with(expected.as_bytes()));
    }

    #[test]
    fn skips_malformed_frames() {
        let mut reader = std::io::BufReader::new("no-headers\r\n\r\n".as_bytes());
        assert!(read_message(&mut reader).unwrap().is_none());
    }
}
