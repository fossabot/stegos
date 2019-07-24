//
// Copyright (c) 2019 Stegos AG
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

//!
//! Data objects used to store information about transaction, produced outputs, and consumed inputs.
//!

use crate::api::*;
use byteorder::{BigEndian, ByteOrder};
use failure::{bail, Error};
use log::{debug, trace};
use rocksdb::{Direction, IteratorMode, WriteBatch, DB};
use serde::{Deserializer, Serializer};
use serde_derive::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;
use stegos_blockchain::{
    PaymentOutput, PaymentPayloadData, PaymentTransaction, PublicPaymentOutput, StakeOutput,
    Timestamp,
};
use stegos_crypto::hash::{Hash, Hashable, Hasher};
use stegos_crypto::scc::{make_random_keys, Fr, PublicKey};
use stegos_node::TransactionStatus;
use stegos_serialization::traits::ProtoConvert;
use tempdir::TempDir;

const LEN_INDEX: [u8; 1] = [0; 1];
const TIME_INDEX: [u8; 2] = [0; 2];

#[derive(Debug, Clone)]
pub enum LogEntry {
    Incoming { output: OutputValue },
    Outgoing { tx: PaymentTransactionValue },
}

/// Currently we support only transaction that have 2 outputs,
/// one for recipient, and one for change.
pub struct AccountLog {
    /// Guard object for temporary directory.
    _temp_dir: Option<TempDir>,
    /// RocksDB database object.
    database: DB,
    /// Len of account log.
    len: u64,
    /// last known system time.
    last_time: Timestamp,
    /// Index of all created transactions by this wallet.
    created_txs: HashMap<Hash, Timestamp>,
    /// Index of all transactions that wasn't rejected or committed.
    pending_txs: HashSet<Hash>,
    /// Transactions that was created in current epoch.
    epoch_transactions: HashSet<Hash>,
}

impl AccountLog {
    /// Open database.
    pub fn open(path: &Path) -> AccountLog {
        debug!("Database path = {}", path.to_string_lossy());
        let database = DB::open_default(path).expect("couldn't open database");

        let len = database
            .get(&LEN_INDEX)
            .expect("No error in database reading")
            .and_then(|v| Self::len_from_bytes(&v))
            .unwrap_or(0);

        let _time = database
            .get(&TIME_INDEX)
            .expect("No error in database reading")
            .and_then(|v| Self::timestamp_from_bytes(&v))
            .unwrap_or(Timestamp::UNIX_EPOCH);
        debug!("Loading database with {} entryes", len);

        let mut log = AccountLog {
            _temp_dir: None,
            database,
            len,
            last_time: Timestamp::now(),
            created_txs: HashMap::new(),
            pending_txs: HashSet::new(),
            epoch_transactions: HashSet::new(),
        };
        log.recover_state();
        log
    }

    #[allow(unused)]
    pub fn testing() -> AccountLog {
        let temp_dir = TempDir::new("account").expect("couldn't create temp dir");
        let len = 0;
        let last_time = Timestamp::UNIX_EPOCH;
        let database = DB::open_default(temp_dir.path()).expect("couldn't open database");
        AccountLog {
            _temp_dir: Some(temp_dir),
            database,
            len,
            last_time,
            created_txs: HashMap::new(),
            pending_txs: HashSet::new(),
            epoch_transactions: HashSet::new(),
        }
    }

    pub fn recover_state(&mut self) {
        // TODO: limit time for recover
        // (for example, if some transaction was created weak ago, it's no reason to resend it)
        let starting_time = Timestamp::UNIX_EPOCH;
        for (timestamp, entry) in self.iter_range(starting_time, u64::max_value()) {
            match entry {
                LogEntry::Incoming { .. } => {}
                LogEntry::Outgoing { tx } => {
                    let tx_hash = Hash::digest(&tx.tx);
                    let status = tx.status;
                    trace!("Recovered tx: tx={}, status={:?}", tx_hash, status);
                    assert!(self.created_txs.insert(tx_hash, timestamp).is_none());
                    self.update_pending_tx(tx_hash, status.clone());
                }
            }
        }
    }

    fn update_pending_tx(&mut self, tx_hash: Hash, status: TransactionStatus) {
        // update epoch transactions
        match status {
            TransactionStatus::Prepared { .. } => {
                self.epoch_transactions.insert(tx_hash);
            }
            _ => {
                trace!("Found status that is not equal to Prepare.");
                let _ = self.epoch_transactions.remove(&tx_hash);
            }
        }

        // update pending transactions
        match status {
            TransactionStatus::Created {} | TransactionStatus::Accepted {} => {
                let tx_timestamp = *self
                    .created_txs
                    .get(&tx_hash)
                    .expect("transaction in created list");
                trace!(
                    "Found transaction with status = {:?}, with timestamp = {}, adding to list.",
                    status,
                    tx_timestamp
                );
                self.pending_txs.insert(tx_hash);
            }
            _ => {
                trace!("Found status that is final, didn't add transaction to pending list.");
                let _ = self.pending_txs.remove(&tx_hash);
            }
        }
    }

    /// Return iterator over transactions
    pub fn pending_txs<'a>(
        &'a self,
    ) -> impl Iterator<Item = Result<PaymentTransactionValue, Error>> + 'a {
        self.pending_txs.iter().map(move |tx_hash| {
            let tx_key = self
                .created_txs
                .get(tx_hash)
                .expect("Transaction should exist");
            let key = Self::bytes_from_timestamp(*tx_key);
            let value = self.database.get(&key)?.expect("Log entry not found.");
            let entry = LogEntry::from_buffer(&value)?;
            Ok(match entry {
                LogEntry::Outgoing { tx } => tx,
                _ => panic!("Found link to incomming entry, in transaaction list."),
            })
        })
    }

    /// Returns exact timestamp of created transaction, if tx found.
    pub fn tx_entry(&self, tx_hash: Hash) -> Option<Timestamp> {
        self.created_txs.get(&tx_hash).cloned()
    }

    /// Insert log entry as last entry in log.
    pub fn push_incomming(
        &mut self,
        timestamp: Timestamp,
        incoming: OutputValue,
    ) -> Result<Timestamp, Error> {
        let entry = LogEntry::Incoming { output: incoming };
        self.push_entry(timestamp, entry)
    }

    /// Insert log entry as last entry in log.
    pub fn push_outgoing(
        &mut self,
        timestamp: Timestamp,
        tx: PaymentTransactionValue,
    ) -> Result<Timestamp, Error> {
        let tx_hash = Hash::digest(&tx.tx);
        let status = tx.status.clone();
        let entry = LogEntry::Outgoing { tx };
        let timestamp = self.push_entry(timestamp, entry)?;
        assert!(self.created_txs.insert(tx_hash, timestamp).is_none());
        self.update_pending_tx(tx_hash, status);
        Ok(timestamp)
    }

    fn push_entry(
        &mut self,
        mut timestamp: Timestamp,
        entry: LogEntry,
    ) -> Result<Timestamp, Error> {
        let data = entry.into_buffer().expect("couldn't serialize block.");

        // avoid key collisions by increasing time.
        if timestamp <= self.last_time {
            self.last_time += Duration::from_millis(1);
            timestamp = self.last_time;
        } else {
            self.last_time = timestamp;
        }
        let len = self.len + 1;
        let mut batch = WriteBatch::default();
        // writebatch put fails if size exceeded u32::max, which is not our case.
        batch.put(&Self::bytes_from_timestamp(timestamp), &data)?;
        batch.put(&LEN_INDEX, &Self::bytes_from_len(len))?;
        batch.put(&TIME_INDEX, &Self::bytes_from_timestamp(timestamp))?;
        self.database.write(batch)?;
        self.len = len;
        Ok(timestamp)
    }

    /// Edit log entry as by idx.
    fn update_log_entry<F>(&mut self, timestamp: Timestamp, mut func: F) -> Result<(), Error>
    where
        F: FnMut(LogEntry) -> Result<LogEntry, Error>,
    {
        let key = Self::bytes_from_timestamp(timestamp);
        let value = self.database.get(&key)?.expect("Log entry not found.");
        let entry = LogEntry::from_buffer(&value)?;

        trace!("Entry before = {:?}", entry);
        let entry = func(entry)?;

        trace!("Entry after = {:?}", entry);
        let data = entry.into_buffer().expect("couldn't serialize block.");

        let mut batch = WriteBatch::default();
        // writebatch put fails if size exceeded u32::max, which is not our case.
        batch.put(&key, &data)?;
        self.database.write(batch)?;

        Ok(())
    }

    /// Edit log entry as by idx.
    pub fn update_tx_status(
        &mut self,
        tx_hash: Hash,
        timestamp: Timestamp,
        status: TransactionStatus,
    ) -> Result<(), Error> {
        self.update_log_entry(timestamp, |mut e| {
            match &mut e {
                LogEntry::Outgoing { ref mut tx } => {
                    tx.status = status.clone();
                }
                LogEntry::Incoming { .. } => bail!("Expected outgoing transaction."),
            };
            Ok(e)
        })?;

        self.update_pending_tx(tx_hash, status);
        Ok(())
    }

    /// Finalize Prepare status for transaction, return list of updated transactions
    pub fn finalize_epoch_txs(&mut self) -> HashMap<Hash, TransactionStatus> {
        debug!("Finalize epoch txs");
        let mut result = HashMap::new();
        let txs = std::mem::replace(&mut self.epoch_transactions, Default::default());
        for tx_hash in txs {
            let timestamp = self
                .tx_entry(tx_hash)
                .expect("Transaction should be found in tx list");

            let mut changed_to_status = None;
            self.update_log_entry(timestamp, |mut e| {
                match &mut e {
                    LogEntry::Outgoing { ref mut tx } => match tx.status {
                        TransactionStatus::Prepared { epoch, .. } => {
                            trace!("Finalize tx={}", tx_hash);
                            let status = TransactionStatus::Committed { epoch };
                            tx.status = status.clone();
                            changed_to_status = Some(status);
                        }
                        _ => {}
                    },
                    LogEntry::Incoming { .. } => bail!("Expected outgoing transaction."),
                };
                Ok(e)
            })
            .expect("error in updating status.");

            if let Some(status) = changed_to_status {
                result.insert(tx_hash, status.clone());
                self.update_pending_tx(tx_hash, status);
            }
        }
        result
    }

    /// List log entries starting from `offset`, limited by `limit`.
    pub fn iter_range(
        &self,
        starting_from: Timestamp,
        limit: u64,
    ) -> impl Iterator<Item = (Timestamp, LogEntry)> {
        let key = Self::bytes_from_timestamp(starting_from);
        let mode = IteratorMode::From(&key, Direction::Forward);
        self.database
            .iterator(mode)
            .map(|(k, v)| {
                let k = Self::timestamp_from_bytes(&k).expect("parsable time");
                let v = LogEntry::from_buffer(&*v).expect("couldn't deserialize entry.");
                (k, v)
            })
            .take(limit as usize)
    }

    //
    // Internal api.
    //

    /// Convert timestamp to bytearray.
    fn bytes_from_timestamp(timestamp: Timestamp) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        BigEndian::write_u64(&mut bytes[0..8], timestamp.into());
        bytes
    }

    /// Convert timestamp to bytearray.
    fn bytes_from_len(len: u64) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        BigEndian::write_u64(&mut bytes[0..8], len);
        bytes
    }

    /// Convert bytearray to timestamp.
    fn timestamp_from_bytes(bytes: &[u8]) -> Option<Timestamp> {
        if bytes.len() == 8 {
            let millis = BigEndian::read_u64(&bytes[0..8]);
            Some(millis.into())
        } else {
            None
        }
    }

    /// Convert bytearray to timestamp.
    fn len_from_bytes(bytes: &[u8]) -> Option<u64> {
        if bytes.len() == 8 {
            let idx = BigEndian::read_u64(&bytes[0..8]);
            Some(idx)
        } else {
            None
        }
    }
}

/// Information about created output.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PaymentCertificate {
    /// Certificate of creation for utxo.
    pub id: u32,
    /// destination PublicKey.
    pub recipient: PublicKey,
    /// Rvalue used to decrypt PaymentPayload in case of unfair recipient.
    #[serde(serialize_with = "PaymentCertificate::serialize_rvalue")]
    #[serde(deserialize_with = "PaymentCertificate::deserialize_rvalue")]
    pub rvalue: Fr,
    /// amount of money sended in utxo.
    pub amount: i64,
}

impl Hashable for PaymentCertificate {
    fn hash(&self, hasher: &mut Hasher) {
        self.id.hash(hasher);
        self.recipient.hash(hasher);
        self.rvalue.hash(hasher);
        self.amount.hash(hasher);
    }
}

/// Information about created transactions
#[derive(Serialize, Clone, Debug)]
pub struct PaymentTransactionValue {
    #[serde(skip_serializing)]
    pub tx: PaymentTransaction,
    pub status: TransactionStatus,
    pub certificates: Vec<PaymentCertificate>,
}

impl Hashable for PaymentTransactionValue {
    fn hash(&self, hasher: &mut Hasher) {
        self.tx.hash(hasher);
        self.status.hash(hasher);
        for certificate in &self.certificates {
            certificate.hash(hasher);
        }
    }
}

/// Represents Outputs created by account.
/// With extended info about its creation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OutputValue {
    Payment(PaymentValue),
    PublicPayment(PublicPaymentOutput),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentValue {
    pub output: PaymentOutput,
    pub amount: i64,
    pub data: PaymentPayloadData,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StakeValue {
    pub output: StakeOutput,
    pub active_until_epoch: u64,
}

impl PaymentValue {
    pub fn to_info(&self) -> PaymentInfo {
        PaymentInfo {
            utxo: Hash::digest(&self.output),
            amount: self.amount,
            data: self.data.clone(),
            locked_timestamp: self.output.locked_timestamp,
        }
    }
}

impl StakeValue {
    pub fn to_info(&self, epoch: u64) -> StakeInfo {
        let is_active = self.active_until_epoch >= epoch;
        StakeInfo {
            account_pkey: self.output.recipient,
            utxo: Hash::digest(&self.output),
            amount: self.output.amount,
            active_until_epoch: self.active_until_epoch,
            is_active,
        }
    }
}

impl OutputValue {
    pub fn to_info(&self) -> OutputInfo {
        match self {
            OutputValue::Payment(o) => o.to_info().into(),
            OutputValue::PublicPayment(o) => public_payment_info(&o).into(),
        }
    }
}

impl LogEntry {
    pub fn to_info(&self, timestamp: Timestamp) -> LogEntryInfo {
        match *self {
            LogEntry::Incoming { ref output } => LogEntryInfo::Incoming {
                timestamp,
                output: output.to_info(),
            },
            LogEntry::Outgoing { ref tx } => LogEntryInfo::Outgoing {
                timestamp,
                tx: tx.to_info(),
            },
        }
    }

    #[allow(unused)]
    fn testing_stub(id: usize) -> LogEntry {
        let (_s, p) = make_random_keys();
        let public = PublicPaymentOutput::new(&p, id as i64);

        LogEntry::Incoming {
            output: OutputValue::PublicPayment(public),
        }
    }

    #[allow(unused)]
    fn is_testing_stub(&self, id: usize) -> bool {
        match self {
            LogEntry::Incoming {
                output: OutputValue::PublicPayment(output),
            } => output.amount == id as i64,
            _ => false,
        }
    }
}

pub fn public_payment_info(output: &PublicPaymentOutput) -> PublicPaymentInfo {
    PublicPaymentInfo {
        utxo: Hash::digest(&output),
        amount: output.amount,
        locked_timestamp: output.locked_timestamp,
    }
}

impl PaymentCertificate {
    fn serialize_rvalue<S>(rvalue: &Fr, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&rvalue.to_hex())
    }

    fn deserialize_rvalue<'de, D>(deserilizer: D) -> Result<Fr, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, Visitor};
        use std::fmt;
        struct FrVisitor;

        impl<'de> Visitor<'de> for FrVisitor {
            type Value = Fr;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a hex representation of Fr")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Fr::try_from_hex(value).map_err(|e| E::custom(e))
            }
        }
        deserilizer.deserialize_string(FrVisitor)
    }
}

impl PaymentTransactionValue {
    pub fn new_payment(
        _data: Option<PaymentPayloadData>,
        recipient: PublicKey,
        tx: PaymentTransaction,
        rvalues: &[Option<Fr>],
        amount: i64,
    ) -> PaymentTransactionValue {
        let certificates: Vec<_> = rvalues
            .iter()
            .enumerate()
            .filter_map(|(id, r)| r.clone().map(|r| (id, r)))
            .map(|(id, rvalue)| PaymentCertificate {
                id: id as u32,
                rvalue,
                recipient,
                amount,
            })
            .collect();

        assert!(tx.txouts.len() <= 2);
        assert!(certificates.len() <= 1);

        PaymentTransactionValue {
            certificates,
            tx,
            status: TransactionStatus::Created {},
        }
    }

    pub fn new_vs(tx: PaymentTransaction) -> PaymentTransactionValue {
        assert!(tx.txouts.len() >= 2);
        PaymentTransactionValue {
            certificates: Vec::new(),
            tx,
            status: TransactionStatus::Created {},
        }
    }

    pub fn new_cloak(tx: PaymentTransaction) -> PaymentTransactionValue {
        assert_eq!(tx.txouts.len(), 1);

        PaymentTransactionValue {
            certificates: Vec::new(),
            tx,
            status: TransactionStatus::Created {},
        }
    }

    pub fn new_stake(tx: PaymentTransaction) -> PaymentTransactionValue {
        PaymentTransactionValue {
            certificates: Vec::new(),
            tx,
            status: TransactionStatus::Created {},
        }
    }

    pub fn to_info(&self) -> PaymentTransactionInfo {
        let tx_hash = Hash::digest(&self.tx);
        PaymentTransactionInfo {
            tx_hash,
            certificates: self.certificates.clone(),
            status: self.status.clone(),
        }
    }
}

impl From<PaymentValue> for OutputValue {
    fn from(value: PaymentValue) -> OutputValue {
        OutputValue::Payment(value)
    }
}

impl From<PublicPaymentOutput> for OutputValue {
    fn from(value: PublicPaymentOutput) -> OutputValue {
        OutputValue::PublicPayment(value)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn create_entry(id: usize) -> (Timestamp, LogEntry) {
        let time = Timestamp::UNIX_EPOCH + Duration::from_millis(id as u64 + 1);
        let entry = LogEntry::testing_stub(id);
        (time, entry)
    }

    #[test]
    fn smoke_test() {
        let _ = simple_logger::init();

        let entries: Vec<_> = (0..5).map(create_entry).collect();

        let mut db = AccountLog::testing();
        for (time, e) in entries.iter() {
            db.push_entry(*time, e.clone()).unwrap();
        }

        // ignore that limit 10, still return 5 items
        for ((id, (t, ref saved)), (time2, _)) in db
            .iter_range(Timestamp::UNIX_EPOCH, 10)
            .enumerate()
            .zip(entries.iter())
        {
            debug!("saved = {:?}", saved);
            assert!(saved.is_testing_stub(id));
            assert_eq!(&t, time2);
        }
    }

    #[test]
    fn iter_order_bytes() {
        let _ = simple_logger::init();

        let entries: Vec<_> = (0..256).map(create_entry).collect();

        let mut db = AccountLog::testing();
        for (time, e) in entries.iter() {
            db.push_entry(*time, e.clone()).unwrap();
        }

        for ((id, (t, ref saved)), (time2, _)) in db
            .iter_range(Timestamp::UNIX_EPOCH, 1000)
            .enumerate()
            .zip(entries.iter())
        {
            debug!("saved = {:?}", saved);
            assert!(saved.is_testing_stub(id));
            assert_eq!(&t, time2);
        }
    }

    #[test]
    fn push_duplicate_time() {
        let _ = simple_logger::init();

        let entries: Vec<_> = (0..2).map(create_entry).collect();
        let time = Timestamp::UNIX_EPOCH + Duration::from_millis(5);
        let mut db = AccountLog::testing();
        for (_, e) in entries.iter() {
            db.push_entry(time, e.clone()).unwrap();
        }

        for (id, (t, ref saved)) in db.iter_range(Timestamp::UNIX_EPOCH, 5).enumerate() {
            debug!("saved = {:?}", saved);
            assert!(saved.is_testing_stub(id));
            assert_eq!(t, time + Duration::from_millis(id as u64));
        }
    }

    // Log can't save data in past time, so old timestamp should be pushed as last_known_time  +1 ms;
    #[test]
    fn push_past_time() {
        let _ = simple_logger::init();

        let entries: Vec<_> = (0..2).map(create_entry).collect();
        let time = Timestamp::UNIX_EPOCH + Duration::from_millis(5);
        let mut db = AccountLog::testing();

        let mut iter = entries.iter();
        let (_, e) = iter.next().unwrap();
        db.push_entry(time, e.clone()).unwrap();

        let (_, e) = iter.next().unwrap();
        db.push_entry(time - Duration::from_millis(1), e.clone())
            .unwrap();

        for (id, (t, ref saved)) in db.iter_range(Timestamp::UNIX_EPOCH, 5).enumerate() {
            debug!("saved = {:?}", saved);
            assert!(saved.is_testing_stub(id));
            assert_eq!(t, time + Duration::from_millis(id as u64));
        }
    }

}
