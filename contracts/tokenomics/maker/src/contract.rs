use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use cosmwasm_std::{
    attr, entry_point, to_json_binary, Addr, Attribute, Binary, Decimal, Deps, DepsMut, Env,
    MessageInfo, Order, Response, StdError, StdResult, SubMsg, Uint128, Uint64,
};
use cw2::{set_contract_version};

use astroport::asset::{Asset, AssetInfo};
use astroport::common::{claim_ownership, drop_ownership_proposal, propose_new_owner};
use astroport::maker::{
    AssetWithLimit, BalancesResponse, Config, ConfigResponse, ExecuteMsg, InstantiateMsg,
    QueryMsg,
};
use astroport::pair::MAX_ALLOWED_SLIPPAGE;

use crate::error::ContractError;
use crate::state::{BRIDGES, CONFIG, LAST_COLLECT_TS, OWNERSHIP_PROPOSAL};
use crate::utils::{
    build_distribute_msg, build_send_msg, build_swap_msg, try_build_swap_msg,
    validate_bridge, validate_cooldown, BRIDGES_EXECUTION_MAX_DEPTH,
    BRIDGES_INITIAL_DEPTH,
};

/// Contract name that is used for migration.
const CONTRACT_NAME: &str = "asteroid-maker";
/// Contract version that is used for migration.
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Sets the default maximum spread (as a percentage) used when swapping fee tokens to ASTRO.
const DEFAULT_MAX_SPREAD: u64 = 5; // 5%

/// Creates a new contract with the specified parameters in [`InstantiateMsg`].
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    
    let max_spread = if let Some(max_spread) = msg.max_spread {
        if max_spread.is_zero() || max_spread.gt(&Decimal::from_str(MAX_ALLOWED_SLIPPAGE)?) {
            return Err(ContractError::IncorrectMaxSpread {});
        };

        max_spread
    } else {
        Decimal::percent(DEFAULT_MAX_SPREAD)
    };

    msg.roids_token.check(deps.api)?;

    if let Some(default_bridge) = &msg.default_bridge {
        default_bridge.check(deps.api)?
    }

    validate_cooldown(msg.collect_cooldown)?;
    LAST_COLLECT_TS.save(deps.storage, &env.block.time.seconds())?;

    let cfg = Config {
        owner: deps.api.addr_validate(&msg.owner)?,
        default_bridge: msg.default_bridge,
        roids_token: msg.roids_token,
        asteroid_contract: deps.api.addr_validate(&msg.asteroid_contract)?,
        factory_contract: deps.api.addr_validate(&msg.factory_contract)?,
        max_spread,
        collect_cooldown: msg.collect_cooldown,
    };

    CONFIG.save(deps.storage, &cfg)?;

    Ok(Response::default().add_attributes([
        attr("owner", msg.owner),
        attr(
            "default_bridge",
            cfg.default_bridge
                .map(|v| v.to_string())
                .unwrap_or_else(|| String::from("none")),
        ),
        attr("roids_token", cfg.roids_token.to_string()),
        attr("factory_contract", msg.factory_contract),
        attr(
            "asteroid_contract",
            msg.asteroid_contract,
        ),
        attr("max_spread", max_spread.to_string()),
    ]))
}

/// Exposes execute functions available in the contract.
///
/// ## Variants
/// * **ExecuteMsg::Collect { assets }** Swaps collected fee tokens to ROIDS
/// and transfers the ROIDS to the Hub burn address
///
/// * **ExecuteMsg::UpdateConfig {
///             factory_contract,
///             max_spread,
///         }** Updates general contract settings stores in the [`Config`].
///
/// * **ExecuteMsg::UpdateBridges { add, remove }** Adds or removes bridge assets used to swap fee tokens to ASTRO.
///
/// * **ExecuteMsg::SwapBridgeAssets { assets }** Swap fee tokens (through bridges) to ASTRO.
///
/// * **ExecuteMsg::DistributeAstro {}** Private method used by the contract to distribute ASTRO rewards.
///
/// * **ExecuteMsg::ProposeNewOwner { owner, expires_in }** Creates a new request to change contract ownership.
///
/// * **ExecuteMsg::DropOwnershipProposal {}** Removes a request to change contract ownership.
///
/// * **ExecuteMsg::ClaimOwnership {}** Claims contract ownership.
///
/// * **ExecuteMsg::EnableRewards** Enables collected ASTRO (pre Maker upgrade) to be distributed to xASTRO stakers.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Collect { assets } => collect(deps, env, assets),
        ExecuteMsg::UpdateConfig {
            factory_contract,
            basic_asset,
            max_spread,
            collect_cooldown,
            roids_token,
            asteroid_contract,
        } => update_config(
            deps,
            info,
            factory_contract,
            basic_asset,
            max_spread,
            collect_cooldown,
            roids_token,
            asteroid_contract,
        ),
        ExecuteMsg::UpdateBridges { add, remove } => update_bridges(deps, info, add, remove),
        ExecuteMsg::SwapBridgeAssets { assets, depth } => {
            swap_bridge_assets(deps, env, info, assets, depth)
        }
        ExecuteMsg::DistributeAstro {} => distribute_astro(deps, env, info),
        ExecuteMsg::ProposeNewOwner { owner, expires_in } => {
            let config: Config = CONFIG.load(deps.storage)?;

            propose_new_owner(
                deps,
                info,
                env,
                owner,
                expires_in,
                config.owner,
                OWNERSHIP_PROPOSAL,
            )
            .map_err(Into::into)
        }
        ExecuteMsg::DropOwnershipProposal {} => {
            let config: Config = CONFIG.load(deps.storage)?;

            drop_ownership_proposal(deps, info, config.owner, OWNERSHIP_PROPOSAL)
                .map_err(Into::into)
        }
        ExecuteMsg::ClaimOwnership {} => {
            claim_ownership(deps, info, env, OWNERSHIP_PROPOSAL, |deps, new_owner| {
                CONFIG.update::<_, StdError>(deps.storage, |mut v| {
                    v.owner = new_owner;
                    Ok(v)
                })?;

                Ok(())
            })
            .map_err(Into::into)
        }
    }
}

/// Swaps fee tokens to ROIDS and distribute the resulting ROIDS to the Hub burn address.
///
/// * **assets** array with fee tokens being swapped to ROIDS.
fn collect(
    deps: DepsMut,
    env: Env,
    assets: Vec<AssetWithLimit>,
) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;

    // Allowing collect only once per cooldown period
    LAST_COLLECT_TS.update(deps.storage, |last_ts| match cfg.collect_cooldown {
        Some(cd_period) if env.block.time.seconds() < last_ts + cd_period => {
            Err(ContractError::Cooldown {
                next_collect_ts: last_ts + cd_period,
            })
        }
        _ => Ok(env.block.time.seconds()),
    })?;

    let roids = cfg.roids_token.clone();

    // Check for duplicate assets
    let mut uniq = HashSet::new();
    if !assets
        .clone()
        .into_iter()
        .all(|a| uniq.insert(a.info.to_string()))
    {
        return Err(ContractError::DuplicatedAsset {});
    }

    // let response = Response::default();

    // Swap all non ROIDS tokens
    let (mut response, bridge_assets) = swap_assets(
        deps.as_ref(),
        &env.contract.address,
        &cfg,
        assets.into_iter().filter(|a| a.info.ne(&roids)).collect(),
    )?;

    // // If no swap messages - send ROIDS directly to x/vxASTRO stakers
    // if response.messages.is_empty() {
    //     let (mut distribute_msg, attributes) = distribute(deps, env, &mut cfg)?;
    //     if !distribute_msg.is_empty() {
    //         response.messages.append(&mut distribute_msg);
    //         response = response.add_attributes(attributes);
    //     }
    // } else {
    //     response.messages.push(build_distribute_msg(
    //         env,
    //         bridge_assets,
    //         BRIDGES_INITIAL_DEPTH,
    //     )?);
    // }

    Ok(response.add_attribute("action", "collect"))
}

/// This enum describes available token types that can be used as a SwapTarget.
enum SwapTarget {
    Roids(SubMsg),
    Bridge { asset: AssetInfo, msg: SubMsg },
}

/// Swap all non ASTRO tokens to ASTRO.
///
/// * **contract_addr** maker contract address.
///
/// * **assets** array with assets to swap to ASTRO.
///
/// * **with_validation** whether the swap operation should be validated or not.
fn swap_assets(
    deps: Deps,
    contract_addr: &Addr,
    cfg: &Config,
    assets: Vec<AssetWithLimit>,
) -> Result<(Response, Vec<AssetInfo>), ContractError> {
    let mut response = Response::default();
    let mut bridge_assets = HashMap::new();

    for a in assets {
        // Get balance
        let mut balance = a.info.query_pool(&deps.querier, contract_addr)?;
        if let Some(limit) = a.limit {
            if limit < balance && limit > Uint128::zero() {
                balance = limit;
            }
        }

        if !balance.is_zero() {
            match swap(deps, cfg, a.info, balance)? {
                SwapTarget::Roids(msg) => {
                    response.messages.push(msg);
                }
                SwapTarget::Bridge { asset, msg } => {
                    response.messages.push(msg);
                    bridge_assets.insert(asset.to_string(), asset);
                }
            }
        }
    }

    Ok((response, bridge_assets.into_values().collect()))
}

/// Checks if all required pools and bridges exists and performs a swap operation to ASTRO.
///
/// * **from_token** token to swap to ASTRO.
///
/// * **amount_in** amount of tokens to swap.
fn swap(
    deps: Deps,
    cfg: &Config,
    from_token: AssetInfo,
    amount_in: Uint128,
) -> Result<SwapTarget, ContractError> {
    // 1. Check if bridge tokens exist
    let bridge_token = BRIDGES.load(deps.storage, from_token.to_string());
    if let Ok(bridge_token) = bridge_token {
        let bridge_pool = validate_bridge(
            deps,
            &cfg.factory_contract,
            &from_token,
            &bridge_token,
            &cfg.roids_token,
            BRIDGES_INITIAL_DEPTH,
        )?;

        let msg = build_swap_msg(
            cfg.max_spread,
            &bridge_pool,
            &from_token,
            Some(&bridge_token),
            amount_in,
        )?;

        let swap_msg = if bridge_token == cfg.roids_token {
            SwapTarget::Roids(msg)
        } else {
            SwapTarget::Bridge {
                asset: bridge_token,
                msg,
            }
        };
        return Ok(swap_msg);
    }

    // 2. Check for a pair with a default bridge
    if let Some(default_bridge) = &cfg.default_bridge {
        if from_token.ne(default_bridge) {
            let swap_to_default =
                try_build_swap_msg(&deps.querier, cfg, &from_token, default_bridge, amount_in);
            if let Ok(msg) = swap_to_default {
                return Ok(SwapTarget::Bridge {
                    asset: default_bridge.clone(),
                    msg,
                });
            }
        }
    }

    // 3. Check for a direct pair with ROIDS
    let swap_to_astro =
        try_build_swap_msg(&deps.querier, cfg, &from_token, &cfg.roids_token, amount_in);
    if let Ok(msg) = swap_to_astro {
        return Ok(SwapTarget::Roids(msg));
    }

    Err(ContractError::CannotSwap(from_token))
}

/// Swaps collected fees using bridge assets.
///
/// * **assets** array with fee tokens to swap as well as amount of tokens to swap.
///
/// * **depth** maximum route length used to swap a fee token.
///
/// ## Executor
/// Only the Maker contract itself can execute this.
fn swap_bridge_assets(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    assets: Vec<AssetInfo>,
    depth: u64,
) -> Result<Response, ContractError> {
    if info.sender != env.contract.address {
        return Err(ContractError::Unauthorized {});
    }

    if assets.is_empty() {
        return Ok(Response::default());
    }

    // Check that the contract doesn't call itself endlessly
    if depth >= BRIDGES_EXECUTION_MAX_DEPTH {
        return Err(ContractError::MaxBridgeDepth(depth));
    }

    let cfg = CONFIG.load(deps.storage)?;

    let bridges = assets
        .into_iter()
        .map(|a| AssetWithLimit {
            info: a,
            limit: None,
        })
        .collect();

    let (response, bridge_assets) =
        swap_assets(deps.as_ref(), &env.contract.address, &cfg, bridges)?;

    // There should always be some messages, if there are none - something went wrong
    if response.messages.is_empty() {
        return Err(ContractError::Std(StdError::generic_err(
            "Empty swap messages",
        )));
    }

    Ok(response
        .add_submessage(build_distribute_msg(env, bridge_assets, depth + 1)?)
        .add_attribute("action", "swap_bridge_assets"))
}

/// Distributes ASTRO rewards to x/vxASTRO holders.
///
/// ## Executor
/// Only the Maker contract itself can execute this.
fn distribute_astro(deps: DepsMut, env: Env, info: MessageInfo) -> Result<Response, ContractError> {
    if info.sender != env.contract.address {
        return Err(ContractError::Unauthorized {});
    }

    let mut cfg = CONFIG.load(deps.storage)?;
    let (distribute_msg, attributes) = distribute(deps, env, &mut cfg)?;
    if distribute_msg.is_empty() {
        return Ok(Response::default());
    }

    Ok(Response::default()
        .add_submessages(distribute_msg)
        .add_attributes(attributes))
}

type DistributeMsgParts = (Vec<SubMsg>, Vec<Attribute>);

/// Private function that performs the ASTRO token distribution to x/vxASTRO.
fn distribute(
    deps: DepsMut,
    env: Env,
    cfg: &mut Config,
) -> Result<DistributeMsgParts, ContractError> {
    let mut result = vec![];
    let mut attributes = vec![];

    let mut amount = cfg
        .roids_token
        .query_pool(&deps.querier, &env.contract.address)?;
    if amount.is_zero() {
        return Ok((result, attributes));
    }
    
    // if !amount.is_zero() {
    //         result.push(SubMsg::new(build_send_msg(
    //             &Asset {
    //                 info: cfg.astro_token.clone(),
    //                 amount,
    //             },
    //             governance_contract.to_string(),
    //             None,
    //         )?))
    //     }

    attributes = vec![
        attr("action", "distribute_roids"),
    ];
    
    Ok((result, attributes))
}

/// Updates general contract parameters.
///
/// * **factory_contract** address of the factory contract.
///
/// * **staking_contract** address of the xASTRO staking contract.
///
/// * **governance_contract** address of the vxASTRO fee distributor contract.
///
/// * **governance_percent** percentage of ASTRO that goes to the vxASTRO fee distributor.
///
/// * **default_bridge_opt** default bridge asset used for intermediate swaps to ASTRO.
///
/// * **max_spread** max spread used when swapping fee tokens to ASTRO.
///
/// * **second_receiver_params** describes the second receiver of fees
///
/// ## Executor
/// Only the owner can execute this.
#[allow(clippy::too_many_arguments)]
fn update_config(
    deps: DepsMut,
    info: MessageInfo,
    factory_contract: Option<String>,
    default_bridge_opt: Option<AssetInfo>,
    max_spread: Option<Decimal>,
    collect_cooldown: Option<u64>,
    roids_token: Option<AssetInfo>,
    asteroid_contract: Option<String>,
) -> Result<Response, ContractError> {
    let mut attributes = vec![attr("action", "set_config")];

    let mut config = CONFIG.load(deps.storage)?;

    // Permission check
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if let Some(factory_contract) = factory_contract {
        config.factory_contract = deps.api.addr_validate(&factory_contract)?;
        attributes.push(attr("factory_contract", &factory_contract));
    };

    if let Some(asteroid_contract) = asteroid_contract {
        config.asteroid_contract = deps.api.addr_validate(&asteroid_contract)?;
        attributes.push(attr("asteroid_contract", &asteroid_contract));
    };

    if let Some(default_bridge) = &default_bridge_opt {
        default_bridge.check(deps.api)?;
        attributes.push(attr("default_bridge", default_bridge.to_string()));
        config.default_bridge = default_bridge_opt;
    }

    if let Some(max_spread) = max_spread {
        if max_spread.is_zero() || max_spread > Decimal::from_str(MAX_ALLOWED_SLIPPAGE)? {
            return Err(ContractError::IncorrectMaxSpread {});
        };

        config.max_spread = max_spread;
        attributes.push(attr("max_spread", max_spread.to_string()));
    };

    if let Some(collect_cooldown) = collect_cooldown {
        validate_cooldown(Some(collect_cooldown))?;
        config.collect_cooldown = Some(collect_cooldown);
        attributes.push(attr("collect_cooldown", collect_cooldown.to_string()));
    }

    if let Some(roids_token) = roids_token {
        roids_token.check(deps.api)?;
        attributes.push(attr("new_roids_token", roids_token.to_string()));
        config.roids_token = roids_token;
    }

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(attributes))
}

/// Adds or removes bridge tokens used to swap fee tokens to ASTRO.
///
/// * **add** array of bridge tokens added to swap fee tokens with.
///
/// * **remove** array of bridge tokens removed from being used to swap certain fee tokens.
///
/// ## Executor
/// Only the owner can execute this.
fn update_bridges(
    deps: DepsMut,
    info: MessageInfo,
    add: Option<Vec<(AssetInfo, AssetInfo)>>,
    remove: Option<Vec<AssetInfo>>,
) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;

    // Permission check
    if info.sender != cfg.owner {
        return Err(ContractError::Unauthorized {});
    }

    // Remove old bridges
    if let Some(remove_bridges) = remove {
        for asset in remove_bridges {
            BRIDGES.remove(deps.storage, asset.to_string());
        }
    }

    // Add new bridges
    let astro = cfg.roids_token.clone();
    if let Some(add_bridges) = add {
        for (asset, bridge) in add_bridges {
            if asset.equal(&bridge) {
                return Err(ContractError::InvalidBridge(asset, bridge));
            }

            // Check that bridge tokens can be swapped to ASTRO
            validate_bridge(
                deps.as_ref(),
                &cfg.factory_contract,
                &asset,
                &bridge,
                &astro,
                BRIDGES_INITIAL_DEPTH,
            )?;

            BRIDGES.save(deps.storage, asset.to_string(), &bridge)?;
        }
    }

    Ok(Response::default().add_attribute("action", "update_bridges"))
}

/// Exposes all the queries available in the contract.
///
/// ## Queries
/// * **QueryMsg::Config {}** Returns the Maker contract configuration using a [`ConfigResponse`] object.
///
/// * **QueryMsg::Balances { assets }** Returns the balances of certain fee tokens accrued by the Maker
/// using a [`ConfigResponse`] object.
///
/// * **QueryMsg::Bridges {}** Returns the bridges used for swapping fee tokens
/// using a vector of [`(String, String)`] denoting Asset -> Bridge connections.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_get_config(deps)?),
        QueryMsg::Balances { assets } => to_json_binary(&query_get_balances(deps, env, assets)?),
        QueryMsg::Bridges {} => to_json_binary(&query_bridges(deps)?),
    }
}

/// Returns information about the Maker configuration using a [`ConfigResponse`] object.
fn query_get_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        owner: config.owner,
        factory_contract: config.factory_contract,
        asteroid_contract: config.asteroid_contract,
        roids_token: config.roids_token,
        max_spread: config.max_spread,
        default_bridge: config.default_bridge,
    })
}

/// Returns Maker's fee token balances for specific tokens using a [`BalancesResponse`] object.
///
/// * **assets** array with assets for which we query the Maker's balances.
fn query_get_balances(deps: Deps, env: Env, assets: Vec<AssetInfo>) -> StdResult<BalancesResponse> {
    let mut resp = BalancesResponse { balances: vec![] };

    for a in assets {
        // Get balance
        let balance = a.query_pool(&deps.querier, &env.contract.address)?;
        if !balance.is_zero() {
            resp.balances.push(Asset {
                info: a,
                amount: balance,
            })
        }
    }

    Ok(resp)
}

/// Returns bridge tokens used for swapping fee tokens to ASTRO.
fn query_bridges(deps: Deps) -> StdResult<Vec<(String, String)>> {
    BRIDGES
        .range(deps.storage, None, None, Order::Ascending)
        .map(|bridge| {
            let (bridge, asset) = bridge?;
            Ok((bridge, asset.to_string()))
        })
        .collect()
}
