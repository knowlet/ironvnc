//! Text clipboard messages from RFC 6143 §§7.5.6/7.6.4 and the public
//! rfbproto Extended Clipboard specification. No private Apple messages.
use anyhow::{bail, ensure, Result};
use flate2::{write::ZlibEncoder, Compression, Decompress, FlushDecompress, Status};
use std::io::Write;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_WIRE_BYTES: usize = MAX_TEXT_BYTES + 64 * 1024;
const TEXT: u32 = 1;
const FORMATS: u32 = 0xffff;
const CAPS: u32 = 1 << 24;
const REQUEST: u32 = 1 << 25;
const PEEK: u32 = 1 << 26;
const NOTIFY: u32 = 1 << 27;
const PROVIDE: u32 = 1 << 28;
const ACTIONS: u32 = 0xff00_0000;
const SUPPORTED: u32 = TEXT | CAPS | REQUEST | PEEK | NOTIFY | PROVIDE;

#[derive(Default)]
pub struct Clipboard {
    server_caps: u32,
    server_text_limit: u32,
    local_text: Option<String>,
}

impl Clipboard {
    pub async fn send_local<S: AsyncWrite + Unpin>(
        &mut self,
        stream: &mut S,
        text: String,
    ) -> Result<()> {
        ensure!(
            text.len() < MAX_TEXT_BYTES,
            "Local clipboard exceeds 8 MiB limit"
        );
        ensure!(
            !text.contains('\0'),
            "Clipboard text contains an embedded NUL"
        );
        let normalized = normalize_lines(&text);
        ensure!(
            normalized.len() + normalized.bytes().filter(|&byte| byte == b'\n').count() + 1
                <= MAX_TEXT_BYTES,
            "UTF-8 clipboard exceeds 8 MiB limit after CRLF conversion"
        );
        self.local_text = Some(normalized);
        if self.server_caps & (TEXT | NOTIFY) == TEXT | NOTIFY {
            send_extended(stream, TEXT | NOTIFY, &[]).await
        } else if self.server_caps & (TEXT | PROVIDE) == TEXT | PROVIDE
            && self.local_text.as_ref().map_or(0, |text| {
                text.len() + text.bytes().filter(|&byte| byte == b'\n').count() + 1
            }) <= self.server_text_limit as usize
        {
            self.send_provide(stream).await
        } else {
            // RFC 6143's basic clipboard cannot represent Unicode. Use one
            // replacement byte per unrepresentable character, never UTF-8.
            let bytes: Vec<u8> = self
                .local_text
                .as_ref()
                .unwrap()
                .chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                .collect();
            send_message(stream, bytes.len() as i32, &bytes).await
        }
    }

    /// Read the body after the ServerCutText message-type byte.
    pub async fn receive<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        stream: &mut S,
    ) -> Result<Option<String>> {
        stream.read_exact(&mut [0; 3]).await?;
        let length = stream.read_i32().await?;
        let size = length
            .checked_abs()
            .ok_or_else(|| anyhow::anyhow!("Invalid clipboard length"))?
            as usize;
        let limit = if length < 0 {
            MAX_WIRE_BYTES
        } else {
            MAX_TEXT_BYTES
        };
        ensure!(size <= limit, "Clipboard message exceeds size limit");
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).await?;
        if length >= 0 {
            return Ok(Some(bytes.into_iter().map(char::from).collect()));
        }
        ensure!(bytes.len() >= 4, "Extended clipboard header is truncated");
        let flags = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        let payload = &bytes[4..];
        if flags & CAPS != 0 {
            let count = (flags & FORMATS).count_ones() as usize;
            ensure!(
                payload.len() == count * 4,
                "Invalid clipboard capability sizes"
            );
            self.server_caps = flags;
            self.server_text_limit = if flags & TEXT != 0 {
                u32::from_be_bytes(payload[..4].try_into().unwrap())
            } else {
                0
            };
            // Advertise only UTF-8 text. A zero unsolicited limit requests
            // notify/request delivery; all inbound allocations remain bounded.
            send_extended(stream, SUPPORTED, &0u32.to_be_bytes()).await?;
            if self.local_text.is_some() && flags & (TEXT | NOTIFY) == TEXT | NOTIFY {
                send_extended(stream, TEXT | NOTIFY, &[]).await?;
            }
            return Ok(None);
        }
        ensure!(
            self.server_caps & CAPS != 0,
            "Extended clipboard action before capabilities"
        );
        let action = flags & ACTIONS;
        ensure!(
            action.count_ones() == 1,
            "Invalid extended clipboard action"
        );
        match action {
            NOTIFY => {
                ensure!(payload.is_empty(), "Unexpected clipboard notify payload");
                if flags & TEXT != 0 && self.server_caps & REQUEST != 0 {
                    send_extended(stream, TEXT | REQUEST, &[]).await?;
                }
            }
            REQUEST => {
                ensure!(payload.is_empty(), "Unexpected clipboard request payload");
                if flags & TEXT != 0 && self.server_caps & PROVIDE != 0 {
                    self.send_provide(stream).await?;
                }
            }
            PEEK => {
                ensure!(payload.is_empty(), "Unexpected clipboard peek payload");
                if self.server_caps & NOTIFY != 0 {
                    let formats = if self.local_text.is_some() { TEXT } else { 0 };
                    send_extended(stream, formats | NOTIFY, &[]).await?;
                }
            }
            PROVIDE => return decode_provide(flags, payload),
            _ => bail!("Unsupported extended clipboard action"),
        }
        Ok(None)
    }

    async fn send_provide<S: AsyncWrite + Unpin>(&self, stream: &mut S) -> Result<()> {
        if let Some(text) = &self.local_text {
            let mut data = text.replace('\n', "\r\n").into_bytes();
            data.push(0);
            ensure!(
                data.len() <= MAX_TEXT_BYTES,
                "UTF-8 clipboard exceeds 8 MiB limit"
            );
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(&(data.len() as u32).to_be_bytes())?;
            encoder.write_all(&data)?;
            send_extended(stream, TEXT | PROVIDE, &encoder.finish()?).await?;
        }
        Ok(())
    }
}

fn normalize_lines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn decode_provide(flags: u32, compressed: &[u8]) -> Result<Option<String>> {
    // A fixed output ceiling prevents tiny zlib bombs from growing a Vec.
    let mut output = vec![0; MAX_TEXT_BYTES + 16 * 4 + 1];
    let mut decoder = Decompress::new(true);
    let status = decoder.decompress(compressed, &mut output, FlushDecompress::Finish)?;
    ensure!(
        status == Status::StreamEnd,
        "Truncated or oversized compressed clipboard"
    );
    ensure!(
        decoder.total_in() as usize == compressed.len(),
        "Trailing clipboard compressed data"
    );
    output.truncate(decoder.total_out() as usize);
    let mut remaining = output.as_slice();
    let mut text = None;
    for bit in 0..16 {
        if flags & (1 << bit) == 0 {
            continue;
        }
        ensure!(remaining.len() >= 4, "Truncated clipboard format length");
        let size = u32::from_be_bytes(remaining[..4].try_into().unwrap()) as usize;
        remaining = &remaining[4..];
        ensure!(
            size <= MAX_TEXT_BYTES && size <= remaining.len(),
            "Invalid clipboard format size"
        );
        if bit == 0 {
            let data = &remaining[..size];
            ensure!(
                data.last() == Some(&0),
                "Clipboard text has no terminating NUL"
            );
            ensure!(
                !data[..size - 1].contains(&0),
                "Clipboard text contains an embedded NUL"
            );
            text = Some(normalize_lines(std::str::from_utf8(&data[..size - 1])?));
        }
        remaining = &remaining[size..];
    }
    ensure!(remaining.is_empty(), "Trailing clipboard format data");
    Ok(text)
}

async fn send_extended<S: AsyncWrite + Unpin>(
    stream: &mut S,
    flags: u32,
    payload: &[u8],
) -> Result<()> {
    ensure!(
        payload.len() + 4 <= MAX_WIRE_BYTES,
        "Clipboard payload exceeds size limit"
    );
    let mut bytes = Vec::with_capacity(4 + payload.len());
    bytes.extend_from_slice(&flags.to_be_bytes());
    bytes.extend_from_slice(payload);
    send_message(stream, -(bytes.len() as i32), &bytes).await
}

async fn send_message<S: AsyncWrite + Unpin>(
    stream: &mut S,
    length: i32,
    bytes: &[u8],
) -> Result<()> {
    stream.write_all(&[6, 0, 0, 0]).await?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compress(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    async fn write_extended_body<S: AsyncWrite + Unpin>(
        stream: &mut S,
        flags: u32,
        payload: &[u8],
    ) {
        stream.write_all(&[0, 0, 0]).await.unwrap();
        stream
            .write_i32(-((4 + payload.len()) as i32))
            .await
            .unwrap();
        stream.write_u32(flags).await.unwrap();
        stream.write_all(payload).await.unwrap();
    }

    async fn read_extended<S: AsyncRead + Unpin>(stream: &mut S) -> (u32, Vec<u8>) {
        let mut header = [0; 4];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [6, 0, 0, 0]);
        let length = stream.read_i32().await.unwrap();
        assert!(length < 0);
        let flags = stream.read_u32().await.unwrap();
        let mut payload = vec![0; -length as usize - 4];
        stream.read_exact(&mut payload).await.unwrap();
        (flags, payload)
    }

    #[tokio::test]
    async fn basic_clipboard_is_latin1_in_both_directions() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        server
            .write_all(&[0, 0, 0, 0, 0, 0, 2, 0xe9, 0xff])
            .await
            .unwrap();
        let mut clipboard = Clipboard::default();
        assert_eq!(
            clipboard.receive(&mut client).await.unwrap().as_deref(),
            Some("éÿ")
        );
        clipboard
            .send_local(&mut client, "é🙂\r\n".into())
            .await
            .unwrap();
        let mut sent = [0; 11];
        server.read_exact(&mut sent).await.unwrap();
        assert_eq!(sent, [6, 0, 0, 0, 0, 0, 0, 3, 0xe9, b'?', b'\n']);
    }

    #[tokio::test]
    async fn extended_capabilities_notify_request_provide_and_peek() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let mut clipboard = Clipboard::default();
        write_extended_body(&mut server, SUPPORTED, &0u32.to_be_bytes()).await;
        assert!(clipboard.receive(&mut client).await.unwrap().is_none());
        let (flags, sizes) = read_extended(&mut server).await;
        assert_eq!(flags, SUPPORTED);
        assert_eq!(sizes, [0, 0, 0, 0]);

        write_extended_body(&mut server, TEXT | NOTIFY, &[]).await;
        assert!(clipboard.receive(&mut client).await.unwrap().is_none());
        assert_eq!(read_extended(&mut server).await, (TEXT | REQUEST, vec![]));
        let remote = "遠端\r\n文字\0".as_bytes();
        let mut provided = (remote.len() as u32).to_be_bytes().to_vec();
        provided.extend_from_slice(remote);
        write_extended_body(&mut server, TEXT | PROVIDE, &compress(&provided)).await;
        assert_eq!(
            clipboard.receive(&mut client).await.unwrap().as_deref(),
            Some("遠端\n文字")
        );

        clipboard
            .send_local(&mut client, "本地\n🦀".into())
            .await
            .unwrap();
        assert_eq!(read_extended(&mut server).await, (TEXT | NOTIFY, vec![]));
        write_extended_body(&mut server, TEXT | REQUEST, &[]).await;
        assert!(clipboard.receive(&mut client).await.unwrap().is_none());
        let (flags, provided) = read_extended(&mut server).await;
        assert_eq!(flags, TEXT | PROVIDE);
        assert_eq!(
            decode_provide(flags, &provided).unwrap().as_deref(),
            Some("本地\n🦀")
        );

        write_extended_body(&mut server, PEEK, &[]).await;
        assert!(clipboard.receive(&mut client).await.unwrap().is_none());
        assert_eq!(read_extended(&mut server).await, (TEXT | NOTIFY, vec![]));
    }

    #[tokio::test]
    async fn rejects_actions_before_caps_and_malformed_action_headers() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let mut clipboard = Clipboard::default();
        write_extended_body(&mut server, TEXT | NOTIFY, &[]).await;
        assert!(clipboard.receive(&mut client).await.is_err());
        write_extended_body(&mut server, SUPPORTED, &[]).await;
        assert!(clipboard.receive(&mut client).await.is_err());
        write_extended_body(&mut server, SUPPORTED, &0u32.to_be_bytes()).await;
        assert!(clipboard.receive(&mut client).await.unwrap().is_none());
        read_extended(&mut server).await;
        write_extended_body(&mut server, TEXT | NOTIFY | REQUEST, &[]).await;
        assert!(clipboard.receive(&mut client).await.is_err());
    }

    #[test]
    fn extended_text_uses_utf8_nul_and_normalizes_crlf() {
        let text = "繁體\r\n🦀\0".as_bytes();
        let mut data = (text.len() as u32).to_be_bytes().to_vec();
        data.extend_from_slice(text);
        assert_eq!(
            decode_provide(TEXT | PROVIDE, &compress(&data))
                .unwrap()
                .as_deref(),
            Some("繁體\n🦀")
        );
        data.pop();
        assert!(decode_provide(TEXT | PROVIDE, &compress(&data)).is_err());
    }

    #[test]
    fn compressed_clipboard_is_bounded_and_rejects_invalid_streams() {
        let bomb = compress(&vec![0; MAX_TEXT_BYTES + 128]);
        assert!(decode_provide(TEXT | PROVIDE, &bomb).is_err());
        let data = compress(&[0, 0, 0, 1, 0]);
        assert!(decode_provide(TEXT | PROVIDE, &data[..data.len() - 1]).is_err());
        let mut trailing = data.clone();
        trailing.push(0);
        assert!(decode_provide(TEXT | PROVIDE, &trailing).is_err());
    }

    #[tokio::test]
    async fn rejects_huge_wire_lengths_before_allocation() {
        let (mut client, mut server) = tokio::io::duplex(32);
        for length in [
            i32::MIN,
            (MAX_TEXT_BYTES + 1) as i32,
            -((MAX_WIRE_BYTES + 1) as i32),
        ] {
            server.write_all(&[0, 0, 0]).await.unwrap();
            server.write_all(&length.to_be_bytes()).await.unwrap();
            assert!(Clipboard::default().receive(&mut client).await.is_err());
        }
    }
}
