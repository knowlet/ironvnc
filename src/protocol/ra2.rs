/// RA2 (RSA-AES) security types — RealVNC's RSA key exchange + AES-EAX channel.
///
/// Wire protocol (community rfbproto spec, "RSA-AES Security Type"):
///   1. Server sends its RSA public key: U32 bits, modulus, exponent (big-endian).
///   2. Client sends its own RSA public key in the same format.
///   3. Each side sends a 16-byte random encrypted with the peer's public key
///      (PKCS#1 v1.5), framed as U16 length + ciphertext.
///   4. Session keys: client→server = H(serverRandom‖clientRandom),
///      server→client = H(clientRandom‖serverRandom); H = SHA-1 truncated to
///      16 bytes (AES-128) for RA2, SHA-256 (AES-256) for RA2_256.
///   5. All further messages are AES-EAX framed: U16 plaintext length (also the
///      associated data) + ciphertext + 16-byte MAC. Nonce = 16-byte
///      little-endian per-direction message counter starting at 0.
///   6. Hash verification: server sends H(serverKeyMsg‖clientKeyMsg), client
///      replies H(clientKeyMsg‖serverKeyMsg).
///   7. Server sends a 1-byte subtype: 1 = username+password, 2 = password only.
///      Client replies [ulen][user][plen][pass] (UTF-8).
///   8. SecurityResult (and for RA2/RA2_256 the whole RFB session) flows through
///      the encrypted channel. The "ne" variants leave post-handshake plaintext.
use crate::connection::Credentials;
use aes::{Aes128, Aes256};
use anyhow::{bail, Context, Result};
use eax::{
    aead::{generic_array::GenericArray, Aead, KeyInit, Payload},
    Eax,
};
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info};

/// Credentials requested by the server inside the authenticated AES channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSubtype {
    UsernamePassword,
    Password,
}

impl CredentialSubtype {
    fn parse(message: &[u8]) -> Result<Self> {
        match message {
            [1] => Ok(Self::UsernamePassword),
            [2] => Ok(Self::Password),
            [subtype] => bail!("RA2: unsupported credential subtype {subtype}"),
            _ => bail!("RA2: credential subtype must be exactly one byte"),
        }
    }
}

enum EaxCipher {
    Aes128(Eax<Aes128>),
    Aes256(Eax<Aes256>),
}

/// One direction of the AES-EAX channel (own key + own message counter).
pub struct Channel {
    cipher: EaxCipher,
    counter: u128,
}

impl Channel {
    fn new(key: &[u8], sha256: bool) -> Self {
        let cipher = if sha256 {
            EaxCipher::Aes256(Eax::<Aes256>::new(GenericArray::from_slice(&key[..32])))
        } else {
            EaxCipher::Aes128(Eax::<Aes128>::new(GenericArray::from_slice(&key[..16])))
        };
        Self { cipher, counter: 0 }
    }

    fn next_nonce(&mut self) -> [u8; 16] {
        let nonce = self.counter.to_le_bytes();
        self.counter += 1;
        nonce
    }

    /// Encrypt `msg` into a framed message: [U16 len][ciphertext][16-byte MAC].
    pub fn seal(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
        let len_be = (msg.len() as u16).to_be_bytes();
        let nonce = self.next_nonce();
        let payload = Payload { msg, aad: &len_be };
        let ct = match &self.cipher {
            EaxCipher::Aes128(c) => c.encrypt(GenericArray::from_slice(&nonce), payload),
            EaxCipher::Aes256(c) => c.encrypt(GenericArray::from_slice(&nonce), payload),
        }
        .map_err(|_| anyhow::anyhow!("RA2: encryption failed"))?;
        let mut out = Vec::with_capacity(2 + ct.len());
        out.extend_from_slice(&len_be);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt a framed message body (`ciphertext‖MAC`) whose length prefix was `len`.
    pub fn open(&mut self, len: u16, ct_and_mac: &[u8]) -> Result<Vec<u8>> {
        let len_be = len.to_be_bytes();
        let nonce = self.next_nonce();
        let payload = Payload {
            msg: ct_and_mac,
            aad: &len_be,
        };
        match &self.cipher {
            EaxCipher::Aes128(c) => c.decrypt(GenericArray::from_slice(&nonce), payload),
            EaxCipher::Aes256(c) => c.decrypt(GenericArray::from_slice(&nonce), payload),
        }
        .map_err(|_| anyhow::anyhow!("RA2: MAC verification failed (corrupt or desynced stream)"))
    }
}

/// Read one framed encrypted message from the wire and decrypt it.
pub async fn read_framed<S>(stream: &mut S, ch: &mut Channel) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let len = stream.read_u16().await?;
    let mut body = vec![0u8; len as usize + 16];
    stream.read_exact(&mut body).await?;
    ch.open(len, &body)
}

async fn write_framed<S>(stream: &mut S, ch: &mut Channel, msg: &[u8]) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let framed = ch.seal(msg)?;
    stream.write_all(&framed).await?;
    Ok(())
}

fn hash(sha256: bool, parts: &[&[u8]]) -> Vec<u8> {
    if sha256 {
        let mut h = Sha256::new();
        for p in parts {
            h.update(p);
        }
        h.finalize().to_vec()
    } else {
        let mut h = Sha1::new();
        for p in parts {
            h.update(p);
        }
        h.finalize().to_vec()
    }
}

/// Left-pad `bytes` with zeros to exactly `len`.
fn pad_be(bytes: Vec<u8>, len: usize) -> Vec<u8> {
    if bytes.len() >= len {
        return bytes;
    }
    let mut out = vec![0u8; len - bytes.len()];
    out.extend_from_slice(&bytes);
    out
}

/// Run the RA2 handshake through the credential exchange.
/// Returns the (client→server, server→client) channels for the caller to keep
/// using — the SecurityResult and (for non-"ne" variants) the whole session
/// flow through them.
pub async fn handshake<S, F>(
    stream: &mut S,
    sha256: bool,
    resolve_credentials: F,
) -> Result<(Channel, Channel)>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(CredentialSubtype) -> Result<Credentials>,
{
    // --- 1. Server public key ---
    let server_bits = stream.read_u32().await?;
    if !(1024..=8192).contains(&server_bits) {
        bail!("RA2: implausible server RSA key size: {server_bits} bits");
    }
    let server_klen = server_bits.div_ceil(8) as usize;
    let mut server_mod = vec![0u8; server_klen];
    stream.read_exact(&mut server_mod).await?;
    let mut server_exp = vec![0u8; server_klen];
    stream.read_exact(&mut server_exp).await?;

    // Keep the exact wire bytes for the hash-verification step.
    let mut server_key_msg = Vec::with_capacity(4 + 2 * server_klen);
    server_key_msg.extend_from_slice(&server_bits.to_be_bytes());
    server_key_msg.extend_from_slice(&server_mod);
    server_key_msg.extend_from_slice(&server_exp);

    let server_pub = RsaPublicKey::new(
        BigUint::from_bytes_be(&server_mod),
        BigUint::from_bytes_be(&server_exp),
    )
    .context("RA2: invalid server RSA key")?;
    debug!("RA2: server key {server_bits} bits");

    // --- 2. Client public key (fresh 2048-bit keypair) ---
    let mut rng = rand::thread_rng();
    let client_priv =
        RsaPrivateKey::new(&mut rng, 2048).context("RA2: client RSA keygen failed")?;
    let client_pub = RsaPublicKey::from(&client_priv);

    const CLIENT_BITS: u32 = 2048;
    const CLIENT_KLEN: usize = 256;
    use rsa::traits::PublicKeyParts;
    let client_mod = pad_be(client_pub.n().to_bytes_be(), CLIENT_KLEN);
    let client_exp = pad_be(client_pub.e().to_bytes_be(), CLIENT_KLEN);

    let mut client_key_msg = Vec::with_capacity(4 + 2 * CLIENT_KLEN);
    client_key_msg.extend_from_slice(&CLIENT_BITS.to_be_bytes());
    client_key_msg.extend_from_slice(&client_mod);
    client_key_msg.extend_from_slice(&client_exp);
    stream.write_all(&client_key_msg).await?;
    stream.flush().await?;

    // --- 3. Random exchange (16 bytes each, RSA-PKCS#1 v1.5) ---
    let enc_len = stream.read_u16().await? as usize;
    if enc_len != CLIENT_KLEN {
        bail!("RA2: unexpected encrypted-random length {enc_len} (expected {CLIENT_KLEN})");
    }
    let mut enc_server_random = vec![0u8; enc_len];
    stream.read_exact(&mut enc_server_random).await?;
    let server_random = client_priv
        .decrypt(Pkcs1v15Encrypt, &enc_server_random)
        .context("RA2: failed to decrypt server random")?;

    let random_len = if sha256 { 32 } else { 16 };
    let mut client_random = vec![0u8; random_len];
    rand::RngCore::fill_bytes(&mut rng, &mut client_random);
    let enc_client_random = server_pub
        .encrypt(&mut rng, Pkcs1v15Encrypt, &client_random)
        .context("RA2: failed to encrypt client random")?;
    stream
        .write_all(&(enc_client_random.len() as u16).to_be_bytes())
        .await?;
    stream.write_all(&enc_client_random).await?;
    stream.flush().await?;

    // --- 4. Session keys ---
    let client_session_key = hash(sha256, &[&server_random, &client_random]);
    let server_session_key = hash(sha256, &[&client_random, &server_random]);
    let mut send = Channel::new(&client_session_key, sha256); // client → server
    let mut recv = Channel::new(&server_session_key, sha256); // server → client

    // --- 6. Hash verification ---
    let server_hash = read_framed(stream, &mut recv).await?;
    let expected = hash(sha256, &[&server_key_msg, &client_key_msg]);
    if server_hash != expected {
        bail!("RA2: server key hash mismatch — possible MITM or protocol error");
    }
    let client_hash = hash(sha256, &[&client_key_msg, &server_key_msg]);
    write_framed(stream, &mut send, &client_hash).await?;
    stream.flush().await?;

    // --- 7. Credential subtype + credentials ---
    exchange_credentials(stream, &mut send, &mut recv, resolve_credentials).await?;

    Ok((send, recv))
}

/// Resolve credentials only after decrypting and validating the server's subtype.
async fn exchange_credentials<S, F>(
    stream: &mut S,
    send: &mut Channel,
    recv: &mut Channel,
    resolve_credentials: F,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(CredentialSubtype) -> Result<Credentials>,
{
    let subtype_msg = read_framed(stream, recv).await?;
    let subtype = CredentialSubtype::parse(&subtype_msg)?;
    info!(
        "RA2: server requests {}",
        match subtype {
            CredentialSubtype::UsernamePassword => "username + password",
            CredentialSubtype::Password => "password only",
        }
    );

    let credentials = resolve_credentials(subtype)?;
    let message = encode_credentials(subtype, &credentials)?;
    write_framed(stream, send, &message).await?;
    stream.flush().await?;
    Ok(())
}

fn encode_credentials(subtype: CredentialSubtype, credentials: &Credentials) -> Result<Vec<u8>> {
    let username = match subtype {
        CredentialSubtype::UsernamePassword => credentials
            .username
            .as_deref()
            .context("RA2: username was not supplied")?,
        CredentialSubtype::Password => "",
    };
    let username_len =
        u8::try_from(username.len()).context("RA2: username too long (max 255 UTF-8 bytes)")?;
    let password_len = u8::try_from(credentials.password.len())
        .context("RA2: password too long (max 255 UTF-8 bytes)")?;
    let mut message = Vec::with_capacity(2 + username.len() + credentials.password.len());
    message.push(username_len);
    message.extend_from_slice(username.as_bytes());
    message.push(password_len);
    message.extend_from_slice(credentials.password.as_bytes());
    Ok(message)
}

/// Pump: decrypt framed messages from the server and feed plaintext to `out`.
pub async fn pump_decrypt<R, W>(mut wire: R, mut out: W, mut ch: Channel)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let plain = match read_framed(&mut wire, &mut ch).await {
            Ok(p) => p,
            Err(e) => {
                debug!("RA2 decrypt pump ended: {e}");
                break;
            }
        };
        if out.write_all(&plain).await.is_err() {
            break;
        }
    }
    // Dropping `out` signals EOF to the reader side.
}

/// Pump: read plaintext from `input`, encrypt, and write framed messages to the wire.
pub async fn pump_encrypt<R, W>(mut input: R, mut wire: W, mut ch: Channel)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 8192];
    loop {
        let n = match input.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let framed = match ch.seal(&buf[..n]) {
            Ok(f) => f,
            Err(_) => break,
        };
        if wire.write_all(&framed).await.is_err() {
            break;
        }
        if wire.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn credentials(username: Option<&str>, password: &str) -> Credentials {
        Credentials {
            username: username.map(str::to_owned),
            password: password.to_owned(),
        }
    }

    #[test]
    fn credentials_use_utf8_byte_lengths_and_interleaved_fields() {
        let encoded = encode_credentials(
            CredentialSubtype::UsernamePassword,
            &credentials(Some("用戶"), "p🔒é"),
        )
        .unwrap();
        assert_eq!(
            encoded,
            b"\x06\xe7\x94\xa8\xe6\x88\xb6\x07p\xf0\x9f\x94\x92\xc3\xa9"
        );
    }

    #[test]
    fn credentials_accept_exactly_255_utf8_bytes_per_field() {
        let username = format!("{}a", "é".repeat(127));
        let password = format!("{}abc", "🔒".repeat(63));
        let encoded = encode_credentials(
            CredentialSubtype::UsernamePassword,
            &credentials(Some(&username), &password),
        )
        .unwrap();
        assert_eq!(encoded.len(), 512);
        assert_eq!(encoded[0], 255);
        assert_eq!(&encoded[1..256], username.as_bytes());
        assert_eq!(encoded[256], 255);
        assert_eq!(&encoded[257..], password.as_bytes());
    }

    #[test]
    fn credentials_reject_oversized_utf8_fields_without_truncating() {
        let oversized = "é".repeat(128);
        let username_error = encode_credentials(
            CredentialSubtype::UsernamePassword,
            &credentials(Some(&oversized), "password"),
        )
        .unwrap_err();
        assert!(username_error.to_string().contains("username too long"));

        let password_error = encode_credentials(
            CredentialSubtype::UsernamePassword,
            &credentials(Some("user"), &oversized),
        )
        .unwrap_err();
        assert!(password_error.to_string().contains("password too long"));
    }

    #[test]
    fn password_subtype_omits_even_an_oversized_supplied_username() {
        let encoded = encode_credentials(
            CredentialSubtype::Password,
            &credentials(Some(&"u".repeat(256)), "p🔒é"),
        )
        .unwrap();
        assert_eq!(encoded, b"\x00\x07p\xf0\x9f\x94\x92\xc3\xa9");
    }

    #[test]
    fn username_subtype_allows_explicit_empty_username_but_requires_a_value() {
        assert_eq!(
            encode_credentials(
                CredentialSubtype::UsernamePassword,
                &credentials(Some(""), ""),
            )
            .unwrap(),
            [0, 0]
        );
        assert!(encode_credentials(
            CredentialSubtype::UsernamePassword,
            &credentials(None, "password"),
        )
        .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn encrypted_subtype_selects_credentials_for_both_aes_sizes() {
        for sha256 in [false, true] {
            for (wire_subtype, expected_subtype, expected_payload) in [
                (
                    1,
                    CredentialSubtype::UsernamePassword,
                    b"\x06\xe7\x94\xa8\xe6\x88\xb6\x07p\xf0\x9f\x94\x92\xc3\xa9".as_slice(),
                ),
                (
                    2,
                    CredentialSubtype::Password,
                    b"\x00\x07p\xf0\x9f\x94\x92\xc3\xa9".as_slice(),
                ),
            ] {
                let (mut client, mut server) = tokio::io::duplex(4096);
                let mut client_send = Channel::new(&[0x31; 32], sha256);
                let mut client_recv = Channel::new(&[0x47; 32], sha256);
                let mut server_send = Channel::new(&[0x47; 32], sha256);
                let mut server_recv = Channel::new(&[0x31; 32], sha256);
                let subtype_sent = Cell::new(false);
                let requested = Cell::new(false);

                let (result, payload) = tokio::join!(
                    exchange_credentials(
                        &mut client,
                        &mut client_send,
                        &mut client_recv,
                        |subtype| {
                            assert!(subtype_sent.get());
                            assert_eq!(subtype, expected_subtype);
                            requested.set(true);
                            Ok(credentials(Some("用戶"), "p🔒é"))
                        },
                    ),
                    async {
                        write_framed(&mut server, &mut server_send, &[wire_subtype])
                            .await
                            .unwrap();
                        subtype_sent.set(true);
                        read_framed(&mut server, &mut server_recv).await.unwrap()
                    },
                );
                result.unwrap();
                assert!(requested.get());
                assert_eq!(payload, expected_payload);
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_encrypted_subtypes_never_resolve_or_send_credentials() {
        for sha256 in [false, true] {
            for message in [vec![], vec![0], vec![3], vec![255], vec![1, 2]] {
                let (mut client, mut server) = tokio::io::duplex(4096);
                let mut send = Channel::new(&[0x31; 32], sha256);
                let mut recv = Channel::new(&[0x47; 32], sha256);
                let mut server_send = Channel::new(&[0x47; 32], sha256);
                write_framed(&mut server, &mut server_send, &message)
                    .await
                    .unwrap();

                let result = exchange_credentials(&mut client, &mut send, &mut recv, |_| {
                    panic!("invalid subtype must not request credentials")
                })
                .await;
                assert!(result.is_err());
                drop(client);
                let mut response = Vec::new();
                server.read_to_end(&mut response).await.unwrap();
                assert!(response.is_empty());
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_subtype_mac_never_resolves_credentials() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let mut send = Channel::new(&[0x31; 32], false);
        let mut recv = Channel::new(&[0x47; 32], false);
        let mut server_send = Channel::new(&[0x47; 32], false);
        let mut frame = server_send.seal(&[1]).unwrap();
        *frame.last_mut().unwrap() ^= 1;
        server.write_all(&frame).await.unwrap();

        let error = exchange_credentials(&mut client, &mut send, &mut recv, |_| {
            panic!("unauthenticated subtype must not request credentials")
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("MAC verification failed"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_credentials_are_rejected_before_any_response_is_sent() {
        for (username, password) in [
            ("é".repeat(128), "password".to_owned()),
            ("user".to_owned(), "é".repeat(128)),
        ] {
            let (mut client, mut server) = tokio::io::duplex(4096);
            let mut send = Channel::new(&[0x31; 32], false);
            let mut recv = Channel::new(&[0x47; 32], false);
            let mut server_send = Channel::new(&[0x47; 32], false);
            write_framed(&mut server, &mut server_send, &[1])
                .await
                .unwrap();

            let result = exchange_credentials(&mut client, &mut send, &mut recv, |_| {
                Ok(credentials(Some(&username), &password))
            })
            .await;
            assert!(result.is_err());
            drop(client);
            let mut response = Vec::new();
            server.read_to_end(&mut response).await.unwrap();
            assert!(response.is_empty());
        }
    }
}
