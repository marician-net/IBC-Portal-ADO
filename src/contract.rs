use crate::amount::Amount;
use crate::error::ContractError;
use crate::ibc::Ics20Packet;
use crate::msg::{
    ChannelResponse, ConfigResponse, ExecuteMsg, InitMsg, ListChannelsResponse, PortResponse,
    QueryMsg, TransferMsg, WhitelistResponse,
};
use crate::state::{
    increase_channel_balance, Config, CHANNEL_INFO, CHANNEL_STATE, CONFIG, WHITE_LIST,
};
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    attr, from_binary, to_binary, Addr, Binary, Deps, DepsMut, Env, IbcMsg, IbcQuery, MessageInfo,
    Order, PortIdResponse, Response, StdResult,
};
use cw0::PaymentError;
use cw2::set_contract_version;
use cw20::{Cw20Coin, Cw20ReceiveMsg};

// version info for migration info
const CONTRACT_NAME: &str = "andromeda-potal-ado";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InitMsg,
) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    let cfg = Config {
        default_timeout: msg.default_timeout,
    };
    CONFIG.save(deps.storage, &cfg)?;

    for white_addr in msg.whitelist {
        let contract = deps.api.addr_validate(&white_addr)?;
        WHITE_LIST.save(deps.storage, &contract, &true)?;
    }

    Ok(Response::new().add_attributes(vec![
        attr("action", "instantiate"),
        attr("default_timeout", msg.default_timeout.to_string()),
    ]))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => execute_receive(deps, env, info, msg),
        ExecuteMsg::Transfer(msg) => {
            let coin = match info.funds.len() {
                0 => Err(PaymentError::NoFunds {}),
                1 => {
                    let coin = &info.funds[0];
                    if coin.amount.is_zero() {
                        Err(PaymentError::NoFunds {})
                    } else {
                        Ok(coin.clone())
                    }
                }
                _ => Err(PaymentError::MultipleDenoms {}),
            }?;
            execute_transfer(deps, env, msg, Amount::Native(coin), info.sender)
        }
    }
}

pub fn execute_receive(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    wrapper: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let msg: TransferMsg = from_binary(&wrapper.msg)?;
    let amount = Amount::Cw20(Cw20Coin {
        address: info.sender.to_string(),
        amount: wrapper.amount,
    });
    let api = deps.api;
    execute_transfer(deps, env, msg, amount, api.addr_validate(&wrapper.sender)?)
}
pub fn execute_transfer(
    deps: DepsMut,
    env: Env,
    msg: TransferMsg,
    amount: Amount,
    sender: Addr,
) -> Result<Response, ContractError> {
    if amount.is_empty() {
        return Err(ContractError::NoFunds {});
    }
    // ensure the requested channel is registered
    if !CHANNEL_INFO.has(deps.storage, &msg.channel) {
        return Err(ContractError::NoSuchChannel { id: msg.channel });
    }

    // if cw20 token, ensure it is whitelisted
    if let Amount::Cw20(coin) = &amount {
        let addr = deps.api.addr_validate(&coin.address)?;
        WHITE_LIST
            .may_load(deps.storage, &addr)?
            .ok_or(ContractError::NotOnAllowList)?;
    };

    // delta from user is in seconds
    let timeout_delta = match msg.timeout {
        Some(t) => t,
        None => CONFIG.load(deps.storage)?.default_timeout,
    };
    // timeout is in nanoseconds
    let timeout = env.block.time.plus_seconds(timeout_delta);

    // build ics20 packet
    let packet = Ics20Packet::new(
        amount.amount(),
        amount.denom(),
        sender.as_ref(),
        &msg.remote_address,
    );
    packet.validate()?;

    // Update the balance now (optimistically) like ibctransfer modules.
    // In on_packet_failure (ack with error message or a timeout), we reduce the balance appropriately.
    // This means the channel works fine if success acks are not relayed.
    increase_channel_balance(deps.storage, &msg.channel, &amount.denom(), amount.amount())?;

    // prepare ibc message
    let msg = IbcMsg::SendPacket {
        channel_id: msg.channel,
        data: to_binary(&packet)?,
        timeout: timeout.into(),
    };

    // send response
    Ok(Response::new().add_message(msg).add_attributes(vec![
        attr("action", "transfer"),
        attr("sender", &packet.sender),
        attr("receiver", &packet.receiver),
        attr("denom", &packet.denom),
        attr("amount", &packet.amount.to_string()),
    ]))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Port {} => to_binary(&query_port(deps)?),
        QueryMsg::ListChannels {} => to_binary(&query_list(deps)?),
        QueryMsg::Channel { id } => to_binary(&query_channel(deps, id)?),
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::Whitelisted { contract } => to_binary(&query_whitelisted(deps, contract)?),
    }
}

fn query_port(deps: Deps) -> StdResult<PortResponse> {
    let query = IbcQuery::PortId {}.into();
    let PortIdResponse { port_id } = deps.querier.query(&query)?;
    Ok(PortResponse { port_id })
}

fn query_list(deps: Deps) -> StdResult<ListChannelsResponse> {
    let channels: StdResult<Vec<_>> = CHANNEL_INFO
        .range(deps.storage, None, None, Order::Ascending)
        .map(|r| r.map(|(_, v)| v))
        .collect();
    Ok(ListChannelsResponse {
        channels: channels?,
    })
}

// make public for ibc tests
pub fn query_channel(deps: Deps, id: String) -> StdResult<ChannelResponse> {
    let info = CHANNEL_INFO.load(deps.storage, &id)?;
    // this returns Vec<(outstanding, total)>
    let state: StdResult<Vec<_>> = CHANNEL_STATE
        .prefix(&id)
        .range(deps.storage, None, None, Order::Ascending)
        .map(|r| {
            let (k, v) = r?;
            let denom = String::from_utf8(k)?;
            let outstanding = Amount::from_parts(denom.clone(), v.outstanding);
            let total = Amount::from_parts(denom, v.total_sent);
            Ok((outstanding, total))
        })
        .collect();
    // we want (Vec<outstanding>, Vec<total>)

    let (balances, total_sent) = state?.into_iter().unzip();

    Ok(ChannelResponse {
        info,
        balances,
        total_sent,
    })
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let cfg = CONFIG.load(deps.storage)?;
    let res = ConfigResponse {
        default_timeout: cfg.default_timeout,
    };
    Ok(res)
}

fn query_whitelisted(deps: Deps, contract: String) -> StdResult<WhitelistResponse> {
    let addr = deps.api.addr_validate(&contract)?;
    let info = WHITE_LIST.may_load(deps.storage, &addr)?;
    let res = match info {
        None => WhitelistResponse {
            is_whitelist: false,
        },
        Some(_) => WhitelistResponse { is_whitelist: true },
    };
    Ok(res)
}
