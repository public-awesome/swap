use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Uint128};

#[cw_serde]
pub struct PairMetadata {
    pub pair_contract: Addr,
    pub shares: Uint128,
}
