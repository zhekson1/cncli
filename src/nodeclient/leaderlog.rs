use std::fs::File;
use std::io::{BufReader, Error};
use std::ops::Mul;
use std::path::PathBuf;

use blake2b_simd::Params;
use byteorder::{ByteOrder, NetworkEndian};
use chrono::{Duration, Local, NaiveDateTime, TimeZone};
use fixed::types::I30F34;
use log::{debug, trace};
use rug::{Integer, Rational};
use rug::integer::Order;
use rug::ops::Pow;
use rusqlite::{Connection, NO_PARAMS};
use serde::{Deserialize, Serialize};
use serde::export::fmt::Display;
use serde_aux::prelude::deserialize_number_from_string;

use crate::nodeclient::leaderlog::deserialize::cbor_hex;
use crate::nodeclient::leaderlog::ledgerstate::calculate_ledger_state_sigma_and_d;
use crate::nodeclient::leaderlog::libsodium::{sodium_crypto_vrf_proof_to_hash, sodium_crypto_vrf_prove};
use crate::nodeclient::LedgerSet;

mod ledgerstate;
mod libsodium;
mod deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ByronGenesis {
    start_time: i64,
    protocol_consts: ProtocolConsts,
    block_version_data: BlockVersionData,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProtocolConsts {
    k: i64
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockVersionData {
    #[serde(deserialize_with = "deserialize_number_from_string")]
    slot_duration: i64
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShelleyGenesis {
    active_slots_coeff: f64,
    network_magic: u32,
    slot_length: i64,
    epoch_length: i64,
}

#[derive(Debug, Deserialize)]
struct VrfSkey {
    #[serde(deserialize_with = "cbor_hex")]
    #[serde(rename(deserialize = "cborHex"))]
    key: Vec<u8>
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaderLog {
    status: String,
    epoch: i64,
    epoch_nonce: String,
    pool_id: String,
    sigma: f64,
    d: f64,
    assigned_slots: Vec<Slot>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Slot {
    no: i64,
    slot: i64,
    slot_in_epoch: i64,
    at: String,
}

fn read_byron_genesis(byron_genesis: &PathBuf) -> Result<ByronGenesis, Error> {
    let buf = BufReader::new(File::open(byron_genesis)?);
    Ok(serde_json::from_reader(buf)?)
}

fn read_shelley_genesis(shelley_genesis: &PathBuf) -> Result<ShelleyGenesis, Error> {
    let buf = BufReader::new(File::open(shelley_genesis)?);
    Ok(serde_json::from_reader(buf)?)
}

fn read_vrf_skey(vrf_skey_path: &PathBuf) -> Result<VrfSkey, Error> {
    let buf = BufReader::new(File::open(vrf_skey_path)?);
    Ok(serde_json::from_reader(buf)?)
}

fn get_tip_slot_number(db: &Connection) -> Result<i64, rusqlite::Error> {
    Ok(
        db.query_row("SELECT MAX(slot_number) FROM chain", NO_PARAMS, |row| Ok(row.get(0)?))?
    )
}

fn get_eta_v_before_slot(db: &Connection, slot_number: i64) -> Result<String, rusqlite::Error> {
    Ok(
        db.query_row("SELECT eta_v FROM chain WHERE orphaned = 0 AND slot_number < ?1 AND ?1 - slot_number < 120 ORDER BY slot_number DESC LIMIT 1", &[&slot_number], |row| {
            Ok(row.get(0)?)
        })?
    )
}

fn get_prev_hash_before_slot(db: &Connection, slot_number: i64) -> Result<String, rusqlite::Error> {
    Ok(
        db.query_row("SELECT prev_hash FROM chain WHERE orphaned = 0 AND slot_number < ?1 AND ?1 - slot_number < 120 ORDER BY slot_number DESC LIMIT 1", &[&slot_number], |row| {
            Ok(row.get(0)?)
        })?
    )
}


fn get_first_slot_of_epoch(byron: &ByronGenesis, shelley: &ShelleyGenesis, current_slot: i64) -> (i64, i64) {
    let shelley_transition_epoch: i64 = if shelley.network_magic == 764824073 {
        // mainnet
        208
    } else {
        // testnet
        74
    };
    let byron_epoch_length = 10 * byron.protocol_consts.k;
    let byron_slots = byron_epoch_length * shelley_transition_epoch;
    let shelley_slots = current_slot - byron_slots;
    let shelley_slot_in_epoch = shelley_slots % shelley.epoch_length;
    let first_slot_of_epoch = current_slot - shelley_slot_in_epoch;
    let epoch = (shelley_slots / shelley.epoch_length) + shelley_transition_epoch;

    (epoch, first_slot_of_epoch)
}

fn slot_to_timestamp(byron: &ByronGenesis, shelley: &ShelleyGenesis, slot: i64) -> String {
    let shelley_transition_epoch: i64 = if shelley.network_magic == 764824073 {
        // mainnet
        208
    } else {
        // testnet
        74
    };

    let network_start_time = NaiveDateTime::from_timestamp(byron.start_time, 0);
    let byron_epoch_length = 10 * byron.protocol_consts.k;
    let byron_slots = byron_epoch_length * shelley_transition_epoch;
    let shelley_slots = slot - byron_slots;

    let byron_secs = (byron.block_version_data.slot_duration * byron_slots) / 1000;
    let shelley_secs = shelley_slots * shelley.slot_length;

    let slot_time = network_start_time + Duration::seconds(byron_secs) + Duration::seconds(shelley_secs);
    Local.from_utc_datetime(&slot_time).to_rfc3339()
}

fn is_overlay_slot(first_slot_of_epoch: &i64, current_slot: &i64, d: &I30F34) -> bool {
    let diff_slot = (current_slot - first_slot_of_epoch).abs();
    d.mul(diff_slot).ceil() < d.mul(diff_slot + 1).ceil()
}

//
// The universal constant nonce. The blake2b hash of the 8 byte long value of 1
// 12dd0a6a7d0e222a97926da03adb5a7768d31cc7c5c2bd6828e14a7d25fa3a60
// Sometimes called seedL in the haskell code
//
const UC_NONCE: [u8; 32] = [0x12, 0xdd, 0x0a, 0x6a, 0x7d, 0x0e, 0x22, 0x2a, 0x97, 0x92, 0x6d, 0xa0, 0x3a, 0xdb, 0x5a, 0x77, 0x68, 0xd3, 0x1c, 0xc7, 0xc5, 0xc2, 0xbd, 0x68, 0x28, 0xe1, 0x4a, 0x7d, 0x25, 0xfa, 0x3a, 0x60];

fn mk_seed(slot: i64, eta0: &Vec<u8>) -> Vec<u8> {
    trace!("mk_seed() start slot {}", slot);
    let mut concat = [0u8; 8 + 32];
    NetworkEndian::write_i64(&mut concat, slot);
    concat[8..].copy_from_slice(eta0);
    trace!("concat: {}", hex::encode(&concat));

    let slot_to_seed = Params::new().hash_length(32).to_state().update(&concat).finalize().as_bytes().to_owned();

    UC_NONCE.iter().enumerate().map(|(i, byte)| byte ^ slot_to_seed[i]).collect()
}

fn vrf_eval_certified(seed: Vec<u8>, pool_vrf_skey: &Vec<u8>) -> Result<Integer, String> {
    let certified_proof: Vec<u8> = sodium_crypto_vrf_prove(pool_vrf_skey, seed)?;
    let certified_proof_hash: Vec<u8> = sodium_crypto_vrf_proof_to_hash(certified_proof)?;
    Ok(Integer::from_digits(&*certified_proof_hash, Order::MsfBe))
}

enum TaylorCmp {
    Above,
    Below,
    MaxReached,
}

fn taylor_exp_cmp(bound_x: i32, cmp: I30F34, x: I30F34) -> TaylorCmp {
    let max_n = 1000;
    let mut divisor = 1;
    let mut acc: I30F34 = I30F34::from_num(1);
    let mut err = x;
    let mut error_term = err * I30F34::from_num(bound_x);
    let mut next_x: I30F34;
    for _n in 0..max_n {
        if cmp >= acc + error_term {
            return TaylorCmp::Above;
        } else if cmp < acc - error_term {
            return TaylorCmp::Below;
        } else {
            divisor += 1;
            next_x = err;
            err = (err * x) / divisor;
            error_term = err * I30F34::from_num(bound_x);
            acc += next_x;
        }
    }

    TaylorCmp::MaxReached
}

// Determine if our pool is a slot leader for this given slot
// @param slot The slot to check
// @param f The activeSlotsCoeff value from protocol params
// @param sigma The controlled stake proportion for the pool
// @param eta0 The epoch nonce value
// @param pool_vrf_skey The vrf signing key for the pool
fn is_slot_leader(slot: i64, f: &f64, sigma: &Rational, eta0: &Vec<u8>, pool_vrf_skey: &Vec<u8>) -> Result<bool, String> {
    let seed = mk_seed(slot, eta0);
    trace!("seed: {}", hex::encode(&seed));
    let cert_nat = vrf_eval_certified(seed, pool_vrf_skey)?;
    trace!("cert_nat: {}", &cert_nat);
    let cert_nat_max = Integer::from(2).pow(512);
    let denominator = &cert_nat_max - cert_nat;
    let recip_q: I30F34 = I30F34::from_num(Rational::from((cert_nat_max, denominator)).to_f64());
    trace!("recip_q: {}", &recip_q);
    let c = (1f64 - f).ln();
    trace!("c: {}", &c);
    let x: I30F34 = I30F34::from_num(-sigma.to_f64() * c);
    trace!("x: {}", &x);

    match taylor_exp_cmp(3, recip_q, x) {
        TaylorCmp::Above => { Ok(false) }
        TaylorCmp::Below => { Ok(true) }
        TaylorCmp::MaxReached => { Ok(false) }
    }
}

pub(crate) fn calculate_leader_logs(db_path: &PathBuf, byron_genesis: &PathBuf, shelley_genesis: &PathBuf, ledger_state: &PathBuf, ledger_set: &LedgerSet, pool_id: &String, pool_vrf_skey_path: &PathBuf) {
    if !db_path.exists() {
        handle_error("database not found!");
        return;
    }
    let db = Connection::open(db_path).unwrap();

    match read_byron_genesis(byron_genesis) {
        Ok(byron) => {
            debug!("{:?}", byron);
            match read_shelley_genesis(shelley_genesis) {
                Ok(shelley) => {
                    debug!("{:?}", shelley);
                    match read_vrf_skey(pool_vrf_skey_path) {
                        Ok(pool_vrf_skey) => {
                            match calculate_ledger_state_sigma_and_d(ledger_state, ledger_set, pool_id) {
                                Ok((sigma, decentralization_param)) => {
                                    debug!("sigma: {:?}", sigma);
                                    debug!("decentralization_param: {:?}", decentralization_param);
                                    let tip_slot_number = get_tip_slot_number(&db).unwrap();
                                    debug!("tip_slot_number: {}", tip_slot_number);
                                    // pretend we're on a different slot number if we want to calculate past or future epochs.
                                    let additional_slots: i64 = match ledger_set {
                                        LedgerSet::Mark => { shelley.epoch_length }
                                        LedgerSet::Set => { 0 }
                                        LedgerSet::Go => { -shelley.epoch_length }
                                    };
                                    let (epoch, first_slot_of_epoch) = get_first_slot_of_epoch(&byron, &shelley, tip_slot_number + additional_slots);
                                    debug!("epoch: {}", epoch);
                                    let first_slot_of_prev_epoch = first_slot_of_epoch - shelley.epoch_length;
                                    debug!("first_slot_of_epoch: {}", first_slot_of_epoch);
                                    debug!("first_slot_of_prev_epoch: {}", first_slot_of_prev_epoch);
                                    let stability_window: i64 = ((3 * byron.protocol_consts.k) as f64 / shelley.active_slots_coeff).ceil() as i64;
                                    let stability_window_start = first_slot_of_epoch - stability_window;
                                    debug!("stability_window: {}", stability_window);
                                    debug!("stability_window_start: {}", stability_window_start);
                                    match get_eta_v_before_slot(&db, stability_window_start) {
                                        Ok(nc) => {
                                            debug!("nc: {}", nc);
                                            match get_prev_hash_before_slot(&db, first_slot_of_prev_epoch) {
                                                Ok(nh) => {
                                                    debug!("nh: {}", nh);
                                                    let mut nc_nh = String::new();
                                                    nc_nh.push_str(&*nc);
                                                    nc_nh.push_str(&*nh);
                                                    let epoch_nonce = Params::new().hash_length(32).to_state().update(&*hex::decode(nc_nh).unwrap()).finalize().as_bytes().to_owned();
                                                    debug!("epoch_nonce: {}", hex::encode(&epoch_nonce));

                                                    let mut leader_log = LeaderLog {
                                                        status: "ok".to_string(),
                                                        epoch,
                                                        epoch_nonce: hex::encode(&epoch_nonce),
                                                        pool_id: pool_id.clone(),
                                                        sigma: sigma.to_f64(),
                                                        d: decentralization_param.to_string().parse().unwrap(),
                                                        assigned_slots: vec![],
                                                    };

                                                    let mut no = 0i64;
                                                    for slot_in_epoch in 0..shelley.epoch_length {
                                                        let slot = first_slot_of_epoch + slot_in_epoch;
                                                        if is_overlay_slot(&first_slot_of_epoch, &slot, &decentralization_param) {
                                                            // Nobody is allowed to make a block in tis slot except maybe BFT nodes.
                                                            continue;
                                                        }

                                                        match is_slot_leader(slot, &shelley.active_slots_coeff, &sigma, &epoch_nonce, &pool_vrf_skey.key) {
                                                            Ok(is_leader) => {
                                                                if is_leader {
                                                                    no += 1;
                                                                    leader_log.assigned_slots.push(
                                                                        Slot {
                                                                            no,
                                                                            slot,
                                                                            slot_in_epoch: slot - first_slot_of_epoch,
                                                                            at: slot_to_timestamp(&byron, &shelley, slot),
                                                                        }
                                                                    );
                                                                }
                                                            }
                                                            Err(error) => { handle_error(error) }
                                                        }
                                                    }
                                                    match serde_json::to_string_pretty(&leader_log) {
                                                        Ok(leader_log_json) => {
                                                            println!("{}", leader_log_json);
                                                        }
                                                        Err(error) => { handle_error(error) }
                                                    }
                                                }
                                                Err(error) => { handle_error(error) }
                                            }
                                        }
                                        Err(error) => { handle_error(error) }
                                    };
                                }
                                Err(error) => { handle_error(error) }
                            }
                        }
                        Err(error) => { handle_error(error) }
                    }
                }
                Err(error) => { handle_error(error) }
            }
        }
        Err(error) => { handle_error(error) }
    }

    match db.close() {
        Err(error) => {
            handle_error(format!("db close error: {}", error.1));
        }
        _ => {}
    }
}

pub fn handle_error<T: Display>(error_message: T) {
    println!("{{\n\
            \x20\"status\": \"error\",\n\
            \x20\"errorMessage\": \"{}\"\n\
            }}", error_message);
}