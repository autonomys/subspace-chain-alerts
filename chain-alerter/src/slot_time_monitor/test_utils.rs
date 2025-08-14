use crate::subspace::{BlockNumber, SubspaceConfig};
use anyhow::Result;
use std::str::FromStr;
use subxt::OnlineClient;
use subxt::blocks::{Block, BlockRef};
use subxt::utils::H256;

pub async fn get_slot_time_testing_blocks(
    subspace_client: &OnlineClient<SubspaceConfig>,
) -> Result<(
    Block<SubspaceConfig, OnlineClient<SubspaceConfig>>,
    Block<SubspaceConfig, OnlineClient<SubspaceConfig>>,
)> {
    let slot_time_testing_blocks: [(BlockNumber, BlockRef<H256>); 2] = [
        (
            100,
            BlockRef::from(
                H256::from_str("7b8b1c70a7c1f5897789f3847f2a8cd46d3ec8bc9c063532a9e5fd24099d4c48")
                    .unwrap(),
            ),
        ),
        (
            200,
            BlockRef::from(
                H256::from_str("98fae467f5cd9fe68030fa93de25a47a8fb91f0f82afdb51cdd1e4907d24ee28")
                    .unwrap(),
            ),
        ),
    ];

    let first_block = subspace_client
        .blocks()
        .at(slot_time_testing_blocks[0].1.hash())
        .await?;

    let second_block = subspace_client
        .blocks()
        .at(slot_time_testing_blocks[1].1.hash())
        .await?;

    Ok((first_block, second_block))
}
