use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Read one JSON-RPC message (Content-Length framed).
pub async fn read_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>, String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut header = String::new();
        let n: usize = reader
            .read_line(&mut header)
            .await
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = header.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(len_str) = trimmed.strip_prefix("Content-Length: ") {
            content_length = Some(
                len_str
                    .parse()
                    .map_err(|_| format!("invalid Content-Length: {len_str}"))?,
            );
        }
    }

    let length = content_length.ok_or("missing Content-Length header")?;
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("read body error: {e}"))?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("invalid JSON: {e}"))
}

/// Write one JSON-RPC message (Content-Length framed).
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Value,
) -> Result<(), String> {
    let body = serde_json::to_string(msg).map_err(|e| format!("serialize error: {e}"))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|e| format!("write error: {e}"))?;
    writer
        .write_all(body.as_bytes())
        .await
        .map_err(|e| format!("write error: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("flush error: {e}"))?;
    Ok(())
}
