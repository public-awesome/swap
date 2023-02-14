use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, DepsMut, StdResult, Storage, Uint128};
use cw_storage_plus::Item;
use sg_swap::pair::PairInfo;

/// This structure stores the main config parameters for a constant product pair contract.
#[cw_serde]
pub struct Config {
    /// General pair information (e.g pair type)
    pub pair_info: PairInfo,
    /// The factory contract address
    pub factory_addr: Addr,
    /// The last timestamp when the pair contract update the asset cumulative prices
    pub block_time_last: u64,
    /// The last cumulative price for asset 0
    pub price0_cumulative_last: Uint128,
    /// The last cumulative price for asset 1
    pub price1_cumulative_last: Uint128,
    /// The block time until which trading is disabled
    pub trading_starts: u64,
}

/// Stores the config struct at the given key
pub const CONFIG: Item<Config> = Item::new("config");

pub const COLLECTION_INDEX: Item<u64> = Item::new("collection_index");

pub fn increment_collection_index(store: &mut dyn Storage) -> StdResult<String> {
    let mut index = COLLECTION_INDEX.load(store)?;
    index += 1;
    COLLECTION_INDEX.save(store, &index)?;
    Ok(index.to_string())
}
