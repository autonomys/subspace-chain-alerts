use crate::error::Error;
use crate::{SubspaceClient, SubspaceRpcClient};
use futures_util::{TryStreamExt, stream};
use humantime::format_duration;
use log::{debug, error, info};
use sp_blockchain::{CachedHeaderMetadata, HashAndNumber};
use sp_runtime::codec::{Decode, Encode};
use sp_runtime::traits::{BlakeTwo256, Block as BlockT, Header as HeaderT};
use sp_runtime::{OpaqueExtrinsic, generic};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use subxt::SubstrateConfig;
use subxt::config::substrate::SubstrateHeader;
use subxt::ext::futures::StreamExt;
use subxt_core::Config;

/// Opaque block header type.
pub(crate) type Header = generic::Header<u32, BlakeTwo256>;
/// Opaque block type.
pub(crate) type Block = generic::Block<Header, OpaqueExtrinsic>;
pub(crate) type BlockHashFor<Block> = <Block as BlockT>::Hash;
pub(crate) type BlockNumberFor<Block> = <<Block as BlockT>::Header as HeaderT>::Number;

/// Maximum number of headers to load in the cache.
const CACHE_HEADER_DEPTH: u32 = 100;

/// Listens for all the blocks being imported,
/// calculate the tree route if there is a re-org,
/// and broadcast the best blocks.
pub(crate) async fn listen_for_all_blocks(
    rpc_client: SubspaceRpcClient,
    client: SubspaceClient,
    non_block_import_threshold: Duration,
) -> Result<(), Error> {
    let blocks_client = client.blocks();
    let mut sub = blocks_client.subscribe_all().await?.fuse();
    let (mut header_metadata, mut current_best_block) =
        load_header_metadata_cache(&rpc_client).await?;
    let mut timeout_fired = None;
    loop {
        match tokio::time::timeout(non_block_import_threshold, sub.next()).await {
            Ok(block) => {
                let block = block.ok_or(Error::MissingBlock)??;
                let block_hash = block.hash();
                let block_number = block.number();

                let last_timeout = timeout_fired.take();
                if let Some(timeout) = last_timeout {
                    // TODO: send slack message on recovery
                    info!("Block import began after: {}", format_duration(timeout));
                }

                header_metadata.add_header(block_hash, block.header().clone());
                if !is_canonical_block(&rpc_client, block_number, block_hash).await? {
                    info!("Fork block: {block_number}[{block_hash}]");
                    continue;
                }

                // calculate tree route from previous best to current best
                let tree_route = sp_blockchain::tree_route(
                    &header_metadata,
                    current_best_block.hash,
                    block_hash,
                )?;
                let enacted = tree_route.enacted();
                let retracted = tree_route.retracted();
                if enacted.is_empty() && retracted.is_empty() {
                    // happens when best block == imported block
                    // we are on the best canonical path
                    continue;
                }

                if enacted.is_empty() {
                    // should not happen where enact nothing but retract blocks
                    continue;
                }

                debug!("Enacted {enacted:?} retracted {retracted:?}");
                current_best_block = enacted
                    .last()
                    .expect("Latest enacted should exist as checked above; qed")
                    .clone();
                if retracted.is_empty() {
                    info!(
                        "Best block: {}[{}]",
                        current_best_block.number, current_best_block.hash,
                    );
                } else {
                    let common_block = tree_route.common_block();
                    info!(
                        "Best block Re-org: {}[{}]. Common block: {}[{}]. Depth: {}",
                        current_best_block.number,
                        current_best_block.hash,
                        common_block.number,
                        common_block.hash,
                        retracted.len()
                    );
                }

                let number_to_clean = block_number.saturating_sub(CACHE_HEADER_DEPTH);
                header_metadata.remove_header(number_to_clean);
            }
            Err(_) => {
                let non_import_duration = timeout_fired
                    .map(|timeout| timeout.saturating_add(non_block_import_threshold))
                    .unwrap_or(non_block_import_threshold);
                // TODO: push slack message on no block imports
                error!(
                    "No block imported in last {}",
                    format_duration(non_import_duration)
                );
                timeout_fired = Some(non_import_duration);
            }
        }
    }
}

async fn is_canonical_block(
    client: &SubspaceRpcClient,
    block_number: BlockNumberFor<Block>,
    block_hash: BlockHashFor<Block>,
) -> Result<bool, Error> {
    let hash = client
        .chain_get_block_hash(Some(block_number.into()))
        .await?
        .ok_or(Error::MissingBlock)?;
    Ok(block_hash == hash)
}

#[derive(Debug, Default)]
struct HeadersMetadataCache {
    block_header_data: BTreeMap<BlockHashFor<Block>, CachedHeaderMetadata<Block>>,
    block_number_hashes: BTreeMap<BlockNumberFor<Block>, BTreeSet<BlockHashFor<Block>>>,
}

impl HeadersMetadataCache {
    fn add_header(
        &mut self,
        hash: BlockHashFor<Block>,
        header: <SubstrateConfig as Config>::Header,
    ) -> bool {
        let SubstrateHeader {
            parent_hash,
            number,
            state_root,
            extrinsics_root,
            digest,
        } = header;
        let encoded = digest.encode();
        let decoded_digest =
            sp_runtime::generic::Digest::decode(&mut &encoded[..]).expect("Digest is always valid");
        let header = Header {
            parent_hash,
            number,
            state_root,
            extrinsics_root,
            digest: decoded_digest,
        };
        assert_eq!(hash, header.hash());
        let header = CachedHeaderMetadata::<Block>::from(&header);
        let number = header.number;
        let replaced = self.block_header_data.insert(hash, header);
        self.block_number_hashes
            .entry(number)
            .or_default()
            .insert(hash);
        debug!("Block[{number}] {hash} cached");
        replaced.is_some()
    }

    fn remove_header(&mut self, number: BlockNumberFor<Block>) {
        debug!("Removing header from cache: {number}");
        let hashes = self.block_number_hashes.remove(&number).unwrap_or_default();
        hashes.into_iter().for_each(|hash| {
            self.block_header_data.remove(&hash);
        })
    }
}

impl sp_blockchain::HeaderMetadata<Block> for HeadersMetadataCache {
    type Error = Error;

    fn header_metadata(
        &self,
        hash: BlockHashFor<Block>,
    ) -> Result<CachedHeaderMetadata<Block>, Self::Error> {
        debug!("Retrieving header from cache: {hash}");
        let block_header_meta = self.block_header_data.get(&hash).cloned();
        block_header_meta.ok_or(Error::MissingBlockHashFromCache(hash))
    }

    fn insert_header_metadata(&self, _: BlockHashFor<Block>, _: CachedHeaderMetadata<Block>) {
        // nothing to do here
    }

    fn remove_header_metadata(&self, _: BlockHashFor<Block>) {
        // nothing to do here
    }
}

async fn load_header_metadata_cache(
    rpc_client: &SubspaceRpcClient,
) -> Result<(HeadersMetadataCache, HashAndNumber<Block>), Error> {
    let latest_hash = rpc_client
        .chain_get_block_hash(None)
        .await?
        .ok_or(Error::MissingBlock)?;
    let latest_head = rpc_client
        .chain_get_header(Some(latest_hash))
        .await?
        .ok_or(Error::MissingBlock)?;

    let cache_start_number = latest_head.number.saturating_sub(CACHE_HEADER_DEPTH);

    info!(
        "Loading header cache from block {} to {}",
        cache_start_number, latest_head.number
    );
    let mut header_metadata = HeadersMetadataCache::default();
    stream::iter(
        (cache_start_number..=latest_head.number).map(|number| async move {
            let block_hash = rpc_client
                .chain_get_block_hash(Some(number.into()))
                .await?
                .ok_or(Error::MissingBlock)?;
            let header = rpc_client
                .chain_get_header(Some(block_hash))
                .await?
                .ok_or(Error::MissingBlock)?;
            debug!("Block header from RPC {number} - {block_hash}");
            Ok::<_, Error>((block_hash, header))
        }),
    )
    .buffered(30)
    .try_collect::<Vec<_>>()
    .await?
    .into_iter()
    .for_each(|(hash, header)| {
        header_metadata.add_header(hash, header);
    });

    info!("Cache header metadata completed.");
    Ok((
        header_metadata,
        HashAndNumber {
            number: latest_head.number,
            hash: latest_hash,
        },
    ))
}
