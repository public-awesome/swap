use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{CustomMsg, Empty, Uint128};
use cw2::set_contract_version;
pub use cw721_base::{ContractError, InstantiateMsg, MinterResponse};
use cw_storage_plus::Item;
use sg_swap::metadata::PairMetadata;

// Version info for migration
const CONTRACT_NAME: &str = "crates.io:sg721-pair";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub type Extension = Option<PairMetadata>;

pub type Sg721PairMetadataContract<'a> =
    cw721_base::Cw721Contract<'a, Extension, Empty, Empty, Sg721PairQueryMsg>;
pub type ExecuteMsg = cw721_base::ExecuteMsg<Extension, Empty>;
pub type QueryMsg = cw721_base::QueryMsg<Sg721PairQueryMsg>;

pub const TOTAL_SHARES: Item<Uint128> = Item::new("total_shares");

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

#[cfg(not(feature = "library"))]
pub mod entry {
    use super::*;

    use cosmwasm_std::{entry_point, to_binary};
    use cosmwasm_std::{Binary, Deps, DepsMut, Env, MessageInfo, Response, StdResult};

    // This makes a conscious choice on the various generics used by the contract
    #[entry_point]
    pub fn instantiate(
        mut deps: DepsMut,
        env: Env,
        info: MessageInfo,
        msg: InstantiateMsg,
    ) -> Result<Response, ContractError> {
        let res =
            Sg721PairMetadataContract::default().instantiate(deps.branch(), env, info, msg)?;
        // Explicitly set contract name and version, otherwise set to cw721-base info
        set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)
            .map_err(ContractError::Std)?;

        TOTAL_SHARES.save(deps.storage, &Uint128::zero())?;

        Ok(res)
    }

    #[entry_point]
    pub fn execute(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        msg: ExecuteMsg,
    ) -> Result<Response, ContractError> {
        Sg721PairMetadataContract::default().execute(deps, env, info, msg)
    }

    #[entry_point]
    pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
        match msg {
            QueryMsg::Extension { msg } => match msg {
                Sg721PairQueryMsg::TotalShares {} => to_binary(&query_total_shares(deps)?),
            },
            _ => Sg721PairMetadataContract::default().query(deps, env, msg),
        }
    }

    pub fn query_total_shares(deps: Deps) -> StdResult<Uint128> {
        TOTAL_SHARES.load(deps.storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use cosmwasm_std::{
        testing::{mock_dependencies, mock_env, mock_info},
        Addr, Uint128,
    };
    use cw721::Cw721Query;
    use sg_swap::metadata::PairMetadata;

    const CREATOR: &str = "creator";

    #[test]
    fn use_metadata_extension() {
        let mut deps = mock_dependencies();
        let contract = Sg721PairMetadataContract::default();

        let info = mock_info(CREATOR, &[]);
        let init_msg = InstantiateMsg {
            name: "SpaceShips".to_string(),
            symbol: "SPACE".to_string(),
            minter: CREATOR.to_string(),
        };
        contract
            .instantiate(deps.as_mut(), mock_env(), info.clone(), init_msg)
            .unwrap();

        let token_id = "Enterprise";
        let token_uri = Some("https://starships.example.com/Starship/Enterprise.json".into());
        let extension = Some(PairMetadata {
            pair_contract: Addr::unchecked("pair_contract"),
            shares: Uint128::from(1000u128),
        });
        let exec_msg = ExecuteMsg::Mint {
            token_id: token_id.to_string(),
            owner: "john".to_string(),
            token_uri: token_uri.clone(),
            extension: extension.clone(),
        };
        contract
            .execute(deps.as_mut(), mock_env(), info, exec_msg)
            .unwrap();

        let res = contract.nft_info(deps.as_ref(), token_id.into()).unwrap();
        assert_eq!(res.token_uri, token_uri);
        assert_eq!(res.extension, extension);
    }
}
