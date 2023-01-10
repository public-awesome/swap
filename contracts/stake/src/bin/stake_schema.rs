use cosmwasm_schema::write_api;
use sg_swap::stake::InstantiateMsg;
use sg_swap_stake::msg::{ExecuteMsg, QueryMsg};

fn main() {
    write_api! {
        instantiate: InstantiateMsg,
        query: QueryMsg,
        execute: ExecuteMsg,
    }
}
