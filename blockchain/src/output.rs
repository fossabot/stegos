//! Transaction output.

//
// Copyright (c) 2018 Stegos AG
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

use crate::timestamp::Timestamp;
use crate::BlockchainError;
use failure::{Error, Fail};
use rand::random;
use serde_derive::{Deserialize, Serialize};
use std::mem::transmute;
use stegos_crypto::bulletproofs::{fee_a, make_range_proof, validate_range_proof, BulletProof};
use stegos_crypto::hash::{Hash, Hashable, Hasher, HASH_SIZE};
use stegos_crypto::pbc;
use stegos_crypto::scc::{
    aes_decrypt, aes_decrypt_with_rvalue, aes_encrypt, sign_hash, validate_sig, EncryptedPayload,
    Fr, Pt, PublicKey, SchnorrSig, SecretKey,
};
use stegos_crypto::CryptoError;

/// A magic value used to encode/decode payload.
const PAYMENT_PAYLOAD_MAGIC: [u8; 4] = [112, 97, 121, 109]; // "paym"

/// Exact size of encrypted payload of PaymentOutput.
pub const PAYMENT_PAYLOAD_LEN: usize = 1024;

/// Maximum length of data field of encrypted payload of PaymentOutput.
/// Equals to PAYMENT_PAYLOAD_LEN - magic - delta - gamma - amount - spenderSignature.
pub const PAYMENT_DATA_LEN: usize = PAYMENT_PAYLOAD_LEN - 4 - 32 - 32 - 8 - 64; // size of SchnorrSig

/// UTXO errors.
#[derive(Debug, Fail)]
pub enum OutputError {
    #[fail(display = "Invalid stake: utxo={}", _0)]
    InvalidStake(Hash),
    #[fail(display = "Invalid bulletproof: utxo={}", _0)]
    InvalidBulletProof(Hash),
    #[fail(
        display = "Invalid payload length: utxo={}, expected={}, got={}",
        _0, _1, _2
    )]
    InvalidPayloadLength(Hash, usize, usize),
    #[fail(display = "Failed to decrypt payload: utxo={}", _0)]
    PayloadDecryptionError(Hash),
    #[fail(display = "Data is too long: max={}, got={}", _0, _1)]
    DataIsTooLong(usize, usize),
    #[fail(display = "Unsupported data type: utxo={}, typecode={}", _0, _1)]
    UnsupportedDataType(Hash, u8),
    #[fail(display = "Trailing garbage in payload: utxo={}", _0)]
    TrailingGarbage(Hash),
    #[fail(display = "Negative amount: utxo={}, amount={}", _0, _1)]
    NegativeAmount(Hash, i64),
    #[fail(display = "Invalid signature on validator pkey: utxo={}", _0)]
    InvalidStakeSignature(Hash),
    #[fail(
        display = "Input is locked: hash={}, tx_time={}, last_macro_block_time={}",
        _0, _1, _2
    )]
    UtxoLocked(Hash, Timestamp, Timestamp),
    #[fail(display = "Crypto error ={}", _0)]
    CryptoError(CryptoError),
    #[fail(display = "Error in decoding utf string ={}", _0)]
    UtfError(std::str::Utf8Error),
    #[fail(display = "Invalid payment certificate")]
    InvalidCertificate,
}

impl From<CryptoError> for OutputError {
    fn from(error: CryptoError) -> Self {
        OutputError::CryptoError(error)
    }
}
impl From<std::str::Utf8Error> for OutputError {
    fn from(error: std::str::Utf8Error) -> Self {
        OutputError::UtfError(error)
    }
}

/// Payment UTXO.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaymentOutput {
    /// Cloaked public key of recipient.
    pub recipient: PublicKey,

    /// Cloaking hint for recipient, to speed up UTXO search.
    pub cloaking_hint: Pt,

    /// Bulletproof on range on amount x.
    /// Contains Pedersen commitment.
    /// Size is approx. 1 KB (very structured data type).
    pub proof: BulletProof,

    /// Encrypted payload.
    ///
    /// E_M(x, γ, δ)
    /// Represents an encrypted packet contain the information about x, γ, δ
    /// that only receiver can red
    /// Size is approx 137 Bytes =
    ///     (R-val 65B, crypto-text 72B = (amount 8B, gamma 32B, delta 32B))
    pub payload: EncryptedPayload,

    /// Timelock for output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_timestamp: Option<Timestamp>,
}

/// PublicPayment UTXO.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicPaymentOutput {
    /// Uncloaked public key of recipient.
    pub recipient: PublicKey,

    /// Randomize for hash collision avoidance
    pub serno: i64,

    /// Uncloaked amount
    pub amount: i64,

    /// Timelock for output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_timestamp: Option<Timestamp>,
}

/// Stake UTXO.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct StakeOutput {
    /// Uncloaked account key of validator.
    pub recipient: PublicKey,

    /// Uncloaked network key of validator.
    pub validator: pbc::PublicKey,

    /// Amount to stake.
    pub amount: i64,

    // some randomization to prevent hash collisions
    pub serno: i64,

    /// BLS signature of recipient, validator and payload.
    pub signature: pbc::Signature,
}

/// Blockchain UTXO.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Output {
    PaymentOutput(PaymentOutput),
    PublicPaymentOutput(PublicPaymentOutput),
    StakeOutput(StakeOutput),
}

/// Cloak recipient's public key.
fn cloak_key(recipient_pkey: &PublicKey, gamma: &Fr) -> Result<(PublicKey, Fr), CryptoError> {
    // h is the digest of the recipients actual public key mixed with a timestamp.
    let mut hasher = Hasher::new();
    recipient_pkey.hash(&mut hasher);
    Fr::random().hash(&mut hasher);
    let h = hasher.result();

    // Use deterministic randomness here too, to protect against PRNG attacks.
    let delta: Fr = Fr::synthetic_random(&"PKey", gamma, &h);

    // Resulting publickey will be a random-like value in a safe range of the field,
    // not too small, and not too large. This helps avoid brute force attacks, looking
    // for the discrete log corresponding to delta.

    let pt = Pt::from(*recipient_pkey);
    let cloaked_pt = {
        if (*gamma) == Fr::zero() {
            pt + delta * Pt::one()
        } else {
            pt + *gamma * delta * Pt::one()
        }
    };
    let cloaked_pkey = PublicKey::from(cloaked_pt);
    Ok((cloaked_pkey, delta))
}

/// Unpacked data field of PaymentPayload.
#[derive(Serialize, Deserialize, Debug, Eq, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub enum PaymentPayloadData {
    /// A string up to PAYLOAD_DATA_LEN - 2 bytes inclusive.
    Comment(String),
    /// A hash of secret content.
    ContentHash(Hash),
}

impl Hashable for PaymentPayloadData {
    fn hash(&self, hasher: &mut Hasher) {
        self.discriminant().hash(hasher);
        match self {
            PaymentPayloadData::Comment(s) => s.hash(hasher),
            PaymentPayloadData::ContentHash(h) => h.hash(hasher),
        }
    }
}

impl PaymentPayloadData {
    fn discriminant(&self) -> u8 {
        match self {
            PaymentPayloadData::Comment(_) => 0,
            PaymentPayloadData::ContentHash(_) => 1,
        }
    }

    pub fn validate(&self) -> Result<(), Error> {
        match &self {
            PaymentPayloadData::Comment(comment) => {
                let data_bytes = comment.as_bytes();
                if data_bytes.len() > PAYMENT_DATA_LEN - 2 {
                    return Err(
                        OutputError::DataIsTooLong(PAYMENT_DATA_LEN - 2, data_bytes.len()).into(),
                    );
                }
            }
            PaymentPayloadData::ContentHash(_hash) => {}
        }
        Ok(())
    }
}

/// Unpacked encrypted payload of PaymentOutput.
#[derive(Debug, Eq, PartialEq)]
pub struct PaymentPayload {
    pub delta: Fr,
    pub gamma: Fr,
    pub amount: i64,
    pub data: PaymentPayloadData,
    /// Signature for rest of data, produced by sender.
    /// This signature used on validating Payment certificate.
    pub signature: SchnorrSig,
}

impl PaymentPayload {
    pub fn new(delta: Fr, gamma: Fr, amount: i64, data: PaymentPayloadData) -> PaymentPayload {
        let signature = SchnorrSig::new();
        PaymentPayload {
            delta,
            gamma,
            amount,
            data,
            signature,
        }
    }

    pub fn new_with_signature(
        sender_key: &SecretKey,
        delta: Fr,
        gamma: Fr,
        amount: i64,
        data: PaymentPayloadData,
    ) -> PaymentPayload {
        let mut payment_payload = PaymentPayload::new(delta, gamma, amount, data);
        let hash = Hash::digest(&payment_payload);
        let signature = sign_hash(&hash, &sender_key);
        payment_payload.signature = signature;
        payment_payload
    }

    /// Serialize and encrypt payload.
    fn encrypt(&self, pkey: &PublicKey) -> Result<(EncryptedPayload, Fr), BlockchainError> {
        let mut payload: [u8; PAYMENT_PAYLOAD_LEN] = [0u8; PAYMENT_PAYLOAD_LEN];
        let mut pos: usize = 0;

        // Magic.
        payload[pos..pos + 4].copy_from_slice(&PAYMENT_PAYLOAD_MAGIC);
        pos += PAYMENT_PAYLOAD_MAGIC.len();

        // Gamma.
        let gamma_bytes: [u8; 32] = self.gamma.to_bytes();
        payload[pos..pos + gamma_bytes.len()].copy_from_slice(&gamma_bytes);
        pos += gamma_bytes.len();

        // Delta.
        let delta_bytes: [u8; 32] = self.delta.to_bytes();
        payload[pos..pos + delta_bytes.len()].copy_from_slice(&delta_bytes);
        pos += delta_bytes.len();

        // Amount.
        let amount_bytes: [u8; 8] = unsafe { transmute(self.amount.to_le()) };
        payload[pos..pos + amount_bytes.len()].copy_from_slice(&amount_bytes);
        pos += amount_bytes.len();

        // Sender signature
        let signature_u = self.signature.u.to_bytes();
        payload[pos..pos + signature_u.len()].copy_from_slice(&signature_u);
        pos += signature_u.len();

        let signature_k = self.signature.K.to_bytes();
        payload[pos..pos + signature_k.len()].copy_from_slice(&signature_k);
        pos += signature_k.len();

        // Data.
        payload[pos] = self.data.discriminant();
        pos += 1;
        self.data.validate().expect("is valid");
        match &self.data {
            PaymentPayloadData::Comment(comment) => {
                let data_bytes = comment.as_bytes();
                assert!(data_bytes.len() <= PAYMENT_DATA_LEN - 2);
                payload[pos..pos + data_bytes.len()].copy_from_slice(data_bytes);
                pos += data_bytes.len();
            }
            PaymentPayloadData::ContentHash(hash) => {
                let data_bytes = &hash.to_bytes();
                payload[pos..pos + data_bytes.len()].copy_from_slice(data_bytes);
                pos += data_bytes.len();
            }
        }
        // The rest is zeros.
        assert!(pos <= PAYMENT_PAYLOAD_LEN);

        // Encrypt payload.
        let (payload, rvalue) = aes_encrypt(&payload, &pkey)?;
        Ok((payload, rvalue))
    }

    /// Decrypt and deserialize payload.
    fn decrypt_with_rvalue(
        output_hash: Hash,
        payload: &EncryptedPayload,
        rvalue: &Fr,
        recipient_pkey: &PublicKey, // uncloaked recipient pkey
    ) -> Result<Self, Error> {
        // this version is used to verify payment, knowing the
        // secret r-value of the encrypted payload keying
        if payload.ctxt.len() != PAYMENT_PAYLOAD_LEN {
            return Err(OutputError::InvalidPayloadLength(
                output_hash,
                PAYMENT_PAYLOAD_LEN,
                payload.ctxt.len(),
            )
            .into());
        }
        let mut payload: Vec<u8> = aes_decrypt_with_rvalue(payload, rvalue, recipient_pkey)?;
        Ok(Self::extract_decrypted_info(output_hash, &mut payload)?)
    }

    fn decrypt(
        output_hash: Hash,
        payload: &EncryptedPayload,
        skey: &SecretKey,
    ) -> Result<Self, BlockchainError> {
        // This version is used by recipients, who know the secret key
        // needed to unlock the encrypted payload
        if payload.ctxt.len() != PAYMENT_PAYLOAD_LEN {
            return Err(OutputError::InvalidPayloadLength(
                output_hash,
                PAYMENT_PAYLOAD_LEN,
                payload.ctxt.len(),
            )
            .into());
        }
        let mut payload: Vec<u8> = aes_decrypt(payload, skey)?;
        Ok(Self::extract_decrypted_info(output_hash, &mut payload)?)
    }

    fn extract_decrypted_info(
        output_hash: Hash,
        payload: &mut Vec<u8>,
    ) -> Result<Self, OutputError> {
        assert_eq!(payload.len(), PAYMENT_PAYLOAD_LEN);
        let mut pos: usize = 0;

        // Magic.
        let mut magic: [u8; 4] = [0u8; 4];
        magic.copy_from_slice(&payload[pos..pos + 4]);
        pos += 4;
        if magic != PAYMENT_PAYLOAD_MAGIC {
            // Invalid payload or invalid secret key supplied.
            return Err(OutputError::PayloadDecryptionError(output_hash).into());
        }

        // Gamma.
        let mut gamma_bytes: [u8; 32] = [0u8; 32];
        gamma_bytes.copy_from_slice(&payload[pos..pos + 32]);
        pos += gamma_bytes.len();
        let gamma: Fr = Fr::try_from_bytes(&gamma_bytes[..]).expect("ok");

        // Delta.
        let mut delta_bytes: [u8; 32] = [0u8; 32];
        delta_bytes.copy_from_slice(&payload[pos..pos + 32]);
        pos += delta_bytes.len();
        let delta: Fr = Fr::try_from_bytes(&delta_bytes[..]).expect("ok");

        // Amount.
        let mut amount_bytes: [u8; 8] = [0u8; 8];
        amount_bytes.copy_from_slice(&payload[pos..pos + 8]);
        pos += amount_bytes.len();
        let amount: i64 = i64::from_le(unsafe { transmute(amount_bytes) });
        if amount < 0 {
            return Err(OutputError::NegativeAmount(output_hash, amount).into());
        }

        // Sender signature
        let mut signature_u_bytes: [u8; 32] = [0u8; 32];
        signature_u_bytes.copy_from_slice(&payload[pos..pos + 32]);
        pos += signature_u_bytes.len();
        let signature_u: Fr = Fr::try_from_bytes(&signature_u_bytes).expect("ok");

        let mut signature_k_bytes: [u8; 32] = [0u8; 32];
        signature_k_bytes.copy_from_slice(&payload[pos..pos + 32]);
        pos += signature_k_bytes.len();
        let signature_k: Pt = Pt::try_from_bytes(&signature_k_bytes)?;

        let signature = SchnorrSig {
            u: signature_u,
            K: signature_k,
        };

        // Data.
        let code: u8 = payload[pos];
        pos += 1;
        let data = match code {
            0 => {
                //TODO: Utf8 can contain \0 inside
                let mut end: usize = payload.len();
                while end > pos && payload[end - 1] == 0 {
                    end -= 1;
                }
                let s = std::str::from_utf8(&payload[pos..end])?;
                pos = payload.len();
                PaymentPayloadData::Comment(s.to_string())
            }
            1 => {
                let hash = Hash::try_from_bytes(&payload[pos..pos + HASH_SIZE])?;
                pos += HASH_SIZE;
                PaymentPayloadData::ContentHash(hash)
            }
            code @ _ => return Err(OutputError::UnsupportedDataType(output_hash, code).into()),
        };

        // Check for trailing garbage.
        for byte in &payload[pos..] {
            if *byte != 0 {
                return Err(OutputError::TrailingGarbage(output_hash).into());
            }
        }

        let payload = PaymentPayload {
            delta,
            gamma,
            amount,
            signature,
            data,
        };
        Ok(payload)
    }
}

impl Hashable for PaymentPayload {
    fn hash(&self, hasher: &mut Hasher) {
        self.delta.hash(hasher);
        self.gamma.hash(hasher);
        self.amount.hash(hasher);
        self.data.hash(hasher);
    }
}

impl PaymentOutput {
    /// Create a new PaymentOutput with generic payload.
    pub fn with_payload(
        sender_key: Option<&SecretKey>,
        recipient_pkey: &PublicKey,
        amount: i64,
        data: PaymentPayloadData,
        locked_timestamp: Option<Timestamp>,
    ) -> Result<(Self, Fr, Fr), BlockchainError> {
        // Create range proofs.
        let (proof, gamma) = make_range_proof(amount);

        // Cloak recipient public key
        let (cloaked_pkey, delta) = cloak_key(recipient_pkey, &gamma)?;

        let payload = if let Some(sender_key) = sender_key {
            PaymentPayload::new_with_signature(
                sender_key,
                delta.clone(),
                gamma.clone(),
                amount,
                data,
            )
        } else {
            PaymentPayload::new(delta.clone(), gamma.clone(), amount, data)
        };
        // NOTE: real public key should be used to encrypt payload
        let (payload, rvalue) = payload.encrypt(recipient_pkey)?;

        // Key cloaking hint for recipient = gamma * delta * Pkey
        let hint = Pt::from(*recipient_pkey) * gamma * delta;

        let output = PaymentOutput {
            recipient: cloaked_pkey,
            cloaking_hint: hint,
            proof,
            locked_timestamp,
            payload,
        };

        Ok((output, gamma, rvalue))
    }

    /// Create a new PaymentOutput.
    pub fn new(recipient_pkey: &PublicKey, amount: i64) -> Result<(Self, Fr), BlockchainError> {
        let data = PaymentPayloadData::Comment(String::new());
        let (output, gamma, _) = Self::with_payload(None, recipient_pkey, amount, data, None)?;
        Ok((output, gamma))
    }

    /// Create a new PaymentOutput with lock.
    pub fn new_locked(
        recipient_pkey: &PublicKey,
        amount: i64,
        locked_timestamp: Timestamp,
    ) -> Result<(Self, Fr), BlockchainError> {
        let data = PaymentPayloadData::Comment(String::new());
        let (output, gamma, _) =
            Self::with_payload(None, recipient_pkey, amount, data, locked_timestamp.into())?;
        Ok((output, gamma))
    }

    /// Decrypt payload.
    pub fn decrypt_payload(&self, skey: &SecretKey) -> Result<PaymentPayload, BlockchainError> {
        let output_hash = Hash::digest(&self);
        PaymentPayload::decrypt(output_hash, &self.payload, skey)
    }

    /// Validates UTXO structure and keying.
    pub fn validate(&self) -> Result<(), BlockchainError> {
        // check Bulletproof
        if !validate_range_proof(&self.proof) {
            let h = Hash::digest(self);
            return Err(OutputError::InvalidBulletProof(h).into());
        };

        // Validate payload.
        if self.payload.ctxt.len() != PAYMENT_PAYLOAD_LEN {
            let h = Hash::digest(self);
            return Err(OutputError::InvalidPayloadLength(
                h,
                PAYMENT_PAYLOAD_LEN,
                self.payload.ctxt.len(),
            )
            .into());
        }
        Ok(())
    }

    /// Returns Pedersen commitment.
    pub fn pedersen_commitment(&self) -> Result<Pt, CryptoError> {
        Ok(self.proof.vcmt)
    }

    // Returns the amount from the encrypted payload,
    // if you know the secret rvalue for the payload keying
    pub fn validate_certificate(
        &self,
        spender_pkey: &PublicKey,
        recipient_pkey: &PublicKey,
        rvalue: &Fr,
    ) -> Result<i64, OutputError> {
        let output_hash = Hash::digest(self);
        let payload =
            PaymentPayload::decrypt_with_rvalue(output_hash, &self.payload, rvalue, recipient_pkey)
                .map_err(|_| OutputError::InvalidCertificate)?;

        let hash = Hash::digest(&payload);
        validate_sig(&hash, &payload.signature, spender_pkey)
            .map_err(|_| OutputError::InvalidCertificate)?;

        Ok(payload.amount)
    }
}

impl PublicPaymentOutput {
    pub fn new(recipient_pkey: &PublicKey, amount: i64) -> Self {
        let serno = random::<i64>();
        PublicPaymentOutput {
            recipient: recipient_pkey.clone(),
            serno,
            amount,
            locked_timestamp: None,
        }
    }

    pub fn new_locked(
        recipient_pkey: &PublicKey,
        amount: i64,
        locked_timestamp: Timestamp,
    ) -> Self {
        let serno = random::<i64>();
        PublicPaymentOutput {
            recipient: recipient_pkey.clone(),
            serno,
            amount,
            locked_timestamp: locked_timestamp.into(),
        }
    }

    /// Validates UTXO structure and keying.
    pub fn validate(&self) -> Result<(), BlockchainError> {
        if self.amount <= 0 {
            let h = Hash::digest(self);
            return Err(OutputError::InvalidStake(h).into());
        }
        Ok(())
    }

    /// Returns Pedersen commitment.
    pub fn pedersen_commitment(&self) -> Result<Pt, CryptoError> {
        Ok(fee_a(self.amount))
    }
}

impl StakeOutput {
    /// Create a new StakeOutput.
    pub fn new(
        recipient_pkey: &PublicKey,
        validator_skey: &pbc::SecretKey,
        validator_pkey: &pbc::PublicKey,
        amount: i64,
    ) -> Result<Self, Error> {
        assert!(amount > 0);

        let serno = random::<i64>();

        let mut output = StakeOutput {
            recipient: recipient_pkey.clone(),
            validator: validator_pkey.clone(),
            amount,
            serno,
            signature: pbc::Signature::zero(),
        };

        // Form BLS signature on the Stake UTXO
        let h = Hash::digest(&output);
        output.signature = pbc::sign_hash(&h, validator_skey);

        Ok(output)
    }

    /// Validates UTXO structure and keying.
    pub fn validate(&self) -> Result<(), BlockchainError> {
        let output_hash = Hash::digest(self);
        if self.amount <= 0 {
            return Err(OutputError::InvalidStake(output_hash).into());
        }

        // Validate BLS signature of validator_pkey
        if let Err(_e) = pbc::check_hash(&output_hash, &self.signature, &self.validator) {
            return Err(OutputError::InvalidStakeSignature(output_hash).into());
        }
        Ok(())
    }

    /// Returns Pedersen commitment.
    pub fn pedersen_commitment(&self) -> Result<Pt, CryptoError> {
        Ok(fee_a(self.amount))
    }
}

impl Output {
    /// Create a new payment UTXO.
    pub fn new_payment(recipient_pkey: &PublicKey, amount: i64) -> Result<(Self, Fr), Error> {
        let (output, gamma) = PaymentOutput::new(recipient_pkey, amount)?;
        Ok((Output::PaymentOutput(output), gamma))
    }

    /// Create a new escrow transaction.
    pub fn new_stake(
        recipient_pkey: &PublicKey,
        validator_skey: &pbc::SecretKey,
        validator_pkey: &pbc::PublicKey,
        amount: i64,
    ) -> Result<Self, Error> {
        let output = StakeOutput::new(recipient_pkey, validator_skey, validator_pkey, amount)?;
        Ok(Output::StakeOutput(output))
    }

    /// Validates UTXO structure and keying.
    pub fn validate(&self) -> Result<(), BlockchainError> {
        match self {
            Output::PaymentOutput(o) => o.validate(),
            Output::PublicPaymentOutput(o) => o.validate(),
            Output::StakeOutput(o) => o.validate(),
        }
    }

    /// Returns decompressed public key.
    pub fn recipient_pkey(&self) -> Result<Pt, CryptoError> {
        Ok(Pt::from(match self {
            Output::PaymentOutput(o) => o.recipient,
            Output::PublicPaymentOutput(o) => o.recipient,
            Output::StakeOutput(o) => o.recipient,
        }))
    }

    /// Returns Pedersen commitment.
    pub fn pedersen_commitment(&self) -> Result<Pt, CryptoError> {
        match self {
            Output::PaymentOutput(o) => o.pedersen_commitment(),
            Output::PublicPaymentOutput(o) => o.pedersen_commitment(),
            Output::StakeOutput(o) => o.pedersen_commitment(),
        }
    }

    /// Returns timestamp when tx could be spent.
    pub fn locked_timestamp(&self) -> Option<Timestamp> {
        match self {
            Output::PaymentOutput(o) => o.locked_timestamp.clone(),
            Output::PublicPaymentOutput(o) => o.locked_timestamp.clone(),
            Output::StakeOutput(_) => None,
        }
    }
}

impl From<PaymentOutput> for Output {
    fn from(output: PaymentOutput) -> Output {
        Output::PaymentOutput(output)
    }
}

impl From<StakeOutput> for Output {
    fn from(output: StakeOutput) -> Output {
        Output::StakeOutput(output)
    }
}

impl From<PublicPaymentOutput> for Output {
    fn from(output: PublicPaymentOutput) -> Output {
        Output::PublicPaymentOutput(output)
    }
}

impl Hashable for PaymentOutput {
    fn hash(&self, state: &mut Hasher) {
        "Payment".hash(state);
        self.recipient.hash(state);
        self.cloaking_hint.hash(state);
        self.proof.hash(state);
        self.payload.hash(state);
        self.locked_timestamp.hash(state);
    }
}

impl Hashable for PublicPaymentOutput {
    fn hash(&self, state: &mut Hasher) {
        "PublicPayment".hash(state);
        self.recipient.hash(state);
        self.serno.hash(state);
        self.amount.hash(state);
        self.locked_timestamp.hash(state);
    }
}

impl Hashable for StakeOutput {
    fn hash(&self, state: &mut Hasher) {
        "Stake".hash(state);
        self.recipient.hash(state);
        self.validator.hash(state);
        self.amount.hash(state);
        self.serno.hash(state);
    }
}

impl Hashable for Output {
    fn hash(&self, state: &mut Hasher) {
        match self {
            Output::PaymentOutput(payment) => payment.hash(state),
            Output::PublicPaymentOutput(payment) => payment.hash(state),
            Output::StakeOutput(stake) => stake.hash(state),
        }
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

    use assert_matches::assert_matches;
    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    use stegos_crypto::scc::make_random_keys;

    fn random_string(len: usize) -> String {
        thread_rng().sample_iter(&Alphanumeric).take(len).collect()
    }

    ///
    /// Tests encoding/decoding of PaymentPayload used by PaymentOutput.
    ///
    #[test]
    fn payment_payload() {
        use simple_logger;
        simple_logger::init_with_level(log::Level::Debug).unwrap_or_default();

        let output_hash = Hash::digest("test");

        fn rt(payload: &PaymentPayload, skey: &SecretKey, pkey: &PublicKey) {
            let output_hash = Hash::digest("test");
            let (encrypted, _rvalue) = payload.encrypt(&pkey).expect("keys are valid");
            let payload2 =
                PaymentPayload::decrypt(output_hash, &encrypted, &skey).expect("keys are valid");
            assert_eq!(payload, &payload2);
        }

        let (skey, pkey) = make_random_keys();

        // With empty comment.
        let gamma: Fr = Fr::random();
        let delta: Fr = Fr::random();
        let amount: i64 = 100500;
        let data = PaymentPayloadData::Comment(String::new());
        let payload = PaymentPayload::new(delta, gamma, amount, data);
        rt(&payload, &skey, &pkey);

        // With non-empty comment.
        let gamma: Fr = Fr::random();
        let delta: Fr = Fr::random();
        let amount: i64 = 100500;
        let data = PaymentPayloadData::ContentHash(Hash::digest(&100500u64));
        let payload = PaymentPayload::new(delta, gamma, amount, data);
        rt(&payload, &skey, &pkey);

        // With long comment.
        let gamma: Fr = Fr::random();
        let delta: Fr = Fr::random();
        let amount: i64 = 100500;
        let data = PaymentPayloadData::Comment(random_string(PAYMENT_DATA_LEN - 2));
        let payload = PaymentPayload::new(delta, gamma, amount, data);
        rt(&payload, &skey, &pkey);

        // Overflow.
        let data = PaymentPayloadData::Comment(random_string(PAYMENT_DATA_LEN - 1));
        let e = data.validate().unwrap_err();
        match e.downcast::<OutputError>().unwrap() {
            OutputError::DataIsTooLong(max, got) => {
                assert_eq!(max, PAYMENT_DATA_LEN - 2);
                assert_eq!(got, PAYMENT_DATA_LEN - 1);
            }
            _ => unreachable!(),
        }

        // With content hash.
        let gamma: Fr = Fr::random();
        let delta: Fr = Fr::random();
        let amount: i64 = 100500;
        let data = PaymentPayloadData::ContentHash(Hash::digest(&100500u64));
        let payload = PaymentPayload::new(delta, gamma, amount, data);
        rt(&payload, &skey, &pkey);

        //
        // Corrupted payload.
        //
        let gamma: Fr = Fr::random();
        let delta: Fr = Fr::random();
        let amount: i64 = 100500;
        let data = PaymentPayloadData::ContentHash(Hash::digest(&100500u64));
        let payload = PaymentPayload::new(delta, gamma, amount, data);
        let (encrypted, _rvalue) = payload.encrypt(&pkey).expect("keys are valid");
        let raw = aes_decrypt(&encrypted, &skey).expect("keys are valid");

        // Invalid length.
        let mut invalid = raw.clone();
        invalid.push(0);
        let (invalid, _rvalue) = aes_encrypt(&invalid, &pkey).expect("keys are valid");
        let e = PaymentPayload::decrypt(output_hash, &invalid, &skey).unwrap_err();
        match e {
            BlockchainError::OutputError(OutputError::InvalidPayloadLength(
                _output_hash,
                expected,
                got,
            )) => {
                assert_eq!(expected, PAYMENT_PAYLOAD_LEN);
                assert_eq!(got, PAYMENT_PAYLOAD_LEN + 1);
            }
            _ => unreachable!(),
        }

        // Invalid magic.
        let mut invalid = raw.clone();
        invalid[3] = 5;
        let (invalid, _rvalue) = aes_encrypt(&invalid, &pkey).expect("keys are valid");
        let e = PaymentPayload::decrypt(output_hash, &invalid, &skey).unwrap_err();
        match e {
            BlockchainError::OutputError(OutputError::PayloadDecryptionError(_output_hash)) => {}
            _ => unreachable!(),
        }

        // Negative amount.
        let mut invalid = raw.clone();
        let amount: i64 = -100500;
        let amount_bytes: [u8; 8] = unsafe { transmute(amount.to_le()) };
        invalid[68..68 + amount_bytes.len()].copy_from_slice(&amount_bytes);
        let (invalid, _rvalue) = aes_encrypt(&invalid, &pkey).expect("keys are valid");
        let e = PaymentPayload::decrypt(output_hash, &invalid, &skey).unwrap_err();
        match e {
            BlockchainError::OutputError(OutputError::NegativeAmount(_output_hash, amount2)) => {
                assert_eq!(amount, amount2)
            }
            _ => unreachable!(),
        }

        // Unsupported type code.
        let mut invalid = raw.clone();
        let code: u8 = 10;
        invalid[PAYMENT_PAYLOAD_LEN - PAYMENT_DATA_LEN] = code;
        let (invalid, _rvalue) = aes_encrypt(&invalid, &pkey).expect("keys are valid");
        let e = PaymentPayload::decrypt(output_hash, &invalid, &skey).unwrap_err();
        match e {
            BlockchainError::OutputError(OutputError::UnsupportedDataType(_output_hash, code2)) => {
                assert_eq!(code, code2)
            }
            _ => unreachable!(),
        }

        // Trailing garbage.
        let mut invalid = raw.clone();
        invalid[PAYMENT_PAYLOAD_LEN - PAYMENT_DATA_LEN + HASH_SIZE + 1] = 1;
        let (invalid, _rvalue) = aes_encrypt(&invalid, &pkey).expect("keys are valid");
        let e = PaymentPayload::decrypt(output_hash, &invalid, &skey).unwrap_err();
        match e {
            BlockchainError::OutputError(OutputError::TrailingGarbage(_output_hash)) => {}
            _ => unreachable!(),
        }
    }

    ///
    /// Tests PaymentOutput encryption/decryption.
    ///
    #[test]
    pub fn payment_encrypt_decrypt() {
        let (skey1, _pkey1) = make_random_keys();
        let (skey2, pkey2) = make_random_keys();

        let amount: i64 = 100500;

        let (output, gamma) = PaymentOutput::new(&pkey2, amount).expect("encryption successful");
        let payload = output
            .decrypt_payload(&skey2)
            .expect("decryption successful");

        assert_eq!(amount, payload.amount);
        assert_eq!(gamma, payload.gamma);

        // Error handling
        match output.decrypt_payload(&skey1).unwrap_err() {
            BlockchainError::OutputError(OutputError::PayloadDecryptionError(_output_hash)) => (),
            _ => panic!(),
        };
    }

    ///
    /// Tests validation of payment certificates.
    #[test]
    fn payment_certificate() {
        use simple_logger;
        simple_logger::init_with_level(log::Level::Debug).unwrap_or_default();
        let (spender_skey, spender_pkey) = make_random_keys();
        let (_recipient_skey, recipient_pkey) = make_random_keys();
        let amount = 100500;
        let data = PaymentPayloadData::Comment("Hello".to_string());
        let (output, _gamma, rvalue) =
            PaymentOutput::with_payload(Some(&spender_skey), &recipient_pkey, amount, data, None)
                .expect("encryption successful");

        let amount2 = output
            .validate_certificate(&spender_pkey, &recipient_pkey, &rvalue)
            .unwrap();
        assert_eq!(amount, amount2);
        let e = output
            .validate_certificate(&recipient_pkey, &spender_pkey, &rvalue)
            .unwrap_err();
        assert_matches!(e, OutputError::InvalidCertificate);
        let e = output
            .validate_certificate(&spender_pkey, &recipient_pkey, &Fr::random())
            .unwrap_err();
        assert_matches!(e, OutputError::InvalidCertificate);
    }
}
