// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the CC0 Public Domain Dedication
// along with this software.
// If not, see <http://creativecommons.org/publicdomain/zero/1.0/>.
//

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::iter::FromIterator;
use std::path::PathBuf;
use std::str::FromStr;
use std::{fmt, result};

use crate::dashcore;
use jsonrpc;
use serde;
use serde_json::{self, Value};

use crate::dashcore::address::NetworkUnchecked;
use crate::dashcore::{block, consensus, ScriptBuf};
use dashcore::hashes::hex::FromHex;
use dashcore::secp256k1::ecdsa::Signature;
use dashcore::{
    Address, Amount, Block, OutPoint, PrivateKey, ProTxHash, PublicKey, QuorumHash, Transaction,
};
use dashcore_rpc_json::dashcore::bls_sig_utils::BLSSignature;
use dashcore_rpc_json::dashcore::{BlockHash, ChainLock};
use dashcore_rpc_json::{ProTxInfo, ProTxListType, QuorumType};
use hex::ToHex;
use log::Level::{Debug, Trace, Warn};
use crate::dashcore::secp256k1::hashes::hex::DisplayHex;
use crate::error::*;
use crate::json;
use crate::queryable;
use crate::Error::UnexpectedStructure;

/// Crate-specific Result type, shorthand for `std::result::Result` with our
/// crate-specific Error type;
pub type Result<T> = result::Result<T, Error>;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JsonOutPoint {
    pub txid: dashcore::Txid,
    pub vout: u32,
}

impl From<OutPoint> for JsonOutPoint {
    fn from(o: OutPoint) -> JsonOutPoint {
        JsonOutPoint {
            txid: o.txid,
            vout: o.vout,
        }
    }
}

impl Into<OutPoint> for JsonOutPoint {
    fn into(self) -> OutPoint {
        OutPoint {
            txid: self.txid,
            vout: self.vout,
        }
    }
}

/// Shorthand for converting a variable into a serde_json::Value.
fn into_json<T>(val: T) -> Result<Value>
where
    T: serde::ser::Serialize,
{
    Ok(serde_json::to_value(val)?)
}

/// Shorthand for converting an Option into an Option<serde_json::Value>.
fn opt_into_json<T>(opt: Option<T>) -> Result<Value>
where
    T: serde::ser::Serialize,
{
    match opt {
        Some(val) => Ok(into_json(val)?),
        None => Ok(Value::Null),
    }
}

/// Shorthand for `serde_json::Value::Null`.
fn null() -> Value {
    Value::Null
}

/// Shorthand for an empty serde_json::Value array.
fn empty_arr() -> Value {
    Value::Array(vec![])
}

/// Shorthand for an empty serde_json object.
fn empty_obj() -> Value {
    Value::Object(Default::default())
}

/// Handle default values in the argument list
///
/// Substitute `Value::Null`s with corresponding values from `defaults` table,
/// except when they are trailing, in which case just skip them altogether
/// in returned list.
///
/// Note, that `defaults` corresponds to the last elements of `args`.
///
/// ```norust
/// arg1 arg2 arg3 arg4
///           def1 def2
/// ```
///
/// Elements of `args` without corresponding `defaults` value, won't
/// be substituted, because they are required.
fn handle_defaults<'a, 'b>(args: &'a mut [Value], defaults: &'b [Value]) -> &'a [Value] {
    assert!(args.len() >= defaults.len());

    // Pass over the optional arguments in backwards order, filling in defaults after the first
    // non-null optional argument has been observed.
    let mut first_non_null_optional_idx = None;
    for i in 0..defaults.len() {
        let args_i = args.len() - 1 - i;
        let defaults_i = defaults.len() - 1 - i;
        if args[args_i] == serde_json::Value::Null {
            if first_non_null_optional_idx.is_some() {
                if defaults[defaults_i] == serde_json::Value::Null {
                    panic!("Missing `default` for argument idx {}", args_i);
                }
                args[args_i] = defaults[defaults_i].clone();
            }
        } else if first_non_null_optional_idx.is_none() {
            first_non_null_optional_idx = Some(args_i);
        }
    }

    let required_num = args.len() - defaults.len();

    if let Some(i) = first_non_null_optional_idx {
        &args[..i + 1]
    } else {
        &args[..required_num]
    }
}

/// Convert a possible-null result into an Option.
fn opt_result<T: for<'a> serde::de::Deserialize<'a>>(result: Value) -> Result<Option<T>> {
    if result == Value::Null {
        Ok(None)
    } else {
        Ok(serde_json::from_value(result)?)
    }
}

/// Used to pass raw txs into the API.
pub trait RawTx: Sized + Clone {
    fn raw_hex(self) -> String;
}

impl<'a> RawTx for &'a Transaction {
    fn raw_hex(self) -> String {
        hex::encode(consensus::encode::serialize(&self))
    }
}

impl<'a> RawTx for &'a [u8] {
    fn raw_hex(self) -> String {
        self.to_lower_hex_string()
    }
}

impl<'a> RawTx for &'a Vec<u8> {
    fn raw_hex(self) -> String {
        self.to_lower_hex_string()
    }
}

impl<'a> RawTx for &'a str {
    fn raw_hex(self) -> String {
        self.to_owned()
    }
}

impl RawTx for String {
    fn raw_hex(self) -> String {
        self
    }
}

/// The different authentication methods for the client.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub enum Auth {
    None,
    UserPass(String, String),
    CookieFile(PathBuf),
}

impl Auth {
    /// Convert into the arguments that jsonrpc::Client needs.
    pub fn get_user_pass(self) -> Result<(Option<String>, Option<String>)> {
        use std::io::Read;
        match self {
            Auth::None => Ok((None, None)),
            Auth::UserPass(u, p) => Ok((Some(u), Some(p))),
            Auth::CookieFile(path) => {
                let mut file = File::open(path)?;
                let mut contents = String::new();
                file.read_to_string(&mut contents)?;
                let mut split = contents.splitn(2, ":");
                Ok((
                    Some(split.next().ok_or(Error::InvalidCookieFile)?.into()),
                    Some(split.next().ok_or(Error::InvalidCookieFile)?.into()),
                ))
            }
        }
    }
}

pub trait RpcApi: Sized {
    /// Call a `cmd` rpc with given `args` list
    fn call<T: for<'a> serde::de::Deserialize<'a>>(&self, cmd: &str, args: &[Value]) -> Result<T>;

    /// Query an object implementing `Querable` type
    fn get_by_id<T: queryable::Queryable<Self>>(
        &self,
        id: &<T as queryable::Queryable<Self>>::Id,
    ) -> Result<T> {
        T::query(&self, &id)
    }

    fn get_network_info(&self) -> Result<json::GetNetworkInfoResult> {
        self.call("getnetworkinfo", &[])
    }

    fn version(&self) -> Result<usize> {
        #[derive(Deserialize)]
        struct Response {
            pub version: usize,
        }
        let res: Response = self.call("getnetworkinfo", &[])?;
        Ok(res.version)
    }

    fn add_multisig_address(
        &self,
        nrequired: usize,
        keys: &[json::PubKeyOrAddress],
        label: Option<&str>,
        address_type: Option<json::AddressType>,
    ) -> Result<json::AddMultiSigAddressResult> {
        let mut args = [
            into_json(nrequired)?,
            into_json(keys)?,
            opt_into_json(label)?,
            opt_into_json(address_type)?,
        ];
        self.call("addmultisigaddress", handle_defaults(&mut args, &[into_json("")?, null()]))
    }

    fn load_wallet(&self, wallet: &str) -> Result<json::LoadWalletResult> {
        self.call("loadwallet", &[wallet.into()])
    }

    fn unload_wallet(&self, wallet: Option<&str>) -> Result<json::UnloadWalletResult> {
        let mut args = [opt_into_json(wallet)?];
        self.call("unloadwallet", handle_defaults(&mut args, &[null()]))
    }

    fn create_wallet(
        &self,
        wallet: &str,
        disable_private_keys: Option<bool>,
        blank: Option<bool>,
        passphrase: Option<&str>,
        avoid_reuse: Option<bool>,
    ) -> Result<json::LoadWalletResult> {
        let mut args = [
            wallet.into(),
            opt_into_json(disable_private_keys)?,
            opt_into_json(blank)?,
            opt_into_json(passphrase)?,
            opt_into_json(avoid_reuse)?,
        ];
        self.call(
            "createwallet",
            handle_defaults(&mut args, &[false.into(), false.into(), into_json("")?, false.into()]),
        )
    }

    fn list_wallets(&self) -> Result<Vec<String>> {
        self.call("listwallets", &[])
    }

    fn get_wallet_info(&self) -> Result<json::GetWalletInfoResult> {
        self.call("getwalletinfo", &[])
    }

    fn backup_wallet(&self, destination: Option<&str>) -> Result<()> {
        let mut args = [opt_into_json(destination)?];
        self.call("backupwallet", handle_defaults(&mut args, &[null()]))
    }

    fn dump_private_key(&self, address: &Address) -> Result<PrivateKey> {
        self.call("dumpprivkey", &[address.to_string().into()])
    }

    fn encrypt_wallet(&self, passphrase: &str) -> Result<()> {
        self.call("encryptwallet", &[into_json(passphrase)?])
    }

    fn get_difficulty(&self) -> Result<f64> {
        self.call("getdifficulty", &[])
    }

    fn get_connection_count(&self) -> Result<usize> {
        self.call("getconnectioncount", &[])
    }

    fn get_block(&self, hash: &BlockHash) -> Result<Block> {
        let hex: String = self.call("getblock", &[into_json(hash)?, 0.into()])?;
        let bytes: Vec<u8> = FromHex::from_hex(&hex)?;
        Ok(dashcore::consensus::encode::deserialize(&bytes)?)
    }

    fn get_block_json(&self, hash: &BlockHash) -> Result<Value> {
        Ok(self.call::<Value>("getblock", &[into_json(hash)?, 1.into()])?)
    }

    fn get_block_hex(&self, hash: &BlockHash) -> Result<String> {
        self.call("getblock", &[into_json(hash)?, 0.into()])
    }

    fn get_block_info(&self, hash: &BlockHash) -> Result<json::GetBlockResult> {
        self.call("getblock", &[into_json(hash)?, 1.into()])
    }
    //TODO(stevenroose) add getblock_txs

    fn get_block_header(&self, hash: &BlockHash) -> Result<block::Header> {
        let hex: String = self.call("getblockheader", &[into_json(hash)?, false.into()])?;
        let bytes: Vec<u8> = FromHex::from_hex(&hex)?;
        Ok(dashcore::consensus::encode::deserialize(&bytes)?)
    }

    fn get_block_header_info(&self, hash: &BlockHash) -> Result<json::GetBlockHeaderResult> {
        self.call("getblockheader", &[into_json(hash)?, true.into()])
    }

    fn get_mining_info(&self) -> Result<json::GetMiningInfoResult> {
        self.call("getmininginfo", &[])
    }

    fn get_block_template(
        &self,
        mode: json::GetBlockTemplateModes,
        rules: &[json::GetBlockTemplateRules],
        capabilities: &[json::GetBlockTemplateCapabilities],
    ) -> Result<json::GetBlockTemplateResult> {
        #[derive(Serialize)]
        struct Argument<'a> {
            mode: json::GetBlockTemplateModes,
            rules: &'a [json::GetBlockTemplateRules],
            capabilities: &'a [json::GetBlockTemplateCapabilities],
        }

        self.call(
            "getblocktemplate",
            &[into_json(Argument {
                mode: mode,
                rules: rules,
                capabilities: capabilities,
            })?],
        )
    }

    /// Returns a data structure containing various state info regarding
    /// blockchain processing.
    fn get_blockchain_info(&self) -> Result<json::GetBlockchainInfoResult> {
        self.call("getblockchaininfo", &[])
    }

    /// Returns the numbers of block in the longest chain.
    fn get_block_count(&self) -> Result<u32> {
        self.call("getblockcount", &[])
    }

    /// Returns the hash of the best (tip) block in the longest blockchain.
    fn get_best_block_hash(&self) -> Result<BlockHash> {
        self.call("getbestblockhash", &[])
    }

    /// Returns information about the best chainlock.
    fn get_best_chain_lock(&self) -> Result<ChainLock> {
        let json::GetBestChainLockResult {
            blockhash,
            height,
            signature,
            known_block: _,
        } = self.call("getbestchainlock", &[])?;

        Ok(ChainLock {
            block_height: height,
            signature: BLSSignature::try_from(signature.as_slice())
                .map_err(|e| UnexpectedStructure(e.to_string()))?,
            block_hash: blockhash,
        })
    }

    /// Get block hash at a given height
    fn get_block_hash(&self, height: u32) -> Result<BlockHash> {
        self.call("getblockhash", &[height.into()])
    }

    fn get_block_stats(&self, height: u32) -> Result<json::GetBlockStatsResult> {
        self.call("getblockstats", &[height.into()])
    }

    fn get_block_stats_fields(
        &self,
        height: u32,
        fields: &[json::BlockStatsFields],
    ) -> Result<json::GetBlockStatsResultPartial> {
        self.call("getblockstats", &[height.into(), fields.into()])
    }

    fn get_raw_change_address(&self) -> Result<Address<NetworkUnchecked>> {
        let data: String = self.call("getrawchangeaddress", &[])?;
        let address = Address::from_str(&data).map_err(|_e| {
            Error::UnexpectedStructure(
                "change address given by core was not an address".to_string(),
            )
        })?;

        Ok(address)
    }

    fn get_raw_transaction(
        &self,
        txid: &dashcore::Txid,
        block_hash: Option<&BlockHash>,
    ) -> Result<Transaction> {
        let mut args = [into_json(txid)?, into_json(false)?, opt_into_json(block_hash)?];
        let hex: String = self.call("getrawtransaction", handle_defaults(&mut args, &[null()]))?;
        let bytes: Vec<u8> = FromHex::from_hex(&hex)?;
        Ok(dashcore::consensus::encode::deserialize(&bytes)?)
    }

    fn get_raw_transaction_multi(
        &self,
        transactions_by_block_hash: BTreeMap<&BlockHash, Vec<&dashcore::Txid>>,
    ) -> Result<BTreeMap<dashcore::Txid, Transaction>> {
        let mut args = [into_json(transactions_by_block_hash)?, into_json(false)?];
        let list = self.call::<Vec<(dashcore::Txid, Transaction)>>(
            "getrawtransactionmulti",
            handle_defaults(&mut args, &[null()]),
        )?;
        Ok(list.into_iter().collect())
    }

    fn get_raw_transaction_hex(
        &self,
        txid: &dashcore::Txid,
        block_hash: Option<&BlockHash>,
    ) -> Result<String> {
        let mut args = [into_json(txid)?, into_json(false)?, opt_into_json(block_hash)?];
        self.call("getrawtransaction", handle_defaults(&mut args, &[null()]))
    }

    fn get_raw_transaction_info(
        &self,
        txid: &dashcore::Txid,
        block_hash: Option<&BlockHash>,
    ) -> Result<json::GetRawTransactionResult> {
        println!("aaa");
        let mut args = [into_json(txid)?, into_json(true)?, opt_into_json(block_hash)?];
        println!("bbb");
        self.call("getrawtransaction", handle_defaults(&mut args, &[null()]))
    }

    fn get_block_filter(&self, block_hash: &BlockHash) -> Result<json::GetBlockFilterResult> {
        self.call("getblockfilter", &[into_json(block_hash)?])
    }

    fn get_balance(
        &self,
        minconf: Option<usize>,
        include_watchonly: Option<bool>,
    ) -> Result<Amount> {
        let mut args = ["*".into(), opt_into_json(minconf)?, opt_into_json(include_watchonly)?];
        Ok(Amount::from_btc(
            self.call("getbalance", handle_defaults(&mut args, &[0.into(), null()]))?,
        )?)
    }

    fn get_balances(&self) -> Result<json::GetBalancesResult> {
        Ok(self.call("getbalances", &[])?)
    }

    fn get_received_by_address(&self, address: &Address, minconf: Option<u32>) -> Result<Amount> {
        let mut args = [address.to_string().into(), opt_into_json(minconf)?];
        Ok(Amount::from_btc(
            self.call("getreceivedbyaddress", handle_defaults(&mut args, &[null()]))?,
        )?)
    }

    fn get_transaction_are_locked(
        &self,
        tx_ids: &Vec<dashcore::Txid>,
    ) -> Result<Vec<Option<json::GetTransactionLockedResult>>> {
        let transaction_ids_json = tx_ids
            .into_iter()
            .map(|tx_id| Ok(into_json(tx_id)?))
            .collect::<Result<Vec<Value>>>()?;
        let args = [transaction_ids_json.into()];
        self.call("gettxchainlocks", &args)
    }

    /// Returns only Chainlocked or Unknown status if height is provided
    fn get_asset_unlock_statuses(
        &self,
        indices: &[u64],
        height: Option<u32>,
    ) -> Result<Vec<json::AssetUnlockStatusResult>> {
        let indices_json = indices
            .into_iter()
            .map(|index| Ok(into_json(index.to_string())?))
            .collect::<Result<Vec<Value>>>()?;
        let args = [indices_json.into(), opt_into_json(height)?];
        self.call("getassetunlockstatuses", &args)
    }

    fn list_transactions(
        &self,
        label: Option<&str>,
        count: Option<usize>,
        skip: Option<usize>,
        include_watchonly: Option<bool>,
    ) -> Result<Vec<json::ListTransactionResult>> {
        let mut args = [
            label.unwrap_or("*").into(),
            opt_into_json(count)?,
            opt_into_json(skip)?,
            opt_into_json(include_watchonly)?,
        ];
        self.call("listtransactions", handle_defaults(&mut args, &[10.into(), 0.into(), null()]))
    }

    fn list_since_block(
        &self,
        blockhash: Option<&BlockHash>,
        target_confirmations: Option<usize>,
        include_watchonly: Option<bool>,
        include_removed: Option<bool>,
    ) -> Result<json::ListSinceBlockResult> {
        let mut args = [
            opt_into_json(blockhash)?,
            opt_into_json(target_confirmations)?,
            opt_into_json(include_watchonly)?,
            opt_into_json(include_removed)?,
        ];
        self.call("listsinceblock", handle_defaults(&mut args, &[null()]))
    }

    fn get_tx_out(
        &self,
        txid: &dashcore::Txid,
        vout: u32,
        include_mempool: Option<bool>,
    ) -> Result<Option<json::GetTxOutResult>> {
        let mut args = [into_json(txid)?, into_json(vout)?, opt_into_json(include_mempool)?];
        opt_result(self.call("gettxout", handle_defaults(&mut args, &[null()]))?)
    }

    fn get_tx_out_proof(
        &self,
        txids: &[dashcore::Txid],
        block_hash: Option<&BlockHash>,
    ) -> Result<Vec<u8>> {
        let mut args = [into_json(txids)?, opt_into_json(block_hash)?];
        let hex: String = self.call("gettxoutproof", handle_defaults(&mut args, &[null()]))?;
        Ok(FromHex::from_hex(&hex)?)
    }

    fn import_public_key(
        &self,
        pubkey: &PublicKey,
        label: Option<&str>,
        rescan: Option<bool>,
    ) -> Result<()> {
        let mut args = [pubkey.to_string().into(), opt_into_json(label)?, opt_into_json(rescan)?];
        self.call("importpubkey", handle_defaults(&mut args, &[into_json("")?, null()]))
    }

    fn import_private_key(
        &self,
        privkey: &PrivateKey,
        label: Option<&str>,
        rescan: Option<bool>,
    ) -> Result<()> {
        let mut args = [privkey.to_string().into(), opt_into_json(label)?, opt_into_json(rescan)?];
        self.call("importprivkey", handle_defaults(&mut args, &[into_json("")?, null()]))
    }

    fn import_address(
        &self,
        address: &Address,
        label: Option<&str>,
        rescan: Option<bool>,
    ) -> Result<()> {
        let mut args = [address.to_string().into(), opt_into_json(label)?, opt_into_json(rescan)?];
        self.call("importaddress", handle_defaults(&mut args, &[into_json("")?, null()]))
    }

    fn import_address_script(
        &self,
        script: &ScriptBuf,
        label: Option<&str>,
        rescan: Option<bool>,
        p2sh: Option<bool>,
    ) -> Result<()> {
        let mut args = [
            script.to_hex_string().into(),
            opt_into_json(label)?,
            opt_into_json(rescan)?,
            opt_into_json(p2sh)?,
        ];
        self.call(
            "importaddress",
            handle_defaults(&mut args, &[into_json("")?, true.into(), null()]),
        )
    }

    fn import_multi(
        &self,
        requests: &[json::ImportMultiRequest],
        options: Option<&json::ImportMultiOptions>,
    ) -> Result<Vec<json::ImportMultiResult>> {
        let mut json_requests = Vec::with_capacity(requests.len());
        for req in requests {
            json_requests.push(serde_json::to_value(req)?);
        }
        let mut args = [json_requests.into(), opt_into_json(options)?];
        self.call("importmulti", handle_defaults(&mut args, &[null()]))
    }

    fn set_label(&self, address: &Address, label: &str) -> Result<()> {
        self.call("setlabel", &[address.to_string().into(), label.into()])
    }

    fn key_pool_refill(&self, new_size: Option<usize>) -> Result<()> {
        let mut args = [opt_into_json(new_size)?];
        self.call("keypoolrefill", handle_defaults(&mut args, &[null()]))
    }

    fn list_unspent(
        &self,
        minconf: Option<usize>,
        maxconf: Option<usize>,
        addresses: Option<&[&Address]>,
        include_unsafe: Option<bool>,
        query_options: Option<json::ListUnspentQueryOptions>,
    ) -> Result<Vec<json::ListUnspentResultEntry>> {
        let mut args = [
            opt_into_json(minconf)?,
            opt_into_json(maxconf)?,
            opt_into_json(addresses)?,
            opt_into_json(include_unsafe)?,
            opt_into_json(query_options)?,
        ];
        let defaults = [into_json(0)?, into_json(9999999)?, empty_arr(), into_json(true)?, null()];
        self.call("listunspent", handle_defaults(&mut args, &defaults))
    }

    /// To unlock, use [unlock_unspent].
    fn lock_unspent(&self, outputs: &[OutPoint]) -> Result<bool> {
        let outputs: Vec<_> = outputs
            .into_iter()
            .map(|o| serde_json::to_value(JsonOutPoint::from(*o)).unwrap())
            .collect();
        self.call("lockunspent", &[false.into(), outputs.into()])
    }

    fn unlock_unspent(&self, outputs: &[OutPoint]) -> Result<bool> {
        let outputs: Vec<_> = outputs
            .into_iter()
            .map(|o| serde_json::to_value(JsonOutPoint::from(*o)).unwrap())
            .collect();
        self.call("lockunspent", &[true.into(), outputs.into()])
    }

    /// Unlock all unspent UTXOs.
    fn unlock_unspent_all(&self) -> Result<bool> {
        self.call("lockunspent", &[true.into()])
    }

    fn list_received_by_address(
        &self,
        address_filter: Option<&Address>,
        minconf: Option<u32>,
        add_locked: Option<bool>,
        include_empty: Option<bool>,
        include_watchonly: Option<bool>,
    ) -> Result<Vec<json::ListReceivedByAddressResult>> {
        let mut args = [
            opt_into_json(minconf)?,
            opt_into_json(add_locked)?,
            opt_into_json(include_empty)?,
            opt_into_json(include_watchonly)?,
            opt_into_json(address_filter)?,
        ];
        let defaults = [1.into(), true.into(), false.into(), false.into(), null()];
        self.call("listreceivedbyaddress", handle_defaults(&mut args, &defaults))
    }

    fn create_raw_transaction_hex(
        &self,
        utxos: &[json::CreateRawTransactionInput],
        outs: &HashMap<String, Amount>,
        locktime: Option<i64>,
    ) -> Result<String> {
        let outs_converted = serde_json::Map::from_iter(
            outs.iter().map(|(k, v)| (k.clone(), serde_json::Value::from(v.to_dash()))),
        );
        let mut args = [into_json(utxos)?, into_json(outs_converted)?, opt_into_json(locktime)?];
        let defaults = [into_json(0i64)?, null()];
        self.call("createrawtransaction", handle_defaults(&mut args, &defaults))
    }

    fn create_raw_transaction(
        &self,
        utxos: &[json::CreateRawTransactionInput],
        outs: &HashMap<String, Amount>,
        locktime: Option<i64>,
    ) -> Result<Transaction> {
        let hex: String = self.create_raw_transaction_hex(utxos, outs, locktime)?;
        let bytes: Vec<u8> = FromHex::from_hex(&hex)?;
        Ok(dashcore::consensus::encode::deserialize(&bytes)?)
    }

    fn fund_raw_transaction<R: RawTx>(
        &self,
        tx: R,
        options: Option<&json::FundRawTransactionOptions>,
    ) -> Result<json::FundRawTransactionResult> {
        let mut args = [tx.raw_hex().into(), opt_into_json(options)?];
        let defaults = [empty_obj(), null()];
        self.call("fundrawtransaction", handle_defaults(&mut args, &defaults))
    }

    fn sign_raw_transaction_with_wallet<R: RawTx>(
        &self,
        tx: R,
        utxos: Option<&[json::SignRawTransactionInput]>,
        sighash_type: Option<json::SigHashType>,
    ) -> Result<json::SignRawTransactionResult> {
        let mut args = [tx.raw_hex().into(), opt_into_json(utxos)?, opt_into_json(sighash_type)?];
        let defaults = [empty_arr(), null()];
        self.call("signrawtransactionwithwallet", handle_defaults(&mut args, &defaults))
    }

    fn sign_raw_transaction_with_key<R: RawTx>(
        &self,
        tx: R,
        privkeys: &[PrivateKey],
        prevtxs: Option<&[json::SignRawTransactionInput]>,
        sighash_type: Option<json::SigHashType>,
    ) -> Result<json::SignRawTransactionResult> {
        let mut args = [
            tx.raw_hex().into(),
            into_json(privkeys)?,
            opt_into_json(prevtxs)?,
            opt_into_json(sighash_type)?,
        ];
        let defaults = [empty_arr(), null()];
        self.call("signrawtransactionwithkey", handle_defaults(&mut args, &defaults))
    }

    fn test_mempool_accept<R: RawTx>(
        &self,
        rawtxs: &[R],
    ) -> Result<Vec<json::TestMempoolAcceptResult>> {
        let hexes: Vec<Value> = rawtxs.to_vec().into_iter().map(|r| r.raw_hex().into()).collect();
        self.call("testmempoolaccept", &[hexes.into()])
    }

    fn stop(&self) -> Result<String> {
        self.call("stop", &[])
    }

    fn verify_message(
        &self,
        address: &Address,
        signature: &Signature,
        message: &str,
    ) -> Result<bool> {
        let args = [address.to_string().into(), signature.to_string().into(), into_json(message)?];
        self.call("verifymessage", &args)
    }

    /// Generate new address under own control
    fn get_new_address(&self, label: Option<&str>) -> Result<Address<NetworkUnchecked>> {
        self.call("getnewaddress", &[opt_into_json(label)?])
    }

    fn get_address_info(&self, address: &Address) -> Result<json::GetAddressInfoResult> {
        self.call("getaddressinfo", &[address.to_string().into()])
    }

    /// Mine `block_num` blocks and pay coinbase to `address`
    ///
    /// Returns hashes of the generated blocks
    fn generate_to_address(&self, block_num: u64, address: &Address) -> Result<Vec<BlockHash>> {
        self.call("generatetoaddress", &[block_num.into(), address.to_string().into()])
    }

    /// Mine up to block_num blocks immediately (before the RPC call returns)
    /// to an address in the wallet.
    fn generate(&self, block_num: u64, maxtries: Option<u64>) -> Result<Vec<BlockHash>> {
        self.call("generate", &[block_num.into(), opt_into_json(maxtries)?])
    }

    /// Mark a block as invalid by `block_hash`
    fn invalidate_block(&self, block_hash: &BlockHash) -> Result<()> {
        self.call("invalidateblock", &[into_json(block_hash)?])
    }

    /// Mark a block as valid by `block_hash`
    fn reconsider_block(&self, block_hash: &BlockHash) -> Result<()> {
        self.call("reconsiderblock", &[into_json(block_hash)?])
    }

    /// Get txids of all transactions in a memory pool
    fn get_raw_mempool(&self) -> Result<Vec<dashcore::Txid>> {
        self.call("getrawmempool", &[])
    }

    /// Get mempool data for given transaction
    fn get_mempool_entry(&self, txid: &dashcore::Txid) -> Result<json::GetMempoolEntryResult> {
        self.call("getmempoolentry", &[into_json(txid)?])
    }

    /// Get information about all known tips in the block tree, including the
    /// main chain as well as stale branches.
    fn get_chain_tips(&self) -> Result<json::GetChainTipsResult> {
        self.call("getchaintips", &[])
    }

    fn send_to_address(
        &self,
        address: &Address,
        amount: Amount,
        comment: Option<&str>,
        comment_to: Option<&str>,
        subtract_fee: Option<bool>,
        use_instant_send: Option<bool>,
        use_coinjoin: Option<bool>,
        confirmation_target: Option<u32>,
        estimate_mode: Option<json::EstimateMode>,
        avoid_reuse: Option<bool>,
    ) -> Result<dashcore::Txid> {
        let mut args = [
            address.to_string().into(),
            into_json(amount.to_dash())?,
            opt_into_json(comment)?,
            opt_into_json(comment_to)?,
            opt_into_json(subtract_fee)?,
            opt_into_json(use_instant_send)?,
            opt_into_json(use_coinjoin)?,
            opt_into_json(confirmation_target)?,
            opt_into_json(estimate_mode)?,
            opt_into_json(avoid_reuse)?,
        ];

        self.call(
            "sendtoaddress",
            handle_defaults(
                &mut args,
                &[
                    "".into(),
                    "".into(),
                    false.into(),
                    true.into(),
                    false.into(),
                    6.into(),
                    "UNSET".into(),
                    true.into(),
                ],
            ),
        )
    }

    /// Attempts to add a node to the addnode list.
    /// Nodes added using addnode (or -connect) are protected from DoS disconnection and are not required to be full nodes/support SegWit as other outbound peers are (though such peers will not be synced from).
    fn add_node(&self, addr: &str) -> Result<()> {
        self.call("addnode", &[into_json(&addr)?, into_json("add")?])
    }

    /// Attempts to remove a node from the addnode list.
    fn remove_node(&self, addr: &str) -> Result<()> {
        self.call("addnode", &[into_json(&addr)?, into_json("remove")?])
    }

    /// Attempts to connect to a node without permanently adding it to the addnode list.
    fn onetry_node(&self, addr: &str) -> Result<()> {
        self.call("addnode", &[into_json(&addr)?, into_json("onetry")?])
    }

    /// Immediately disconnects from the specified peer node.
    fn disconnect_node(&self, addr: &str) -> Result<()> {
        self.call("disconnectnode", &[into_json(&addr)?])
    }

    fn disconnect_node_by_id(&self, node_id: u32) -> Result<()> {
        self.call("disconnectnode", &[into_json("")?, into_json(node_id)?])
    }

    /// Returns information about the given added node, or all added nodes (note that onetry addnodes are not listed here)
    fn get_added_node_info(&self, node: Option<&str>) -> Result<Vec<json::GetAddedNodeInfoResult>> {
        if let Some(addr) = node {
            self.call("getaddednodeinfo", &[into_json(&addr)?])
        } else {
            self.call("getaddednodeinfo", &[])
        }
    }

    /// Return known addresses which can potentially be used to find new nodes in the network
    fn get_node_addresses(
        &self,
        count: Option<usize>,
    ) -> Result<Vec<json::GetNodeAddressesResult>> {
        let cnt = count.unwrap_or(1);
        self.call("getnodeaddresses", &[into_json(&cnt)?])
    }

    /// List all banned IPs/Subnets.
    fn list_banned(&self) -> Result<Vec<json::ListBannedResult>> {
        self.call("listbanned", &[])
    }

    /// Clear all banned IPs.
    fn clear_banned(&self) -> Result<()> {
        self.call("clearbanned", &[])
    }

    /// Attempts to add an IP/Subnet to the banned list.
    fn add_ban(&self, subnet: &str, bantime: u64, absolute: bool) -> Result<()> {
        self.call(
            "setban",
            &[into_json(&subnet)?, into_json("add")?, into_json(&bantime)?, into_json(&absolute)?],
        )
    }

    /// Attempts to remove an IP/Subnet from the banned list.
    fn remove_ban(&self, subnet: &str) -> Result<()> {
        self.call("setban", &[into_json(&subnet)?, into_json("remove")?])
    }

    /// Disable/enable all p2p network activity.
    fn set_network_active(&self, state: bool) -> Result<bool> {
        self.call("setnetworkactive", &[into_json(&state)?])
    }

    /// Returns data about each connected network node as an array of
    /// [`PeerInfo`][]
    ///
    /// [`PeerInfo`]: net/struct.PeerInfo.html
    fn get_peer_info(&self) -> Result<Vec<json::GetPeerInfoResult>> {
        self.call("getpeerinfo", &[])
    }

    /// Requests that a ping be sent to all other nodes, to measure ping
    /// time.
    ///
    /// Results provided in `getpeerinfo`, `pingtime` and `pingwait` fields
    /// are decimal seconds.
    ///
    /// Ping command is handled in queue with all other commands, so it
    /// measures processing backlog, not just network ping.
    fn ping(&self) -> Result<()> {
        self.call("ping", &[])
    }

    fn send_raw_transaction<R: RawTx>(&self, tx: R) -> Result<dashcore::Txid> {
        self.call("sendrawtransaction", &[tx.raw_hex().into()])
    }

    fn estimate_smart_fee(
        &self,
        conf_target: u16,
        estimate_mode: Option<json::EstimateMode>,
    ) -> Result<json::EstimateSmartFeeResult> {
        let mut args = [into_json(conf_target)?, opt_into_json(estimate_mode)?];
        self.call("estimatesmartfee", handle_defaults(&mut args, &[null()]))
    }

    /// Waits for a specific new block and returns useful info about it.
    /// Returns the current block on timeout or exit.
    ///
    /// # Arguments
    ///
    /// 1. `timeout`: Time in milliseconds to wait for a response. 0
    /// indicates no timeout.
    fn wait_for_new_block(&self, timeout: u64) -> Result<json::BlockRef> {
        self.call("waitfornewblock", &[into_json(timeout)?])
    }

    /// Waits for a specific new block and returns useful info about it.
    /// Returns the current block on timeout or exit.
    ///
    /// # Arguments
    ///
    /// 1. `blockhash`: Block hash to wait for.
    /// 2. `timeout`: Time in milliseconds to wait for a response. 0
    /// indicates no timeout.
    fn wait_for_block(
        &self,
        blockhash: &dashcore::BlockHash,
        timeout: u64,
    ) -> Result<json::BlockRef> {
        let args = [into_json(blockhash)?, into_json(timeout)?];
        self.call("waitforblock", &args)
    }

    fn wallet_create_funded_psbt(
        &self,
        inputs: &[json::CreateRawTransactionInput],
        outputs: &HashMap<String, Amount>,
        locktime: Option<i64>,
        options: Option<json::WalletCreateFundedPsbtOptions>,
        bip32derivs: Option<bool>,
    ) -> Result<json::WalletCreateFundedPsbtResult> {
        let outputs_converted = serde_json::Map::from_iter(
            outputs.iter().map(|(k, v)| (k.clone(), serde_json::Value::from(v.to_dash()))),
        );
        let mut args = [
            into_json(inputs)?,
            into_json(outputs_converted)?,
            opt_into_json(locktime)?,
            opt_into_json(options)?,
            opt_into_json(bip32derivs)?,
        ];
        self.call(
            "walletcreatefundedpsbt",
            handle_defaults(&mut args, &[0.into(), serde_json::Map::new().into(), false.into()]),
        )
    }

    fn wallet_process_psbt(
        &self,
        psbt: &str,
        sign: Option<bool>,
        sighash_type: Option<json::SigHashType>,
        bip32derivs: Option<bool>,
    ) -> Result<json::WalletProcessPsbtResult> {
        let mut args = [
            into_json(psbt)?,
            opt_into_json(sign)?,
            opt_into_json(sighash_type)?,
            opt_into_json(bip32derivs)?,
        ];
        let defaults = [
            true.into(),
            into_json(json::SigHashType::from(dashcore::EcdsaSighashType::All))?,
            true.into(),
        ];
        self.call("walletprocesspsbt", handle_defaults(&mut args, &defaults))
    }

    fn get_descriptor_info(&self, desc: &str) -> Result<json::GetDescriptorInfoResult> {
        self.call("getdescriptorinfo", &[desc.to_string().into()])
    }

    fn combine_psbt(&self, psbts: &[String]) -> Result<String> {
        self.call("combinepsbt", &[into_json(psbts)?])
    }

    fn finalize_psbt(&self, psbt: &str, extract: Option<bool>) -> Result<json::FinalizePsbtResult> {
        let mut args = [into_json(psbt)?, opt_into_json(extract)?];
        self.call("finalizepsbt", handle_defaults(&mut args, &[true.into()]))
    }

    fn derive_addresses(
        &self,
        descriptor: &str,
        range: Option<[u32; 2]>,
    ) -> Result<Vec<Address<NetworkUnchecked>>> {
        let mut args = [into_json(descriptor)?, opt_into_json(range)?];
        self.call("deriveaddresses", handle_defaults(&mut args, &[null()]))
    }

    fn rescan_blockchain(
        &self,
        start_from: Option<usize>,
        stop_height: Option<usize>,
    ) -> Result<(usize, Option<usize>)> {
        let mut args = [opt_into_json(start_from)?, opt_into_json(stop_height)?];

        #[derive(Deserialize)]
        struct Response {
            pub start_height: usize,
            pub stop_height: Option<usize>,
        }
        let res: Response =
            self.call("rescanblockchain", handle_defaults(&mut args, &[0.into(), null()]))?;
        Ok((res.start_height, res.stop_height))
    }

    /// Returns statistics about the unspent transaction output set.
    /// This call may take some time.
    fn get_tx_out_set_info(&self) -> Result<json::GetTxOutSetInfoResult> {
        self.call("gettxoutsetinfo", &[])
    }

    /// Returns information about network traffic, including bytes in, bytes out,
    /// and current time.
    fn get_net_totals(&self) -> Result<json::GetNetTotalsResult> {
        self.call("getnettotals", &[])
    }

    /// Returns the estimated network hashes per second based on the last n blocks.
    fn get_network_hash_ps(&self, nblocks: Option<u64>, height: Option<u64>) -> Result<f64> {
        let mut args = [opt_into_json(nblocks)?, opt_into_json(height)?];
        self.call("getnetworkhashps", handle_defaults(&mut args, &[null(), null()]))
    }

    /// Returns the total uptime of the server in seconds
    fn uptime(&self) -> Result<u64> {
        self.call("uptime", &[])
    }

    fn scan_tx_out_set_blocking(
        &self,
        descriptors: &[json::ScanTxOutRequest],
    ) -> Result<json::ScanTxOutResult> {
        self.call("scantxoutset", &["start".into(), into_json(descriptors)?])
    }

    // --------------------------- Masternode -------------------------------

    /// Returns information about the number of known masternodes
    fn get_masternode_count(&self) -> Result<json::GetMasternodeCountResult> {
        self.call("masternode", &["count".into()])
    }

    /// Returns a list of known masternodes
    fn get_masternode_list(
        &self,
        mode: Option<&str>,
        filter: Option<&str>,
    ) -> Result<HashMap<String, json::Masternode>> {
        let mut args = ["list".into(), into_json(mode)?, opt_into_json(filter)?];
        self.call::<HashMap<String, json::Masternode>>(
            "masternode",
            handle_defaults(&mut args, &["json".into(), null()]),
        )
    }

    /// Returns masternode compatible outputs
    fn get_masternode_outputs(&self) -> Result<HashMap<String, String>> {
        let mut args = ["outputs".into()];
        self.call::<HashMap<String, String>>("masternode", handle_defaults(&mut args, &[]))
    }

    /// Returns an array of deterministic masternodes and their payments for the specified block
    fn get_masternode_payments(
        &self,
        block_hash: Option<&str>,
        count: Option<&str>,
    ) -> Result<Vec<json::GetMasternodePaymentsResult>> {
        let mut args = ["payments".into(), opt_into_json(block_hash)?, opt_into_json(count)?];
        self.call::<Vec<json::GetMasternodePaymentsResult>>(
            "masternode",
            handle_defaults(&mut args, &[null(), null()]),
        )
    }

    /// Returns masternode status information
    fn get_masternode_status(&self) -> Result<json::MasternodeStatus> {
        self.call("masternode", &["status".into()])
    }

    /// Returns the list of masternode winners
    fn get_masternode_winners(
        &self,
        count: Option<&str>,
        filter: Option<&str>,
    ) -> Result<HashMap<String, String>> {
        let mut args = ["winners".into(), opt_into_json(count)?, opt_into_json(filter)?];
        self.call::<HashMap<String, String>>(
            "masternode",
            handle_defaults(&mut args, &["10".into(), null()]),
        )
    }

    // -------------------------- BLS -------------------------------

    /// Parses a BLS secret key and returns the secret/public key pair
    fn get_bls_fromsecret(&self, secret: &str) -> Result<json::BLS> {
        let mut args = ["fromsecret".into(), into_json(secret)?];
        self.call::<json::BLS>("bls", handle_defaults(&mut args, &[null()]))
    }

    /// Parses a BLS secret key and returns the secret/public key pair
    fn get_bls_generate(&self) -> Result<json::BLS> {
        self.call::<json::BLS>("bls", &["generate".into()])
    }

    // -------------------------- Quorum -------------------------------

    /// Returns a list of on-chain quorums
    fn get_quorum_list(
        &self,
        count: Option<u8>,
    ) -> Result<json::QuorumListResult<Vec<QuorumHash>>> {
        let mut args = ["list".into(), opt_into_json(count)?];
        self.call::<json::QuorumListResult<Vec<QuorumHash>>>(
            "quorum",
            handle_defaults(&mut args, &[1.into(), null()]),
        )
    }

    /// Returns an extended list of on-chain quorums
    fn get_quorum_listextended(
        &self,
        height: Option<u32>,
    ) -> Result<json::ExtendedQuorumListResult> {
        let mut args = ["listextended".into(), opt_into_json(height)?];
        self.call::<json::ExtendedQuorumListResult>("quorum", handle_defaults(&mut args, &[]))
    }

    /// Returns information about a specific quorum
    fn get_quorum_info(
        &self,
        llmq_type: QuorumType,
        quorum_hash: &QuorumHash,
        include_sk_share: Option<bool>,
    ) -> Result<json::QuorumInfoResult> {
        let mut args = [
            "info".into(),
            into_json(llmq_type as u8)?,
            into_json(quorum_hash)?,
            opt_into_json(include_sk_share)?,
        ];
        self.call::<json::QuorumInfoResult>("quorum", handle_defaults(&mut args, &[null()]))
    }

    /// Returns the status of the current DKG process
    fn get_quorum_dkgstatus(&self, detail_level: Option<u8>) -> Result<json::QuorumDKGStatus> {
        let mut args = ["dkgstatus".into(), opt_into_json(detail_level)?];
        self.call::<json::QuorumDKGStatus>(
            "quorum",
            handle_defaults(&mut args, &[0.into(), null()]),
        )
    }

    /// Requests threshold-signing for a message
    fn get_quorum_sign(
        &self,
        llmq_type: QuorumType,
        id: &str,
        msg_hash: &str,
        quorum_hash: Option<&str>,
        submit: Option<bool>,
    ) -> Result<json::QuorumSignResult> {
        let mut args = [
            "sign".into(),
            into_json(llmq_type)?,
            into_json(id)?,
            into_json(msg_hash)?,
            opt_into_json(quorum_hash)?,
            opt_into_json(submit)?,
        ];
        self.call::<json::QuorumSignResult>("quorum", handle_defaults(&mut args, &[null()]))
    }

    /// Returns the recovered signature for a previous threshold-signing message request
    fn get_quorum_getrecsig(
        &self,
        llmq_type: QuorumType,
        id: &str,
        msg_hash: &str,
    ) -> Result<json::QuorumSignature> {
        let mut args =
            ["getrecsig".into(), into_json(llmq_type)?, into_json(id)?, into_json(msg_hash)?];
        self.call::<json::QuorumSignature>("quorum", handle_defaults(&mut args, &[null()]))
    }

    /// Checks for a recovered signature for a previous threshold-signing message request
    fn get_quorum_hasrecsig(
        &self,
        llmq_type: QuorumType,
        id: &str,
        msg_hash: &str,
    ) -> Result<bool> {
        let mut args =
            ["hasrecsig".into(), into_json(llmq_type)?, into_json(id)?, into_json(msg_hash)?];
        self.call::<bool>("quorum", handle_defaults(&mut args, &[null()]))
    }

    /// Checks if there is a conflict for a threshold-signing message request
    fn get_quorum_isconflicting(
        &self,
        llmq_type: QuorumType,
        id: &str,
        msg_hash: &str,
    ) -> Result<bool> {
        let mut args =
            ["isconflicting".into(), into_json(llmq_type)?, into_json(id)?, into_json(msg_hash)?];
        self.call::<bool>("quorum", handle_defaults(&mut args, &[null()]))
    }

    /// Checks which quorums the given masternode is a member of
    fn get_quorum_memberof(
        &self,
        pro_tx_hash: &ProTxHash,
        scan_quorums_count: Option<u8>,
    ) -> Result<json::QuorumMemberOfResult> {
        let mut args =
            ["memberof".into(), into_json(pro_tx_hash)?, opt_into_json(scan_quorums_count)?];
        self.call::<json::QuorumMemberOfResult>("quorum", handle_defaults(&mut args, &[null()]))
    }

    /// Returns quorum rotation information
    fn get_quorum_rotationinfo(
        &self,
        block_request_hash: &BlockHash,
        extra_share: Option<bool>,
        base_block_hash: Option<&str>,
    ) -> Result<json::QuorumRotationInfo> {
        let mut args = [
            "rotationinfo".into(),
            into_json(block_request_hash)?,
            opt_into_json(extra_share)?,
            opt_into_json(base_block_hash)?,
        ];
        self.call::<json::QuorumRotationInfo>(
            "quorum",
            handle_defaults(&mut args, &[false.into(), "".into(), null()]),
        )
    }

    /// Returns information about the quorum that would/should sign a request
    fn get_quorum_selectquorum(
        &self,
        llmq_type: QuorumType,
        id: &str,
    ) -> Result<json::SelectQuorumResult> {
        let mut args = ["selectquorum".into(), into_json(llmq_type)?, into_json(id)?];
        self.call::<json::SelectQuorumResult>("quorum", handle_defaults(&mut args, &[null()]))
    }

    /// Tests if a quorum signature is valid for a request id and a message hash
    fn get_quorum_verify(
        &self,
        llmq_type: QuorumType,
        id: &str,
        msg_hash: &str,
        signature: &str,
        quorum_hash: Option<QuorumHash>,
        sign_height: Option<u32>,
    ) -> Result<bool> {
        let mut args = [
            "verify".into(),
            into_json(llmq_type)?,
            into_json(id)?,
            into_json(msg_hash)?,
            into_json(signature)?,
            opt_into_json(quorum_hash)?,
            opt_into_json(sign_height)?,
        ];
        self.call::<bool>("quorum", handle_defaults(&mut args, &[null()]))
    }

    // --------------------------- ProTx -------------------------------

    /// Returns a diff and a proof between two masternode list
    fn get_protx_diff(&self, base_block: u32, block: u32) -> Result<json::MasternodeDiff> {
        let mut args = ["diff".into(), into_json(base_block)?, into_json(block)?];
        self.call::<json::MasternodeDiff>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Returns a full deterministic masternode list diff between two heigts
    fn get_protx_listdiff(&self, base_block: u32, block: u32) -> Result<json::MasternodeListDiff> {
        let mut args = ["listdiff".into(), into_json(base_block)?, into_json(block)?];
        self.call::<json::MasternodeListDiff>(
            "protx",
            handle_defaults(&mut args, &[null(), null()]),
        )
    }

    /// Returns a returns detailed information about a deterministic masternode
    fn get_protx_info(&self, protx_hash: &ProTxHash, block_hash: Option<&BlockHash>) -> Result<json::ProTxInfo> {
        let mut args = ["info".into(), into_json(protx_hash.to_hex())?, opt_into_json(block_hash)?];

        self.call::<json::ProTxInfo>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Returns a list of provider transactions
    fn get_protx_list(
        &self,
        protx_type: Option<ProTxListType>,
        detailed: Option<bool>,
        height: Option<u32>,
    ) -> Result<json::ProTxList> {
        let mut args = [
            "list".into(),
            opt_into_json(protx_type)?,
            opt_into_json(detailed)?,
            opt_into_json(height)?,
        ];
        self.call::<json::ProTxList>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Creates a ProRegTx referencing an existing collateral and and sends it to the network
    fn get_protx_register(
        &self,
        collateral_hash: &str,
        collateral_index: u32,
        ip_and_port: &str,
        owner_address: &str,
        operator_pub_key: &str,
        voting_address: &str,
        operator_reward: f32,
        payout_address: &str,
        fee_source_address: Option<&str>,
        submit: Option<bool>,
    ) -> Result<ProTxHash> {
        let mut args = [
            "register".into(),
            into_json(collateral_hash)?,
            into_json(collateral_index)?,
            into_json(ip_and_port)?,
            into_json(owner_address)?,
            into_json(operator_pub_key)?,
            into_json(voting_address)?,
            into_json(operator_reward)?,
            into_json(payout_address)?,
            opt_into_json(fee_source_address)?,
            opt_into_json(submit)?,
        ];
        self.call::<ProTxHash>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Creates and funds a ProRegTx with the 1,000 DASH necessary for a masternode and then sends it to the network
    fn get_protx_register_fund(
        &self,
        collateral_address: &str,
        ip_and_port: &str,
        owner_address: &str,
        operator_pub_key: &str,
        voting_address: &str,
        operator_reward: f32,
        payout_address: &str,
        fund_address: Option<&str>,
        submit: Option<bool>,
    ) -> Result<ProTxHash> {
        let mut args = [
            "register_fund".into(),
            into_json(collateral_address)?,
            into_json(ip_and_port)?,
            into_json(owner_address)?,
            into_json(operator_pub_key)?,
            into_json(voting_address)?,
            into_json(operator_reward)?,
            into_json(payout_address)?,
            opt_into_json(fund_address)?,
            opt_into_json(submit)?,
        ];
        self.call::<ProTxHash>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Creates an unsigned ProTx and a message that must be signed externally
    fn get_protx_register_prepare(
        &self,
        collateral_hash: &str,
        collateral_index: u32,
        ip_and_port: &str,
        owner_address: dashcore::Address,
        operator_pub_key: &str,
        voting_address: dashcore::Address,
        operator_reward: f32,
        payout_address: dashcore::Address,
        fee_source_address: Option<dashcore::Address>,
    ) -> Result<json::ProTxRegPrepare> {
        let mut args = [
            "register_prepare".into(),
            into_json(collateral_hash)?,
            into_json(collateral_index)?,
            into_json(ip_and_port)?,
            into_json(owner_address)?,
            into_json(operator_pub_key)?,
            into_json(voting_address)?,
            into_json(operator_reward)?,
            into_json(payout_address)?,
            opt_into_json(fee_source_address)?,
        ];
        self.call::<json::ProTxRegPrepare>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Combines the unsigned ProTx and a signature of the signMessage, signs all inputs which were added to
    /// cover fees and submits the resulting transaction to the network
    fn get_protx_register_submit(&self, tx: &str, sig: &str) -> Result<ProTxHash> {
        let mut args = ["register_submit".into(), into_json(tx)?, into_json(sig)?];
        self.call::<ProTxHash>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Creates and sends a ProUpRevTx to the network
    fn get_protx_revoke(
        &self,
        pro_tx_hash: &str,
        operator_pub_key: &str,
        reason: json::ProTxRevokeReason,
        fee_source_address: Option<dashcore::Address>,
    ) -> Result<ProTxHash> {
        let mut args = [
            "revoke".into(),
            into_json(pro_tx_hash)?,
            into_json(operator_pub_key)?,
            into_json(reason as u8)?,
            opt_into_json(fee_source_address)?,
        ];
        self.call::<ProTxHash>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Creates and sends a ProUpRegTx to the network
    fn get_protx_update_registrar(
        &self,
        pro_tx_hash: &str,
        operator_pub_key: &str,
        voting_address: dashcore::Address,
        payout_address: dashcore::Address,
        fee_source_address: Option<dashcore::Address>,
    ) -> Result<ProTxHash> {
        let mut args = [
            "update_registrar".into(),
            into_json(pro_tx_hash)?,
            into_json(operator_pub_key)?,
            into_json(voting_address)?,
            into_json(payout_address)?,
            opt_into_json(fee_source_address)?,
        ];
        self.call::<ProTxHash>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Creates and sends a ProUpServTx to the network
    fn get_protx_update_service(
        &self,
        pro_tx_hash: &str,
        ip_and_port: &str,
        operator_key: &str,
        operator_payout_address: Option<dashcore::Address>,
        fee_source_address: Option<dashcore::Address>,
    ) -> Result<ProTxHash> {
        let mut args = [
            "update_service".into(),
            into_json(pro_tx_hash)?,
            into_json(ip_and_port)?,
            into_json(operator_key)?,
            opt_into_json(operator_payout_address)?,
            opt_into_json(fee_source_address)?,
        ];
        self.call::<ProTxHash>("protx", handle_defaults(&mut args, &[null()]))
    }

    /// Tests if a quorum signature is valid for a ChainLock
    fn get_verifychainlock(
        &self,
        block_hash: &str,
        signature: &str,
        block_height: Option<u32>,
    ) -> Result<bool> {
        let mut args =
            [into_json(block_hash)?, into_json(signature)?, opt_into_json(block_height)?];
        self.call::<bool>("verifychainlock", handle_defaults(&mut args, &[null()]))
    }

    /// Submits a chain lock if needed
    /// This will return an error only if the chain lock signature is invalid
    /// If the returned height is less than the given chain lock height this means that the chain lock was accepted but we did not yet have the block
    /// If the returned height is equal to the chain lock height given this means that we are at the height of the chain lock
    /// If the returned height is higher that the given chain lock this means that we ignored the chain lock because core had something better.
    fn submit_chain_lock(&self, chain_lock: &ChainLock) -> Result<u32> {
        let mut args = [
            into_json(hex::encode(chain_lock.block_hash))?,
            into_json(hex::encode(chain_lock.signature.as_bytes()))?,
            into_json(chain_lock.block_height)?,
        ];
        self.call::<u32>("submitchainlock", handle_defaults(&mut args, &[null()]))
    }

    /// Tests  if a quorum signature is valid for an InstantSend Lock
    fn get_verifyislock(
        &self,
        id: &str,
        tx_id: &str,
        signature: &str,
        max_height: Option<u32>,
    ) -> Result<bool> {
        let mut args =
            [into_json(id)?, into_json(tx_id)?, into_json(signature)?, opt_into_json(max_height)?];
        self.call::<bool>("verifyislock", handle_defaults(&mut args, &[null()]))
    }

    /// Returns masternode sync status
    fn mnsync_status(&self) -> Result<json::MnSyncStatus> {
        self.call::<json::MnSyncStatus>("mnsync", &["status".into()])
    }
}

/// Client implements a JSON-RPC client for the Dash Core daemon or compatible APIs.
pub struct Client {
    client: jsonrpc::client::Client,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "dashcore_rpc::Client({:?})", self.client)
    }
}

impl Client {
    /// Creates a client to a dashd JSON-RPC server.
    ///
    /// Can only return [Err] when using cookie authentication.
    pub fn new(url: &str, auth: Auth) -> Result<Self> {
        let (user, pass) = auth.get_user_pass()?;
        jsonrpc::client::Client::simple_http(url, user, pass)
            .map(|client| Client {
                client,
            })
            .map_err(|e| super::error::Error::JsonRpc(e.into()))
    }

    /// Create a new Client using the given [jsonrpc::Client].
    pub fn from_jsonrpc(client: jsonrpc::client::Client) -> Client {
        Client {
            client,
        }
    }

    /// Get the underlying JSONRPC client.
    pub fn get_jsonrpc_client(&self) -> &jsonrpc::client::Client {
        &self.client
    }
}

impl RpcApi for Client {
    /// Call an `cmd` rpc with given `args` list
    fn call<T: for<'a> serde::de::Deserialize<'a>>(&self, cmd: &str, args: &[Value]) -> Result<T> {
        let raw_args: Vec<_> = args
            .iter()
            .map(|a| {
                let json_string = serde_json::to_string(a)?;
                serde_json::value::RawValue::from_string(json_string) // we can't use to_raw_value here due to compat with Rust 1.29
            })
            .map(|a| a.map_err(|e| Error::Json(e)))
            .collect::<Result<Vec<_>>>()?;
        let req = self.client.build_request(&cmd, &raw_args);
        if log_enabled!(Debug) {
            debug!(target: "dashcore_rpc", "JSON-RPC request: {} {}", cmd, serde_json::Value::from(args));
        }

        let resp = self.client.send_request(req).map_err(Error::from);
        log_response(cmd, &resp);
        Ok(resp?.result()?)
    }
}

fn log_response(cmd: &str, resp: &Result<jsonrpc::Response>) {
    if log_enabled!(Warn) || log_enabled!(Debug) || log_enabled!(Trace) {
        match resp {
            Err(ref e) => {
                if log_enabled!(Debug) {
                    debug!(target: "dashcore_rpc", "JSON-RPC failed parsing reply of {}: {:?}", cmd, e);
                }
            }
            Ok(ref resp) => {
                if let Some(ref e) = resp.error {
                    if log_enabled!(Debug) {
                        debug!(target: "dashcore_rpc", "JSON-RPC error for {}: {:?}", cmd, e);
                    }
                } else if log_enabled!(Trace) {
                    // we can't use to_raw_value here due to compat with Rust 1.29
                    let def = serde_json::value::RawValue::from_string(
                        serde_json::Value::Null.to_string(),
                    )
                    .unwrap();
                    let result = resp.result.as_ref().unwrap_or(&def);
                    trace!(target: "dashcore_rpc", "JSON-RPC response for {}: {}", cmd, result);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_raw_tx() {
        use dashcore::consensus::encode;
        let client = Client::new("http://localhost/".into(), Auth::None).unwrap();
        let tx: Transaction = encode::deserialize(&Vec::<u8>::from_hex("0200000001586bd02815cf5faabfec986a4e50d25dbee089bd2758621e61c5fab06c334af0000000006b483045022100e85425f6d7c589972ee061413bcf08dc8c8e589ce37b217535a42af924f0e4d602205c9ba9cb14ef15513c9d946fa1c4b797883e748e8c32171bdf6166583946e35c012103dae30a4d7870cd87b45dd53e6012f71318fdd059c1c2623b8cc73f8af287bb2dfeffffff021dc4260c010000001976a914f602e88b2b5901d8aab15ebe4a97cf92ec6e03b388ac00e1f505000000001976a914687ffeffe8cf4e4c038da46a9b1d37db385a472d88acfd211500").unwrap()).unwrap();

        assert!(client.send_raw_transaction(&tx).is_err());
        assert!(client.send_raw_transaction(&encode::serialize(&tx)).is_err());
        assert!(client.send_raw_transaction("deadbeef").is_err());
        assert!(client.send_raw_transaction("deadbeef".to_owned()).is_err());
    }

    fn test_handle_defaults_inner() -> Result<()> {
        {
            let mut args = [into_json(0)?, null(), null()];
            let defaults = [into_json(1)?, into_json(2)?];
            let res = [into_json(0)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [into_json(0)?, into_json(1)?, null()];
            let defaults = [into_json(2)?];
            let res = [into_json(0)?, into_json(1)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [into_json(0)?, null(), into_json(5)?];
            let defaults = [into_json(2)?, into_json(3)?];
            let res = [into_json(0)?, into_json(2)?, into_json(5)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [into_json(0)?, null(), into_json(5)?, null()];
            let defaults = [into_json(2)?, into_json(3)?, into_json(4)?];
            let res = [into_json(0)?, into_json(2)?, into_json(5)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [null(), null()];
            let defaults = [into_json(2)?, into_json(3)?];
            let res: [serde_json::Value; 0] = [];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [null(), into_json(1)?];
            let defaults = [];
            let res = [null(), into_json(1)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [];
            let defaults = [];
            let res: [serde_json::Value; 0] = [];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [into_json(0)?];
            let defaults = [into_json(2)?];
            let res = [into_json(0)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        Ok(())
    }

    #[test]
    fn test_handle_defaults() {
        test_handle_defaults_inner().unwrap();
    }
}
