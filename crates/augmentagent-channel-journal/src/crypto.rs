//! Entry-content envelope crypto, bit-compatible with the ShadowNote app.
//!
//! The app stores `Entry.content` as `JSON.stringify({ciphertext,
//! ciphertextDEK})` where:
//!
//! - `ciphertextDEK` = base64 of the KMS `GenerateDataKey` ciphertext blob;
//! - `ciphertext` = `CryptoJS.AES.encrypt(JSON.stringify(html), passphrase)`
//!   with `passphrase = base64(plaintext data key)`.
//!
//! CryptoJS passphrase mode is the OpenSSL EVP salted format: base64 of
//! `"Salted__" || salt(8) || AES-256-CBC ciphertext`, key+IV derived by
//! `EVP_BytesToKey` with MD5 and one iteration. The interop test vector
//! below was generated with the openssl CLI (same KDF), not with CryptoJS,
//! precisely so this module never needs a JS runtime to prove
//! compatibility.
//!
//! KMS access is behind [`DekProvider`] so unit tests run without AWS.
//! Never log plaintext journal content from this module.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use md5::{Digest, Md5};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

const OPENSSL_MAGIC: &[u8; 8] = b"Salted__";

/// The JSON object stored in `Entry.content` for encrypted entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeCiphertext {
    pub ciphertext: String,
    #[serde(rename = "ciphertextDEK")]
    pub ciphertext_dek: String,
}

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("base64 decode: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("ciphertext lacks the OpenSSL Salted__ header")]
    MissingHeader,
    #[error("decrypt failed (bad padding — wrong key or corrupt ciphertext)")]
    BadDecrypt,
    #[error("kms: {0}")]
    Kms(String),
    #[error("decrypted plaintext is not valid UTF-8")]
    Utf8,
    #[error("envelope serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Seam over KMS so tests inject a fixed key. The real impl is
/// [`KmsDekProvider`]; IAM needs `kms:Decrypt` (reads) and
/// `kms:GenerateDataKey` (writes) on the app's CMK.
#[async_trait]
pub trait DekProvider: Send + Sync {
    async fn decrypt_dek(&self, ciphertext_blob: &[u8]) -> Result<Vec<u8>, CryptoError>;
    async fn generate_dek(&self, key_arn: &str) -> Result<GeneratedDek, CryptoError>;
}

pub struct GeneratedDek {
    pub plaintext: Vec<u8>,
    pub ciphertext_blob: Vec<u8>,
}

pub struct KmsDekProvider {
    client: aws_sdk_kms::Client,
}

impl KmsDekProvider {
    pub fn new(sdk_config: &aws_config::SdkConfig) -> Self {
        Self {
            client: aws_sdk_kms::Client::new(sdk_config),
        }
    }
}

#[async_trait]
impl DekProvider for KmsDekProvider {
    async fn decrypt_dek(&self, ciphertext_blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let out = self
            .client
            .decrypt()
            .ciphertext_blob(aws_sdk_kms::primitives::Blob::new(ciphertext_blob.to_vec()))
            .send()
            .await
            .map_err(|e| CryptoError::Kms(e.to_string()))?;
        Ok(out
            .plaintext()
            .ok_or_else(|| CryptoError::Kms("Decrypt returned no plaintext".into()))?
            .as_ref()
            .to_vec())
    }

    async fn generate_dek(&self, key_arn: &str) -> Result<GeneratedDek, CryptoError> {
        let out = self
            .client
            .generate_data_key()
            .key_id(key_arn)
            .key_spec(aws_sdk_kms::types::DataKeySpec::Aes256)
            .send()
            .await
            .map_err(|e| CryptoError::Kms(e.to_string()))?;
        let plaintext = out
            .plaintext()
            .ok_or_else(|| CryptoError::Kms("GenerateDataKey returned no plaintext".into()))?
            .as_ref()
            .to_vec();
        let ciphertext_blob = out
            .ciphertext_blob()
            .ok_or_else(|| CryptoError::Kms("GenerateDataKey returned no ciphertext".into()))?
            .as_ref()
            .to_vec();
        Ok(GeneratedDek {
            plaintext,
            ciphertext_blob,
        })
    }
}

/// OpenSSL `EVP_BytesToKey(md5, count=1)`: D_i = MD5(D_{i-1} || pass || salt),
/// concatenated until 48 bytes → 32-byte key + 16-byte IV.
fn evp_bytes_to_key_md5(passphrase: &[u8], salt: &[u8; 8]) -> ([u8; 32], [u8; 16]) {
    let mut derived = Vec::with_capacity(48);
    let mut prev: Vec<u8> = Vec::new();
    while derived.len() < 48 {
        let mut h = Md5::new();
        h.update(&prev);
        h.update(passphrase);
        h.update(salt);
        prev = h.finalize().to_vec();
        derived.extend_from_slice(&prev);
    }
    let mut key = [0u8; 32];
    let mut iv = [0u8; 16];
    key.copy_from_slice(&derived[..32]);
    iv.copy_from_slice(&derived[32..48]);
    (key, iv)
}

/// Decrypt a CryptoJS-passphrase-mode payload (base64 `Salted__` format).
pub fn evp_decrypt(payload_b64: &str, passphrase: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let raw = BASE64.decode(payload_b64.trim())?;
    if raw.len() < 17 || &raw[..8] != OPENSSL_MAGIC {
        return Err(CryptoError::MissingHeader);
    }
    let salt: [u8; 8] = raw[8..16].try_into().expect("slice length checked");
    let (key, iv) = evp_bytes_to_key_md5(passphrase, &salt);
    Aes256CbcDec::new(&key.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&raw[16..])
        .map_err(|_| CryptoError::BadDecrypt)
}

/// Encrypt to the same format the app's CryptoJS call produces.
pub fn evp_encrypt(plaintext: &[u8], passphrase: &[u8], salt: [u8; 8]) -> String {
    let (key, iv) = evp_bytes_to_key_md5(passphrase, &salt);
    let ct = Aes256CbcEnc::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext);
    let mut out = Vec::with_capacity(16 + ct.len());
    out.extend_from_slice(OPENSSL_MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&ct);
    BASE64.encode(out)
}

/// `Entry.content` → plaintext (TinyMCE HTML).
///
/// Content that doesn't parse as an [`EnvelopeCiphertext`] is returned
/// verbatim — the app has the same legacy-plaintext fallback. The
/// passphrase is primarily `base64(DEK)` (Buffer polyfill semantics); the
/// comma-joined-decimal `Uint8Array.toString()` form is tried as a
/// fallback in case any historical client build lacked the polyfill.
pub async fn decrypt_entry_content(
    content: &str,
    dek_provider: &dyn DekProvider,
) -> Result<String, CryptoError> {
    let envelope: EnvelopeCiphertext = match serde_json::from_str(content) {
        Ok(e) => e,
        Err(_) => return Ok(content.to_string()),
    };
    let dek_blob = BASE64.decode(envelope.ciphertext_dek.trim())?;
    let dek = dek_provider.decrypt_dek(&dek_blob).await?;

    let pass_b64 = BASE64.encode(&dek);
    let plain = match evp_decrypt(&envelope.ciphertext, pass_b64.as_bytes()) {
        Ok(p) => p,
        Err(first_err) => {
            let pass_csv = dek
                .iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join(",");
            evp_decrypt(&envelope.ciphertext, pass_csv.as_bytes()).map_err(|_| first_err)?
        }
    };
    // The app encrypts JSON.stringify(html) — unwrap the JSON string; keep
    // raw bytes if some entry was encrypted without the stringify step.
    match serde_json::from_slice::<String>(&plain) {
        Ok(html) => Ok(html),
        Err(_) => String::from_utf8(plain).map_err(|_| CryptoError::Utf8),
    }
}

/// Plaintext HTML → `Entry.content` JSON, exactly as the app writes it.
pub async fn encrypt_entry_content(
    html: &str,
    key_arn: &str,
    dek_provider: &dyn DekProvider,
) -> Result<String, CryptoError> {
    let generated = dek_provider.generate_dek(key_arn).await?;
    let pass_b64 = BASE64.encode(&generated.plaintext);
    let mut salt = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut salt);
    let json_plain = serde_json::to_string(html)?;
    let envelope = EnvelopeCiphertext {
        ciphertext: evp_encrypt(json_plain.as_bytes(), pass_b64.as_bytes(), salt),
        ciphertext_dek: BASE64.encode(&generated.ciphertext_blob),
    };
    Ok(serde_json::to_string(&envelope)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed-key provider: "decrypting" any blob yields KEY; generate
    /// returns KEY with a recognizable blob.
    struct FixedDek;
    const KEY: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];

    #[async_trait]
    impl DekProvider for FixedDek {
        async fn decrypt_dek(&self, _blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
            Ok(KEY.to_vec())
        }
        async fn generate_dek(&self, _key_arn: &str) -> Result<GeneratedDek, CryptoError> {
            Ok(GeneratedDek {
                plaintext: KEY.to_vec(),
                ciphertext_blob: b"fake-kms-blob".to_vec(),
            })
        }
    }

    /// Generated with the openssl CLI (OpenSSL 3.0.13), which shares the
    /// EVP_BytesToKey(md5) KDF with CryptoJS passphrase mode:
    ///   printf '%s' '"<p>Rust interop vector</p>"' \
    ///     | openssl enc -aes-256-cbc -md md5 -pass pass:AAECAw… -S 0102030405060708
    /// then `Salted__` + salt + ciphertext, base64. (OpenSSL 3.x omits the
    /// header when -S is explicit; CryptoJS always includes it.)
    const INTEROP_VECTOR: &str =
        "U2FsdGVkX18BAgMEBQYHCIc16ySzcldP/wKGCp6Im5+ZFTTvmRtXbeFCniZ7bR7B";
    const INTEROP_PASS: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    #[test]
    fn evp_decrypt_matches_openssl_cryptojs_format() {
        let plain = evp_decrypt(INTEROP_VECTOR, INTEROP_PASS.as_bytes()).unwrap();
        assert_eq!(plain, br#""<p>Rust interop vector</p>""#);
    }

    #[test]
    fn evp_round_trip() {
        let plain = br#""<p>round trip</p>""#;
        let ct = evp_encrypt(plain, b"passphrase", [7u8; 8]);
        assert_eq!(evp_decrypt(&ct, b"passphrase").unwrap(), plain);
    }

    #[test]
    fn evp_wrong_passphrase_fails() {
        let ct = evp_encrypt(b"secret", b"right", [1u8; 8]);
        assert!(matches!(
            evp_decrypt(&ct, b"wrong"),
            Err(CryptoError::BadDecrypt)
        ));
    }

    #[test]
    fn evp_rejects_headerless_payload() {
        assert!(matches!(
            evp_decrypt(&BASE64.encode(b"no header here!!"), b"p"),
            Err(CryptoError::MissingHeader)
        ));
    }

    #[tokio::test]
    async fn entry_content_round_trip_through_envelope() {
        let html = "<p>dear journal</p>";
        let stored = encrypt_entry_content(html, "arn:fake", &FixedDek).await.unwrap();
        // Stored form is the app's envelope JSON…
        let env: EnvelopeCiphertext = serde_json::from_str(&stored).unwrap();
        assert_eq!(env.ciphertext_dek, BASE64.encode(b"fake-kms-blob"));
        // …and decrypts back to the HTML.
        let out = decrypt_entry_content(&stored, &FixedDek).await.unwrap();
        assert_eq!(out, html);
    }

    #[tokio::test]
    async fn interop_vector_decrypts_via_envelope_path() {
        // KEY's base64 == INTEROP_PASS, so the full envelope path (KMS fake
        // → passphrase derivation → EVP) must reproduce the openssl vector.
        let stored = serde_json::to_string(&EnvelopeCiphertext {
            ciphertext: INTEROP_VECTOR.into(),
            ciphertext_dek: BASE64.encode(b"whatever"),
        })
        .unwrap();
        let out = decrypt_entry_content(&stored, &FixedDek).await.unwrap();
        assert_eq!(out, "<p>Rust interop vector</p>");
    }

    #[tokio::test]
    async fn legacy_plaintext_passes_through() {
        let out = decrypt_entry_content("<p>never encrypted</p>", &FixedDek)
            .await
            .unwrap();
        assert_eq!(out, "<p>never encrypted</p>");
    }
}
