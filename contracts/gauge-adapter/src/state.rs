use cosmwasm_schema::cw_serde;
use cosmwasm_std::Addr;
use cw_storage_plus::Item;
use sg_swap::asset::AssetValidated;
use wynd_curve_utils::ScalableCurve;

#[cw_serde]
pub struct Config {
    /// Address of the factory contract
    pub factory: Addr,
    /// Owner of the creator (instantiator of the factory)
    pub owner: Addr,
    /// The asset to send to the voted-for lp staking contracts every epoch
    pub rewards_asset: AssetValidated,
    pub distribution_curve: ScalableCurve,
}

pub const CONFIG: Item<Config> = Item::new("config");
