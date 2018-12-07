//! Transaction output.

//
// Copyright (c) 2018 Stegos
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use failure::{Error, Fail};
use std::fmt;
use std::mem::size_of;
use std::mem::transmute;
use stegos_crypto::bulletproofs::{make_range_proof, BulletProof};
use stegos_crypto::curve1174::cpt::{
    aes_decrypt, aes_encrypt, EncryptedPayload, Pt, PublicKey, SecretKey,
};
use stegos_crypto::curve1174::ecpt::ECp;
use stegos_crypto::curve1174::fields::Fr;
use stegos_crypto::curve1174::G;
use stegos_crypto::hash::{Hash, Hashable, Hasher};
use stegos_crypto::CryptoError;

/// A magic value used to encode/decode payload.
const PAYLOAD_MAGIC: [u8; 4] = [112, 97, 121, 109]; // "paym"

/// Payload size.
const PAYLOAD_LEN: usize = 76;

/// Errors.
#[derive(Debug, Fail)]
pub enum OutputError {
    #[fail(display = "Failed to decrypt payload")]
    PayloadDecryptionError,
}

/// Transaction output.
/// (ID, P_{M, δ}, Bp, E_M(x, γ, δ))
#[derive(Debug, Clone)]
pub struct Output {
    /// Clocked public key of recipient.
    /// P_M + δG
    pub recipient: PublicKey,

    /// Bulletproof on range on amount x.
    /// Contains Pedersen commitment.
    /// Size is approx. 3-5 KB (very structured data type).
    pub proof: BulletProof,

    /// Encrypted payload.
    ///
    /// E_M(x, γ, δ)
    /// Represents an encrypted packet contain the information about x, γ, δ
    /// that only receiver can red
    /// Size is approx 137 Bytes =
    ///     (R-val 65B, crypto-text 72B = (amount 8B, gamma 32B, delta 32B))
    pub payload: EncryptedPayload,
}

impl Output {
    /// Constructor for Output.
    pub fn new(
        timestamp: u64,
        sender_skey: &SecretKey,
        recipient_pkey: &PublicKey,
        amount: i64,
    ) -> Result<(Self, Fr), Error> {
        // Clock recipient public key
        let (cloaked_pkey, delta) = Self::cloak_key(sender_skey, recipient_pkey, timestamp)?;

        let (proof, gamma) = make_range_proof(amount);

        // NOTE: real public key should be used to encrypt payload.
        let payload = Self::encrypt_payload(delta, gamma, amount, recipient_pkey)?;

        let output = Output {
            recipient: cloaked_pkey,
            proof,
            payload,
        };

        Ok((output, gamma))
    }

    /// Cloak recipient's public key.
    fn cloak_key(
        sender_skey: &SecretKey,
        recipient_pkey: &PublicKey,
        timestamp: u64,
    ) -> Result<(PublicKey, Fr), CryptoError> {
        // h is the digest of the recipients actual public key mixed with a timestamp.
        let mut hasher = Hasher::new();
        recipient_pkey.hash(&mut hasher);
        timestamp.hash(&mut hasher);
        let h = hasher.result();

        // Use deterministic randomness here too, to protect against PRNG attacks.
        let delta: Fr = Fr::synthetic_random(&"PKey", sender_skey, &h);

        // Resulting publickey will be a random-like value in a safe range of the field,
        // not too small, and not too large. This helps avoid brute force attacks, looking
        // for the discrete log corresponding to delta.

        let pt = Pt::from(*recipient_pkey);
        let pt = ECp::decompress(pt)?;
        let cloaked_pkey = PublicKey::from(pt + delta * (*G));
        Ok((cloaked_pkey, delta))
    }

    /// Create a new monetary transaction.
    fn encrypt_payload(
        delta: Fr,
        gamma: Fr,
        amount: i64,
        pkey: &PublicKey,
    ) -> Result<EncryptedPayload, CryptoError> {
        // Convert amount to BE vector.

        let amount_bytes: [u8; 8] = unsafe { transmute(amount.to_be()) };

        let gamma_bytes: [u8; 32] = gamma.to_lev_u8();
        let delta_bytes: [u8; 32] = delta.to_lev_u8();

        let payload: Vec<u8> = [
            &PAYLOAD_MAGIC[..],
            &amount_bytes[..],
            &delta_bytes[..],
            &gamma_bytes[..],
        ]
            .concat();

        // Ensure that the total length of package is 76 bytes.
        assert_eq!(payload.len(), PAYLOAD_LEN);

        // String together a gamma, delta, and Amount (i64) all in one long vector and encrypt it.
        aes_encrypt(&payload, &pkey)
    }

    /// Decrypt monetary transaction.
    pub fn decrypt_payload(&self, skey: &SecretKey) -> Result<(Fr, Fr, i64), Error> {
        let payload: Vec<u8> = aes_decrypt(&self.payload, &skey)?;

        if payload.len() != PAYLOAD_LEN {
            // Invalid payload or invalid secret key supplied.
            return Err(OutputError::PayloadDecryptionError.into());
        }

        let mut magic: [u8; 4] = [0u8; 4];
        let mut amount_bytes: [u8; 8] = [0u8; 8];
        let mut delta_bytes: [u8; 32] = [0u8; 32];
        let mut gamma_bytes: [u8; 32] = [0u8; 32];
        magic.copy_from_slice(&payload[0..4]);
        amount_bytes.copy_from_slice(&payload[4..12]);
        delta_bytes.copy_from_slice(&payload[12..44]);
        gamma_bytes.copy_from_slice(&payload[44..76]);

        if magic != PAYLOAD_MAGIC {
            // Invalid payload or invalid secret key supplied.
            return Err(OutputError::PayloadDecryptionError.into());
        }

        let amount: i64 = i64::from_be(unsafe { transmute(amount_bytes) });
        let gamma: Fr = Fr::from_lev_u8(gamma_bytes);
        let delta: Fr = Fr::from_lev_u8(delta_bytes);

        Ok((delta, gamma, amount))
    }

    /// Returns approximate the size of a UTXO in bytes.
    pub fn size_of(&self) -> usize {
        size_of::<Output>() + self.payload.ctxt.len() * size_of::<u8>()
    }
}

impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Output({})", Hash::digest(self))
    }
}

impl Hashable for Output {
    /// Unique identifier of the output.
    /// Formed by hashing all fields of this structure.
    /// H_r(P_{M, δ},B_p, E_M(x, γ, δ)).
    fn hash(&self, state: &mut Hasher) {
        self.recipient.hash(state);
        self.proof.hash(state);
        self.payload.hash(state);
    }
}

impl Hashable for Box<Output> {
    fn hash(&self, state: &mut Hasher) {
        let output = self.as_ref();
        output.hash(state)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use chrono::Utc;
    use stegos_crypto::curve1174::cpt::make_random_keys;

    #[test]
    pub fn encrypt_decrypt() {
        let (skey1, _pkey1, _sig1) = make_random_keys();
        let (skey2, pkey2, _sig2) = make_random_keys();

        let timestamp = Utc::now().timestamp() as u64;
        let amount: i64 = 100500;

        let (output, gamma) =
            Output::new(timestamp, &skey1, &pkey2, amount).expect("encryption successful");
        let (_delta2, gamma2, amount2) = output
            .decrypt_payload(&skey2)
            .expect("decryption successful");
        assert!(output.size_of() > 1200 || output.size_of() < 1400);

        assert_eq!(amount, amount2);
        assert_eq!(gamma, gamma2);

        // Error handling
        if let Err(e) = output.decrypt_payload(&skey1) {
            match e.downcast::<OutputError>() {
                Ok(OutputError::PayloadDecryptionError) => (),
                _ => assert!(false),
            };
        } else {
            assert!(false);
        }
    }
}
