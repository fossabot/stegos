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

#![allow(unused)]

pub use stegos_node::test::*;
mod account_transaction;
use super::Account;
use crate::{
    AccountEvent, AccountNotification, AccountResponse, AccountService, UnsealedAccountService,
};
use stegos_blockchain::{Blockchain, ChainConfig};
use stegos_crypto::scc;
use stegos_network::Network;
use tempdir::TempDir;

use futures::sync::{mpsc, oneshot};
use futures::{Async, Future};
use log::info;
use stegos_node::Node;

const PASSWORD: &str = "1234";

fn genesis_accounts(s: &mut Sandbox) -> Vec<AccountSandbox> {
    let mut accounts = Vec::new();
    for i in 0..s.nodes.len() {
        let account = AccountSandbox::new_genesis(s, i);
        accounts.push(account);
    }
    accounts
}

struct AccountSandbox {
    //TODO: moove tempdir out of account sandbox
    _tmp_dir: TempDir,
    #[allow(dead_code)]
    network: Loopback,
    account: Account,
    account_service: UnsealedAccountService,
}

impl AccountSandbox {
    pub fn new(
        stake_epochs: u64,
        keys: KeyChain,
        node: Node,
        chain: &Blockchain,
        network_service: Loopback,
        network: Network,
    ) -> AccountSandbox {
        let temp_dir = TempDir::new("account").unwrap();
        let network_pkey = keys.network_pkey;
        let network_skey = keys.network_skey.clone();
        let account_pkey = keys.account_pkey;
        let account_skey = keys.account_skey.clone();
        // init network
        let mut database_dir = temp_dir.path().to_path_buf();

        database_dir.push("database_path");
        let account_skey_file = temp_dir.path().join("account.skey");
        let account_pkey_file = temp_dir.path().join("account.pkey");
        stegos_keychain::keyfile::write_account_skey(&account_skey_file, &account_skey, PASSWORD)
            .unwrap();
        stegos_keychain::keyfile::write_account_pkey(&account_pkey_file, &account_pkey).unwrap();

        info!(
            "Wrote account key pair: skey_file={:?}, pkey_file={:?}",
            account_skey_file, account_pkey_file
        );

        let (outbox, events) = mpsc::unbounded::<AccountEvent>();
        let subscribers: Vec<mpsc::UnboundedSender<AccountNotification>> = Vec::new();
        let account_service = UnsealedAccountService::new(
            database_dir,
            account_skey_file,
            account_pkey_file,
            account_skey,
            account_pkey,
            network_skey,
            network_pkey,
            network,
            node,
            stake_epochs,
            subscribers,
            events,
        );
        let account = Account { outbox };

        AccountSandbox {
            account,
            account_service,
            network: network_service,
            _tmp_dir: temp_dir,
        }
    }

    pub fn new_genesis(s: &mut Sandbox, node_id: usize) -> AccountSandbox {
        let stake_epochs = s.config.chain.stake_epochs;
        let node = s.nodes[node_id].node.clone();
        let keys = s.keychains[node_id].clone();
        // genesis accounts should reuse the same network.
        let (network_service, network) = s.nodes[node_id].clone_network();
        Self::new(
            stake_epochs,
            keys,
            node,
            &s.nodes[node_id].chain(),
            network_service,
            network,
        )
    }

    #[allow(dead_code)]
    pub fn new_custom(s: &mut Sandbox, node_id: usize) -> AccountSandbox {
        let stake_epochs = s.config.chain.stake_epochs;
        let node = s.nodes[node_id].node.clone();
        let mut keys = s.keychains[node_id].clone();
        // change account keys to custom
        let (skey, pkey) = scc::make_random_keys();
        keys.account_pkey = pkey;
        keys.account_skey = skey;

        let (network_service, network) = Loopback::new();
        Self::new(
            stake_epochs,
            keys,
            node,
            &s.nodes[node_id].chain(),
            network_service,
            network,
        )
    }

    pub fn poll(&mut self) {
        futures_testing::execute(&mut self.account_service);
    }
}

fn get_request(mut rx: oneshot::Receiver<AccountResponse>) -> AccountResponse {
    match rx.poll() {
        Ok(Async::Ready(msg)) => return msg,
        _ => panic!("No message received in time, or error when receiving message"),
    }
}

// test::
// Create regular transaction.
// check that it was not committed.
// skip_micro_block().
// check that tx was committed.
