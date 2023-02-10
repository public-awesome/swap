use anyhow::Result as AnyResult;

use cosmwasm_schema::serde::Serialize;
use cosmwasm_std::{coin, to_binary, Addr, Coin, Decimal, Uint128};
use cw20::{BalanceResponse, Cw20ExecuteMsg, Cw20QueryMsg, MinterResponse};
use cw20_base::msg::InstantiateMsg as Cw20BaseInstantiateMsg;
use cw_multi_test::{App, AppResponse, BankSudo, ContractWrapper, Executor, SudoMsg};

use sg_swap::asset::{Asset, AssetInfo};
use sg_swap::factory::{
    DefaultStakeConfig, DistributionFlow, ExecuteMsg as FactoryExecuteMsg,
    InstantiateMsg as FactoryInstantiateMsg, PairConfig, PairType, PartialStakeConfig,
    QueryMsg as FactoryQueryMsg,
};
use sg_swap::fee_config::FeeConfig;
use sg_swap::multi_hop::{
    ExecuteMsg, InstantiateMsg, QueryMsg, SimulateSwapOperationsResponse, SwapOperation,
};
use sg_swap::pair::{ExecuteMsg as PairExecuteMsg, PairInfo};
use sg_swap::stake::UnbondingPeriod;
use sg_swap_stake::msg::ExecuteMsg as StakeExecuteMsg;

fn store_multi_hop(app: &mut App) -> u64 {
    let contract = Box::new(ContractWrapper::new_with_empty(
        sg_swap_multi_hop::contract::execute,
        sg_swap_multi_hop::contract::instantiate,
        sg_swap_multi_hop::contract::query,
    ));

    app.store_code(contract)
}

fn store_factory(app: &mut App) -> u64 {
    let contract = Box::new(
        ContractWrapper::new_with_empty(
            sg_swap_factory::contract::execute,
            sg_swap_factory::contract::instantiate,
            sg_swap_factory::contract::query,
        )
        .with_reply_empty(sg_swap_factory::contract::reply),
    );

    app.store_code(contract)
}

fn store_pair(app: &mut App) -> u64 {
    let contract = Box::new(
        ContractWrapper::new_with_empty(
            sg_swap_pair::contract::execute,
            sg_swap_pair::contract::instantiate,
            sg_swap_pair::contract::query,
        )
        .with_reply_empty(sg_swap_pair::contract::reply),
    );

    app.store_code(contract)
}

fn store_staking(app: &mut App) -> u64 {
    let contract = Box::new(ContractWrapper::new(
        sg_swap_stake::contract::execute,
        sg_swap_stake::contract::instantiate,
        sg_swap_stake::contract::query,
    ));

    app.store_code(contract)
}

fn store_cw20(app: &mut App) -> u64 {
    let contract = Box::new(ContractWrapper::new(
        cw20_base::contract::execute,
        cw20_base::contract::instantiate,
        cw20_base::contract::query,
    ));

    app.store_code(contract)
}

#[derive(Debug)]
pub struct SuiteBuilder {
    funds: Vec<(Addr, Vec<Coin>)>,
    max_referral_commission: Decimal,
    stake_config: DefaultStakeConfig,
    trading_starts: Option<u64>,
}

impl Default for SuiteBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SuiteBuilder {
    pub fn new() -> Self {
        Self {
            funds: vec![],
            max_referral_commission: Decimal::one(),
            stake_config: DefaultStakeConfig {
                staking_code_id: 0, // will be set in build()
                tokens_per_power: Uint128::new(1000),
                min_bond: Uint128::new(1000),
                unbonding_periods: vec![60 * 60 * 24 * 7, 60 * 60 * 24 * 14, 60 * 60 * 24 * 21],
                max_distributions: 6,
            },
            trading_starts: None,
        }
    }

    pub fn with_stake_config(mut self, stake_config: DefaultStakeConfig) -> Self {
        self.stake_config = stake_config;
        self
    }

    pub fn with_trading_starts(mut self, trading_starts: u64) -> Self {
        self.trading_starts = Some(trading_starts);
        self
    }

    pub fn with_funds(mut self, addr: &str, funds: &[Coin]) -> Self {
        self.funds.push((Addr::unchecked(addr), funds.into()));
        self
    }

    pub fn with_max_referral_commission(mut self, max: Decimal) -> Self {
        self.max_referral_commission = max;
        self
    }

    #[track_caller]
    pub fn build(self) -> Suite {
        let mut app = App::default();
        let owner = Addr::unchecked("owner");

        let cw20_code_id = store_cw20(&mut app);
        let pair_code_id = store_pair(&mut app);
        let staking_code_id = store_staking(&mut app);
        let factory_code_id = store_factory(&mut app);
        let factory = app
            .instantiate_contract(
                factory_code_id,
                owner.clone(),
                &FactoryInstantiateMsg {
                    pair_configs: vec![
                        PairConfig {
                            code_id: pair_code_id,
                            pair_type: PairType::Xyk {},
                            fee_config: FeeConfig {
                                total_fee_bps: 0,
                                protocol_fee_bps: 0,
                            },
                            is_disabled: false,
                        },
                        PairConfig {
                            code_id: pair_code_id,
                            pair_type: PairType::Stable {},
                            fee_config: FeeConfig {
                                total_fee_bps: 0,
                                protocol_fee_bps: 0,
                            },
                            is_disabled: false,
                        },
                    ],
                    token_code_id: cw20_code_id,
                    fee_address: None,
                    owner: owner.to_string(),
                    max_referral_commission: self.max_referral_commission,
                    default_stake_config: DefaultStakeConfig {
                        staking_code_id,
                        ..self.stake_config
                    },
                    trading_starts: self.trading_starts,
                },
                &[],
                "Stargaze Swap Factory",
                None,
            )
            .unwrap();

        let multi_hop_code_id = store_multi_hop(&mut app);
        let multi_hop = app
            .instantiate_contract(
                multi_hop_code_id,
                owner.clone(),
                &InstantiateMsg {
                    sg_swap_factory: factory.to_string(),
                },
                &[],
                "Stargaze Swap Multi Hop",
                None,
            )
            .unwrap();

        let funds = self.funds;
        app.init_modules(|router, _, storage| -> AnyResult<()> {
            for (addr, coin) in funds {
                router.bank.init_balance(storage, &addr, coin)?;
            }
            Ok(())
        })
        .unwrap();

        Suite {
            owner: owner.to_string(),
            app,
            factory,
            multi_hop,
            cw20_code_id,
        }
    }
}

pub struct Suite {
    pub owner: String,
    pub app: App,
    pub factory: Addr,
    multi_hop: Addr,
    cw20_code_id: u64,
}

impl Suite {
    pub fn advance_time(&mut self, seconds: u64) {
        self.app
            .update_block(|block| block.time = block.time.plus_seconds(seconds));
    }

    pub fn create_pair(
        &mut self,
        sender: &str,
        pair_type: PairType,
        tokens: [AssetInfo; 2],
        staking_config: Option<PartialStakeConfig>,
        total_fee_bps: Option<u16>,
    ) -> AnyResult<Addr> {
        self.app.execute_contract(
            Addr::unchecked(sender),
            self.factory.clone(),
            &FactoryExecuteMsg::CreatePair {
                pair_type,
                asset_infos: tokens.to_vec(),
                init_params: None,
                staking_config: staking_config.unwrap_or_default(),
                total_fee_bps,
            },
            &[],
        )?;

        let factory = self.factory.clone();
        let res: PairInfo = self.app.wrap().query_wasm_smart(
            Addr::unchecked(factory),
            &FactoryQueryMsg::Pair {
                asset_infos: tokens.to_vec(),
            },
        )?;
        Ok(res.contract_addr)
    }

    pub fn create_pair_and_distributions(
        &mut self,
        sender: &str,
        pair_type: PairType,
        asset_infos: Vec<AssetInfo>,
        staking_config: Option<PartialStakeConfig>,
        distribution_flows: Vec<DistributionFlow>,
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(sender),
            self.factory.clone(),
            &FactoryExecuteMsg::CreatePairAndDistributionFlows {
                pair_type,
                asset_infos,
                init_params: None,
                staking_config: staking_config.unwrap_or_default(),
                distribution_flows,
                total_fee_bps: None,
            },
            &[],
        )
    }

    pub fn provide_liquidity(
        &mut self,
        owner: &str,
        pair: &Addr,
        assets: [Asset; 2],
        send_funds: &[Coin],
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(owner),
            pair.clone(),
            &PairExecuteMsg::ProvideLiquidity {
                assets: assets.to_vec(),
                slippage_tolerance: None,
                receiver: None,
            },
            send_funds,
        )
    }

    fn increase_allowance(
        &mut self,
        owner: &str,
        contract: &Addr,
        spender: &str,
        amount: u128,
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(owner),
            contract.clone(),
            &Cw20ExecuteMsg::IncreaseAllowance {
                spender: spender.to_owned(),
                amount: amount.into(),
                expires: None,
            },
            &[],
        )
    }

    /// Create LP for provided assets and provides some liquidity to them.
    /// Requirement: if using native token provide coins to sent as last argument
    pub fn create_pair_and_provide_liquidity(
        &mut self,
        pair_type: PairType,
        first_asset: (AssetInfo, u128),
        second_asset: (AssetInfo, u128),
        native_tokens: Vec<Coin>,
    ) -> AnyResult<Addr> {
        let owner = self.owner.clone();
        let whale = "whale";

        let pair = self.create_pair(
            &owner,
            pair_type,
            [first_asset.0.clone(), second_asset.0.clone()],
            None,
            None,
        )?;

        match first_asset.0.clone() {
            AssetInfo::Token(addr) => {
                // Mint some initial balances for whale user
                self.mint_cw20(&owner, &Addr::unchecked(&addr), first_asset.1, whale)
                    .unwrap();
                // Increases allowances for given LP contracts in order to provide liquidity to pool
                self.increase_allowance(
                    whale,
                    &Addr::unchecked(addr),
                    pair.as_str(),
                    first_asset.1,
                )
                .unwrap();
            }
            AssetInfo::Native(denom) => {
                self.app
                    .sudo(SudoMsg::Bank(BankSudo::Mint {
                        to_address: whale.to_owned(),
                        amount: vec![coin(first_asset.1, denom)],
                    }))
                    .unwrap();
            }
        };
        match second_asset.0.clone() {
            AssetInfo::Token(addr) => {
                // Mint some initial balances for whale user
                self.mint_cw20(&owner, &Addr::unchecked(&addr), second_asset.1, whale)
                    .unwrap();
                // Increases allowances for given LP contracts in order to provide liquidity to pool
                self.increase_allowance(
                    whale,
                    &Addr::unchecked(addr),
                    pair.as_str(),
                    second_asset.1,
                )
                .unwrap();
            }
            AssetInfo::Native(denom) => {
                self.app
                    .sudo(SudoMsg::Bank(BankSudo::Mint {
                        to_address: whale.to_owned(),
                        amount: vec![coin(second_asset.1, denom)],
                    }))
                    .unwrap();
            }
        };

        self.provide_liquidity(
            whale,
            &pair,
            [
                Asset {
                    info: first_asset.0,
                    amount: first_asset.1.into(),
                },
                Asset {
                    info: second_asset.0,
                    amount: second_asset.1.into(),
                },
            ],
            &native_tokens, // for native token you need to transfer tokens manually
        )
        .unwrap();

        Ok(pair)
    }

    /// Create a distribution flow through the factory contract
    pub fn create_distribution_flow(
        &mut self,
        sender: &str,
        asset_infos: Vec<AssetInfo>,
        asset: AssetInfo,
        rewards: Vec<(UnbondingPeriod, Decimal)>,
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(sender),
            self.factory.clone(),
            &FactoryExecuteMsg::CreateDistributionFlow {
                asset_infos,
                asset,
                rewards,
            },
            &[],
        )
    }

    pub fn distribute_funds(
        &mut self,
        staking_contract: Addr,
        sender: &str,
        funds: &[Coin],
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(sender),
            staking_contract,
            &StakeExecuteMsg::DistributeRewards { sender: None },
            funds,
        )
    }

    pub fn instantiate_token(&mut self, owner: &str, token: &str) -> Addr {
        self.app
            .instantiate_contract(
                self.cw20_code_id,
                Addr::unchecked(owner),
                &Cw20BaseInstantiateMsg {
                    name: token.to_owned(),
                    symbol: token.to_owned(),
                    decimals: 6,
                    initial_balances: vec![],
                    mint: Some(MinterResponse {
                        minter: owner.to_string(),
                        cap: None,
                    }),
                    marketing: None,
                },
                &[],
                token,
                None,
            )
            .unwrap()
    }

    pub fn mint_cw20(
        &mut self,
        owner: &str,
        token: &Addr,
        amount: u128,
        recipient: &str,
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(owner),
            token.clone(),
            &Cw20ExecuteMsg::Mint {
                recipient: recipient.to_owned(),
                amount: amount.into(),
            },
            &[],
        )
    }

    pub fn send_cw20(
        &mut self,
        owner: &str,
        token: &Addr,
        amount: u128,
        contract: &str,
        msg: impl Serialize,
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(owner),
            token.clone(),
            &Cw20ExecuteMsg::Send {
                contract: contract.to_owned(),
                amount: amount.into(),
                msg: to_binary(&msg)?,
            },
            &[],
        )
    }

    pub fn swap_operations(
        &mut self,
        sender: &str,
        amount: Coin,
        operations: Vec<SwapOperation>,
    ) -> AnyResult<AppResponse> {
        self.swap_operations_ref(sender, amount, operations, None, None)
    }

    pub fn swap_operations_ref(
        &mut self,
        sender: &str,
        amount: Coin,
        operations: Vec<SwapOperation>,
        referral_address: impl Into<Option<String>>,
        referral_commission: impl Into<Option<Decimal>>,
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(sender),
            self.multi_hop.clone(),
            &ExecuteMsg::ExecuteSwapOperations {
                operations,
                minimum_receive: None,
                receiver: None,
                max_spread: None,
                referral_address: referral_address.into(),
                referral_commission: referral_commission.into(),
            },
            &[amount],
        )
    }

    pub fn swap_operations_cw20(
        &mut self,
        sender: &str,
        token_in: &Addr,
        amount: u128,
        operations: Vec<SwapOperation>,
    ) -> AnyResult<AppResponse> {
        self.swap_operations_cw20_ref(sender, token_in, amount, operations, None, None)
    }

    pub fn swap_operations_cw20_ref(
        &mut self,
        sender: &str,
        token_in: &Addr,
        amount: u128,
        operations: Vec<SwapOperation>,
        referral_address: impl Into<Option<String>>,
        referral_commission: impl Into<Option<Decimal>>,
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(sender),
            token_in.clone(),
            &Cw20ExecuteMsg::Send {
                contract: self.multi_hop.to_string(),
                amount: amount.into(),
                msg: to_binary(&ExecuteMsg::ExecuteSwapOperations {
                    operations,
                    minimum_receive: None,
                    receiver: None,
                    max_spread: None,
                    referral_address: referral_address.into(),
                    referral_commission: referral_commission.into(),
                })
                .unwrap(),
            },
            &[],
        )
    }

    pub fn assert_minimum_receive(
        &mut self,
        receiver: &str,
        asset_info: AssetInfo,
        minimum_receive: impl Into<Uint128>,
    ) -> AnyResult<AppResponse> {
        self.app.execute_contract(
            Addr::unchecked(receiver),
            self.multi_hop.clone(),
            &ExecuteMsg::AssertMinimumReceive {
                asset_info,
                prev_balance: Uint128::zero(),
                minimum_receive: minimum_receive.into(),
                receiver: receiver.into(),
            },
            &[],
        )
    }

    pub fn query_balance(&self, sender: &str, denom: &str) -> AnyResult<u128> {
        let amount = self
            .app
            .wrap()
            .query_balance(&Addr::unchecked(sender), denom)?
            .amount;
        Ok(amount.into())
    }

    pub fn query_cw20_balance(&self, sender: &str, address: &Addr) -> AnyResult<u128> {
        let balance: BalanceResponse = self.app.wrap().query_wasm_smart(
            address,
            &Cw20QueryMsg::Balance {
                address: sender.to_owned(),
            },
        )?;
        Ok(balance.balance.into())
    }

    pub fn query_simulate_swap_operations(
        &self,
        offer_amount: impl Into<Uint128>,
        operations: Vec<SwapOperation>,
    ) -> AnyResult<u128> {
        let amount: SimulateSwapOperationsResponse = self.app.wrap().query_wasm_smart(
            self.multi_hop.clone(),
            &QueryMsg::SimulateSwapOperations {
                offer_amount: offer_amount.into(),
                operations,
                referral: false,
                referral_commission: None,
            },
        )?;
        Ok(amount.amount.into())
    }

    pub fn query_simulate_swap_operations_ref(
        &self,
        offer_amount: impl Into<Uint128>,
        operations: Vec<SwapOperation>,
        referral_commission: impl Into<Option<Decimal>>,
    ) -> AnyResult<u128> {
        let amount: SimulateSwapOperationsResponse = self.app.wrap().query_wasm_smart(
            self.multi_hop.clone(),
            &QueryMsg::SimulateSwapOperations {
                offer_amount: offer_amount.into(),
                operations,
                referral: true,
                referral_commission: referral_commission.into(),
            },
        )?;
        Ok(amount.amount.into())
    }

    /// Queries the info of the given pair from the factory
    pub fn query_pair(&self, asset_infos: Vec<AssetInfo>) -> AnyResult<PairInfo> {
        Ok(self
            .app
            .wrap()
            .query_wasm_smart(self.factory.clone(), &FactoryQueryMsg::Pair { asset_infos })?)
    }
}
