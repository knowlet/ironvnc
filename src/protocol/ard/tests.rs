use super::*;
use cipher::BlockDecrypt;

// RFC 2409 section 6.2 and RFC 3526 section 5. Modern Apple servers have
// been observed using the latter prime with generator 5 rather than 2.
const GROUP2: &str = "
FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1
29024E08 8A67CC74 020BBEA6 3B139B22 514A0879 8E3404DD
EF9519B3 CD3A431B 302B0A6D F25F1437 4FE1356D 6D51C245
E485B576 625E7EC6 F44C42E9 A637ED6B 0BFF5CB6 F406B7ED
EE386BFB 5A899FA5 AE9F2411 7C4B1FE6 49286651 ECE65381
FFFFFFFF FFFFFFFF";

const GROUP16: &str = "
FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1
29024E08 8A67CC74 020BBEA6 3B139B22 514A0879 8E3404DD
EF9519B3 CD3A431B 302B0A6D F25F1437 4FE1356D 6D51C245
E485B576 625E7EC6 F44C42E9 A637ED6B 0BFF5CB6 F406B7ED
EE386BFB 5A899FA5 AE9F2411 7C4B1FE6 49286651 ECE45B3D
C2007CB8 A163BF05 98DA4836 1C55D39A 69163FA8 FD24CF5F
83655D23 DCA3AD96 1C62F356 208552BB 9ED52907 7096966D
670C354E 4ABC9804 F1746C08 CA18217C 32905E46 2E36CE3B
E39E772C 180E8603 9B2783A2 EC07A28F B5C55DF0 6F4C52C9
DE2BCBF6 95581718 3995497C EA956AE5 15D22618 98FA0510
15728E5A 8AAAC42D AD33170D 04507A33 A85521AB DF1CBA64
ECFB8504 58DBEF0A 8AEA7157 5D060C7D B3970F85 A6E1E4C7
ABF5AE8C DB0933D7 1E8C94E0 4A25619D CEE3D226 1AD2EE6B
F12FFA06 D98A0864 D8760273 3EC86A64 521F2B18 177B200C
BBE11757 7A615D6C 770988C0 BAD946E2 08E24FA0 74E5AB31
43DB5BFC E0FD108E 4B82D120 A9210801 1A723C12 A787E6D7
88719A10 BDBA5B26 99C32718 6AF4E23C 1A946834 B6150BDA
2583E9CA 2AD44CE8 DBBBC2DB 04DE8EF9 2E8EFC14 1FBECAA6
287C5947 4E6BC05D 99B2964F A090C3A2 233BA186 515BE7ED
1F612970 CEE2D7AF B81BDD76 2170481C D0069127 D5B05AA9
93B4EA98 8D8FDDC1 86FFB7DC 90A6C08F 4DF435C9 34063199
FFFFFFFF FFFFFFFF";

fn hex(value: &str) -> Vec<u8> {
    let compact: String = value.split_whitespace().collect();
    (0..compact.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&compact[index..index + 2], 16).unwrap())
        .collect()
}

fn credentials(username: &str, password: &str) -> Credentials {
    Credentials {
        username: Some(username.to_owned()),
        password: password.to_owned(),
    }
}

fn small_public(width: usize, value: u8) -> Vec<u8> {
    let mut bytes = vec![0; width];
    bytes[width - 1] = value;
    bytes
}

enum Fill {
    Counting(u8),
    Small(u8),
    Failure,
}

// Deterministic entropy is confined to these tests. Tracking calls also checks
// that invalid credentials are rejected before any encryption is attempted.
struct TestRng {
    fill: Fill,
    calls: usize,
}

impl TestRng {
    fn new(fill: Fill) -> Self {
        Self { fill, calls: 0 }
    }
}

impl CryptoRng for TestRng {}

impl RngCore for TestRng {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.try_fill_bytes(dest).unwrap();
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), rand::Error> {
        self.calls += 1;
        match &mut self.fill {
            Fill::Counting(next) => {
                for byte in dest {
                    *byte = *next;
                    *next = next.wrapping_add(1);
                }
            }
            Fill::Small(value) => {
                dest.fill(0);
                *dest.last_mut().unwrap() = *value;
            }
            Fill::Failure => return Err(rand::Error::new("synthetic entropy failure")),
        }
        Ok(())
    }
}

#[test]
fn credentials_match_published_novnc_ecb_vector() {
    // noVNC 4a2cc175ae8af50fe8cb85a05d40908c01c43f6a,
    // tests/test.rfb.js: "should return properly encrypted credentials and public key".
    // The published vector was independently checked with Python MD5/OpenSSL.
    let key: [u8; 16] = hex("8592cc5e5b4b2da071a8087a15a1b8b5").try_into().unwrap();
    let expected = hex("c727cc5fbe467f42056a99e47bec96ce
        3e6b0b0415f25cb80951237d38a701d7 b691b74bf5c52f137a5e404c4da3de8f
        baae5427f4b3e37253e72a6acd2b9f6e d1f09df6edce8699c3705c3c1cea5b42
        8326bbc36ea7d4f120fad4d5ca59b415 47d9d1512a3d76f8417b624e8b6fca89
        32b925ad3a63bb352a7d0da5e8a3972a");
    let encrypted = encrypt_credentials(
        &credentials("user", "password"),
        &key,
        &mut TestRng::new(Fill::Counting(0)),
    )
    .unwrap();
    assert_eq!(encrypted.as_ref(), expected.as_slice());
}

#[test]
fn credential_fields_accept_63_utf8_bytes_and_explicit_empty_values() {
    for (username, password) in [
        ("用".repeat(21), "密".repeat(21)),
        (String::new(), String::new()),
    ] {
        let mut encrypted = encrypt_credentials(
            &credentials(&username, &password),
            &[7; 16],
            &mut TestRng::new(Fill::Counting(0)),
        )
        .unwrap();
        let cipher = Aes128::new_from_slice(&[7; 16]).unwrap();
        for block in encrypted.chunks_exact_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(block));
        }
        assert_eq!(&encrypted[..username.len()], username.as_bytes());
        assert_eq!(encrypted[username.len()], 0);
        assert_eq!(&encrypted[64..64 + password.len()], password.as_bytes());
        assert_eq!(encrypted[64 + password.len()], 0);
    }
}

#[test]
fn credentials_reject_byte_overflow_without_truncation() {
    for value in ["x".repeat(64), "用".repeat(22)] {
        for (field, creds) in [
            ("username", credentials(&value, "p")),
            ("password", credentials("u", &value)),
        ] {
            let mut rng = TestRng::new(Fill::Counting(0));
            let error = encrypt_credentials(&creds, &[0; 16], &mut rng).unwrap_err();
            assert!(error.to_string().contains(&format!("{field} too long")));
            assert_eq!(rng.calls, 0);
        }
    }
}

#[test]
fn credentials_reject_nul_in_either_field() {
    for (field, creds) in [
        ("username", credentials("u\0hidden", "p")),
        ("password", credentials("u", "p\0hidden")),
    ] {
        let mut rng = TestRng::new(Fill::Counting(0));
        let error = encrypt_credentials(&creds, &[0; 16], &mut rng).unwrap_err();
        assert!(error
            .to_string()
            .contains(&format!("{field} contains an embedded NUL")));
        assert_eq!(rng.calls, 0);
    }
}

#[test]
fn missing_username_is_not_silently_replaced_with_empty() {
    let mut creds = credentials("", "password");
    creds.username = None;
    let error =
        encrypt_credentials(&creds, &[0; 16], &mut TestRng::new(Fill::Counting(0))).unwrap_err();
    assert!(error.to_string().contains("username was not supplied"));
}

#[test]
fn parameters_accept_legacy_and_modern_apple_group_boundaries() {
    for (group, width, generator) in [(GROUP2, 128, 2), (GROUP16, 512, 5)] {
        let modulus = hex(group);
        assert_eq!(modulus.len(), width);
        let parsed = Parameters::parse(generator, &modulus, &small_public(width, 32)).unwrap();
        assert_eq!(parsed.key_bytes, width);
    }
}

#[tokio::test]
async fn rejected_key_lengths_do_not_wait_for_a_parameter_body() {
    for length in [0u16, 127, 513, u16::MAX] {
        let header = [0, 2, (length >> 8) as u8, length as u8];
        let mut stream = header.as_slice();
        let error = Parameters::read(&mut stream)
            .await
            .err()
            .expect("invalid length");
        assert!(error.to_string().contains("unsupported DH key length"));
    }
}

#[test]
fn malformed_dh_parameters_are_rejected() {
    let modulus = hex(GROUP2);
    let public = small_public(128, 32);
    for generator in [0, 1] {
        assert!(Parameters::parse(generator, &modulus, &public).is_err());
    }
    let mut upper = modulus.clone();
    *upper.last_mut().unwrap() -= 1;
    for invalid in [
        small_public(128, 0),
        small_public(128, 1),
        upper.clone(),
        modulus.clone(),
    ] {
        assert!(Parameters::parse(2, &modulus, &invalid).is_err());
    }
    let mut undersized = modulus.clone();
    undersized[0] = 0x7f;
    for invalid in [
        small_public(128, 0),
        small_public(128, 1),
        upper,
        undersized,
    ] {
        assert!(Parameters::parse(2, &invalid, &public).is_err());
    }
    assert!(Parameters::parse(2, &modulus, &public[..127]).is_err());
}

#[test]
fn dh_known_answer_preserves_wire_width_including_non_limb_padding() {
    // Independent arithmetic: g=2, x=3, B=2^5=32; A=8 and shared=32768.
    // hashlib.md5((32768).to_bytes(width, "big")) gives these fixed digests.
    for (width, expected_key) in [
        (128, "0bd822ed5926edce93fccfee0412f6e3"),
        (129, "a2153bf4b1ebca36355e7f621842ba62"),
    ] {
        let mut modulus = vec![0; width - 128];
        modulus.extend(hex(GROUP2));
        let parameters = Parameters::parse(2, &modulus, &small_public(width, 32)).unwrap();
        let mut rng = TestRng::new(Fill::Small(3));
        let (public, key) = parameters.exchange(&mut rng).unwrap();
        assert_eq!(public, small_public(width, 8));
        assert_eq!(key.as_ref(), hex(expected_key).as_slice());
        assert_eq!(rng.calls, 1);
    }
}

#[test]
fn entropy_failure_and_invalid_private_sampling_fail_cleanly() {
    let parameters = Parameters::parse(2, &hex(GROUP2), &small_public(128, 32)).unwrap();
    let mut failure = TestRng::new(Fill::Failure);
    let error = parameters.exchange(&mut failure).unwrap_err();
    assert!(error.to_string().contains("randomness unavailable"));
    let error = encrypt_credentials(&credentials("u", "p"), &[0; 16], &mut failure).unwrap_err();
    assert!(error.to_string().contains("randomness unavailable"));
    let mut zero = TestRng::new(Fill::Small(0));
    let error = parameters.private_key(&mut zero).unwrap_err();
    assert!(error
        .to_string()
        .contains("could not generate a valid DH private key"));
    assert_eq!(zero.calls, 128);
}

async fn negotiated_tcp_handshake(minor: u8, denied: bool) {
    use crate::protocol::security;
    use tokio::net::{TcpListener, TcpStream};

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(&[4, 30, 33, 36, 35]).await.unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), 30);
            stream.write_u16(2).await.unwrap();
            stream.write_u16(128).await.unwrap();
            stream.write_all(&hex(GROUP2)).await.unwrap();
            // Test server private exponent is 1, so its public value is g=2
            // and shared secret is exactly the client's received public value.
            // This keeps KDF verification independent of the viewer's DH code.
            stream.write_all(&small_public(128, 2)).await.unwrap();
            let mut encrypted = [0; 128];
            let mut client_public = [0; 128];
            stream.read_exact(&mut encrypted).await.unwrap();
            stream.read_exact(&mut client_public).await.unwrap();
            let cipher = Aes128::new_from_slice(&md5::compute(client_public).0).unwrap();
            for block in encrypted.chunks_exact_mut(16) {
                cipher.decrypt_block(GenericArray::from_mut_slice(block));
            }
            for (offset, expected) in [(0, "測試帳號"), (64, "密碼🔑")] {
                let field = &encrypted[offset..offset + 64];
                let end = field
                    .iter()
                    .position(|byte| *byte == 0)
                    .expect("NUL terminator");
                assert_eq!(&field[..end], expected.as_bytes());
            }
            if denied {
                stream.write_u32(1).await.unwrap();
                if minor >= 8 {
                    let reason = b"Synthetic ARD denial";
                    stream.write_u32(reason.len() as u32).await.unwrap();
                    stream.write_all(reason).await.unwrap();
                }
                // Keep the socket open until the client closes: incorrectly
                // reading a 3.8 reason for 3.7 must time out, not pass on EOF.
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            } else {
                stream.write_all(&[0, 0, 0, 0, 0xa5]).await.unwrap();
                assert_eq!(stream.read_u8().await.unwrap(), 0x5a);
            }
        };
        let client = async {
            let stream = TcpStream::connect(address).await.unwrap();
            let (event_tx, event_rx) = crossbeam_channel::unbounded();
            // A regression that prompts despite supplied credentials must
            // return an error instead of blocking the single-threaded runtime.
            let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
            drop(cmd_tx);
            let result = security::negotiate(
                stream,
                Some("測試帳號"),
                Some("密碼🔑"),
                minor,
                &event_tx,
                &cmd_rx,
            )
            .await;
            assert!(
                event_rx.is_empty(),
                "supplied credentials unexpectedly prompted"
            );
            if denied {
                let error = result.err().expect("authentication must fail");
                let expected = if minor >= 8 {
                    "Authentication failed: Synthetic ARD denial"
                } else {
                    "Authentication failed (wrong password?)"
                };
                assert_eq!(error.to_string(), expected);
            } else {
                let mut plain = result.unwrap();
                // SecurityResult was consumed exactly once and following RFB
                // traffic remains plaintext in both directions.
                assert_eq!(plain.read_u8().await.unwrap(), 0xa5);
                plain.write_u8(0x5a).await.unwrap();
            }
        };
        tokio::join!(server, client);
    })
    .await
    .expect("bounded ARD TCP handshake");
}

#[tokio::test]
async fn negotiated_ard_consumes_security_result_then_returns_plain_stream() {
    for minor in [7, 8] {
        negotiated_tcp_handshake(minor, false).await;
    }
}

#[tokio::test]
async fn negotiated_ard_denial_follows_the_negotiated_rfb_version() {
    for minor in [7, 8] {
        negotiated_tcp_handshake(minor, true).await;
    }
}
