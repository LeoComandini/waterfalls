use crate::{
    fetch::Client,
    server::{Error, State},
    store::Store,
    TxSeen, WaterfallRequest, WaterfallResponse,
};
use age::x25519::Identity;
use elements::{
    encode::{serialize, serialize_hex, Decodable},
    BlockHash, Transaction, Txid,
};
use elements_miniscript::DescriptorPublicKey;
use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    header::{CACHE_CONTROL, CONTENT_TYPE},
    Method, Request, Response, StatusCode,
};
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    sync::Arc,
    time::Instant,
};
use tokio::sync::Mutex;

use super::encryption;

const GAP_LIMIT: u32 = 20;
const MAX_BATCH: u32 = 500; // TODO reduce to 50 and implement paging
const MAX_ADDRESSES: u32 = GAP_LIMIT * MAX_BATCH;

// needed endpoint to make this self-contained for testing, in prod they should probably be never hit cause proxied by nginx
// https://waterfalls.liquidwebwallet.org/liquidtestnet/api/blocks/tip/hash
// https://waterfalls.liquidwebwallet.org/liquidtestnet/api/block/bddf520b05c7552dca87289a035043a5c434133b3d1bb07b255fb1a30592b2d4/header
// https://waterfalls.liquidwebwallet.org/liquidtestnet/api/tx/3fb1f808534a881cc16c10745a2b861c7b33e13cfe2f5bf3fc872fd943d0bfca/raw
// https://waterfalls.liquidwebwallet.org/liquidtestnet/api/block-height/1424507
pub async fn route(
    state: &Arc<State>,
    client: &Arc<Mutex<Client>>,
    req: Request<Incoming>,
    is_testnet: bool,
) -> Result<Response<Full<Bytes>>, Error> {
    log::debug!("---> {req:?}");
    let res = match (req.method(), req.uri().path(), req.uri().query()) {
        (&Method::GET, "/v1/server_recipient", None) => {
            str_resp(state.key.to_public().to_string(), StatusCode::OK)
        }
        (&Method::GET, "/v1/waterfalls", Some(query)) => {
            let inputs = parse_query(query, &state.key)?;
            handle_waterfalls_req(state, &inputs, is_testnet).await
        }
        (&Method::GET, "/blocks/tip/hash", None) => {
            let block_hash = state.tip_hash().await;
            block_hash_resp(block_hash)
        }
        (&Method::POST, "/tx", None) => {
            let whole_body = req
                .collect()
                .await
                .map_err(|e| Error::String(e.to_string()))?
                .to_bytes();
            let result = std::str::from_utf8(&whole_body)
                .map_err(|e| Error::String(e.to_string()))?
                .to_string();
            let tx_bytes = hex::decode(result).map_err(|e| Error::String(e.to_string()))?;
            let tx = Transaction::consensus_decode(&tx_bytes[..])
                .map_err(|e| Error::String(e.to_string()))?;
            client
                .lock()
                .await
                .broadcast(&tx)
                .await
                .map_err(|e| Error::String(e.to_string()))?;
            str_resp(tx.txid().to_string(), StatusCode::OK)
        }
        (&Method::GET, path, None) => {
            let mut s = path.split('/');
            match (s.next(), s.next(), s.next(), s.next(), s.next()) {
                (Some(""), Some("block-height"), Some(v), None, None) => {
                    let height: u32 = v.parse().map_err(|_| Error::CannotParseHeight)?;
                    let block_hash = state.block_hash(height).await;
                    block_hash_resp(block_hash)
                }
                (Some(""), Some("tx"), Some(v), Some("raw"), None) => {
                    let txid = Txid::from_str(v).map_err(|_| Error::InvalidTxid)?;
                    let tx = client
                        .lock()
                        .await
                        .tx(txid)
                        .await
                        .map_err(|_| Error::CannotFindTx)?;
                    let result = serialize(&tx);
                    any_resp(
                        result,
                        StatusCode::OK,
                        Some("application/octet-stream"),
                        Some(157784630),
                    )
                }
                (Some(""), Some("block"), Some(v), Some("header"), None) => {
                    let block_hash = BlockHash::from_str(v).map_err(|_| Error::InvalidBlockHash)?;
                    let block = client
                        .lock()
                        .await
                        .block(block_hash) // TODO should ask only header
                        .await
                        .map_err(|_| Error::CannotFindBlockHeader)?;
                    let result = serialize_hex(&block.header);
                    any_resp(
                        result.into_bytes(),
                        StatusCode::OK,
                        Some("text/plain"),
                        Some(157784630),
                    )
                }
                _ => str_resp("endpoint not found".to_string(), StatusCode::NOT_FOUND),
            }
        }
        _ => str_resp("endpoint not found".to_string(), StatusCode::NOT_FOUND),
    };
    log::debug!("<--- {res:?}");
    res
}

fn block_hash_resp(
    block_hash: Option<elements::BlockHash>,
) -> Result<Response<Full<Bytes>>, Error> {
    match block_hash {
        Some(h) => str_resp(h.to_string(), StatusCode::OK),
        None => str_resp("cannot find it".to_string(), StatusCode::NOT_FOUND),
    }
}

fn parse_query(query: &str, key: &Identity) -> Result<WaterfallRequest, Error> {
    let mut params = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<HashMap<String, String>>();
    let descriptor = params
        .remove("descriptor")
        .ok_or(Error::DescriptorFieldMandatory)?;
    let descriptor = encryption::decrypt(&descriptor, key).unwrap_or(descriptor);

    let page = params
        .get("page")
        .map(|e| e.parse().unwrap_or(0))
        .unwrap_or(0u16);

    Ok(WaterfallRequest { descriptor, page })
}

fn str_resp(s: String, status: StatusCode) -> Result<Response<Full<Bytes>>, Error> {
    any_resp(s.into_bytes(), status, Some("text/plain"), None)
}
fn any_resp(
    bytes: Vec<u8>,
    status: StatusCode,
    content: Option<&str>,
    cache: Option<u32>,
) -> Result<Response<Full<Bytes>>, Error> {
    let mut builder = Response::builder().status(status);
    if let Some(content) = content {
        builder = builder.header(CONTENT_TYPE, content)
    }
    let cache = cache.unwrap_or(5);
    builder = builder.header(CACHE_CONTROL, format!("public, max-age={cache}"));

    Ok(builder
        .body(Full::new(bytes.into()))
        .map_err(|_| Error::Other)?)
}

async fn handle_waterfalls_req(
    state: &Arc<State>,
    inputs: &WaterfallRequest,
    is_testnet: bool,
) -> Result<Response<Full<Bytes>>, Error> {
    let desc_str = &inputs.descriptor;
    let db = &state.store;
    let start = Instant::now();
    match desc_str.parse::<elements_miniscript::descriptor::Descriptor<DescriptorPublicKey>>() {
        Ok(desc) => {
            if is_testnet == desc_str.contains("xpub") {
                return str_resp("Wrong network".to_string(), hyper::StatusCode::BAD_REQUEST);
            }
            let mut map = BTreeMap::new();
            for desc in desc.into_single_descriptors().unwrap().iter() {
                let mut result = Vec::with_capacity(GAP_LIMIT as usize); // At least
                for batch in 0..MAX_BATCH {
                    let mut scripts = Vec::with_capacity(GAP_LIMIT as usize);

                    let start = batch * GAP_LIMIT + inputs.page as u32 * MAX_ADDRESSES;
                    for index in start..start + GAP_LIMIT {
                        let l = desc.at_derivation_index(index).unwrap();
                        let script_pubkey = l.script_pubkey();
                        log::debug!("{}/{} {}", desc, index, script_pubkey);
                        scripts.push(db.hash(&script_pubkey));
                    }
                    let mut seen_blockchain = db.get_history(&scripts).unwrap();
                    let seen_mempool = state.mempool.lock().await.seen(&scripts);

                    for (conf, unconf) in seen_blockchain.iter_mut().zip(seen_mempool.iter()) {
                        for txid in unconf {
                            conf.push(TxSeen::mempool(*txid))
                        }
                    }
                    let is_last = seen_blockchain.iter().all(|e| e.is_empty());
                    result.extend(seen_blockchain);

                    if is_last {
                        break;
                    }
                }
                map.insert(desc.to_string(), result);
            }

            // enrich with block hashes and timestamps
            {
                let blocks_hash_ts = state.blocks_hash_ts.lock().await;
                for v in map.values_mut() {
                    for tx_seens in v.iter_mut() {
                        for tx_seen in tx_seens.iter_mut() {
                            if tx_seen.height > 0 {
                                // unconfirmed has height 0, we don't want to map those to the genesis block
                                let (hash, ts) = blocks_hash_ts[tx_seen.height as usize];
                                tx_seen.block_hash = Some(hash);
                                tx_seen.block_timestamp = Some(ts);
                            }
                        }
                    }
                }
            }

            let elements: usize = map.iter().map(|(_, v)| v.len()).sum();
            let result = serde_json::to_string(&WaterfallResponse {
                txs_seen: map,
                page: inputs.page,
            })
            .unwrap();
            log::info!(
                "returning: {elements} elements, elapsed: {}ms",
                start.elapsed().as_millis()
            );
            any_resp(
                result.into_bytes(),
                hyper::StatusCode::OK,
                Some("application/json"),
                Some(5),
            )
        }
        Err(e) => str_resp(e.to_string(), hyper::StatusCode::BAD_REQUEST),
    }
}
