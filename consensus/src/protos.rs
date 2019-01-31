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

use failure::Error;
use stegos_serialization::traits::*;

use crate::blockchain::{BlockProof, MonetaryBlockProof};
use crate::message::*;

use stegos_blockchain::*;
use stegos_crypto::hash::Hash;
use stegos_crypto::pbc::secure::PublicKey as SecurePublicKey;
use stegos_crypto::pbc::secure::Signature as SecureSignature;

// link protobuf dependencies
use stegos_blockchain::protos::*;
use stegos_crypto::protos::*;
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));

impl ProtoConvert for ConsensusMessageBody<Block, BlockProof> {
    type Proto = consensus::ConsensusMessageBody;
    fn into_proto(&self) -> Self::Proto {
        let mut proto = consensus::ConsensusMessageBody::new();
        match self {
            ConsensusMessageBody::Proposal { request, proof } => match (request, proof) {
                (Block::KeyBlock(block), BlockProof::KeyBlockProof) => {
                    let mut proposal = consensus::KeyBlockProposal::new();
                    proposal.set_block(block.into_proto());
                    proto.set_key_block_proposal(proposal);
                }
                (Block::MonetaryBlock(block), BlockProof::MonetaryBlockProof(proof)) => {
                    let mut proposal = consensus::MonetaryBlockProposal::new();
                    proposal.set_block(block.into_proto());
                    for tx_hash in &proof.tx_hashes {
                        proposal.tx_hashes.push(tx_hash.into_proto());
                    }
                    if let Some(ref fee_output) = proof.fee_output {
                        proposal.set_fee_output(fee_output.into_proto());
                    }
                    proto.set_monetary_block_proposal(proposal);
                }
                _ => unreachable!(),
            },
            ConsensusMessageBody::Prevote {} => {
                proto.set_prevote(consensus::Prevote::new());
            }
            ConsensusMessageBody::Precommit { request_hash_sig } => {
                let mut msg = consensus::Precommit::new();
                msg.set_request_hash_sig(request_hash_sig.into_proto());
                proto.set_precommit(msg);
            }
        }
        proto
    }

    fn from_proto(proto: &Self::Proto) -> Result<Self, Error> {
        let msg = match proto.body {
            Some(consensus::ConsensusMessageBody_oneof_body::monetary_block_proposal(ref msg)) => {
                let request = Block::MonetaryBlock(MonetaryBlock::from_proto(msg.get_block())?);
                let fee_output = if msg.has_fee_output() {
                    Some(Output::from_proto(msg.get_fee_output())?)
                } else {
                    None
                };
                let mut tx_hashes = Vec::with_capacity(msg.tx_hashes.len());
                for tx_hash in msg.tx_hashes.iter() {
                    tx_hashes.push(Hash::from_proto(tx_hash)?);
                }
                let proof = MonetaryBlockProof {
                    fee_output,
                    tx_hashes,
                };
                let proof = BlockProof::MonetaryBlockProof(proof);
                ConsensusMessageBody::Proposal { request, proof }
            }
            Some(consensus::ConsensusMessageBody_oneof_body::key_block_proposal(ref msg)) => {
                let request = Block::KeyBlock(KeyBlock::from_proto(msg.get_block())?);
                let proof = BlockProof::KeyBlockProof;
                ConsensusMessageBody::Proposal { request, proof }
            }
            Some(consensus::ConsensusMessageBody_oneof_body::prevote(ref _msg)) => {
                ConsensusMessageBody::Prevote {}
            }
            Some(consensus::ConsensusMessageBody_oneof_body::precommit(ref msg)) => {
                let request_hash_sig = SecureSignature::from_proto(msg.get_request_hash_sig())?;
                ConsensusMessageBody::Precommit { request_hash_sig }
            }
            None => {
                return Err(ProtoError::MissingField("body".to_string(), "body".to_string()).into());
            }
        };
        Ok(msg)
    }
}

impl ProtoConvert for ConsensusMessage<Block, BlockProof> {
    type Proto = consensus::ConsensusMessage;
    fn into_proto(&self) -> Self::Proto {
        let mut proto = consensus::ConsensusMessage::new();
        proto.set_height(self.height);
        proto.set_epoch(self.epoch);
        proto.set_request_hash(self.request_hash.into_proto());
        proto.set_body(self.body.into_proto());
        proto.set_sig(self.sig.into_proto());
        proto.set_pkey(self.pkey.into_proto());
        proto
    }
    fn from_proto(proto: &Self::Proto) -> Result<Self, Error> {
        let height = proto.get_height();
        let epoch = proto.get_epoch();
        let request_hash = Hash::from_proto(proto.get_request_hash())?;
        let body = ConsensusMessageBody::from_proto(proto.get_body())?;
        let sig = SecureSignature::from_proto(proto.get_sig())?;
        let pkey = SecurePublicKey::from_proto(proto.get_pkey())?;
        Ok(ConsensusMessage {
            height,
            epoch,
            request_hash,
            body,
            sig,
            pkey,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::BTreeSet;
    use stegos_crypto::curve1174::cpt::make_random_keys;
    use stegos_crypto::hash::Hashable;
    use stegos_crypto::pbc::secure::make_random_keys as make_secure_random_keys;

    fn roundtrip<T>(x: &T) -> T
    where
        T: ProtoConvert + Hashable + std::fmt::Debug,
    {
        let r = T::from_proto(&x.clone().into_proto()).unwrap();
        assert_eq!(Hash::digest(x), Hash::digest(&r));
        r
    }

    #[test]
    fn consensus() {
        let (cosi_skey, cosi_pkey, cosi_sig) = make_secure_random_keys();

        let body = ConsensusMessageBody::Prevote {};
        let msg = ConsensusMessage::new(1, 1, Hash::digest(&1u64), &cosi_skey, &cosi_pkey, body);
        roundtrip(&msg);

        let body = ConsensusMessageBody::Precommit {
            request_hash_sig: cosi_sig,
        };
        let msg = ConsensusMessage::new(1, 1, Hash::digest(&1u64), &cosi_skey, &cosi_pkey, body);
        roundtrip(&msg);
    }

    #[test]
    fn key_blocks() {
        let (_skey0, pkey0, _sig0) = make_secure_random_keys();

        let version: u64 = 1;
        let epoch: u64 = 1;
        let timestamp = Utc::now().timestamp() as u64;
        let previous = Hash::digest(&"test".to_string());

        let base = BaseBlockHeader::new(version, previous, epoch, timestamp);
        let witnesses: BTreeSet<SecurePublicKey> = [pkey0].iter().cloned().collect();
        let leader = pkey0.clone();
        let facilitator = pkey0.clone();

        let block = KeyBlock::new(base, leader, facilitator, witnesses);

        let block = Block::KeyBlock(block);

        //
        // KeyBlockProposal
        //
        let proof = BlockProof::KeyBlockProof;
        let proposal = ConsensusMessageBody::Proposal {
            request: block.clone(),
            proof,
        };
        roundtrip(&proposal);
    }

    #[test]
    fn monetary_blocks() {
        let (skey0, _pkey0, _sig0) = make_random_keys();
        let (skey1, pkey1, _sig1) = make_random_keys();
        let (_skey2, pkey2, _sig2) = make_random_keys();

        let version: u64 = 1;
        let epoch: u64 = 1;
        let timestamp = Utc::now().timestamp() as u64;
        let amount: i64 = 1_000_000;
        let previous = Hash::digest(&"test".to_string());

        // "genesis" output by 0
        let (output0, gamma0) = Output::new_payment(timestamp, &skey0, &pkey1, amount).unwrap();

        // Transaction from 1 to 2
        let inputs1 = [Hash::digest(&output0)];
        let (output1, gamma1) = Output::new_payment(timestamp, &skey1, &pkey2, amount).unwrap();
        let outputs1 = [output1];
        let gamma = gamma0 - gamma1;

        let base = BaseBlockHeader::new(version, previous, epoch, timestamp);

        let block = MonetaryBlock::new(base, gamma.clone(), 0, &inputs1, &outputs1);

        let block = Block::MonetaryBlock(block);

        //
        // Monetary block proposal
        //

        let (fee_output, _fee_gamma) =
            Output::new_payment(timestamp, &skey1, &pkey1, 100).expect("keys are valid");
        let mut tx_hashes = Vec::new();
        tx_hashes.push(Hash::digest(&1u64));
        let proof = MonetaryBlockProof {
            fee_output: Some(fee_output),
            tx_hashes,
        };
        let proof = BlockProof::MonetaryBlockProof(proof);

        let proposal = ConsensusMessageBody::Proposal {
            request: block.clone(),
            proof,
        };

        roundtrip(&proposal);
        let proof = MonetaryBlockProof {
            fee_output: None,
            tx_hashes: Vec::new(),
        };
        let proof = BlockProof::MonetaryBlockProof(proof);
        let proposal = ConsensusMessageBody::Proposal {
            request: block.clone(),
            proof,
        };
        roundtrip(&proposal);
    }

}