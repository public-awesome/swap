use cosmwasm_std::{coin, testing::mock_env};
use sg_swap::{
    asset::{AssetInfo, AssetInfoExt},
    multi_hop::SwapOperation,
};
use tests::SuiteBuilder;

#[test]
fn trading_frozen() {
    let ujuno = "ujuno";
    let uluna = "uluna";
    let user = "user";

    let ujuno_info = AssetInfo::Native(ujuno.to_string());
    let uluna_info = AssetInfo::Native(uluna.to_string());

    let mut suite = SuiteBuilder::new()
        .with_funds(user, &[coin(100_000, ujuno)])
        .with_trading_starts(mock_env().block.time.seconds() + 1000)
        .build();

    suite
        .create_pair_and_provide_liquidity(
            sg_swap::factory::PairType::Xyk {},
            (ujuno_info.clone(), 1_000_000),
            (uluna_info.clone(), 1_000_000),
            vec![coin(1_000_000, ujuno), coin(1_000_000, uluna)],
        )
        .unwrap();

    let err = suite
        .swap_operations(
            user,
            coin(1000, ujuno),
            vec![SwapOperation::StargazeSwap {
                ask_asset_info: uluna_info.clone(),
                offer_asset_info: ujuno_info.clone(),
            }],
        )
        .unwrap_err();

    assert_eq!(err.root_cause().to_string(), "Trading has not started yet");

    // wait until trading starts
    suite.advance_time(1000);

    suite
        .swap_operations(
            user,
            coin(1000, ujuno),
            vec![SwapOperation::StargazeSwap {
                ask_asset_info: uluna_info,
                offer_asset_info: ujuno_info,
            }],
        )
        .unwrap();
}

#[test]
fn custom_fee_works() {
    let ujuno = "ujuno";
    let uluna = "uluna";
    let user = "user";

    let ujuno_info = AssetInfo::Native(ujuno.to_string());
    let uluna_info = AssetInfo::Native(uluna.to_string());

    let mut suite = SuiteBuilder::new()
        .with_funds(user, &[coin(1_001_000, ujuno), coin(1_000_000, uluna)])
        .build();

    let pair = suite
        .create_pair(
            &suite.owner.clone(),
            sg_swap::factory::PairType::Xyk {},
            [ujuno_info.clone(), uluna_info.clone()],
            None,
            5_000.into(), // 50% fee for this pair
        )
        .unwrap();

    suite
        .provide_liquidity(
            user,
            &pair,
            [
                ujuno_info.with_balance(1_000_000u128),
                uluna_info.with_balance(1_000_000u128),
            ],
            &[coin(1_000_000, ujuno), coin(1_000_000, uluna)],
        )
        .unwrap();

    suite
        .swap_operations(
            user,
            coin(1000, ujuno),
            vec![SwapOperation::StargazeSwap {
                ask_asset_info: uluna_info,
                offer_asset_info: ujuno_info,
            }],
        )
        .unwrap();

    assert_eq!(0, suite.query_balance(user, ujuno).unwrap());
    assert_eq!(
        500,
        suite.query_balance(user, uluna).unwrap(),
        "should only receive 50% due to fee"
    );
}
