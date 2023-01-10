use tests::SuiteBuilder;

use cosmwasm_std::{coin, from_slice, Addr, Decimal, Uint128};

use sg_swap::{
    asset::{AssetInfo, AssetInfoExt},
    factory::PartialStakeConfig,
};
use sg_swap_stake::msg::{QueryMsg as StakeQueryMsg, ReceiveDelegationMsg, StakedResponse};
use sg_swap_stake::state::Config as StargazeStakeConfig;

mod staking {
    use sg_swap::factory::{DefaultStakeConfig, DistributionFlow};

    use super::*;

    #[test]
    fn basic() {
        let ujuno = "ujuno";
        let uluna = "uluna";

        let liquidity_provider = "liquidity_provider";

        let ujuno_info = AssetInfo::Native(ujuno.to_string());
        let uluna_info = AssetInfo::Native(uluna.to_string());

        let mut suite = SuiteBuilder::new()
            .with_funds(
                liquidity_provider,
                &[coin(100_000, ujuno), coin(100_000, uluna)],
            )
            .with_stake_config(DefaultStakeConfig {
                staking_code_id: 0,
                tokens_per_power: Uint128::new(1),
                min_bond: Uint128::new(1),
                unbonding_periods: vec![1, 2],
                max_distributions: 1,
            })
            .build();

        // create pair
        let pair = suite
            .create_pair(
                "owner",
                sg_swap::factory::PairType::Xyk {},
                [ujuno_info.clone(), uluna_info.clone()],
                Some(PartialStakeConfig {
                    tokens_per_power: Some(Uint128::new(100)),
                    min_bond: Some(Uint128::new(100)),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();

        // provide liquidity
        suite
            .provide_liquidity(
                liquidity_provider,
                &pair,
                [
                    ujuno_info.with_balance(10_000u128),
                    uluna_info.with_balance(10_000u128),
                ],
                &[coin(10_000, ujuno), coin(10_000, uluna)],
            )
            .unwrap();

        let pair_info = suite
            .query_pair(vec![ujuno_info.clone(), uluna_info.clone()])
            .unwrap();

        // create rewards distribution
        suite
            .create_distribution_flow(
                "owner",
                vec![ujuno_info.clone(), uluna_info],
                ujuno_info,
                vec![(1, Decimal::percent(50)), (2, Decimal::one())],
            )
            .unwrap();

        // stake
        suite
            .send_cw20(
                liquidity_provider,
                &pair_info.liquidity_token,
                1000,
                pair_info.staking_addr.as_str(),
                ReceiveDelegationMsg::Delegate {
                    unbonding_period: 1,
                    delegate_as: None,
                },
            )
            .unwrap();

        let resp: StakedResponse = suite
            .app
            .wrap()
            .query_wasm_smart(
                pair_info.staking_addr,
                &StakeQueryMsg::Staked {
                    address: Addr::unchecked(liquidity_provider).to_string(),
                    unbonding_period: 1,
                },
            )
            .unwrap();

        assert_eq!(1000, resp.stake.u128());
    }

    #[test]
    fn stake_has_correct_instantiator() {
        let ujuno = "ujuno";
        let uluna = "uluna";

        let ujuno_info = AssetInfo::Native(ujuno.to_string());
        let uluna_info = AssetInfo::Native(uluna.to_string());

        let mut suite = SuiteBuilder::new().build();

        // create a pair
        let pair = suite
            .create_pair_and_provide_liquidity(
                sg_swap::factory::PairType::Xyk {},
                (ujuno_info.clone(), 100_000),
                (uluna_info.clone(), 100_000),
                vec![coin(100_000, ujuno), coin(100_000, uluna)],
            )
            .unwrap();

        // get info with staking contract address
        let pair_info = suite.query_pair(vec![ujuno_info, uluna_info]).unwrap();

        let stake_config: StargazeStakeConfig = from_slice(
            &suite
                .app
                .wrap()
                .query_wasm_raw(
                    &pair_info.staking_addr,
                    sg_swap_pair::state::CONFIG.as_slice(),
                )
                .unwrap()
                .unwrap(),
        )
        .unwrap();

        assert_eq!(
            stake_config.instantiator, pair,
            "stake should be instantiated by pair"
        );
    }

    #[test]
    fn create_pair_and_distributions() {
        let ujuno = "ujuno";
        let uluna = "uluna";
        let test = "test";
        let no_dist = "not_distributable";

        let owner = "owner";
        let user = "user";

        let ujuno_info = AssetInfo::Native(ujuno.to_string());
        let uluna_info = AssetInfo::Native(uluna.to_string());
        let test_info = AssetInfo::Native(test.to_string());

        let mut suite = SuiteBuilder::new()
            .with_funds(
                user,
                &[
                    coin(100, ujuno),
                    coin(100, uluna),
                    coin(100, test),
                    coin(100, no_dist),
                ],
            )
            .with_stake_config(DefaultStakeConfig {
                staking_code_id: 0,
                tokens_per_power: Uint128::new(1),
                min_bond: Uint128::new(1),
                unbonding_periods: vec![1],
                max_distributions: 3,
            })
            .build();

        // create pair
        suite
            .create_pair_and_distributions(
                owner,
                sg_swap::factory::PairType::Xyk {},
                vec![ujuno_info.clone(), uluna_info.clone()],
                None,
                vec![
                    DistributionFlow {
                        asset: ujuno_info.clone(),
                        rewards: vec![(1, Decimal::one())],
                        reward_duration: 100,
                    },
                    DistributionFlow {
                        asset: uluna_info.clone(),
                        rewards: vec![(1, Decimal::one())],
                        reward_duration: 100,
                    },
                    DistributionFlow {
                        asset: test_info,
                        rewards: vec![(1, Decimal::one())],
                        reward_duration: 100,
                    },
                ],
            )
            .unwrap();

        let pair_info = suite.query_pair(vec![ujuno_info, uluna_info]).unwrap();

        // should be able to distribute those assets now
        suite
            .distribute_funds(pair_info.staking_addr.clone(), user, &[coin(100, ujuno)])
            .unwrap();
        suite
            .distribute_funds(pair_info.staking_addr.clone(), user, &[coin(100, uluna)])
            .unwrap();
        suite
            .distribute_funds(pair_info.staking_addr.clone(), user, &[coin(100, test)])
            .unwrap();
        suite
            .distribute_funds(pair_info.staking_addr, user, &[coin(100, no_dist)])
            .unwrap_err();
    }
}
