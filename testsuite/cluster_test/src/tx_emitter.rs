use crate::{cluster::Cluster, instance::Instance};
use admission_control_proto::proto::{
    admission_control::SubmitTransactionRequest, admission_control_grpc::AdmissionControlClient,
};
use grpcio::{ChannelBuilder, EnvBuilder};
use proto_conv::IntoProto;
use std::{
    env, slice,
    str::FromStr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use crypto::{
    ed25519::{Ed25519PrivateKey, Ed25519PublicKey},
    test_utils::KeyPair,
    traits::Uniform,
};
use failure::{
    self,
    prelude::{bail, format_err},
};
use generate_keypair::load_key_from_file;
use itertools::zip;
use proto_conv::FromProto;
use rand::{
    rngs::{EntropyRng, StdRng},
    Rng, SeedableRng,
};
use types::{
    account_address::AccountAddress,
    account_config::{association_address, get_account_resource_or_default},
    get_with_proof::ResponseItem,
    proto::get_with_proof::{
        GetAccountStateRequest, RequestItem, RequestItem_oneof_requested_items,
        UpdateToLatestLedgerRequest,
    },
    transaction::{Script, TransactionPayload},
    transaction_helpers::create_signed_txn,
};

const ACCOUNT_PER_CLIENT_DEFAULT: usize = 10;
const THREADS_PER_CLIENT_DEFAULT: usize = 1;
const MAX_TXN_BATCH_SIZE: usize = 100; // Max transactions per account in mempool

pub struct TxEmitter {
    clients: Vec<(Instance, AdmissionControlClient)>,
}

impl TxEmitter {
    pub fn new(cluster: &Cluster) -> Self {
        let clients = Self::create_ac_clients(cluster);

        Self { clients }
    }

    fn create_ac_clients(cluster: &Cluster) -> Vec<(Instance, AdmissionControlClient)> {
        let mut clients = vec![];
        let threads_per_client = get_env("THREADS_PER_CLIENT", THREADS_PER_CLIENT_DEFAULT);
        for instance in cluster.instances() {
            let address = format!("{}:8000", instance.ip());
            for _ in 0..threads_per_client {
                let env_builder = Arc::new(EnvBuilder::new().name_prefix("ac-grpc-").build());
                let ch = ChannelBuilder::new(env_builder).connect(&address);
                clients.push((instance.clone(), AdmissionControlClient::new(ch)));
            }
        }
        clients
    }

    pub fn run(self) {
        let mint_client = &self.clients[0].1;
        let mut faucet_account = load_faucet_account(mint_client, "mint.key");
        let account_per_client = get_env("ACCOUNT_PER_CLIENT", ACCOUNT_PER_CLIENT_DEFAULT);
        let num_accounts = account_per_client * self.clients.len();
        println!("Minting accounts");
        let mut all_accounts: Vec<AccountData> = vec![];
        for _ in 0..(num_accounts + MAX_TXN_BATCH_SIZE - 1) / MAX_TXN_BATCH_SIZE {
            let mut accounts = gen_random_accounts(MAX_TXN_BATCH_SIZE);
            let mint_requests = gen_mint_txn_requests(&mut faucet_account, &accounts);
            execute_and_wait_transactions(&mint_client, &faucet_account, mint_requests);
            all_accounts.append(&mut accounts);
        }
        println!("Mint is done");
        let mut join_handles = vec![];
        let mut all_accounts = all_accounts.into_iter();
        for (index, (instance, client)) in self.clients.into_iter().enumerate() {
            let accounts = (&mut all_accounts).take(account_per_client).collect();
            let thread = SubmissionThread {
                accounts,
                instance,
                client,
            };
            let join_handle = thread::Builder::new()
                .name(format!("thread-{}", index))
                .spawn(move || thread.run())
                .unwrap();
            join_handles.push(join_handle);
            thread::sleep(Duration::from_millis(10)); // Small stagger between starting threads
        }
        println!("Threads started");
        for join_handle in join_handles {
            join_handle.join().unwrap();
        }
    }
}

fn get_env<F: FromStr>(name: &str, default: F) -> F {
    match env::var(name) {
        Ok(v) => match v.parse() {
            Ok(v) => v,
            _ => panic!("Failed to parse env {}", name),
        },
        _ => default,
    }
}

struct SubmissionThread {
    accounts: Vec<AccountData>,
    instance: Instance,
    client: AdmissionControlClient,
}

impl SubmissionThread {
    fn run(mut self) {
        let wait_millis = get_env("WAIT_MILLIS", 50);
        let wait = Duration::from_millis(wait_millis);
        let wait_committed = get_env("WAIT_COMMITTED", true);
        loop {
            let gen_requests = gen_ring_requests(&mut self.accounts);
            for request in gen_requests {
                let wait_util = Instant::now() + wait;
                let resp = self.client.submit_transaction(&request);
                match resp {
                    Err(e) => println!("Failed to submit request to {}: {:?}", self.instance, e),
                    Ok(_r) => {
                        //                        println!("r: {:?}", _r)
                    }
                }
                let now = Instant::now();
                if wait_util > now {
                    thread::sleep(wait_util - now);
                } else {
                    println!("Thread for {} won't sleep", self.instance);
                }
            }
            if wait_committed {
                wait_for_accounts_sequence(&self.client, &self.accounts);
            }
        }
    }
}

fn wait_for_accounts_sequence(client: &AdmissionControlClient, accounts: &[AccountData]) {
    let addresses: Vec<_> = accounts.iter().map(|d| d.address).collect();
    loop {
        match query_sequence_numbers(client, &addresses) {
            Err(e) => println!("Failed to query ledger info: {:?}", e),
            Ok(sequence_numbers) => {
                if is_sequence_equal(accounts, &sequence_numbers) {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn is_sequence_equal(accounts: &[AccountData], sequence_numbers: &[u64]) -> bool {
    for (account, sequence_number) in zip(accounts, sequence_numbers) {
        if *sequence_number != account.sequence_number {
            return false;
        }
    }
    true
}

fn query_sequence_numbers(
    client: &AdmissionControlClient,
    addresses: &[AccountAddress],
) -> failure::Result<Vec<u64>> {
    let mut update_request = UpdateToLatestLedgerRequest::new();
    for address in addresses {
        let mut request_item = RequestItem::new();
        let mut account_state_request = GetAccountStateRequest::new();
        account_state_request.address = address.to_vec();
        request_item.requested_items = Some(
            RequestItem_oneof_requested_items::get_account_state_request(account_state_request),
        );
        update_request.requested_items.push(request_item);
    }
    let resp = client
        .update_to_latest_ledger(&update_request)
        .map_err(|e| format_err!("update_to_latest_ledger failed: {:?} ", e))?;
    let mut result = Vec::with_capacity(resp.response_items.len());
    for item in resp.response_items.into_iter() {
        let item = ResponseItem::from_proto(item)
            .map_err(|e| format_err!("ResponseItem::from_proto failed: {:?} ", e))?;
        if let ResponseItem::GetAccountState {
            account_state_with_proof,
        } = item
        {
            let account_resource = get_account_resource_or_default(&account_state_with_proof.blob)
                .map_err(|e| format_err!("get_account_resource_or_default failed: {:?} ", e))?;
            result.push(account_resource.sequence_number());
        } else {
            bail!(
                "Unexpected item in UpdateToLatestLedgerResponse: {:?}",
                item
            );
        }
    }
    Ok(result)
}

const MAX_GAS_AMOUNT: u64 = 1_000_000;
const GAS_UNIT_PRICE: u64 = 0;
const TXN_EXPIRATION: i64 = 100;

fn gen_submit_transaction_request(
    script: Script,
    sender_account: &mut AccountData,
) -> SubmitTransactionRequest {
    let signed_txn = create_signed_txn(
        &sender_account.key_pair,
        TransactionPayload::Script(script),
        sender_account.address,
        sender_account.sequence_number,
        MAX_GAS_AMOUNT,
        GAS_UNIT_PRICE,
        TXN_EXPIRATION,
    )
    .expect("Failed to create signed transaction");
    let mut req = SubmitTransactionRequest::new();
    req.set_signed_txn(signed_txn.into_proto());
    sender_account.sequence_number += 1;
    req
}

fn gen_transfer_txn_request(
    sender: &mut AccountData,
    receiver: &AccountAddress,
    num_coins: u64,
) -> SubmitTransactionRequest {
    let script = transaction_builder::encode_transfer_script(&receiver, num_coins);
    gen_submit_transaction_request(script, sender)
}

fn gen_ring_requests(accounts: &mut [AccountData]) -> Vec<SubmitTransactionRequest> {
    let mut receiver_addrs: Vec<AccountAddress> =
        accounts.iter().map(|account| account.address).collect();
    receiver_addrs.rotate_left(1);
    accounts
        .iter_mut()
        .zip(receiver_addrs.iter())
        .map(|(sender, receiver_addr)| gen_transfer_txn_request(sender, receiver_addr, 1))
        .collect()
}

fn gen_random_account(rng: &mut StdRng) -> AccountData {
    let key_pair = KeyPair::generate_for_testing(rng);
    AccountData {
        address: AccountAddress::from_public_key(&key_pair.public_key),
        key_pair,
        sequence_number: 0,
    }
}

fn gen_random_accounts(num_accounts: usize) -> Vec<AccountData> {
    let seed: [u8; 32] = EntropyRng::new().gen();
    let mut rng = StdRng::from_seed(seed);
    (0..num_accounts)
        .map(|_| gen_random_account(&mut rng))
        .collect()
}

fn gen_mint_txn_request(
    faucet_account: &mut AccountData,
    receiver: &AccountAddress,
) -> SubmitTransactionRequest {
    let program = transaction_builder::encode_mint_script(receiver, 1_000_000);
    gen_submit_transaction_request(program, faucet_account)
}

fn gen_mint_txn_requests(
    faucet_account: &mut AccountData,
    accounts: &[AccountData],
) -> Vec<SubmitTransactionRequest> {
    accounts
        .iter()
        .map(|account| gen_mint_txn_request(faucet_account, &account.address))
        .collect()
}

/// Executes many transactions for single client
/// This method batches transactions in batches of MAX_TXN_BATCH_SIZE size
/// Usage example: mint
fn execute_and_wait_transactions(
    client: &AdmissionControlClient,
    account: &AccountData,
    txn: Vec<SubmitTransactionRequest>,
) {
    for request in txn {
        let resp = client.submit_transaction(&request);
        match resp {
            Err(e) => println!("Failed to submit request: {:?}", e),
            Ok(_r) => {}
        }
    }
    wait_for_accounts_sequence(client, slice::from_ref(account));
}

fn load_faucet_account(client: &AdmissionControlClient, faucet_account_path: &str) -> AccountData {
    let key_pair: KeyPair<Ed25519PrivateKey, Ed25519PublicKey> =
        load_key_from_file(faucet_account_path).expect("invalid faucet keypair file");
    let address = association_address();
    let sequence_number = query_sequence_numbers(client, &[address])
        .expect("query_sequence_numbers for faucet account failed")[0];
    AccountData {
        address,
        key_pair,
        sequence_number,
    }
}

struct AccountData {
    pub address: AccountAddress,
    pub key_pair: KeyPair<Ed25519PrivateKey, Ed25519PublicKey>,
    pub sequence_number: u64,
}