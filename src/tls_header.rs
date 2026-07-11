use std::io::Cursor;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rustls::server::Acceptor;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const DEFAULT_MAX_CLIENT_HELLO_SIZE: usize = 64 * 1024;

#[derive(Debug)]
pub struct ClientHello {
    pub sni_host: String,
    pub random: [u8; 32],
    pub buffered: Vec<u8>,
}

pub async fn read_client_hello<R>(
    reader: &mut R,
    timeout: Duration,
    max_size: usize,
) -> Result<ClientHello>
where
    R: AsyncRead + Unpin,
{
    tokio::time::timeout(timeout, read_client_hello_inner(reader, max_size))
        .await
        .map_err(|_| anyhow!("TLS ClientHello timed out after {timeout:?}"))?
}

async fn read_client_hello_inner<R>(reader: &mut R, max_size: usize) -> Result<ClientHello>
where
    R: AsyncRead + Unpin,
{
    let mut acceptor = Acceptor::default();
    let mut buffered = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    loop {
        match acceptor.accept() {
            Ok(Some(accepted)) => {
                let server_name = accepted
                    .client_hello()
                    .server_name()
                    .ok_or_else(|| anyhow!("TLS ClientHello does not contain SNI"))?
                    .to_string();
                let random = extract_client_random(&buffered)
                    .ok_or_else(|| anyhow!("unable to extract TLS ClientHello random"))?;
                return Ok(ClientHello {
                    sni_host: server_name,
                    random,
                    buffered,
                });
            }
            Ok(None) => {}
            Err((error, _alert)) => {
                return Err(anyhow!("invalid TLS ClientHello: {error}"));
            }
        }

        if buffered.len() >= max_size {
            bail!("TLS ClientHello exceeds the {max_size}-byte limit");
        }
        let read_size = (max_size - buffered.len()).min(chunk.len());
        let count = reader
            .read(&mut chunk[..read_size])
            .await
            .context("failed to read TLS ClientHello")?;
        if count == 0 {
            bail!("connection closed before a complete TLS ClientHello was received");
        }
        buffered.extend_from_slice(&chunk[..count]);
        let mut cursor = Cursor::new(&chunk[..count]);
        acceptor
            .read_tls(&mut cursor)
            .context("failed to buffer TLS ClientHello")?;
    }
}

/// Reassembles the handshake payload from the buffered TLS records and returns
/// the 32-byte ClientHello random (handshake header 4 bytes + legacy version 2
/// bytes, then the random). The hello may span multiple records.
pub(crate) fn extract_client_random(buffered: &[u8]) -> Option<[u8; 32]> {
    const HANDSHAKE_PREFIX: usize = 4 + 2; // handshake header + legacy version
    const NEEDED: usize = HANDSHAKE_PREFIX + 32;
    let mut handshake = Vec::with_capacity(NEEDED);
    let mut offset = 0usize;
    while handshake.len() < NEEDED {
        let header = buffered.get(offset..offset + 5)?;
        if header[0] != 0x16 {
            return None; // not a handshake record
        }
        let record_len = u16::from_be_bytes([header[3], header[4]]) as usize;
        let payload_end = (offset + 5 + record_len).min(buffered.len());
        handshake.extend_from_slice(buffered.get(offset + 5..payload_end)?);
        offset += 5 + record_len;
    }
    if handshake[0] != 0x01 {
        return None; // not a ClientHello
    }
    handshake[HANDSHAKE_PREFIX..NEEDED].try_into().ok()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection, RootCertStore};
    use tokio::io::AsyncWriteExt;

    use super::{read_client_hello, DEFAULT_MAX_CLIENT_HELLO_SIZE};

    fn client_hello(hostname: &str) -> Vec<u8> {
        let config = ClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        let name = ServerName::try_from(hostname.to_string()).unwrap();
        let mut connection = ClientConnection::new(Arc::new(config), name).unwrap();
        let mut bytes = Vec::new();
        connection.write_tls(&mut Cursor::new(&mut bytes)).unwrap();
        bytes
    }

    #[tokio::test]
    async fn reads_fragmented_client_hello() {
        let bytes = client_hello("fragmented.example");
        let (mut writer, mut reader) = tokio::io::duplex(bytes.len() * 2);
        let expected = bytes.clone();
        tokio::spawn(async move {
            for chunk in bytes.chunks(7) {
                writer.write_all(chunk).await.unwrap();
            }
        });
        let parsed = read_client_hello(
            &mut reader,
            std::time::Duration::from_secs(1),
            DEFAULT_MAX_CLIENT_HELLO_SIZE,
        )
        .await
        .unwrap();
        assert_eq!(parsed.sni_host, "fragmented.example");
        assert_eq!(parsed.buffered, expected);
        // record header is 5 bytes, handshake header 4, legacy version 2
        assert_eq!(parsed.random.as_slice(), &expected[11..43]);
    }

    #[tokio::test]
    async fn rejects_missing_sni() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"not tls").await.unwrap();
        writer.shutdown().await.unwrap();
        assert!(read_client_hello(
            &mut reader,
            std::time::Duration::from_secs(1),
            DEFAULT_MAX_CLIENT_HELLO_SIZE,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn rejects_client_hello_over_limit() {
        let bytes = client_hello("oversized.example");
        let mut reader = Cursor::new(bytes);
        let error = read_client_hello(&mut reader, std::time::Duration::from_secs(1), 16)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn times_out_incomplete_client_hello() {
        let (_writer, mut reader) = tokio::io::duplex(64);
        let error = read_client_hello(&mut reader, std::time::Duration::from_millis(10), 64)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }
}
