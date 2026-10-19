//! Apple Remote Desktop authentication (RFB security type 30).
//!
//! Wire reference: rfbproto, "Diffie-Hellman Authentication" (section 7.2.13).
//! The server supplies DH parameters; the client sends 128 AES-128-ECB encrypted
//! credential bytes followed by its fixed-width, big-endian public value.
//! MD5 of the fixed-width shared secret is the AES key. These legacy primitives
//! are required for interoperability, not a choice of modern session encryption:
//! ARD neither authenticates the server nor encrypts the subsequent RFB session.

use crate::connection::Credentials;
use aes::Aes128;
use anyhow::{bail, ensure, Context, Result};
use cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use crypto_bigint::{
    modular::{BoxedMontyForm, BoxedMontyParams},
    BoxedUint, Odd,
};
use rand::{rngs::OsRng, CryptoRng, RngCore};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

// Include both Apple's older 1024-bit parameters and modern 4096-bit/generator-5
// offers. The upper bound is inclusive, checked before allocation or DH work.
const MIN_KEY_BYTES: usize = 128;
const MAX_KEY_BYTES: usize = 512;
const FIELD_BYTES: usize = 64;

struct Parameters {
    key_bytes: usize,
    modulus: Odd<BoxedUint>,
    generator: BoxedUint,
    server_public: BoxedUint,
}

impl Parameters {
    async fn read<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Self> {
        let generator = stream.read_u16().await.context("ARD: missing generator")?;
        let key_bytes = usize::from(stream.read_u16().await.context("ARD: missing key length")?);
        ensure!(
            (MIN_KEY_BYTES..=MAX_KEY_BYTES).contains(&key_bytes),
            "ARD: unsupported DH key length {key_bytes} bytes (expected 128..=512)"
        );
        let mut modulus_bytes = vec![0; key_bytes];
        let mut public_bytes = vec![0; key_bytes];
        stream
            .read_exact(&mut modulus_bytes)
            .await
            .context("ARD: truncated DH modulus")?;
        stream
            .read_exact(&mut public_bytes)
            .await
            .context("ARD: truncated server public value")?;
        Self::parse(generator, &modulus_bytes, &public_bytes)
    }

    fn parse(generator: u16, modulus_bytes: &[u8], public_bytes: &[u8]) -> Result<Self> {
        let key_bytes = modulus_bytes.len();
        ensure!(
            (MIN_KEY_BYTES..=MAX_KEY_BYTES).contains(&key_bytes) && public_bytes.len() == key_bytes,
            "ARD: invalid DH parameter lengths"
        );
        let bits = (key_bytes * 8) as u32;
        let modulus = BoxedUint::from_be_slice(modulus_bytes, bits)?;
        // Leading zero padding is legal, but must not disguise a tiny modulus.
        ensure!(
            modulus.bits() >= 1024,
            "ARD: DH modulus is smaller than 1024 bits"
        );
        let modulus = Odd::new(modulus)
            .into_option()
            .context("ARD: DH modulus must be odd")?;
        let one = BoxedUint::one_with_precision(bits);
        let upper = modulus.as_ref().wrapping_sub(&one);
        let generator = BoxedUint::from_be_slice(&generator.to_be_bytes(), bits)?;
        let server_public = BoxedUint::from_be_slice(public_bytes, bits)?;
        ensure!(
            generator > one && generator < upper,
            "ARD: invalid DH generator"
        );
        ensure!(
            server_public > one && server_public < upper,
            "ARD: invalid server DH public value"
        );
        Ok(Self {
            key_bytes,
            modulus,
            generator,
            server_public,
        })
    }

    fn private_key<R: RngCore + CryptoRng>(&self, rng: &mut R) -> Result<Zeroizing<BoxedUint>> {
        let bits = self.modulus.as_ref().bits_precision();
        let modulus_bits = self.modulus.as_ref().bits();
        let one = BoxedUint::one_with_precision(bits);
        let upper = self.modulus.as_ref().wrapping_sub(&one);
        let mut bytes = Zeroizing::new(vec![0; modulus_bits.div_ceil(8) as usize]);
        // Rejection sampling is uniform in [2, p-2]. Mask only public excess bits;
        // a padded modulus must not make sampling exponentially unlikely.
        let mask = 0xff >> ((8 - modulus_bits % 8) % 8);
        for _ in 0..128 {
            rng.try_fill_bytes(&mut bytes)
                .context("ARD: operating system randomness unavailable")?;
            bytes[0] &= mask;
            let private = Zeroizing::new(BoxedUint::from_be_slice(&bytes, bits)?);
            if *private > one && *private < upper {
                return Ok(private);
            }
        }
        bail!("ARD: could not generate a valid DH private key")
    }

    fn exchange<R: RngCore + CryptoRng>(
        &self,
        rng: &mut R,
    ) -> Result<(Vec<u8>, Zeroizing<[u8; 16]>)> {
        let private = self.private_key(rng)?;
        let params = BoxedMontyParams::new(self.modulus.clone());
        // pow() uses the fixed precision of the exponent, never its secret bit
        // length. All secret arithmetic is provided by RustCrypto crypto-bigint.
        let public = BoxedMontyForm::new(self.generator.clone(), &params)
            .pow(&private)
            .retrieve();
        let shared_monty =
            Zeroizing::new(BoxedMontyForm::new(self.server_public.clone(), &params).pow(&private));
        let shared = Zeroizing::new(shared_monty.retrieve());
        let one = BoxedUint::one_with_precision(self.modulus.as_ref().bits_precision());
        let upper = self.modulus.as_ref().wrapping_sub(&one);
        ensure!(
            public > one && public < upper && *shared > one && *shared < upper,
            "ARD: degenerate DH exchange"
        );
        // BoxedUint rounds its storage to machine limbs; the wire and the KDF
        // require exactly key_bytes, including any leading zeroes.
        let shared_bytes = Zeroizing::new(shared.to_be_bytes());
        let key =
            Zeroizing::new(md5::compute(&shared_bytes[shared_bytes.len() - self.key_bytes..]).0);
        let public_bytes = public.to_be_bytes();
        Ok((
            public_bytes[public_bytes.len() - self.key_bytes..].to_vec(),
            key,
        ))
    }
}

fn encrypt_credentials<R: RngCore + CryptoRng>(
    credentials: &Credentials,
    key: &[u8; 16],
    rng: &mut R,
) -> Result<Zeroizing<[u8; 128]>> {
    let username = credentials
        .username
        .as_deref()
        .context("ARD: username was not supplied")?;
    for (name, value) in [
        ("username", username),
        ("password", credentials.password.as_str()),
    ] {
        ensure!(
            value.len() < FIELD_BYTES,
            "ARD: {name} too long (max 63 UTF-8 bytes)"
        );
        ensure!(
            !value.as_bytes().contains(&0),
            "ARD: {name} contains an embedded NUL"
        );
    }
    let mut message = Zeroizing::new([0u8; 128]);
    rng.try_fill_bytes(message.as_mut())
        .context("ARD: operating system randomness unavailable")?;
    for (offset, value) in [(0, username), (FIELD_BYTES, credentials.password.as_str())] {
        message[offset..offset + value.len()].copy_from_slice(value.as_bytes());
        message[offset + value.len()] = 0;
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    for block in message.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(message)
}

/// Complete the type-30 exchange. The caller reads SecurityResult and then
/// continues using the plain stream; no ARD cipher wraps framebuffer traffic.
pub async fn handshake<S, F>(stream: &mut S, resolve_credentials: F) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce() -> Result<Credentials>,
{
    let parameters = Parameters::read(stream).await?;
    let mut rng = OsRng;
    let (public, key) = parameters.exchange(&mut rng)?;
    // Resolve only after validating the server's parameters. This reuses the
    // account prompt, supplied-field preservation and cancellation behavior.
    let mut credentials = resolve_credentials()?;
    let result = encrypt_credentials(&credentials, &key, &mut rng);
    // The existing session model owns saved credentials; erase this handshake's
    // temporary copies even when credential validation fails.
    use zeroize::Zeroize;
    credentials.username.zeroize();
    credentials.password.zeroize();
    let message = result?;
    stream
        .write_all(message.as_ref())
        .await
        .context("ARD: failed to send credentials")?;
    stream
        .write_all(&public)
        .await
        .context("ARD: failed to send client public value")?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
