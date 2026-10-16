//! Each machine's own Navette identity: a key pair and a self-signed certificate made by
//! Navette. It opens the link between the two Navette instances and nothing else; it is neither
//! a work nor a desk credential. Peers pin each other by the SHA-256 of the certificate, so there
//! is no CA and no host name to check.

use std::fmt;
use std::io;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

/// The key pair and certificate of this machine. Its `Debug` never shows either.
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// Makes a fresh identity (ECDSA P-256, which every rustls provider verifies).
    pub fn generate() -> io::Result<Self> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(io::Error::other)?;
        let params =
            rcgen::CertificateParams::new(vec!["navette".to_owned()]).map_err(io::Error::other)?;
        let cert = params.self_signed(&key).map_err(io::Error::other)?;
        Ok(Self::from_der(cert.der().to_vec(), key.serialize_der()))
    }

    /// Rebuilds an identity from the DER bytes written by [`Identity::to_der`].
    pub fn from_der(cert: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            cert: CertificateDer::from(cert),
            key: PrivatePkcs8KeyDer::from(key),
        }
    }

    /// The certificate and the PKCS#8 key, as written to disk.
    pub fn to_der(&self) -> (&[u8], &[u8]) {
        (self.cert.as_ref(), self.key.secret_pkcs8_der())
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.cert)
    }

    pub fn certificate(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Identity(..)")
    }
}

/// The SHA-256 of a certificate's DER: what a machine trusts of its peer after pairing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(cert: &[u8]) -> Self {
        Self(Sha256::digest(cert).into())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Fingerprint(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_identities_have_different_fingerprints() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn a_fingerprint_survives_its_hex_form() {
        let fingerprint = Identity::generate().unwrap().fingerprint();
        let hex = fingerprint.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(Fingerprint::from_hex(&hex), Some(fingerprint));
        assert_eq!(Fingerprint::from_hex("not hex"), None);
        assert_eq!(Fingerprint::from_hex(&hex[..62]), None);
    }

    #[test]
    fn a_fingerprint_in_hex_has_hex_digits_only() {
        let hex = Identity::generate().unwrap().fingerprint().to_hex();
        for sign in ['+', '-', ' '] {
            let signed = format!("{sign}{}", &hex[1..]);
            assert_eq!(signed.len(), 64);
            assert_eq!(Fingerprint::from_hex(&signed), None, "{sign:?}");
        }
    }

    #[test]
    fn no_key_or_certificate_is_printed_by_debug() {
        let identity = Identity::generate().unwrap();
        let printed = format!("{identity:?} {:?}", identity.fingerprint());
        let (cert, key) = identity.to_der();
        assert!(!printed.contains(&identity.fingerprint().to_hex()));
        for bytes in [cert, key] {
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            assert!(!printed.contains(&hex[..16]));
        }
        assert_eq!(printed, "Identity(..) Fingerprint(..)");
    }
}
