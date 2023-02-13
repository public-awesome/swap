#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_slice, to_binary, Addr, Binary, Decimal, Deps, DepsMut, Env, MessageInfo, Order, Response,
    StdError, StdResult, Storage, SubMsg, Uint128, WasmMsg,
};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use sg_swap::asset::{AssetInfo, AssetInfoValidated};
use sg_swap::stake::{InstantiateMsg, UnbondingPeriod};

use crate::distribution::{
    apply_points_correction, execute_delegate_withdrawal, execute_distribute_rewards,
    execute_withdraw_rewards, query_delegated, query_distributed_rewards, query_distribution_data,
    query_undistributed_rewards, query_withdraw_adjustment_data, query_withdrawable_rewards,
};
use crate::utils::CurveExt;
use cw2::set_contract_version;
use cw_utils::{maybe_addr, Expiration};

use crate::error::ContractError;
use crate::msg::{
    AllStakedResponse, AnnualizedReward, AnnualizedRewardsResponse, BondingInfoResponse,
    BondingPeriodInfo, ExecuteMsg, QueryMsg, ReceiveDelegationMsg, RewardsPowerResponse,
    StakedResponse, TotalStakedResponse, TotalUnbondingResponse,
};
use crate::state::{
    load_total_of_period, Config, Distribution, TokenInfo, TotalStake, ADMIN, CLAIMS, CONFIG,
    DISTRIBUTION, REWARD_CURVE, STAKE, TOTAL_PER_PERIOD, TOTAL_STAKED,
};
use wynd_curve_utils::Curve;

const SECONDS_PER_YEAR: u64 = 365 * 24 * 60 * 60;

// version info for migration info
const CONTRACT_NAME: &str = concat!("crates.io:", env!("CARGO_CRATE_NAME"));
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

// Note, you can use StdResult in some functions where you do not
// make use of the custom errors
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    mut deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    mut msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    let api = deps.api;
    // Set the admin if provided
    ADMIN.set(deps.branch(), maybe_addr(api, msg.admin.clone())?)?;

    // min_bond is at least 1, so 0 stake -> non-membership
    let min_bond = std::cmp::max(msg.min_bond, Uint128::new(1));

    TOTAL_STAKED.save(deps.storage, &TokenInfo::default())?;

    // make sure they are sorted, this is important because the rest of the contract assumes the same
    // order everywhere and uses binary search in some places.
    msg.unbonding_periods.sort_unstable();

    // initialize total stake
    TOTAL_PER_PERIOD.save(
        deps.storage,
        &msg.unbonding_periods
            .iter()
            .map(|unbonding_period| (*unbonding_period, TotalStake::default()))
            .collect(),
    )?;

    let config = Config {
        instantiator: info.sender,
        // cw20_contract: deps.api.addr_validate(&msg.cw20_contract)?,
        // TODO: remove this
        cw20_contract: Addr::unchecked("terra1hzh9vpxhsk82503se0vv5jj6etdvxu3nv8x7zu"),
        cw721_contract: deps.api.addr_validate(&msg.cw721_contract)?,
        tokens_per_power: msg.tokens_per_power,
        min_bond,
        unbonding_periods: msg.unbonding_periods,
        max_distributions: msg.max_distributions,
    };
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::default())
}

// And declare a custom Error variant for the ones where you will want to make use of it
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    let api = deps.api;
    match msg {
        ExecuteMsg::UpdateAdmin { admin } => {
            Ok(ADMIN.execute_update_admin(deps, info, maybe_addr(api, admin)?)?)
        }
        ExecuteMsg::CreateDistributionFlow {
            manager,
            asset,
            rewards,
        } => execute_create_distribution_flow(deps, info, manager, asset, rewards),
        ExecuteMsg::Rebond {
            tokens,
            bond_from,
            bond_to,
        } => execute_rebond(deps, env, info, tokens, bond_from, bond_to),
        ExecuteMsg::Unbond {
            tokens: amount,
            unbonding_period,
        } => execute_unbond(deps, env, info, amount, unbonding_period),
        ExecuteMsg::Claim {} => execute_claim(deps, env, info),
        ExecuteMsg::Receive(msg) => execute_receive_delegation(deps, env, info, msg),
        ExecuteMsg::DistributeRewards { sender } => {
            execute_distribute_rewards(deps, env, info, sender)
        }
        ExecuteMsg::WithdrawRewards { owner, receiver } => {
            execute_withdraw_rewards(deps, info, owner, receiver)
        }
        ExecuteMsg::DelegateWithdrawal { delegated } => {
            execute_delegate_withdrawal(deps, info, delegated)
        }
        ExecuteMsg::FundDistribution { curve } => execute_fund_distribution(env, deps, info, curve),
    }
}

/// Fund a previously created distribution flow with the given amount of native tokens.
/// Allows for providing multiple native tokens at once to update multiple distribution flows with the same optionally provided Curve.
pub fn execute_fund_distribution(
    env: Env,
    deps: DepsMut,
    info: MessageInfo,
    schedule: Curve,
) -> Result<Response, ContractError> {
    let api = deps.api;
    let storage = deps.storage;

    for fund in info.funds {
        let asset = AssetInfo::Native(fund.denom);
        let validated_asset = asset.validate(api)?;
        update_reward_config(
            &env,
            storage,
            validated_asset,
            fund.amount,
            schedule.clone(),
        )?;
    }
    Ok(Response::default())
}

/// Update reward config for the given asset with an additional amount of funding
fn update_reward_config(
    env: &Env,
    storage: &mut dyn Storage,
    validated_asset: AssetInfoValidated,
    amount: Uint128,
    schedule: Curve,
) -> Result<(), ContractError> {
    // How can we validate the amount and curve? Monotonic decreasing check is below, given this is there still a need to test the amount?
    let previous_reward_curve = REWARD_CURVE.load(storage, &validated_asset)?;
    let (min, max) = schedule.range();
    // Validate the the curve locks at most the amount provided and also fully unlocks all rewards sent
    if min != 0 || max > amount.u128() {
        return Err(ContractError::InvalidRewards {});
    }

    // Move the curve to the right, so as to not overlap with the past (could mess things up with previous withdrawals).
    // The idea here is that the person sending the reward can specify how many rewards are locked up until when.
    // However, every point on the rewards curve represents the rewards locked up at that point in time,
    // so in order to prevent them from influencing the rewards curve at a point in the past,
    // we shift it to the right by the current time.
    // They can then provide a curve starting at `0`, meaning "right now".
    let schedule = schedule.shift(env.block.time.seconds());
    // combine the two curves
    let new_reward_curve = previous_reward_curve.combine(&schedule);
    new_reward_curve.validate_monotonic_decreasing()?;

    REWARD_CURVE.save(storage, &validated_asset, &new_reward_curve)?;
    Ok(())
}

/// Create a new rewards distribution flow for the given asset as a reward
pub fn execute_create_distribution_flow(
    deps: DepsMut,
    info: MessageInfo,
    manager: String,
    asset: AssetInfo,
    rewards: Vec<(UnbondingPeriod, Decimal)>,
) -> Result<Response, ContractError> {
    // only admin can create distribution flow
    ADMIN.assert_admin(deps.as_ref(), &info.sender)?;

    // input validation
    let asset = asset.validate(deps.api)?;
    let manager = deps.api.addr_validate(&manager)?;

    // make sure the asset is not the staked token, since we distribute this contract's balance
    // and we definitely do not want to distribute the staked tokens.
    let config = CONFIG.load(deps.storage)?;
    if let AssetInfoValidated::Token(addr) = &asset {
        if addr == &config.cw20_contract {
            return Err(ContractError::InvalidAsset {});
        }
    }

    // validate rewards unbonding periods
    if rewards
        .iter()
        .map(|(period, _)| period)
        .ne(config.unbonding_periods.iter())
    {
        return Err(ContractError::InvalidRewards {});
    }
    // make sure rewards are monotonically increasing (equality is allowed)
    // this assumes that `config.unbonding_periods` (and therefore also `rewards`) is sorted (checked in instantiate)
    if rewards.windows(2).any(|w| w[0].1 > w[1].1) {
        return Err(ContractError::InvalidRewards {});
    }

    // make sure to respect the distribution count limit to create an upper bound for all the staking operations
    let keys = DISTRIBUTION
        .keys(deps.storage, None, None, Order::Ascending)
        .collect::<StdResult<Vec<_>>>()?;
    if keys.len() >= (config.max_distributions as usize) {
        return Err(ContractError::TooManyDistributions(
            config.max_distributions,
        ));
    }

    // make sure the distribution does not exist already
    if keys.contains(&asset) {
        return Err(ContractError::DistributionAlreadyExists(asset));
    }

    REWARD_CURVE.save(deps.storage, &asset, &Curve::constant(0))?;

    DISTRIBUTION.save(
        deps.storage,
        &asset,
        &Distribution {
            manager,
            reward_multipliers: rewards,
            shares_per_point: Uint128::zero(),
            shares_leftover: 0,
            distributed_total: Uint128::zero(),
            withdrawable_total: Uint128::zero(),
        },
    )?;

    Ok(Response::default())
}

pub fn execute_rebond(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    amount: Uint128,
    bond_from: u64,
    bond_to: u64,
) -> Result<Response, ContractError> {
    // Raise if no amount was provided
    if amount == Uint128::zero() {
        return Err(ContractError::NoRebondAmount {});
    }
    // Short out with an error if trying to rebond to itself
    if bond_from == bond_to {
        return Err(ContractError::SameUnbondingRebond {});
    }

    let cfg = CONFIG.load(deps.storage)?;

    if cfg.unbonding_periods.binary_search(&bond_from).is_err() {
        return Err(ContractError::NoUnbondingPeriodFound(bond_from));
    }
    if cfg.unbonding_periods.binary_search(&bond_to).is_err() {
        return Err(ContractError::NoUnbondingPeriodFound(bond_to));
    }

    let distributions: Vec<_> = DISTRIBUTION
        .range(deps.storage, None, None, Order::Ascending)
        .collect::<StdResult<Vec<_>>>()?;

    // calculate rewards power before updating the stake
    let old_rewards = calc_rewards_powers(deps.storage, &cfg, &info.sender, distributions.iter())?;

    // Reduce the bond_from
    let mut old_stake_from = Uint128::zero();
    let new_stake_from = STAKE
        .update(
            deps.storage,
            (&info.sender, bond_from),
            |bonding_info| -> StdResult<_> {
                let mut bonding_info = bonding_info.unwrap_or_default();
                old_stake_from = bonding_info.total_stake();
                // Release the stake, also accounting for locked tokens, raising if there is not enough tokens
                bonding_info.release_stake(&env, amount)?;
                Ok(bonding_info)
            },
        )?
        .total_stake();

    // Increase the bond_to
    let mut old_stake_to = Uint128::zero();
    let new_stake_to = STAKE
        .update(
            deps.storage,
            (&info.sender, bond_to),
            |bonding_info| -> StdResult<_> {
                let mut bonding_info = bonding_info.unwrap_or_default();
                old_stake_to = bonding_info.total_stake();

                if bond_from > bond_to {
                    bonding_info.add_locked_tokens(
                        env.block.time.plus_seconds(bond_from - bond_to),
                        amount,
                    );
                } else {
                    bonding_info.add_unlocked_tokens(amount);
                };
                Ok(bonding_info)
            },
        )?
        .total_stake();

    update_total_stake(
        deps.storage,
        &cfg,
        bond_from,
        old_stake_from,
        new_stake_from,
    )?;
    update_total_stake(deps.storage, &cfg, bond_to, old_stake_to, new_stake_to)?;

    // update the adjustment data for all distributions
    for ((asset_info, mut distribution), old_reward_power) in
        distributions.into_iter().zip(old_rewards.into_iter())
    {
        let new_reward_power = distribution.calc_rewards_power(deps.storage, &cfg, &info.sender)?;
        update_rewards(
            deps.storage,
            &asset_info,
            &info.sender,
            &mut distribution,
            old_reward_power,
            new_reward_power,
        )?;

        // save updated distribution
        DISTRIBUTION.save(deps.storage, &asset_info, &distribution)?;
    }

    Ok(Response::new()
        .add_attribute("action", "rebond")
        .add_attribute("amount", amount)
        .add_attribute("bond_from", bond_from.to_string())
        .add_attribute("bond_to", bond_to.to_string()))
}

pub fn execute_bond(
    deps: DepsMut,
    env: Env,
    sender_cw20_contract: Addr,
    amount: Uint128,
    unbonding_period: u64,
    sender: Addr,
) -> Result<Response, ContractError> {
    let delegations = vec![(sender.to_string(), amount)];
    let res = execute_mass_bond(
        deps,
        env,
        sender_cw20_contract,
        amount,
        unbonding_period,
        delegations,
    )?;
    Ok(res.add_attribute("sender", sender))
}

pub fn execute_mass_bond(
    deps: DepsMut,
    _env: Env,
    sender_cw20_contract: Addr,
    amount_sent: Uint128,
    unbonding_period: u64,
    delegate_to: Vec<(String, Uint128)>,
) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;

    // ensure that cw20 token contract's addresses matches
    if cfg.cw20_contract != sender_cw20_contract {
        return Err(ContractError::Cw20AddressesNotMatch {
            got: sender_cw20_contract.into(),
            expected: cfg.cw20_contract.into(),
        });
    }

    if cfg
        .unbonding_periods
        .binary_search(&unbonding_period)
        .is_err()
    {
        return Err(ContractError::NoUnbondingPeriodFound(unbonding_period));
    }

    // ensure total is <= amount sent
    let total = delegate_to.iter().map(|(_, x)| x).sum();
    if total > amount_sent {
        return Err(ContractError::MassDelegateTooMuch { total, amount_sent });
    }

    // update this for every user
    let mut distributions: Vec<_> = DISTRIBUTION
        .range(deps.storage, None, None, Order::Ascending)
        .collect::<StdResult<Vec<_>>>()?;

    // loop over all delegates, adding to their stake
    for (sender, amount) in delegate_to {
        let sender = deps.api.addr_validate(&sender)?;

        // calculate rewards power before updating the stake
        let old_rewards = calc_rewards_powers(deps.storage, &cfg, &sender, distributions.iter())?;

        // add to the sender's stake
        let mut old_stake = Uint128::zero();
        let new_stake = STAKE
            .update(
                deps.storage,
                (&sender, unbonding_period),
                |bonding_info| -> StdResult<_> {
                    let mut bonding_info = bonding_info.unwrap_or_default();
                    old_stake = bonding_info.total_stake();
                    bonding_info.add_unlocked_tokens(amount);
                    Ok(bonding_info)
                },
            )?
            .total_stake();

        update_total_stake(deps.storage, &cfg, unbonding_period, old_stake, new_stake)?;

        // update the adjustment data for all distributions
        distributions = distributions
            .into_iter()
            .zip(old_rewards.into_iter())
            .map(|((asset_info, mut distribution), old_reward_power)| {
                let new_reward_power =
                    distribution.calc_rewards_power(deps.storage, &cfg, &sender)?;
                update_rewards(
                    deps.storage,
                    &asset_info,
                    &sender,
                    &mut distribution,
                    old_reward_power,
                    new_reward_power,
                )?;
                Ok((asset_info, distribution))
            })
            .collect::<StdResult<Vec<_>>>()?;
    }

    // save all distributions (now updated)
    for (asset_info, distribution) in distributions {
        DISTRIBUTION.save(deps.storage, &asset_info, &distribution)?;
    }

    // update total after all individuals are handled
    TOTAL_STAKED.update::<_, StdError>(deps.storage, |token_info| {
        Ok(TokenInfo {
            staked: token_info.staked + amount_sent,
            unbonding: token_info.unbonding,
        })
    })?;

    Ok(Response::new()
        .add_attribute("action", "bond")
        .add_attribute("amount", amount_sent))
}

/// Updates the total stake for the given unbonding period
/// Make sure to always pass in the full old and new stake of one staker for the given unbonding period
fn update_total_stake(
    storage: &mut dyn Storage,
    cfg: &Config,
    unbonding_period: UnbondingPeriod,
    old_stake: Uint128,
    new_stake: Uint128,
) -> Result<(), ContractError> {
    // get current total stakes
    let mut totals = TOTAL_PER_PERIOD.load(storage)?;
    let total_idx = totals
        .binary_search_by(|(period, _)| period.cmp(&unbonding_period))
        .map_err(|_| ContractError::NoUnbondingPeriodFound(unbonding_period))?;
    let total = &mut totals[total_idx].1;

    // update the total amount staked in this unbonding period
    total.staked = if old_stake <= new_stake {
        total.staked.checked_add(new_stake - old_stake)?
    } else {
        total.staked.checked_sub(old_stake - new_stake)?
    };

    // Update the total of all stakes above min_bond.
    // Some variables and consts for readability
    let previously_above_min_bond = old_stake >= cfg.min_bond;
    let now_above_min_bond = new_stake >= cfg.min_bond;
    // Case distinction:
    match (previously_above_min_bond, now_above_min_bond) {
        (false, false) => {} // rewards power does not change, so do nothing
        (false, true) => {
            // stake was previously not counted, but should be now, so add new_stake to total
            total.powered_stake += new_stake;
        }
        (true, false) => {
            // stake was counted previously, but should not be now, so remove old_stake from total
            total.powered_stake -= old_stake;
        }
        (true, true) => {
            // stake was counted previously, but is different now, so add / remove difference to / from total
            if new_stake >= old_stake {
                total.powered_stake += new_stake - old_stake;
            } else {
                total.powered_stake -= old_stake - new_stake;
            }
        }
    }

    // save updated total
    TOTAL_PER_PERIOD.save(storage, &totals)?;

    Ok(())
}

pub fn execute_receive_delegation(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    wrapper: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    // info.sender is the address of the cw20 contract (that re-sent this message).
    // wrapper.sender is the address of the user that requested the cw20 contract to send this.
    // This cannot be fully trusted (the cw20 contract can fake it), so only use it for actions
    // in the address's favor (like paying/bonding tokens, not withdrawls)

    let msg: ReceiveDelegationMsg = from_slice(&wrapper.msg)?;
    let api = deps.api;
    match msg {
        ReceiveDelegationMsg::Delegate {
            unbonding_period,
            delegate_as,
        } => execute_bond(
            deps,
            env,
            info.sender,
            wrapper.amount,
            unbonding_period,
            api.addr_validate(&delegate_as.unwrap_or(wrapper.sender))?,
        ),
        ReceiveDelegationMsg::MassDelegate {
            unbonding_period,
            delegate_to,
        } => execute_mass_bond(
            deps,
            env,
            info.sender,
            wrapper.amount,
            unbonding_period,
            delegate_to,
        ),
        ReceiveDelegationMsg::Fund { curve } => {
            let validated_asset = AssetInfo::Token(info.sender.to_string()).validate(deps.api)?;
            update_reward_config(&env, deps.storage, validated_asset, wrapper.amount, curve)?;
            Ok(Response::default())
        }
    }
}

pub fn execute_unbond(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    amount: Uint128,
    unbonding_period: u64,
) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;

    if cfg
        .unbonding_periods
        .binary_search(&unbonding_period)
        .is_err()
    {
        return Err(ContractError::NoUnbondingPeriodFound(unbonding_period));
    }

    let distributions: Vec<_> = DISTRIBUTION
        .range(deps.storage, None, None, Order::Ascending)
        .collect::<StdResult<Vec<_>>>()?;
    // calculate rewards power before updating the stake
    let old_rewards = calc_rewards_powers(deps.storage, &cfg, &info.sender, distributions.iter())?;

    // reduce the sender's stake - aborting if insufficient
    let mut old_stake = Uint128::zero();
    let new_stake = STAKE
        .update(
            deps.storage,
            (&info.sender, unbonding_period),
            |bonding_info| -> StdResult<_> {
                let mut bonding_info = bonding_info.unwrap_or_default();
                old_stake = bonding_info.total_stake();
                bonding_info.release_stake(&env, amount)?;
                Ok(bonding_info)
            },
        )?
        .total_stake();

    update_total_stake(deps.storage, &cfg, unbonding_period, old_stake, new_stake)?;

    // update the adjustment data for all distributions
    for ((asset_info, mut distribution), old_reward_power) in
        distributions.into_iter().zip(old_rewards.into_iter())
    {
        let new_reward_power = distribution.calc_rewards_power(deps.storage, &cfg, &info.sender)?;
        update_rewards(
            deps.storage,
            &asset_info,
            &info.sender,
            &mut distribution,
            old_reward_power,
            new_reward_power,
        )?;

        // save updated distribution
        DISTRIBUTION.save(deps.storage, &asset_info, &distribution)?;
    }
    // update total
    TOTAL_STAKED.update::<_, StdError>(deps.storage, |token_info| {
        Ok(TokenInfo {
            staked: token_info.staked.saturating_sub(amount),
            unbonding: token_info.unbonding + amount,
        })
    })?;

    // provide them a claim
    CLAIMS.create_claim(
        deps.storage,
        &info.sender,
        amount,
        Expiration::AtTime(env.block.time.plus_seconds(unbonding_period)),
    )?;

    Ok(Response::new()
        .add_attribute("action", "unbond")
        .add_attribute("amount", amount)
        .add_attribute("sender", info.sender))
}

/// Calculates rewards power of the user for all given distributions (for all unbonding periods).
/// They are returned in the same order as the distributions.
fn calc_rewards_powers<'a>(
    storage: &dyn Storage,
    cfg: &Config,
    staker: &Addr,
    distributions: impl Iterator<Item = &'a (AssetInfoValidated, Distribution)>,
) -> StdResult<Vec<Uint128>> {
    // go through distributions and calculate old reward power for all of them
    let old_rewards = distributions
        .map(|(_, distribution)| {
            let old_reward_power = distribution.calc_rewards_power(storage, cfg, staker)?;
            Ok(old_reward_power)
        })
        .collect::<StdResult<Vec<_>>>()?;

    Ok(old_rewards)
}

fn update_rewards(
    storage: &mut dyn Storage,
    asset_info: &AssetInfoValidated,
    sender: &Addr,
    distribution: &mut Distribution,
    old_reward_power: Uint128,
    new_reward_power: Uint128,
) -> StdResult<()> {
    // short-circuit if no change
    if old_reward_power == new_reward_power {
        return Ok(());
    }

    // update their share of the distribution
    let ppw = distribution.shares_per_point.u128();
    let diff = new_reward_power.u128() as i128 - old_reward_power.u128() as i128;
    apply_points_correction(storage, sender, asset_info, ppw, diff)?;

    Ok(())
}

pub fn execute_claim(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let release = CLAIMS.claim_tokens(deps.storage, &info.sender, &env.block, None)?;
    if release.is_zero() {
        return Err(ContractError::NothingToClaim {});
    }

    let config = CONFIG.load(deps.storage)?;
    let amount_str = coin_to_string(release, config.cw20_contract.as_str());
    let undelegate = Cw20ExecuteMsg::Transfer {
        recipient: info.sender.to_string(),
        amount: release,
    };
    let undelegate_msg = SubMsg::new(WasmMsg::Execute {
        contract_addr: config.cw20_contract.to_string(),
        msg: to_binary(&undelegate)?,
        funds: vec![],
    });

    TOTAL_STAKED.update::<_, StdError>(deps.storage, |token_info| {
        Ok(TokenInfo {
            staked: token_info.staked,
            unbonding: token_info.unbonding.saturating_sub(release),
        })
    })?;

    Ok(Response::new()
        .add_submessage(undelegate_msg)
        .add_attribute("action", "claim")
        .add_attribute("tokens", amount_str)
        .add_attribute("sender", info.sender))
}

#[inline]
fn coin_to_string(amount: Uint128, address: &str) -> String {
    format!("{} {}", amount, address)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Claims { address } => {
            to_binary(&CLAIMS.query_claims(deps, &deps.api.addr_validate(&address)?)?)
        }
        QueryMsg::Staked {
            address,
            unbonding_period,
        } => to_binary(&query_staked(deps, &env, address, unbonding_period)?),
        QueryMsg::AnnualizedRewards {} => to_binary(&query_annualized_rewards(deps, env)?),
        QueryMsg::BondingInfo {} => to_binary(&query_bonding_info(deps)?),
        QueryMsg::AllStaked { address } => to_binary(&query_all_staked(deps, env, address)?),
        QueryMsg::TotalStaked {} => to_binary(&query_total_staked(deps)?),
        QueryMsg::TotalUnbonding {} => to_binary(&query_total_unbonding(deps)?),
        QueryMsg::Admin {} => to_binary(&ADMIN.query_admin(deps)?),
        QueryMsg::TotalRewardsPower {} => to_binary(&query_total_rewards(deps)?),
        QueryMsg::RewardsPower { address } => to_binary(&query_rewards(deps, address)?),
        QueryMsg::WithdrawableRewards { owner } => {
            to_binary(&query_withdrawable_rewards(deps, owner)?)
        }
        QueryMsg::DistributedRewards {} => to_binary(&query_distributed_rewards(deps)?),
        QueryMsg::UndistributedRewards {} => to_binary(&query_undistributed_rewards(deps, env)?),
        QueryMsg::Delegated { owner } => to_binary(&query_delegated(deps, owner)?),
        QueryMsg::DistributionData {} => to_binary(&query_distribution_data(deps)?),
        QueryMsg::WithdrawAdjustmentData { addr, asset } => {
            to_binary(&query_withdraw_adjustment_data(deps, addr, asset)?)
        }
    }
}

fn query_annualized_rewards(deps: Deps, env: Env) -> StdResult<AnnualizedRewardsResponse> {
    let distributions = DISTRIBUTION
        .range(deps.storage, None, None, Order::Ascending)
        .collect::<StdResult<Vec<_>>>()?;
    let config = CONFIG.load(deps.storage)?;

    let mut aprs = Vec::with_capacity(config.unbonding_periods.len());

    for &unbonding_period in &config.unbonding_periods {
        let mut rewards = Vec::with_capacity(distributions.len());
        for (asset_info, dist) in &distributions {
            let total_stake = load_total_of_period(deps.storage, unbonding_period)
                .unwrap()
                .powered_stake;
            let total_power = dist.total_rewards_power(deps.storage, &config);

            if total_stake.is_zero() || total_power.is_zero() {
                rewards.push(AnnualizedReward {
                    info: asset_info.clone(),
                    amount: None,
                });
                continue;
            }

            let power_of_period = dist
                .total_rewards_power_of_period(deps.storage, &config, unbonding_period)
                .unwrap();

            let reward_curve = REWARD_CURVE.load(deps.storage, asset_info)?;

            let rewards_per_year = (reward_curve.value(env.block.time.seconds())
                + reward_curve.value(env.block.time.seconds() + SECONDS_PER_YEAR))
                * Uint128::from(SECONDS_PER_YEAR).checked_div(Uint128::from(100u128))?;

            let period_rewards =
                Decimal::from_ratio(rewards_per_year * power_of_period, total_power);
            let rewards_per_token = period_rewards / total_stake;

            rewards.push(AnnualizedReward {
                info: asset_info.clone(),
                amount: Some(rewards_per_token),
            });
        }
        aprs.push((unbonding_period, rewards));
    }
    Ok(AnnualizedRewardsResponse { rewards: aprs })
}

fn query_rewards(deps: Deps, addr: String) -> StdResult<RewardsPowerResponse> {
    let addr = deps.api.addr_validate(&addr)?;
    let rewards = DISTRIBUTION
        .range(deps.storage, None, None, Order::Ascending)
        .map(|dist| {
            let (asset_info, distribution) = dist?;
            let cfg = CONFIG.load(deps.storage)?;

            distribution
                .calc_rewards_power(deps.storage, &cfg, &addr)
                .map(|power| (asset_info, power))
        })
        .filter(|dist| matches!(dist, Ok((_, power)) if !power.is_zero()))
        .collect::<StdResult<Vec<_>>>()?;

    Ok(RewardsPowerResponse { rewards })
}

fn query_total_rewards(deps: Deps) -> StdResult<RewardsPowerResponse> {
    Ok(RewardsPowerResponse {
        rewards: DISTRIBUTION
            .range(deps.storage, None, None, Order::Ascending)
            .map(|distr| {
                let (asset_info, distribution) = distr?;

                let cfg = CONFIG.load(deps.storage)?;
                Ok((
                    asset_info,
                    distribution.total_rewards_power(deps.storage, &cfg),
                ))
            })
            .collect::<StdResult<Vec<_>>>()?,
    })
}

fn query_bonding_info(deps: Deps) -> StdResult<BondingInfoResponse> {
    let total_stakes = TOTAL_PER_PERIOD.load(deps.storage)?;

    let bonding = total_stakes
        .into_iter()
        .map(|(unbonding_period, total_staked)| -> StdResult<_> {
            Ok(BondingPeriodInfo {
                unbonding_period,
                total_staked: total_staked.staked,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BondingInfoResponse { bonding })
}

pub fn query_staked(
    deps: Deps,
    env: &Env,
    addr: String,
    unbonding_period: u64,
) -> StdResult<StakedResponse> {
    let addr = deps.api.addr_validate(&addr)?;
    // sanity check if such unbonding period exists
    let totals = TOTAL_PER_PERIOD.load(deps.storage)?;
    totals
        .binary_search_by_key(&unbonding_period, |&(entry, _)| entry)
        .map_err(|_| {
            StdError::generic_err(format!("No unbonding period found: {}", unbonding_period))
        })?;

    let stake = STAKE
        .may_load(deps.storage, (&addr, unbonding_period))?
        .unwrap_or_default();
    let cw20_contract = CONFIG.load(deps.storage)?.cw20_contract.to_string();
    Ok(StakedResponse {
        stake: stake.total_stake(),
        total_locked: stake.total_locked(env),
        unbonding_period,
        cw20_contract,
    })
}

pub fn query_all_staked(deps: Deps, env: Env, addr: String) -> StdResult<AllStakedResponse> {
    let addr = deps.api.addr_validate(&addr)?;
    let config = CONFIG.load(deps.storage)?;
    let cw20_contract = config.cw20_contract.to_string();

    let stakes = config
        .unbonding_periods
        .into_iter()
        .filter_map(|up| match STAKE.may_load(deps.storage, (&addr, up)) {
            Ok(Some(stake)) => Some(Ok(StakedResponse {
                stake: stake.total_stake(),
                total_locked: stake.total_locked(&env),
                unbonding_period: up,
                cw20_contract: cw20_contract.clone(),
            })),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<StdResult<Vec<StakedResponse>>>()?;

    Ok(AllStakedResponse { stakes })
}

pub fn query_total_staked(deps: Deps) -> StdResult<TotalStakedResponse> {
    Ok(TotalStakedResponse {
        total_staked: TOTAL_STAKED.load(deps.storage).unwrap_or_default().staked,
    })
}

pub fn query_total_unbonding(deps: Deps) -> StdResult<TotalUnbondingResponse> {
    Ok(TotalUnbondingResponse {
        total_unbonding: TOTAL_STAKED
            .load(deps.storage)
            .unwrap_or_default()
            .unbonding,
    })
}

#[cfg(test)]
mod tests {
    use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info};
    use cosmwasm_std::{from_slice, Coin, CosmosMsg, Decimal, WasmMsg};
    use cw_controllers::Claim;
    use cw_utils::Duration;
    use sg_swap::asset::{native_asset_info, token_asset_info};

    use crate::error::ContractError;
    use crate::msg::{DistributionDataResponse, WithdrawAdjustmentDataResponse};
    use crate::state::{Distribution, WithdrawAdjustment};

    use super::*;

    const INIT_ADMIN: &str = "admin";
    const USER1: &str = "user1";
    const USER2: &str = "user2";
    const USER3: &str = "user3";
    const TOKENS_PER_POWER: Uint128 = Uint128::new(1_000);
    const MIN_BOND: Uint128 = Uint128::new(5_000);
    const UNBONDING_BLOCKS: u64 = 100;
    const UNBONDING_PERIOD: u64 = UNBONDING_BLOCKS / 5;
    const UNBONDING_PERIOD_2: u64 = 2 * UNBONDING_PERIOD;
    const CW20_ADDRESS: &str = "wasm1234567890";
    const CW721_ADDRESS: &str = "wasm1234567891";
    const DENOM: &str = "juno";

    #[test]
    fn check_crate_name() {
        assert_eq!(CONTRACT_NAME, "crates.io:sg_swap_stake");
    }

    fn default_instantiate(deps: DepsMut, env: Env) {
        cw20_instantiate(
            deps,
            env,
            TOKENS_PER_POWER,
            MIN_BOND,
            vec![UNBONDING_PERIOD],
        )
    }

    fn cw20_instantiate(
        deps: DepsMut,
        env: Env,
        tokens_per_power: Uint128,
        min_bond: Uint128,
        stake_config: Vec<UnbondingPeriod>,
    ) {
        let msg = InstantiateMsg {
            cw20_contract: CW20_ADDRESS.to_owned(),
            cw721_contract: CW721_ADDRESS.to_owned(),
            tokens_per_power,
            min_bond,
            unbonding_periods: stake_config,
            admin: Some(INIT_ADMIN.into()),
            max_distributions: 6,
        };
        let info = mock_info("creator", &[]);
        instantiate(deps, env, info, msg).unwrap();
    }

    fn bond_cw20_with_period(
        mut deps: DepsMut,
        user1: u128,
        user2: u128,
        user3: u128,
        unbonding_period: u64,
        time_delta: u64,
    ) {
        let mut env = mock_env();
        env.block.time = env.block.time.plus_seconds(time_delta);

        for (addr, stake) in &[(USER1, user1), (USER2, user2), (USER3, user3)] {
            if *stake != 0 {
                let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                    sender: addr.to_string(),
                    amount: Uint128::new(*stake),
                    msg: to_binary(&ReceiveDelegationMsg::Delegate {
                        unbonding_period,
                        delegate_as: None,
                    })
                    .unwrap(),
                });
                let info = mock_info(CW20_ADDRESS, &[]);
                execute(deps.branch(), env.clone(), info, msg).unwrap();
            }
        }
    }

    fn bond_cw20(deps: DepsMut, user1: u128, user2: u128, user3: u128, time_delta: u64) {
        bond_cw20_with_period(deps, user1, user2, user3, UNBONDING_PERIOD, time_delta);
    }

    fn rebond_with_period(
        mut deps: DepsMut,
        user1: u128,
        user2: u128,
        user3: u128,
        bond_from: u64,
        bond_to: u64,
        time_delta: u64,
    ) {
        let mut env = mock_env();
        env.block.time = env.block.time.plus_seconds(time_delta);

        for (addr, stake) in &[(USER1, user1), (USER2, user2), (USER3, user3)] {
            if *stake != 0 {
                let msg = ExecuteMsg::Rebond {
                    bond_from,
                    bond_to,
                    tokens: Uint128::new(*stake),
                };
                let info = mock_info(addr, &[]);
                execute(deps.branch(), env.clone(), info, msg).unwrap();
            }
        }
    }

    fn unbond_with_period(
        mut deps: DepsMut,
        user1: u128,
        user2: u128,
        user3: u128,
        time_delta: u64,
        unbonding_period: u64,
    ) {
        let mut env = mock_env();
        env.block.time = env.block.time.plus_seconds(time_delta);

        for (addr, stake) in &[(USER1, user1), (USER2, user2), (USER3, user3)] {
            if *stake != 0 {
                let msg = ExecuteMsg::Unbond {
                    tokens: Uint128::new(*stake),
                    unbonding_period,
                };
                let info = mock_info(addr, &[]);
                execute(deps.branch(), env.clone(), info, msg).unwrap();
            }
        }
    }

    fn unbond(deps: DepsMut, user1: u128, user2: u128, user3: u128, time_delta: u64) {
        unbond_with_period(deps, user1, user2, user3, time_delta, UNBONDING_PERIOD);
    }

    fn native(denom: &str) -> AssetInfoValidated {
        AssetInfoValidated::Native(denom.to_string())
    }

    #[test]
    fn proper_instantiation() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        default_instantiate(deps.as_mut(), env);

        // it worked, let's query the state
        let res = ADMIN.query_admin(deps.as_ref()).unwrap();
        assert_eq!(Some(INIT_ADMIN.into()), res.admin);

        // setup distribution flow
        execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![(UNBONDING_PERIOD, Decimal::percent(1))],
        )
        .unwrap();

        // make sure distribution logic is set up properly
        let raw = query(deps.as_ref(), mock_env(), QueryMsg::DistributionData {}).unwrap();
        let res: DistributionDataResponse = from_slice(&raw).unwrap();
        assert_eq!(
            res.distributions,
            vec![(
                AssetInfoValidated::Native(DENOM.to_string()),
                Distribution {
                    shares_per_point: Uint128::zero(),
                    shares_leftover: 0,
                    distributed_total: Uint128::zero(),
                    withdrawable_total: Uint128::zero(),
                    manager: Addr::unchecked(INIT_ADMIN),
                    reward_multipliers: vec![(UNBONDING_PERIOD, Decimal::percent(1))],
                }
            )]
        );

        let raw = query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::WithdrawAdjustmentData {
                addr: USER1.to_owned(),
                asset: native_asset_info(DENOM),
            },
        )
        .unwrap();
        let res: WithdrawAdjustmentDataResponse = from_slice(&raw).unwrap();
        assert_eq!(
            res,
            WithdrawAdjustment {
                shares_correction: 0,
                withdrawn_rewards: Uint128::zero(),
            }
        );
    }

    fn assert_stake_in_period(
        deps: Deps,
        env: &Env,
        user1_stake: u128,
        user2_stake: u128,
        user3_stake: u128,
        unbonding_period: u64,
    ) {
        let stake1 = query_staked(deps, env, USER1.into(), unbonding_period).unwrap();
        assert_eq!(stake1.stake.u128(), user1_stake);

        let stake2 = query_staked(deps, env, USER2.into(), unbonding_period).unwrap();
        assert_eq!(stake2.stake.u128(), user2_stake);

        let stake3 = query_staked(deps, env, USER3.into(), unbonding_period).unwrap();
        assert_eq!(stake3.stake.u128(), user3_stake);
    }

    // this tests the member queries
    fn assert_stake(
        deps: Deps,
        env: &Env,
        user1_stake: u128,
        user2_stake: u128,
        user3_stake: u128,
    ) {
        assert_stake_in_period(
            deps,
            env,
            user1_stake,
            user2_stake,
            user3_stake,
            UNBONDING_PERIOD,
        );
    }

    fn assert_cw20_undelegate(res: cosmwasm_std::Response, recipient: &str, amount: u128) {
        match &res.messages[0].msg {
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr,
                msg,
                funds,
            }) => {
                assert_eq!(contract_addr.as_str(), CW20_ADDRESS);
                assert_eq!(funds.len(), 0);
                let parsed: Cw20ExecuteMsg = from_slice(msg).unwrap();
                assert_eq!(
                    parsed,
                    Cw20ExecuteMsg::Transfer {
                        recipient: recipient.into(),
                        amount: Uint128::new(amount)
                    }
                );
            }
            _ => panic!("Must initiate undelegate!"),
        }
    }

    fn assert_native_rewards(
        response: Vec<(AssetInfoValidated, Uint128)>,
        expected: &[(&str, u128)],
        msg: &str,
    ) {
        assert_eq!(
            expected
                .iter()
                .map(|(denom, power)| (native(denom), Uint128::new(*power)))
                .collect::<Vec<_>>(),
            response,
            "{}",
            msg
        );
    }

    #[test]
    fn cw20_token_bond() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        cw20_instantiate(
            deps.as_mut(),
            env.clone(),
            TOKENS_PER_POWER,
            MIN_BOND,
            vec![UNBONDING_PERIOD],
        );

        // ensure it rounds down, and respects cut-off
        bond_cw20(deps.as_mut(), 12_000, 7_500, 4_000, 1);

        // Assert updated powers
        assert_stake(deps.as_ref(), &env, 12_000, 7_500, 4_000);
    }

    #[test]
    fn cw20_token_claim() {
        let unbonding_period: u64 = 20;

        let mut deps = mock_dependencies();
        let mut env = mock_env();
        let unbonding = Duration::Time(unbonding_period);
        cw20_instantiate(
            deps.as_mut(),
            env.clone(),
            TOKENS_PER_POWER,
            MIN_BOND,
            vec![unbonding_period],
        );

        // bond some tokens
        bond_cw20(deps.as_mut(), 20_000, 13_500, 500, 5);

        // unbond part
        unbond(deps.as_mut(), 7_900, 4_600, 0, unbonding_period);

        // Assert updated powers
        assert_stake(deps.as_ref(), &env, 12_100, 8_900, 500);

        // with proper claims
        env.block.time = env.block.time.plus_seconds(unbonding_period);
        let expires = unbonding.after(&env.block);
        assert_eq!(
            get_claims(deps.as_ref(), &Addr::unchecked(USER1)),
            vec![Claim::new(7_900, expires)]
        );

        // wait til they expire and get payout
        env.block.time = env.block.time.plus_seconds(unbonding_period);
        let res = execute(
            deps.as_mut(),
            env.clone(),
            mock_info(USER1, &[]),
            ExecuteMsg::Claim {},
        )
        .unwrap();
        assert_eq!(res.messages.len(), 1);

        assert_stake(deps.as_ref(), &env, 12_100, 8_900, 500);
        assert_cw20_undelegate(res, USER1, 7_900)
    }

    fn get_claims(deps: Deps, addr: &Addr) -> Vec<Claim> {
        CLAIMS.query_claims(deps, addr).unwrap().claims
    }

    #[test]
    fn unbond_claim_workflow() {
        let mut deps = mock_dependencies();
        let mut env = mock_env();
        default_instantiate(deps.as_mut(), env.clone());

        // create some data
        bond_cw20(deps.as_mut(), 12_000, 7_500, 4_000, 5);
        unbond(deps.as_mut(), 4_500, 2_600, 0, 10);
        env.block.time = env.block.time.plus_seconds(10);

        // check the claims for each user
        let expires = Duration::Time(UNBONDING_PERIOD).after(&env.block);
        assert_eq!(
            get_claims(deps.as_ref(), &Addr::unchecked(USER1)),
            vec![Claim::new(4_500, expires)]
        );
        assert_eq!(
            get_claims(deps.as_ref(), &Addr::unchecked(USER2)),
            vec![Claim::new(2_600, expires)]
        );
        assert_eq!(get_claims(deps.as_ref(), &Addr::unchecked(USER3)), vec![]);

        // do another unbond later on
        let mut env2 = mock_env();
        env2.block.time = env2.block.time.plus_seconds(22);
        unbond(deps.as_mut(), 0, 1_345, 1_500, 22);

        // with updated claims
        let expires2 = Duration::Time(UNBONDING_PERIOD).after(&env2.block);
        assert_eq!(
            get_claims(deps.as_ref(), &Addr::unchecked(USER1)),
            vec![Claim::new(4_500, expires)]
        );
        assert_eq!(
            get_claims(deps.as_ref(), &Addr::unchecked(USER2)),
            vec![Claim::new(2_600, expires), Claim::new(1_345, expires2)]
        );
        assert_eq!(
            get_claims(deps.as_ref(), &Addr::unchecked(USER3)),
            vec![Claim::new(1_500, expires2)]
        );

        // nothing can be withdrawn yet
        let err = execute(
            deps.as_mut(),
            env2,
            mock_info(USER1, &[]),
            ExecuteMsg::Claim {},
        )
        .unwrap_err();
        assert_eq!(err, ContractError::NothingToClaim {});

        // now mature first section, withdraw that
        let mut env3 = mock_env();
        env3.block.time = env3.block.time.plus_seconds(UNBONDING_PERIOD + 10);
        // first one can now release
        let res = execute(
            deps.as_mut(),
            env3.clone(),
            mock_info(USER1, &[]),
            ExecuteMsg::Claim {},
        )
        .unwrap();
        assert_cw20_undelegate(res, USER1, 4_500);

        // second releases partially
        let res = execute(
            deps.as_mut(),
            env3.clone(),
            mock_info(USER2, &[]),
            ExecuteMsg::Claim {},
        )
        .unwrap();
        assert_cw20_undelegate(res, USER2, 2_600);

        // but the third one cannot release
        let err = execute(
            deps.as_mut(),
            env3,
            mock_info(USER3, &[]),
            ExecuteMsg::Claim {},
        )
        .unwrap_err();
        assert_eq!(err, ContractError::NothingToClaim {});

        // claims updated properly
        assert_eq!(get_claims(deps.as_ref(), &Addr::unchecked(USER1)), vec![]);
        assert_eq!(
            get_claims(deps.as_ref(), &Addr::unchecked(USER2)),
            vec![Claim::new(1_345, expires2)]
        );
        assert_eq!(
            get_claims(deps.as_ref(), &Addr::unchecked(USER3)),
            vec![Claim::new(1_500, expires2)]
        );

        // add another few claims for 2
        unbond(deps.as_mut(), 0, 600, 0, 6 + UNBONDING_PERIOD);
        unbond(deps.as_mut(), 0, 1_005, 0, 10 + UNBONDING_PERIOD);

        // ensure second can claim all tokens at once
        let mut env4 = mock_env();
        env4.block.time = env4.block.time.plus_seconds(UNBONDING_PERIOD * 2 + 12);
        let res = execute(
            deps.as_mut(),
            env4,
            mock_info(USER2, &[]),
            ExecuteMsg::Claim {},
        )
        .unwrap();
        assert_cw20_undelegate(res, USER2, 2_950); // 1_345 + 600 + 1_005
        assert_eq!(get_claims(deps.as_ref(), &Addr::unchecked(USER2)), vec![]);
    }

    fn rewards(deps: Deps, user: &str) -> Vec<(AssetInfoValidated, Uint128)> {
        query_rewards(deps, user.to_string()).unwrap().rewards
    }

    #[test]
    fn rewards_saved() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        cw20_instantiate(
            deps.as_mut(),
            env,
            TOKENS_PER_POWER,
            MIN_BOND,
            vec![UNBONDING_PERIOD],
        );

        // create distribution flow to be able to receive rewards
        execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![(UNBONDING_PERIOD, Decimal::percent(1))],
        )
        .unwrap();

        // assert original rewards
        assert_eq!(rewards(deps.as_ref(), USER1), vec![]);
        assert_eq!(rewards(deps.as_ref(), USER2), vec![]);
        assert_eq!(rewards(deps.as_ref(), USER3), vec![]);

        // ensure it rounds down, and respects cut-off
        bond_cw20(deps.as_mut(), 1_200_000, 770_000, 4_000_000, 1);

        // assert updated rewards
        assert_native_rewards(
            rewards(deps.as_ref(), USER1),
            &[(DENOM, 12)],
            "1_200_000 * 1% / 1_000 = 12",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER2),
            &[(DENOM, 7)],
            "770_000 * 1% / 1_000 = 7",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER3),
            &[(DENOM, 40)],
            "4_000_000 * 1% / 1_000 = 40",
        );

        // unbond some tokens
        unbond(deps.as_mut(), 100_000, 99_600, 3_600_000, UNBONDING_PERIOD);

        assert_native_rewards(
            rewards(deps.as_ref(), USER1),
            &[(DENOM, 11)],
            "1_100_000 * 1% / 1_000 = 11",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER2),
            &[(DENOM, 6)],
            "600_955 * 1% / 1_000 = 6",
        );
        // USER3 has 400_000 left, this is above min_bound. But the rewards (4_000) would have been less
        assert_native_rewards(
            rewards(deps.as_ref(), USER3),
            &[(DENOM, 4)],
            "min_bound applied to stake (400_000), before reward multiplier (4_000)",
        );
    }

    #[test]
    fn rewards_rebonding() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        cw20_instantiate(
            deps.as_mut(),
            env.clone(),
            TOKENS_PER_POWER,
            Uint128::new(1000),
            vec![UNBONDING_PERIOD, UNBONDING_PERIOD_2],
        );

        // create distribution flow to be able to receive rewards
        execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![
                (UNBONDING_PERIOD, Decimal::percent(1)),
                (UNBONDING_PERIOD_2, Decimal::percent(10)),
            ],
        )
        .unwrap();

        // assert original rewards
        assert_eq!(rewards(deps.as_ref(), USER1), vec![]);
        assert_eq!(rewards(deps.as_ref(), USER2), vec![]);
        assert_eq!(rewards(deps.as_ref(), USER3), vec![]);

        // bond some tokens for first period
        bond_cw20(deps.as_mut(), 1_000_000, 180_000, 10_000, 1);

        // assert updated rewards
        assert_native_rewards(
            rewards(deps.as_ref(), USER1),
            &[(DENOM, 10)],
            "1_000_000 * 1% / 1_000 = 10",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER2),
            &[(DENOM, 1)],
            "180_000 * 1% / 1_000 = 1",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER3),
            &[],
            "10_000 * 1% = 100 < min_bond",
        );

        // bond some more tokens for second period
        bond_cw20_with_period(
            deps.as_mut(),
            1_000_000,
            100_000,
            9_000,
            UNBONDING_PERIOD_2,
            2,
        );

        // assert updated rewards
        assert_native_rewards(
            rewards(deps.as_ref(), USER1),
            &[(DENOM, 110)],
            "10 + 1_000_000 * 10% / 1_000 = 110",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER2),
            &[(DENOM, 11)],
            "1 + 100_000 * 10% / 1_000 = 11",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER3),
            &[],
            "0 + 9_000 * 10% = 900 < min_bond",
        );

        // rebond tokens
        rebond_with_period(
            deps.as_mut(),
            100_000,
            180_000,
            10_000,
            UNBONDING_PERIOD,
            UNBONDING_PERIOD_2,
            3,
        );

        // assert stake
        assert_stake(deps.as_ref(), &env, 900_000, 0, 0);
        assert_stake_in_period(
            deps.as_ref(),
            &env,
            1_100_000,
            280_000,
            19_000,
            UNBONDING_PERIOD_2,
        );
        // assert updated rewards
        assert_native_rewards(
            rewards(deps.as_ref(), USER1),
            &[(DENOM, 119)],
            "900_000 * 1% / 1_000 + 1_100_000 * 10% / 1_000 = 9 + 110 = 119",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER2),
            &[(DENOM, 28)],
            "0 + 280_000 * 10% / 1_000 = 28",
        );
        assert_native_rewards(
            rewards(deps.as_ref(), USER3),
            &[(DENOM, 1)],
            "0 + 19_000 * 10% / 1_000 = 1",
        );
    }

    #[test]
    fn ensure_bonding_edge_cases() {
        // use min_bond 0, tokens_per_power 500
        let mut deps = mock_dependencies();
        let env = mock_env();
        cw20_instantiate(
            deps.as_mut(),
            env,
            Uint128::new(100),
            Uint128::zero(),
            vec![UNBONDING_PERIOD],
        );

        // setting 50 tokens, gives us None power
        bond_cw20(deps.as_mut(), 50, 1, 102, 1);

        // reducing to 0 token makes us None even with min_bond 0
        unbond(deps.as_mut(), 49, 1, 102, 2);
    }

    #[test]
    fn test_query_bonding_info() {
        let mut deps = mock_dependencies();
        default_instantiate(deps.as_mut(), mock_env());

        let bonding_info_response = query_bonding_info(deps.as_ref()).unwrap();
        assert_eq!(
            bonding_info_response,
            BondingInfoResponse {
                bonding: vec!(BondingPeriodInfo {
                    unbonding_period: 20,
                    total_staked: Uint128::zero(),
                })
            }
        );
    }

    #[test]
    fn max_distribution_limit() {
        let mut deps = mock_dependencies();
        default_instantiate(deps.as_mut(), mock_env());

        // create distribution flows up to the maximum
        const DENOMS: [&str; 6] = ["a", "b", "c", "d", "e", "f"];
        for denom in &DENOMS {
            execute_create_distribution_flow(
                deps.as_mut(),
                mock_info(INIT_ADMIN, &[]),
                INIT_ADMIN.to_string(),
                native_asset_info(denom),
                vec![(UNBONDING_PERIOD, Decimal::one())],
            )
            .unwrap();
        }
        // next one should fail
        let err = execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![(UNBONDING_PERIOD, Decimal::one())],
        )
        .unwrap_err();
        assert_eq!(err, ContractError::TooManyDistributions(6));
    }

    #[test]
    fn distribution_already_exists() {
        let mut deps = mock_dependencies();
        default_instantiate(deps.as_mut(), mock_env());

        // create distribution flow
        execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![(UNBONDING_PERIOD, Decimal::one())],
        )
        .unwrap();

        // next one should fail
        let err = execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![(UNBONDING_PERIOD, Decimal::one())],
        )
        .unwrap_err();

        assert_eq!(
            err,
            ContractError::DistributionAlreadyExists(AssetInfoValidated::Native(
                "juno".to_string()
            ))
        );
    }

    #[test]
    fn distribute_unsupported_token_fails() {
        let mut deps = mock_dependencies();
        default_instantiate(deps.as_mut(), mock_env());

        // create distribution flow
        execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![(UNBONDING_PERIOD, Decimal::one())],
        )
        .unwrap();

        // call distribute, but send unsupported funds
        let unsupported_funds = Coin {
            denom: "unsupported".to_string(),
            amount: Uint128::new(100),
        };
        let err = execute_distribute_rewards(
            deps.as_mut(),
            mock_env(),
            mock_info(INIT_ADMIN, &[unsupported_funds.clone()]),
            None,
        )
        .unwrap_err();

        assert_eq!(err, ContractError::NoDistributionFlow(unsupported_funds));
    }

    #[test]
    fn cannot_distribute_staking_token() {
        let mut deps = mock_dependencies();
        default_instantiate(deps.as_mut(), mock_env());

        // try to create distribution flow for staking token
        let err = execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            token_asset_info(CW20_ADDRESS),
            vec![(UNBONDING_PERIOD, Decimal::one())],
        )
        .unwrap_err();

        assert_eq!(err, ContractError::InvalidAsset {});
    }

    #[test]
    fn cannot_distribute_staking_token_without_enough_per_block() {
        let mut deps = mock_dependencies();
        default_instantiate(deps.as_mut(), mock_env());

        // try to create distribution flow for staking token
        let _res = execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![(UNBONDING_PERIOD, Decimal::one())],
        )
        .unwrap();
        let err = execute_fund_distribution(
            mock_env(),
            deps.as_mut(),
            mock_info(
                INIT_ADMIN,
                &[Coin {
                    denom: DENOM.to_string(),
                    amount: Uint128::zero(),
                }],
            ),
            Curve::saturating_linear((0, 1), (100, 0)),
        )
        .unwrap_err();

        assert_eq!(err, ContractError::InvalidRewards {});
    }

    #[test]
    fn distribution_flow_wrong_unbonding_period_fails() {
        let mut deps = mock_dependencies();
        default_instantiate(deps.as_mut(), mock_env());

        // try to create distribution flow with wrong unbonding period
        let err = execute_create_distribution_flow(
            deps.as_mut(),
            mock_info(INIT_ADMIN, &[]),
            INIT_ADMIN.to_string(),
            native_asset_info(DENOM),
            vec![(UNBONDING_PERIOD + 1, Decimal::one())],
        )
        .unwrap_err();
        assert_eq!(err, ContractError::InvalidRewards {});
    }

    #[test]
    fn delegate_as_someone_else() {
        let mut deps = mock_dependencies();
        default_instantiate(deps.as_mut(), mock_env());

        execute_receive_delegation(
            deps.as_mut(),
            mock_env(),
            mock_info(CW20_ADDRESS, &[]),
            Cw20ReceiveMsg {
                sender: "delegator".to_string(),
                amount: 100u128.into(),
                msg: to_binary(&ReceiveDelegationMsg::Delegate {
                    unbonding_period: UNBONDING_PERIOD,
                    delegate_as: Some("owner_of_stake".to_string()),
                })
                .unwrap(),
            },
        )
        .unwrap();

        // owner_of_stake should have the stake
        let stake = query_staked(
            deps.as_ref(),
            &mock_env(),
            "owner_of_stake".to_string(),
            UNBONDING_PERIOD,
        )
        .unwrap()
        .stake
        .u128();
        assert_eq!(stake, 100u128);
    }
}
