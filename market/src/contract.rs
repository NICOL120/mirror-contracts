use cosmwasm_std::{
    log, to_binary, to_vec, Api, BankMsg, Binary, CanonicalAddr, Coin, CosmosMsg, Decimal, Env,
    Extern, HandleResponse, HumanAddr, InitResponse, Querier, StdError, StdResult, Storage,
    Uint128, WasmMsg,
};

use crate::msg::{
    ConfigAssetResponse, ConfigGeneralResponse, ConfigSwapResponse, HandleMsg, InitMsg,
    PoolResponse, ProviderResponse, QueryMsg, ReverseSimulationResponse, SimulationResponse,
    SwapOperation,
};

use crate::math::{decimal_multiplication, decimal_subtraction, reverse_decimal};
use crate::querier::{load_balance, load_price, load_supply, load_token_balance};

use cw20::Cw20HandleMsg;
use terra_cosmwasm::TerraQuerier;

use crate::state::{
    provider_share_read, provider_share_store, read_config_asset, read_config_general,
    read_config_swap, store_config_asset, store_config_general, store_config_swap, ConfigAsset,
    ConfigGeneral, ConfigSwap,
};

pub fn init<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    msg: InitMsg,
) -> StdResult<InitResponse> {
    if msg.max_minus_spread > Decimal::one()
        || msg.max_minus_spread > Decimal::one()
        || msg.active_commission > Decimal::one()
        || msg.inactive_commission > Decimal::one()
    {
        return Err(StdError::generic_err("rate cannot be bigger than one"));
    }

    let config_general = ConfigGeneral {
        owner: deps.api.canonical_address(&env.message.sender)?,
        contract_addr: deps.api.canonical_address(&env.contract.address)?,
        liquidity_token: deps.api.canonical_address(&msg.liquidity_token)?,
        commission_collector: deps.api.canonical_address(&msg.commission_collector)?,
        collateral_denom: msg.collateral_denom,
    };

    let config_swap = ConfigSwap {
        active_commission: msg.active_commission,
        inactive_commission: msg.inactive_commission,
        max_spread: msg.max_spread,
        max_minus_spread: msg.max_minus_spread,
    };

    let config_asset = ConfigAsset {
        oracle: deps.api.canonical_address(&msg.asset_oracle)?,
        token: deps.api.canonical_address(&msg.asset_token)?,
        symbol: msg.asset_symbol.to_string(),
    };

    store_config_general(&mut deps.storage, &config_general)?;
    store_config_swap(&mut deps.storage, &config_swap)?;
    store_config_asset(&mut deps.storage, &config_asset)?;

    Ok(InitResponse::default())
}

pub fn handle<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    msg: HandleMsg,
) -> StdResult<HandleResponse> {
    match msg {
        HandleMsg::UpdateConfig {
            owner,
            active_commission,
            inactive_commission,
            max_minus_spread,
            max_spread,
        } => try_update_config(
            deps,
            env,
            owner,
            active_commission,
            inactive_commission,
            max_minus_spread,
            max_spread,
        ),
        HandleMsg::ProvideLiquidity { coins } => try_provide_liquidity(deps, env, coins),
        HandleMsg::WithdrawLiquidity { amount } => try_withdraw_liquidity(deps, env, amount),
        HandleMsg::Buy { max_spread } => try_buy(deps, env, max_spread),
        HandleMsg::Sell { amount, max_spread } => try_sell(deps, env, amount, max_spread),
    }
}

pub fn try_update_config<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    owner: Option<HumanAddr>,
    active_commission: Option<Decimal>,
    inactive_commission: Option<Decimal>,
    max_minus_spread: Option<Decimal>,
    max_spread: Option<Decimal>,
) -> StdResult<HandleResponse> {
    let mut config_general: ConfigGeneral = read_config_general(&deps.storage)?;
    let mut config_swap: ConfigSwap = read_config_swap(&deps.storage)?;

    // permission check
    if deps.api.canonical_address(&env.message.sender)? != config_general.owner {
        return Err(StdError::unauthorized());
    }

    if let Some(owner) = owner {
        config_general.owner = deps.api.canonical_address(&owner)?;
    }

    if let Some(active_commission) = active_commission {
        if active_commission > Decimal::one() {
            return Err(StdError::generic_err("rate cannot be bigger than one"));
        }
        config_swap.active_commission = active_commission;
    }

    if let Some(inactive_commission) = inactive_commission {
        if inactive_commission > Decimal::one() {
            return Err(StdError::generic_err("rate cannot be bigger than one"));
        }

        config_swap.inactive_commission = inactive_commission;
    }

    if let Some(max_minus_spread) = max_minus_spread {
        if max_minus_spread > Decimal::one() {
            return Err(StdError::generic_err("rate cannot be bigger than one"));
        }

        config_swap.max_minus_spread = max_minus_spread;
    }

    if let Some(max_spread) = max_spread {
        if max_spread > Decimal::one() {
            return Err(StdError::generic_err("rate cannot be bigger than one"));
        }

        config_swap.max_spread = max_spread;
    }

    store_config_swap(&mut deps.storage, &config_swap)?;
    store_config_general(&mut deps.storage, &config_general)?;

    Ok(HandleResponse {
        messages: vec![],
        log: vec![log("action", "update_config")],
        data: None,
    })
}

/// CONTRACT - should approve contract to use the amount of token
pub fn try_provide_liquidity<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    coins: Vec<Coin>,
) -> StdResult<HandleResponse> {
    let provider_addr_raw: CanonicalAddr = deps.api.canonical_address(&env.message.sender)?;
    let config_general: ConfigGeneral = read_config_general(&deps.storage)?;
    let config_asset: ConfigAsset = read_config_asset(&deps.storage)?;

    // check the collateral amount
    let collateral_amount = amount_of(&coins, config_general.collateral_denom.to_string());
    let asset_amount = amount_of(&coins, config_asset.symbol.to_string());
    let sent_collateral_amount = amount_of(
        &env.message.sent_funds,
        config_general.collateral_denom.to_string(),
    );
    if collateral_amount != sent_collateral_amount {
        return Err(StdError::generic_err(
            "Collateral amount missmatch between the argument and the transferred",
        ));
    }

    let price: Decimal = load_price(
        &deps,
        &deps.api.human_address(&config_asset.oracle)?,
        Some(env.block.time),
    )?;

    // calculate share amount
    let asset_value = asset_amount * price;
    let share = if asset_value > collateral_amount {
        collateral_amount
    } else {
        asset_value
    };

    // increase share
    let current_share = provider_share_read(&deps.storage, &provider_addr_raw)?;
    provider_share_store(&mut deps.storage).set(
        provider_addr_raw.as_slice(),
        &to_vec(&(share + current_share))?,
    );

    // update total share
    Ok(HandleResponse {
        messages: vec![
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: deps.api.human_address(&config_asset.token)?,
                msg: to_binary(&Cw20HandleMsg::TransferFrom {
                    owner: env.message.sender.clone(),
                    recipient: env.contract.address,
                    amount: asset_amount,
                })?,
                send: vec![],
            }),
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: deps.api.human_address(&config_general.liquidity_token)?,
                msg: to_binary(&Cw20HandleMsg::Mint {
                    recipient: env.message.sender,
                    amount: share,
                })?,
                send: vec![],
            }),
        ],
        log: vec![
            log("action", "provide_liquidity"),
            log("coins", &(coins_to_string(coins))),
            log("share", &share),
        ],
        data: None,
    })
}

pub fn try_withdraw_liquidity<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    amount: Uint128,
) -> StdResult<HandleResponse> {
    let provider_addr_raw: CanonicalAddr = deps.api.canonical_address(&env.message.sender)?;

    let config_general: ConfigGeneral = read_config_general(&deps.storage)?;
    let config_asset: ConfigAsset = read_config_asset(&deps.storage)?;
    let asset_addr: HumanAddr = deps.api.human_address(&config_asset.token)?;
    let liquidity_addr: HumanAddr = deps.api.human_address(&config_general.liquidity_token)?;

    let share: Uint128 = provider_share_read(&deps.storage, &provider_addr_raw)?;
    if share < amount {
        return Err(StdError::generic_err(
            "Can't withdraw more than you provided",
        ));
    }

    let total_share: Uint128 = load_supply(&deps, &liquidity_addr)?;
    let asset_pool: Uint128 =
        load_token_balance(&deps, &asset_addr, &config_general.contract_addr)?;
    let collateral_pool: Uint128 = load_balance(
        &deps,
        &env.contract.address,
        config_general.collateral_denom.to_string(),
    )?;

    let share_ratio: Decimal = Decimal::from_ratio(amount, total_share);
    let refund_collateral_amount: Uint128 = collateral_pool * share_ratio;
    let refund_asset_amount: Uint128 = asset_pool * share_ratio;

    // update provider share
    if share == amount {
        provider_share_store(&mut deps.storage).remove(provider_addr_raw.as_slice());
    } else {
        provider_share_store(&mut deps.storage)
            .set(provider_addr_raw.as_slice(), &to_vec(&(share - amount)?)?)
    }

    // update pool info
    let asset_addr: HumanAddr = deps.api.human_address(&config_asset.token)?;
    Ok(HandleResponse {
        messages: vec![
            // refund asset
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: asset_addr,
                msg: to_binary(&Cw20HandleMsg::Transfer {
                    recipient: env.message.sender.clone(),
                    amount: refund_asset_amount,
                })?,
                send: vec![],
            }),
            // refund collateral
            CosmosMsg::Bank(BankMsg::Send {
                from_address: env.contract.address,
                to_address: env.message.sender.clone(),
                amount: vec![deduct_tax(
                    &deps,
                    Coin {
                        denom: config_general.collateral_denom,
                        amount: refund_collateral_amount,
                    },
                )?],
            }),
            // burn liquidity token
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: deps.api.human_address(&config_general.liquidity_token)?,
                msg: to_binary(&Cw20HandleMsg::BurnFrom {
                    owner: env.message.sender,
                    amount,
                })?,
                send: vec![],
            }),
        ],
        log: vec![
            log("action", "withdraw_liquidity"),
            log("withdrawn_share", &amount.to_string()),
            log("refund_asset_amount", &refund_asset_amount.to_string()),
            log("refund_collateral_amount", &refund_asset_amount.to_string()),
        ],
        data: None,
    })
}

pub fn try_buy<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    max_spread: Option<Decimal>,
) -> StdResult<HandleResponse> {
    let config_general: ConfigGeneral = read_config_general(&deps.storage)?;
    let config_asset: ConfigAsset = read_config_asset(&deps.storage)?;
    let config_swap: ConfigSwap = read_config_swap(&deps.storage)?;

    let asset_addr = deps.api.human_address(&config_asset.token)?;

    // extract offer amount from sent funds
    let collateral_amount = amount_of(
        &env.message.sent_funds,
        config_general.collateral_denom.to_string(),
    );
    if collateral_amount.is_zero() {
        return Err(StdError::generic_err(format!(
            "No {} tokens sent",
            &config_general.collateral_denom
        )));
    }

    let price: Decimal = load_price(
        &deps,
        &deps.api.human_address(&config_asset.oracle)?,
        Some(env.block.time),
    )?;

    let asset_pool: Uint128 =
        load_token_balance(&deps, &asset_addr, &config_general.contract_addr)?;
    let collateral_pool: Uint128 = load_balance(
        &deps,
        &env.contract.address,
        config_general.collateral_denom.to_string(),
    )?;

    // active commission is absorbed to ask pool
    let offer_amount = collateral_amount;
    let (return_amount, spread_amount, minus_spread_amount, active_commission, inactive_commission) =
        compute_swap(
            &config_swap,
            collateral_pool,
            asset_pool,
            offer_amount,
            price,
            SwapOperation::Buy,
        )?;

    // check max spread limit if exist
    assert_max_spread(max_spread, return_amount, spread_amount)?;

    Ok(HandleResponse {
        messages: vec![
            // send sold asset token to buyer
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: asset_addr.clone(),
                msg: to_binary(&Cw20HandleMsg::Transfer {
                    recipient: env.message.sender,
                    amount: return_amount,
                })?,
                send: vec![],
            }),
            // send inactive commission to commission collector
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: asset_addr,
                msg: to_binary(&Cw20HandleMsg::Transfer {
                    recipient: deps
                        .api
                        .human_address(&config_general.commission_collector)?,
                    amount: inactive_commission,
                })?,
                send: vec![],
            }),
        ],
        log: vec![
            log("action", "buy"),
            log(
                "offer_amount",
                &(offer_amount.to_string() + config_general.collateral_denom.as_str()),
            ),
            log(
                "return_amount",
                &(return_amount.to_string() + config_asset.symbol.as_str()),
            ),
            log(
                "spread_amount",
                &(spread_amount.to_string() + config_asset.symbol.as_str()),
            ),
            log(
                "minus_spread_amount",
                &(minus_spread_amount.to_string() + config_asset.symbol.as_str()),
            ),
            log(
                "commission_amount",
                &((active_commission + inactive_commission).to_string()
                    + config_asset.symbol.as_str()),
            ),
        ],
        data: None,
    })
}

// CONTRACT - a user must do firstly token approval
pub fn try_sell<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    asset_amount: Uint128,
    max_spread: Option<Decimal>,
) -> StdResult<HandleResponse> {
    let config_general: ConfigGeneral = read_config_general(&deps.storage)?;
    let config_asset: ConfigAsset = read_config_asset(&deps.storage)?;
    let config_swap: ConfigSwap = read_config_swap(&deps.storage)?;
    let asset_addr = deps.api.human_address(&config_asset.token)?;

    // convert asset amount to unit of colalteral
    let price: Decimal = load_price(
        &deps,
        &deps.api.human_address(&config_asset.oracle)?,
        Some(env.block.time),
    )?;

    let asset_pool: Uint128 =
        load_token_balance(&deps, &asset_addr, &config_general.contract_addr)?;
    let collateral_pool: Uint128 = load_balance(
        &deps,
        &env.contract.address,
        config_general.collateral_denom.to_string(),
    )?;

    let offer_amount = asset_amount;
    let (return_amount, spread_amount, minus_spread_amount, active_commission, inactive_commission) =
        compute_swap(
            &config_swap,
            collateral_pool,
            asset_pool,
            offer_amount,
            price,
            SwapOperation::Sell,
        )?;

    // check max spread limit if exist
    assert_max_spread(max_spread, return_amount, spread_amount)?;

    // 1. send asset token from a user to the contract
    // 2. send collateral token from the contract to a user
    // 3. send inactive commission to collector
    Ok(HandleResponse {
        messages: vec![
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: asset_addr,
                msg: to_binary(&Cw20HandleMsg::TransferFrom {
                    owner: env.message.sender.clone(),
                    recipient: env.contract.address.clone(),
                    amount: asset_amount,
                })?,
                send: vec![],
            }),
            CosmosMsg::Bank(BankMsg::Send {
                from_address: env.contract.address.clone(),
                to_address: env.message.sender,
                amount: vec![deduct_tax(
                    &deps,
                    Coin {
                        denom: config_general.collateral_denom.to_string(),
                        amount: return_amount,
                    },
                )?],
            }),
            CosmosMsg::Bank(BankMsg::Send {
                from_address: env.contract.address,
                to_address: deps
                    .api
                    .human_address(&config_general.commission_collector)?,
                amount: vec![deduct_tax(
                    &deps,
                    Coin {
                        denom: config_general.collateral_denom.to_string(),
                        amount: inactive_commission,
                    },
                )?],
            }),
        ],
        log: vec![
            log("action", "sell"),
            log(
                "offer_amount",
                &(asset_amount.to_string() + config_asset.symbol.as_str()),
            ),
            log(
                "return_amount",
                &(return_amount.to_string() + config_general.collateral_denom.as_str()),
            ),
            log(
                "spread_amount",
                &(spread_amount.to_string() + config_general.collateral_denom.as_str()),
            ),
            log(
                "minus_spread_amount",
                &(minus_spread_amount.to_string() + config_general.collateral_denom.as_str()),
            ),
            log(
                "commission_amount",
                &((active_commission + inactive_commission).to_string()
                    + config_general.collateral_denom.as_str()),
            ),
        ],
        data: None,
    })
}

pub fn query<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    msg: QueryMsg,
) -> StdResult<Binary> {
    match msg {
        QueryMsg::ConfigGeneral {} => to_binary(&query_config_general(deps)?),
        QueryMsg::ConfigAsset {} => to_binary(&query_config_asset(deps)?),
        QueryMsg::ConfigSwap {} => to_binary(&query_config_swap(deps)?),
        QueryMsg::Provider { address } => to_binary(&query_provider(deps, address)?),
        QueryMsg::Pool {} => to_binary(&query_pool(deps)?),
        QueryMsg::Simulation {
            offer_amount,
            operation,
        } => to_binary(&query_simulation(deps, offer_amount, operation)?),
        QueryMsg::ReverseSimulation {
            ask_amount,
            operation,
        } => to_binary(&query_reverse_simulation(deps, ask_amount, operation)?),
    }
}

pub fn query_config_general<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
) -> StdResult<ConfigGeneralResponse> {
    let state: ConfigGeneral = read_config_general(&deps.storage)?;
    let resp = ConfigGeneralResponse {
        owner: deps.api.human_address(&state.owner)?,
        liquidity_token: deps.api.human_address(&state.liquidity_token)?,
        commission_collector: deps.api.human_address(&state.commission_collector)?,
        collateral_denom: state.collateral_denom,
    };

    Ok(resp)
}

pub fn query_config_asset<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
) -> StdResult<ConfigAssetResponse> {
    let state: ConfigAsset = read_config_asset(&deps.storage)?;
    let resp = ConfigAssetResponse {
        oracle: deps.api.human_address(&state.oracle)?,
        token: deps.api.human_address(&state.token)?,
        symbol: state.symbol,
    };

    Ok(resp)
}

pub fn query_config_swap<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
) -> StdResult<ConfigSwapResponse> {
    let state: ConfigSwap = read_config_swap(&deps.storage)?;
    let resp = ConfigSwapResponse {
        active_commission: state.active_commission,
        inactive_commission: state.inactive_commission,
        max_minus_spread: state.max_minus_spread,
        max_spread: state.max_spread,
    };

    Ok(resp)
}

pub fn query_pool<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
) -> StdResult<PoolResponse> {
    let config_asset: ConfigAsset = read_config_asset(&deps.storage)?;
    let config_general: ConfigGeneral = read_config_general(&deps.storage)?;

    let asset_pool: Uint128 = load_token_balance(
        &deps,
        &deps.api.human_address(&config_asset.token)?,
        &config_general.contract_addr,
    )?;

    let collateral_pool: Uint128 = load_balance(
        &deps,
        &deps.api.human_address(&config_general.contract_addr)?,
        config_general.collateral_denom,
    )?;

    let total_share: Uint128 = load_token_balance(
        &deps,
        &deps.api.human_address(&config_general.liquidity_token)?,
        &config_general.contract_addr,
    )?;

    let resp = PoolResponse {
        asset_pool,
        collateral_pool,
        total_share,
    };

    Ok(resp)
}

pub fn query_provider<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    address: HumanAddr,
) -> StdResult<ProviderResponse> {
    let share: Uint128 =
        provider_share_read(&deps.storage, &deps.api.canonical_address(&address)?)?;

    let resp = ProviderResponse { share };

    Ok(resp)
}

pub fn query_simulation<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    offer_amount: Uint128,
    operation: SwapOperation,
) -> StdResult<SimulationResponse> {
    let config_general: ConfigGeneral = read_config_general(&deps.storage)?;
    let config_swap: ConfigSwap = read_config_swap(&deps.storage)?;
    let config_asset: ConfigAsset = read_config_asset(&deps.storage)?;

    let asset_pool: Uint128 = load_token_balance(
        &deps,
        &deps.api.human_address(&config_asset.token)?,
        &config_general.contract_addr,
    )?;

    let collateral_pool: Uint128 = load_balance(
        &deps,
        &deps.api.human_address(&config_general.contract_addr)?,
        config_general.collateral_denom.to_string(),
    )?;

    let price: Decimal = load_price(&deps, &deps.api.human_address(&config_asset.oracle)?, None)?;
    let (return_amount, spread_amount, minus_spread_amount, active_commission, inactive_commission) =
        compute_swap(
            &config_swap,
            collateral_pool,
            asset_pool,
            offer_amount,
            price,
            operation,
        )?;

    let denom = match operation {
        SwapOperation::Buy => config_asset.symbol,
        SwapOperation::Sell => config_general.collateral_denom,
    };

    Ok(SimulationResponse {
        return_amount: Coin {
            denom: denom.to_string(),
            amount: return_amount,
        },
        spread_amount: Coin {
            denom: denom.to_string(),
            amount: spread_amount,
        },
        minus_spread_amount: Coin {
            denom: denom.to_string(),
            amount: minus_spread_amount,
        },
        commission_amount: Coin {
            denom,
            amount: active_commission + inactive_commission,
        },
    })
}

pub fn query_reverse_simulation<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    ask_amount: Uint128,
    operation: SwapOperation,
) -> StdResult<ReverseSimulationResponse> {
    let config_general: ConfigGeneral = read_config_general(&deps.storage)?;
    let config_swap: ConfigSwap = read_config_swap(&deps.storage)?;
    let config_asset: ConfigAsset = read_config_asset(&deps.storage)?;

    let asset_pool: Uint128 = load_token_balance(
        &deps,
        &deps.api.human_address(&config_asset.token)?,
        &config_general.contract_addr,
    )?;

    let collateral_pool: Uint128 = load_balance(
        &deps,
        &deps.api.human_address(&config_general.contract_addr)?,
        config_general.collateral_denom.to_string(),
    )?;

    let price: Decimal = load_price(&deps, &deps.api.human_address(&config_asset.oracle)?, None)?;
    let (offer_amount, spread_amount, minus_spread_amount, active_commission, inactive_commission) =
        compute_offer_amount(
            &config_swap,
            collateral_pool,
            asset_pool,
            ask_amount,
            price,
            operation,
        )?;

    let (offer_denom, ask_denom) = match operation {
        SwapOperation::Buy => (config_general.collateral_denom, config_asset.symbol),
        SwapOperation::Sell => (config_asset.symbol, config_general.collateral_denom),
    };

    Ok(ReverseSimulationResponse {
        offer_amount: Coin {
            denom: offer_denom,
            amount: offer_amount,
        },
        spread_amount: Coin {
            denom: ask_denom.to_string(),
            amount: spread_amount,
        },
        minus_spread_amount: Coin {
            denom: ask_denom.to_string(),
            amount: minus_spread_amount,
        },
        commission_amount: Coin {
            denom: ask_denom,
            amount: active_commission + inactive_commission,
        },
    })
}

pub fn amount_of(coins: &[Coin], denom: String) -> Uint128 {
    match coins.iter().find(|x| x.denom == denom) {
        Some(coin) => coin.amount,
        None => Uint128::zero(),
    }
}

// Stringifyer for coins
pub fn coins_to_string(coins: Vec<Coin>) -> String {
    if coins.is_empty() {
        return String::from("");
    }

    let (coin, coins) = coins.as_slice().split_first().unwrap();
    let mut coins_string: String = coin.amount.to_string() + &coin.denom;
    for coin in coins {
        coins_string = coins_string + "," + &coin.amount.to_string() + &coin.denom;
    }

    coins_string
}

fn compute_swap(
    config: &ConfigSwap,
    collateral_pool: Uint128,
    asset_pool: Uint128,
    offer_amount: Uint128,
    price: Decimal,
    swap_operation: SwapOperation,
) -> StdResult<(Uint128, Uint128, Uint128, Uint128, Uint128)> {
    let offer_pool: Uint128;
    let ask_pool: Uint128;
    let exchange_rate: Decimal;
    match swap_operation {
        SwapOperation::Buy => {
            offer_pool = collateral_pool;
            ask_pool = asset_pool;
            exchange_rate = reverse_decimal(price);
        }
        SwapOperation::Sell => {
            offer_pool = asset_pool;
            ask_pool = collateral_pool;
            exchange_rate = price;
        }
    }

    // offer => ask
    // ask_amount = (ask_pool - cp / (offer_pool + offer_amount)) * (1 - commission_rate)
    // max_minus_spread_ask_amount = offer_amount * exchange_rate * (1 + max_minus_spread) * (1 - commission)
    // max_spread_ask_amount = offer_amount * exchange_rate * (1 - max_spread) * (1 - commission)
    // ask_amount = max(min(uniswap, max_minus_spread), max_spread)

    // basic uniswap operation; return_amount = ask_pool - constant_product / (offer_pool + offer_amount);
    let cp = Uint128(offer_pool.u128() * ask_pool.u128());
    let return_amount = offer_amount * exchange_rate;
    let return_amount = std::cmp::max(
        std::cmp::min(
            (ask_pool - cp.multiply_ratio(1u128, offer_pool + offer_amount))?,
            return_amount * (Decimal::one() + config.max_minus_spread),
        ),
        return_amount * decimal_subtraction(Decimal::one(), config.max_spread)?,
    );

    // calculate spread & commission
    let spread_amount: Uint128 =
        (offer_amount * exchange_rate - return_amount).unwrap_or_else(|_| Uint128::zero());
    let minus_spread_amount =
        (return_amount - offer_amount * exchange_rate).unwrap_or_else(|_| Uint128::zero());
    let active_commission: Uint128 = return_amount * config.active_commission;
    let inactive_commission: Uint128 = return_amount * config.inactive_commission;

    // commission will be absorbed to pool
    let return_amount: Uint128 =
        (return_amount - (active_commission + inactive_commission)).unwrap();

    Ok((
        return_amount,
        spread_amount,
        minus_spread_amount,
        active_commission,
        inactive_commission,
    ))
}

fn compute_offer_amount(
    config: &ConfigSwap,
    collateral_pool: Uint128,
    asset_pool: Uint128,
    ask_amount: Uint128,
    price: Decimal,
    swap_operation: SwapOperation,
) -> StdResult<(Uint128, Uint128, Uint128, Uint128, Uint128)> {
    let offer_pool: Uint128;
    let ask_pool: Uint128;
    let exchange_rate: Decimal;
    match swap_operation {
        SwapOperation::Buy => {
            offer_pool = collateral_pool;
            ask_pool = asset_pool;
            exchange_rate = reverse_decimal(price);
        }
        SwapOperation::Sell => {
            offer_pool = asset_pool;
            ask_pool = collateral_pool;
            exchange_rate = price;
        }
    }

    // ask => offer
    // max_minus_spread_offer_amount = ask_amount / (exchange_rate * (1 + max_minus_spread) * (1 - commission))
    // max_spread_offer_amount = ask_amount / (exchange_rate * (1 - max_spread) * (1 - commission))
    // uniswap_offer_amount = cp / (ask_pool - ask_amount * (1 - commission_rate)) - offer_pool
    // offer_amount = min(max(uniswap, max_minus_spread), max_spread)

    let cp = Uint128(offer_pool.u128() * ask_pool.u128());
    let one_minus_commission = decimal_subtraction(
        Decimal::one(),
        config.active_commission + config.inactive_commission,
    )?;

    let max_minus_spread_offer_amount: Uint128 = ask_amount
        * reverse_decimal(decimal_multiplication(
            decimal_multiplication(exchange_rate, Decimal::one() + config.max_minus_spread),
            one_minus_commission,
        ));

    let max_spread_offer_amount: Uint128 = ask_amount
        * reverse_decimal(decimal_multiplication(
            decimal_multiplication(
                exchange_rate,
                decimal_subtraction(Decimal::one(), config.max_spread)?,
            ),
            one_minus_commission,
        ));

    let uniswap_offer_amount: Uint128 = (cp.multiply_ratio(
        1u128,
        (ask_pool - ask_amount * reverse_decimal(one_minus_commission))?,
    ) - offer_pool)?;

    let offer_amount: Uint128 = std::cmp::min(
        std::cmp::max(uniswap_offer_amount, max_minus_spread_offer_amount),
        max_spread_offer_amount,
    );

    let before_commission_deduction = ask_amount * reverse_decimal(one_minus_commission);
    let spread_amount = (offer_amount * exchange_rate - before_commission_deduction)
        .unwrap_or_else(|_| Uint128::zero());
    let minus_spread_amount = (before_commission_deduction - offer_amount * exchange_rate)
        .unwrap_or_else(|_| Uint128::zero());
    let active_commission = before_commission_deduction * config.active_commission;
    let inactive_commission = before_commission_deduction * config.inactive_commission;
    Ok((
        offer_amount,
        spread_amount,
        minus_spread_amount,
        active_commission,
        inactive_commission,
    ))
}

fn assert_max_spread(
    max_spread: Option<Decimal>,
    return_amount: Uint128,
    spread_amount: Uint128,
) -> StdResult<()> {
    if let Some(max_spread) = max_spread {
        if Decimal::from_ratio(spread_amount, return_amount + spread_amount) > max_spread {
            return Err(StdError::generic_err("Operation exceeds max spread limit"));
        }
    }

    Ok(())
}

pub fn deduct_tax<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    coin: Coin,
) -> StdResult<Coin> {
    let terra_querier = TerraQuerier::new(&deps.querier);
    let tax_rate: Decimal = terra_querier.query_tax_rate()?;
    let tax_cap: Uint128 = terra_querier.query_tax_cap(coin.denom.to_string())?;
    Ok(Coin {
        amount: std::cmp::max(
            (coin.amount - coin.amount * tax_rate)?,
            (coin.amount - tax_cap).unwrap_or_else(|_| Uint128::zero()),
        ),
        ..coin
    })
}