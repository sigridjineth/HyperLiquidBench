use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use clap::{Parser, ValueEnum};
use ethers::signers::{LocalWallet, Signer};
use hl_common::{
    load_plan_from_spec,
    plan::{
        ActionStep, CancelAllStep, CancelLastStep, CancelOidsStep, OrderPrice, PerpOrder,
        PerpOrdersStep, Plan, SetLeverageStep, UsdClassTransferStep,
    },
    time::timestamp_ms,
    RoutedOrderRecord, RunArtifacts,
};
use hyperliquid_rust_sdk::{
    BaseUrl, BuilderInfo, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest,
    ExchangeClient, ExchangeDataStatus, ExchangeResponseStatus, InfoClient, LedgerUpdate,
    LedgerUpdateData, Message, Subscription,
};
use serde_json::json;
use tokio::{
    sync::{broadcast, mpsc, Mutex},
    time::timeout,
};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Execute HyperLiquidBench plans against Hyperliquid APIs"
)]
struct Cli {
    /// Plan specification: a JSON file or JSONL file with :line selector (1-based)
    #[arg(long)]
    plan: String,

    /// Output directory. Defaults to runs/<timestamp>
    #[arg(long)]
    out: Option<PathBuf>,

    /// Network to target (mainnet, testnet, local)
    #[arg(long, value_enum, default_value = "testnet")]
    network: Network,

    /// Builder code to attach to orders (overridden by per-step builder code)
    #[arg(long)]
    builder_code: Option<String>,

    /// Hex-encoded private key for the trading wallet (env: HL_PRIVATE_KEY)
    #[arg(long, env = "HL_PRIVATE_KEY")]
    private_key: String,

    /// Max time (ms) to wait for websocket confirmation effects
    #[arg(long, default_value_t = 2_000)]
    effect_timeout_ms: u64,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Network {
    Mainnet,
    Testnet,
    Local,
}

impl Network {
    fn base_url(&self) -> BaseUrl {
        match self {
            Network::Mainnet => BaseUrl::Mainnet,
            Network::Testnet => BaseUrl::Testnet,
            Network::Local => BaseUrl::Localhost,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
            Network::Local => "local",
        }
    }
}

#[derive(Clone, Debug)]
struct PlacedOrder {
    coin: String,
    oid: u64,
}

#[derive(Clone, Debug)]
enum ObservedEvent {
    OrderUpdate {
        oid: u64,
        _status: String,
        payload: serde_json::Value,
    },
    UserFill {
        oid: u64,
        payload: serde_json::Value,
    },
    LedgerClassTransfer {
        to_perp: bool,
        _usdc: f64,
        payload: serde_json::Value,
    },
    Other {
        _channel: String,
        payload: serde_json::Value,
    },
}

impl ObservedEvent {
    fn payload(&self) -> &serde_json::Value {
        match self {
            ObservedEvent::OrderUpdate { payload, .. }
            | ObservedEvent::UserFill { payload, .. }
            | ObservedEvent::LedgerClassTransfer { payload, .. }
            | ObservedEvent::Other { payload, .. } => payload,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let plan = load_plan_from_spec(&cli.plan)?;
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let out_dir = cli
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("runs").join(&timestamp));

    let plan_json = plan.as_json();
    let artifacts = RunArtifacts::create(&out_dir, &plan_json, None, None)?;
    let artifacts = Arc::new(Mutex::new(artifacts));

    let wallet = LocalWallet::from_str(cli.private_key.trim())
        .map_err(|e| anyhow!("failed to parse wallet private key: {e}"))?;
    let wallet_address = wallet.address();

    let base_url = cli.network.base_url();

    let exchange = ExchangeClient::new(None, wallet.clone(), Some(base_url), None, None)
        .await
        .context("failed to initialise exchange client")?;

    let info_http = InfoClient::new(None, Some(base_url))
        .await
        .context("failed to initialise info client")?;
    let info_ws = InfoClient::with_reconnect(None, Some(base_url))
        .await
        .context("failed to initialise websocket info client")?;

    let (event_tx, _) = broadcast::channel::<ObservedEvent>(256);
    spawn_ws_task(info_ws, wallet_address, artifacts.clone(), event_tx.clone());

    execute_plan(
        plan,
        artifacts.clone(),
        exchange,
        info_http,
        event_tx.clone(),
        cli.builder_code.clone(),
        cli.effect_timeout_ms,
    )
    .await?;

    let meta = json!({
        "network": cli.network.as_str(),
        "builderCode": cli.builder_code,
        "plan": { "steps": plan_json["steps"].clone() },
        "wallet": format!("0x{:x}", wallet_address),
        "outDir": out_dir.display().to_string(),
        "effectTimeoutMs": cli.effect_timeout_ms,
        "timestamp": timestamp,
    });
    artifacts.lock().await.write_meta(&meta)?;

    info!("run artifacts stored under {}", out_dir.display());
    Ok(())
}

fn spawn_ws_task(
    mut info_ws: InfoClient,
    wallet_address: ethers::types::H160,
    artifacts: Arc<Mutex<RunArtifacts>>,
    broadcaster: broadcast::Sender<ObservedEvent>,
) {
    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let subscriptions = vec![
            Subscription::OrderUpdates {
                user: wallet_address,
            },
            Subscription::UserFills {
                user: wallet_address,
            },
            Subscription::UserNonFundingLedgerUpdates {
                user: wallet_address,
            },
        ];

        for sub in subscriptions {
            if let Err(err) = info_ws.subscribe(sub, tx.clone()).await {
                error!("failed to subscribe to websocket channel: {err}");
            }
        }

        drop(tx); // retain rx only

        while let Some(message) = rx.recv().await {
            if let Err(err) = handle_ws_message(&artifacts, &broadcaster, message).await {
                warn!("failed to process websocket message: {err:?}");
            }
        }
    });
}

async fn handle_ws_message(
    artifacts: &Arc<Mutex<RunArtifacts>>,
    broadcaster: &broadcast::Sender<ObservedEvent>,
    message: Message,
) -> Result<()> {
    let (value, events) = encode_message(message);
    {
        let mut artifacts = artifacts.lock().await;
        artifacts.log_ws_event(&value)?;
    }
    for event in events {
        let _ = broadcaster.send(event);
    }
    Ok(())
}

fn encode_message(message: Message) -> (serde_json::Value, Vec<ObservedEvent>) {
    match message {
        Message::OrderUpdates(order_updates) => {
            let mut events = Vec::new();
            let data: Vec<_> = order_updates
                .data
                .iter()
                .map(|upd| {
                    let payload = json!({
                        "channel": "orderUpdates",
                        "coin": upd.order.coin.clone(),
                        "oid": upd.order.oid,
                        "side": upd.order.side.clone(),
                        "limitPx": upd.order.limit_px.clone(),
                        "sz": upd.order.sz.clone(),
                        "status": upd.status.clone(),
                        "statusTimestamp": upd.status_timestamp,
                    });
                    events.push(ObservedEvent::OrderUpdate {
                        oid: upd.order.oid,
                        _status: upd.status.clone(),
                        payload: payload.clone(),
                    });
                    payload
                })
                .collect();
            (json!({ "channel": "orderUpdates", "data": data }), events)
        }
        Message::UserFills(fills) => {
            let mut events = Vec::new();
            let data: Vec<_> = fills
                .data
                .fills
                .iter()
                .map(|fill| {
                    let payload = json!({
                        "channel": "userFills",
                        "oid": fill.oid,
                        "coin": fill.coin.clone(),
                        "px": fill.px.clone(),
                        "sz": fill.sz.clone(),
                        "time": fill.time,
                        "side": fill.side.clone(),
                    });
                    events.push(ObservedEvent::UserFill {
                        oid: fill.oid,
                        payload: payload.clone(),
                    });
                    payload
                })
                .collect();
            let root = json!({
                "channel": "userFills",
                "isSnapshot": fills.data.is_snapshot,
                "fills": data,
            });
            (root, events)
        }
        Message::UserNonFundingLedgerUpdates(ledger) => {
            let mut events = Vec::new();
            let data: Vec<_> = ledger
                .data
                .non_funding_ledger_updates
                .iter()
                .map(|update| {
                    let entry = encode_ledger_update(update);
                    if let Some(event) = entry.1 {
                        events.push(event);
                    }
                    entry.0
                })
                .collect();
            (
                json!({
                    "channel": "userNonFundingLedgerUpdates",
                    "isSnapshot": ledger.data.is_snapshot,
                    "updates": data,
                }),
                events,
            )
        }
        other => (
            json!({"channel": "other", "debug": format!("{:?}", other)}),
            vec![ObservedEvent::Other {
                _channel: "other".to_string(),
                payload: json!({ "debug": format!("{:?}", other) }),
            }],
        ),
    }
}

fn encode_ledger_update(update: &LedgerUpdateData) -> (serde_json::Value, Option<ObservedEvent>) {
    match &update.delta {
        LedgerUpdate::AccountClassTransfer(transfer) => {
            let usdc = transfer.usdc.parse::<f64>().unwrap_or_default() / 1_000_000f64;
            let payload = json!({
                "channel": "accountClassTransfer",
                "time": update.time,
                "usdc": usdc,
                "toPerp": transfer.to_perp,
            });
            (
                payload.clone(),
                Some(ObservedEvent::LedgerClassTransfer {
                    to_perp: transfer.to_perp,
                    _usdc: usdc,
                    payload,
                }),
            )
        }
        other => {
            let payload = json!({
                "channel": "ledger",
                "time": update.time,
                "kind": format!("{:?}", other),
            });
            (
                payload.clone(),
                Some(ObservedEvent::Other {
                    _channel: "ledger".to_string(),
                    payload,
                }),
            )
        }
    }
}

fn exchange_status_json(status: &ExchangeResponseStatus) -> serde_json::Value {
    match status {
        ExchangeResponseStatus::Ok(resp) => {
            let data = resp.data.as_ref().map(|collection| {
                let entries: Vec<_> = collection
                    .statuses
                    .iter()
                    .map(|status| match status {
                        ExchangeDataStatus::Success => {
                            json!({"kind": "success"})
                        }
                        ExchangeDataStatus::WaitingForFill => {
                            json!({"kind": "waitingForFill"})
                        }
                        ExchangeDataStatus::WaitingForTrigger => {
                            json!({"kind": "waitingForTrigger"})
                        }
                        ExchangeDataStatus::Error(err) => {
                            json!({"kind": "error", "message": err})
                        }
                        ExchangeDataStatus::Resting(order) => json!({
                            "kind": "resting",
                            "oid": order.oid,
                        }),
                        ExchangeDataStatus::Filled(filled) => json!({
                            "kind": "filled",
                            "oid": filled.oid,
                            "avgPx": filled.avg_px,
                            "totalSz": filled.total_sz,
                        }),
                    })
                    .collect();
                json!({"statuses": entries})
            });
            json!({
                "status": "ok",
                "responseType": resp.response_type,
                "data": data,
            })
        }
        ExchangeResponseStatus::Err(err) => json!({
            "status": "err",
            "message": err,
        }),
    }
}

fn extract_oids(status: &ExchangeResponseStatus) -> Vec<u64> {
    match status {
        ExchangeResponseStatus::Ok(resp) => resp
            .data
            .as_ref()
            .map(|collection| {
                collection
                    .statuses
                    .iter()
                    .filter_map(|status| match status {
                        ExchangeDataStatus::Resting(order) => Some(order.oid),
                        ExchangeDataStatus::Filled(filled) => Some(filled.oid),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ExchangeResponseStatus::Err(_) => Vec::new(),
    }
}

async fn resolve_limit_price(
    order: &PerpOrder,
    info_http: &mut InfoClient,
    mid_cache: &mut HashMap<String, f64>,
) -> Result<f64> {
    match &order.px {
        OrderPrice::Absolute(px) => Ok(*px),
        OrderPrice::MidPercent { .. } => {
            if let Some(mid) = mid_cache.get(&order.coin) {
                Ok(order.px.resolve_with_mid(*mid))
            } else {
                let mids = info_http
                    .all_mids()
                    .await
                    .context("failed to fetch all mids")?;
                for (coin, price_str) in mids {
                    if let Ok(px) = price_str.parse::<f64>() {
                        mid_cache.insert(coin, px);
                    }
                }
                let mid = mid_cache
                    .get(&order.coin)
                    .copied()
                    .ok_or_else(|| anyhow!("mid price unavailable for {}", order.coin))?;
                Ok(order.px.resolve_with_mid(mid))
            }
        }
    }
}

fn build_client_order(order: &PerpOrder, limit_px: f64) -> Result<ClientOrderRequest> {
    if let Some(trigger) = &order.trigger {
        match trigger {
            hl_common::plan::OrderTrigger::None => {}
            _ => {
                return Err(anyhow!(
                    "trigger orders are not yet supported in the runner"
                ));
            }
        }
    }

    let cloid = order
        .cloid
        .as_deref()
        .and_then(|raw| Uuid::parse_str(raw).ok());

    Ok(ClientOrderRequest {
        asset: order.coin.clone(),
        is_buy: order.is_buy(),
        reduce_only: order.reduce_only,
        limit_px,
        sz: order.sz,
        cloid,
        order_type: ClientOrder::Limit(ClientLimit {
            tif: order.tif.as_sdk_str().to_string(),
        }),
    })
}

fn order_price_label(price: &OrderPrice) -> String {
    match price {
        OrderPrice::Absolute(px) => px.to_string(),
        OrderPrice::MidPercent { offset_pct } => {
            if *offset_pct >= 0.0 {
                format!("mid+{}%", offset_pct)
            } else {
                format!("mid{}%", offset_pct)
            }
        }
    }
}

async fn wait_for_order_event(
    receiver: &mut broadcast::Receiver<ObservedEvent>,
    oid: u64,
    timeout_duration: Duration,
) -> Option<ObservedEvent> {
    use tokio::time::Instant;

    let deadline = Instant::now() + timeout_duration;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining = deadline - now;
        match timeout(remaining, receiver.recv()).await {
            Ok(Ok(event)) => match &event {
                ObservedEvent::OrderUpdate { oid: ev_oid, .. }
                | ObservedEvent::UserFill { oid: ev_oid, .. } => {
                    if *ev_oid == oid {
                        return Some(event);
                    }
                }
                _ => {}
            },
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => return None,
            Err(_) => return None,
        }
    }
}

async fn wait_for_ledger_event(
    receiver: &mut broadcast::Receiver<ObservedEvent>,
    to_perp: bool,
    timeout_duration: Duration,
) -> Option<ObservedEvent> {
    use tokio::time::Instant;

    let deadline = Instant::now() + timeout_duration;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining = deadline - now;
        match timeout(remaining, receiver.recv()).await {
            Ok(Ok(event)) => {
                if let ObservedEvent::LedgerClassTransfer {
                    to_perp: observed, ..
                } = &event
                {
                    if *observed == to_perp {
                        return Some(event);
                    }
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => return None,
            Err(_) => return None,
        }
    }
}

fn remove_tracked_oids(placed_orders: &mut VecDeque<PlacedOrder>, target_oids: &[u64]) {
    placed_orders.retain(|placed| !target_oids.contains(&placed.oid));
}

async fn execute_plan(
    plan: Plan,
    artifacts: Arc<Mutex<RunArtifacts>>,
    exchange: ExchangeClient,
    mut info_http: InfoClient,
    broadcaster: broadcast::Sender<ObservedEvent>,
    default_builder_code: Option<String>,
    effect_timeout_ms: u64,
) -> Result<()> {
    let mut placed_orders: VecDeque<PlacedOrder> = VecDeque::new();
    let mut mid_cache: HashMap<String, f64> = HashMap::new();

    for (idx, step) in plan.steps.iter().enumerate() {
        match step {
            ActionStep::PerpOrders { perp_orders } => {
                execute_perp_orders(
                    idx,
                    perp_orders,
                    &artifacts,
                    &exchange,
                    &mut info_http,
                    &mut mid_cache,
                    &mut placed_orders,
                    &broadcaster,
                    default_builder_code.as_deref(),
                    effect_timeout_ms,
                )
                .await?;
            }
            ActionStep::CancelLast { cancel_last } => {
                execute_cancel_last(
                    idx,
                    cancel_last,
                    &artifacts,
                    &exchange,
                    &mut placed_orders,
                    &broadcaster,
                    effect_timeout_ms,
                )
                .await?;
            }
            ActionStep::CancelOids { cancel_oids } => {
                execute_cancel_oids(
                    idx,
                    cancel_oids,
                    &artifacts,
                    &exchange,
                    &mut placed_orders,
                    &broadcaster,
                    effect_timeout_ms,
                )
                .await?;
            }
            ActionStep::CancelAll { cancel_all } => {
                execute_cancel_all(
                    idx,
                    cancel_all,
                    &artifacts,
                    &exchange,
                    &mut placed_orders,
                    &broadcaster,
                    effect_timeout_ms,
                )
                .await?;
            }
            ActionStep::UsdClassTransfer { usd_class_transfer } => {
                execute_class_transfer(
                    idx,
                    usd_class_transfer,
                    &artifacts,
                    &exchange,
                    &broadcaster,
                    effect_timeout_ms,
                )
                .await?;
            }
            ActionStep::SetLeverage { set_leverage } => {
                execute_set_leverage(idx, set_leverage, &artifacts, &exchange).await?;
            }
            ActionStep::Sleep { sleep_ms } => {
                tokio::time::sleep(Duration::from_millis(sleep_ms.duration_ms)).await;
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_perp_orders(
    step_idx: usize,
    step: &PerpOrdersStep,
    artifacts: &Arc<Mutex<RunArtifacts>>,
    exchange: &ExchangeClient,
    info_http: &mut InfoClient,
    mid_cache: &mut HashMap<String, f64>,
    placed_orders: &mut VecDeque<PlacedOrder>,
    broadcaster: &broadcast::Sender<ObservedEvent>,
    default_builder: Option<&str>,
    effect_timeout_ms: u64,
) -> Result<()> {
    if step.orders.is_empty() {
        return Ok(());
    }

    let submit_ts = timestamp_ms();
    let mut client_orders = Vec::with_capacity(step.orders.len());
    let mut resolved_prices = Vec::with_capacity(step.orders.len());

    for order in &step.orders {
        let limit_px = resolve_limit_price(order, info_http, mid_cache).await?;
        resolved_prices.push(limit_px);
        client_orders.push(build_client_order(order, limit_px)?);
    }

    let builder_code = step
        .builder_code
        .as_deref()
        .or(default_builder)
        .map(|code| code.to_string());

    let mut receiver = broadcaster.subscribe();

    let response = match (builder_code.clone(), client_orders) {
        (Some(code), orders) => {
            let builder = BuilderInfo {
                builder: code.to_lowercase(),
                fee: 0,
            };
            exchange
                .bulk_order_with_builder(orders, None, builder)
                .await
        }
        (None, orders) => exchange.bulk_order(orders, None).await,
    }
    .context("failed to post perp orders")?;

    let ack_value = exchange_status_json(&response);
    let ack_oids = extract_oids(&response);
    let mut per_order_oid: Vec<Option<u64>> = step
        .orders
        .iter()
        .enumerate()
        .map(|(idx, _)| ack_oids.get(idx).copied())
        .collect();

    for (idx, maybe_oid) in per_order_oid.iter_mut().enumerate() {
        if maybe_oid.is_none() {
            continue;
        }
        let oid = maybe_oid.unwrap();
        placed_orders.push_back(PlacedOrder {
            coin: step.orders[idx].coin.clone(),
            oid,
        });
    }

    let mut routed_records = Vec::new();
    for ((order, limit_px), maybe_oid) in step
        .orders
        .iter()
        .zip(resolved_prices.iter())
        .zip(per_order_oid.iter().cloned())
    {
        let builder = order.builder_code.clone().or_else(|| builder_code.clone());
        routed_records.push(RoutedOrderRecord {
            ts_ms: submit_ts,
            oid: maybe_oid,
            coin: order.coin.clone(),
            side: if order.is_buy() {
                "buy".to_string()
            } else {
                "sell".to_string()
            },
            px: *limit_px,
            sz: order.sz,
            tif: order.tif.as_sdk_str().to_string(),
            reduce_only: order.reduce_only,
            builder_code: builder,
        });
    }

    let mut observed_events = Vec::new();
    let mut missing = Vec::new();
    if !ack_oids.is_empty() {
        for maybe_oid in per_order_oid.iter().flatten() {
            let wait = Duration::from_millis(effect_timeout_ms);
            match wait_for_order_event(&mut receiver, *maybe_oid, wait).await {
                Some(event) => observed_events.push(event.payload().clone()),
                None => missing.push(*maybe_oid),
            }
        }
    }

    let observed_value = if observed_events.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(observed_events))
    };

    let notes = if missing.is_empty() {
        None
    } else {
        Some(format!("no websocket confirmation for oids: {:?}", missing))
    };

    let request_orders: Vec<_> = step
        .orders
        .iter()
        .zip(resolved_prices.iter())
        .map(|(order, limit_px)| {
            json!({
                "coin": order.coin,
                "side": if order.is_buy() { "buy" } else { "sell" },
                "sz": order.sz,
                "tif": order.tif.as_sdk_str(),
                "reduceOnly": order.reduce_only,
                "builderCode": order.builder_code,
                "px": order_price_label(&order.px),
                "resolvedPx": limit_px,
                "trigger": "none",
            })
        })
        .collect();
    let mut request_value = json!({
        "perp_orders": {
            "orders": request_orders,
        }
    });
    if let Some(code) = &builder_code {
        request_value["perp_orders"]["builderCode"] = json!(code);
    }

    {
        let mut artifacts = artifacts.lock().await;
        let record = artifacts.make_action_record(
            step_idx,
            "perp_orders",
            submit_ts,
            request_value,
            Some(ack_value),
            observed_value,
            notes,
        );
        artifacts.log_action(&record)?;
        for record in routed_records {
            artifacts.log_routed_order(&record)?;
        }
    }

    Ok(())
}

async fn execute_cancel_last(
    step_idx: usize,
    step: &CancelLastStep,
    artifacts: &Arc<Mutex<RunArtifacts>>,
    exchange: &ExchangeClient,
    placed_orders: &mut VecDeque<PlacedOrder>,
    broadcaster: &broadcast::Sender<ObservedEvent>,
    effect_timeout_ms: u64,
) -> Result<()> {
    let target = if let Some(coin) = &step.coin {
        placed_orders
            .iter()
            .rfind(|order| &order.coin == coin)
            .cloned()
    } else {
        placed_orders.back().cloned()
    };

    let mut notes = None;
    let mut observed_value = None;
    let submit_ts = timestamp_ms();
    let mut ack_value = json!({ "status": "skipped" });

    if let Some(target_order) = target {
        let mut receiver = broadcaster.subscribe();
        let request = ClientCancelRequest {
            asset: target_order.coin.clone(),
            oid: target_order.oid,
        };
        let response = exchange
            .cancel(request, None)
            .await
            .context("failed to cancel order")?;
        ack_value = exchange_status_json(&response);
        if matches!(response, ExchangeResponseStatus::Ok(_)) {
            placed_orders.retain(|order| order.oid != target_order.oid);

            let wait = Duration::from_millis(effect_timeout_ms);
            if let Some(event) = wait_for_order_event(&mut receiver, target_order.oid, wait).await {
                observed_value = Some(event.payload().clone());
            } else {
                notes = Some(format!(
                    "no cancel confirmation for oid {}",
                    target_order.oid
                ));
            }
        } else {
            notes = Some("cancel request rejected".to_string());
        }
    } else {
        notes = Some("no tracked order available for cancel_last".to_string());
    }

    let request_value = json!({
        "cancel_last": {
            "coin": step.coin,
        }
    });

    {
        let mut artifacts = artifacts.lock().await;
        let record = artifacts.make_action_record(
            step_idx,
            "cancel_last",
            submit_ts,
            request_value,
            Some(ack_value),
            observed_value,
            notes,
        );
        artifacts.log_action(&record)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_cancel_oids(
    step_idx: usize,
    step: &CancelOidsStep,
    artifacts: &Arc<Mutex<RunArtifacts>>,
    exchange: &ExchangeClient,
    placed_orders: &mut VecDeque<PlacedOrder>,
    broadcaster: &broadcast::Sender<ObservedEvent>,
    effect_timeout_ms: u64,
) -> Result<()> {
    if step.oids.is_empty() {
        return Ok(());
    }

    let submit_ts = timestamp_ms();
    let mut receiver = broadcaster.subscribe();
    let cancels: Vec<ClientCancelRequest> = step
        .oids
        .iter()
        .map(|oid| ClientCancelRequest {
            asset: step.coin.clone(),
            oid: *oid,
        })
        .collect();

    let response = exchange
        .bulk_cancel(cancels, None)
        .await
        .context("failed to cancel specified oids")?;
    let ack_value = exchange_status_json(&response);
    let success = matches!(response, ExchangeResponseStatus::Ok(_));

    let (observed_value, notes) = if success {
        remove_tracked_oids(placed_orders, &step.oids);

        let mut observed = Vec::new();
        let mut missing = Vec::new();
        let wait = Duration::from_millis(effect_timeout_ms);
        for oid in &step.oids {
            match wait_for_order_event(&mut receiver, *oid, wait).await {
                Some(event) => observed.push(event.payload().clone()),
                None => missing.push(*oid),
            }
        }
        let observed_value = if observed.is_empty() {
            None
        } else {
            Some(serde_json::Value::Array(observed))
        };
        let notes = if missing.is_empty() {
            None
        } else {
            Some(format!("missing cancel confirmations for {:?}", missing))
        };
        (observed_value, notes)
    } else {
        (None, Some("cancel request rejected".to_string()))
    };

    let request_value = json!({
        "cancel_oids": {
            "coin": step.coin,
            "oids": step.oids,
        }
    });

    {
        let mut artifacts = artifacts.lock().await;
        let record = artifacts.make_action_record(
            step_idx,
            "cancel_oids",
            submit_ts,
            request_value,
            Some(ack_value),
            observed_value,
            notes,
        );
        artifacts.log_action(&record)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_cancel_all(
    step_idx: usize,
    step: &CancelAllStep,
    artifacts: &Arc<Mutex<RunArtifacts>>,
    exchange: &ExchangeClient,
    placed_orders: &mut VecDeque<PlacedOrder>,
    broadcaster: &broadcast::Sender<ObservedEvent>,
    effect_timeout_ms: u64,
) -> Result<()> {
    let targets: Vec<PlacedOrder> = placed_orders
        .iter()
        .filter(|order| match &step.coin {
            Some(coin) => &order.coin == coin,
            None => true,
        })
        .cloned()
        .collect();

    let submit_ts = timestamp_ms();
    let mut notes = None;
    let mut ack_value = json!({ "status": "skipped" });
    let mut observed_value = None;

    if targets.is_empty() {
        notes = Some("no orders to cancel".to_string());
    } else {
        let mut receiver = broadcaster.subscribe();
        let cancels: Vec<ClientCancelRequest> = targets
            .iter()
            .map(|order| ClientCancelRequest {
                asset: order.coin.clone(),
                oid: order.oid,
            })
            .collect();

        let response = exchange
            .bulk_cancel(cancels, None)
            .await
            .context("failed to cancel tracked orders")?;
        ack_value = exchange_status_json(&response);
        if matches!(response, ExchangeResponseStatus::Ok(_)) {
            let oids: Vec<u64> = targets.iter().map(|order| order.oid).collect();
            remove_tracked_oids(placed_orders, &oids);

            let wait = Duration::from_millis(effect_timeout_ms);
            let mut observed = Vec::new();
            let mut missing = Vec::new();
            for oid in oids {
                match wait_for_order_event(&mut receiver, oid, wait).await {
                    Some(event) => observed.push(event.payload().clone()),
                    None => missing.push(oid),
                }
            }

            observed_value = if observed.is_empty() {
                None
            } else {
                Some(serde_json::Value::Array(observed))
            };
            if !missing.is_empty() {
                notes = Some(format!("missing cancel confirmations for {:?}", missing));
            }
        } else {
            notes = Some("cancel request rejected".to_string());
        }
    }

    let request_value = json!({
        "cancel_all": {
            "coin": step.coin,
        }
    });

    {
        let mut artifacts = artifacts.lock().await;
        let record = artifacts.make_action_record(
            step_idx,
            "cancel_all",
            submit_ts,
            request_value,
            Some(ack_value),
            observed_value,
            notes,
        );
        artifacts.log_action(&record)?;
    }

    Ok(())
}

async fn execute_class_transfer(
    step_idx: usize,
    step: &UsdClassTransferStep,
    artifacts: &Arc<Mutex<RunArtifacts>>,
    exchange: &ExchangeClient,
    broadcaster: &broadcast::Sender<ObservedEvent>,
    effect_timeout_ms: u64,
) -> Result<()> {
    let submit_ts = timestamp_ms();
    let mut receiver = broadcaster.subscribe();
    let response = exchange
        .class_transfer(step.usdc, step.to_perp, None)
        .await
        .context("failed to submit class transfer")?;
    let ack_value = exchange_status_json(&response);

    let wait = Duration::from_millis(effect_timeout_ms);
    let (observed_value, notes) = if matches!(response, ExchangeResponseStatus::Ok(_)) {
        let observed = wait_for_ledger_event(&mut receiver, step.to_perp, wait).await;
        if let Some(event) = observed {
            (Some(event.payload().clone()), None)
        } else {
            (None, Some("no ledger update observed".to_string()))
        }
    } else {
        (None, Some("class transfer rejected".to_string()))
    };

    let request_value = json!({
        "usd_class_transfer": {
            "toPerp": step.to_perp,
            "usdc": step.usdc,
        }
    });

    {
        let mut artifacts = artifacts.lock().await;
        let record = artifacts.make_action_record(
            step_idx,
            "usd_class_transfer",
            submit_ts,
            request_value,
            Some(ack_value),
            observed_value,
            notes,
        );
        artifacts.log_action(&record)?;
    }

    Ok(())
}

async fn execute_set_leverage(
    step_idx: usize,
    step: &SetLeverageStep,
    artifacts: &Arc<Mutex<RunArtifacts>>,
    exchange: &ExchangeClient,
) -> Result<()> {
    let submit_ts = timestamp_ms();
    let response = exchange
        .update_leverage(step.leverage, &step.coin, step.cross, None)
        .await
        .context("failed to update leverage")?;
    let ack_value = exchange_status_json(&response);
    let notes = if matches!(response, ExchangeResponseStatus::Ok(_)) {
        None
    } else {
        Some("set leverage rejected".to_string())
    };

    let request_value = json!({
        "set_leverage": {
            "coin": step.coin,
            "leverage": step.leverage,
            "cross": step.cross,
        }
    });

    {
        let mut artifacts = artifacts.lock().await;
        let record = artifacts.make_action_record(
            step_idx,
            "set_leverage",
            submit_ts,
            request_value,
            Some(ack_value),
            None,
            notes,
        );
        artifacts.log_action(&record)?;
    }

    Ok(())
}
