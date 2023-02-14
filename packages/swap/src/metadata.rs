use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, CustomMsg, Uint128};

#[cw_serde]
pub struct PairMetadata {
    pub pair_contract: Addr,
    pub shares: Uint128,
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum Sg721PairQueryMsg {
    #[returns(Uint128)]
    TotalShares {},
}

impl Default for Sg721PairQueryMsg {
    fn default() -> Self {
        Sg721PairQueryMsg::TotalShares {}
    }
}

impl CustomMsg for Sg721PairQueryMsg {}
