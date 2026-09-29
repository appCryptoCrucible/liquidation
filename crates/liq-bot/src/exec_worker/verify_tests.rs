use super::*;
use alloy_primitives::{address, Bytes};
use liq_types::{AssetId, FlashProvider, MarketId, PositionKey, ProtocolId, TraceId, TriggerKind};
use serde_json::{json, Value};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Minimal JSON-RPC node: `answer` maps each request to its result; every
/// request body is kept.
async fn node(answer: fn(&Value) -> Value) -> (String, Arc<Mutex<Vec<Value>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_t = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = l.accept().await {
            let mut buf = vec![0u8; 65_536];
            let mut got = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                if let Some(p) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&got[..p]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if got.len() >= p + 4 + len {
                        break;
                    }
                }
            }
            let p = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let req: Value = serde_json::from_slice(&got[p + 4..]).unwrap();
            let body =
                serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 1, "result": answer(&req)}))
                    .unwrap();
            seen_t.lock().unwrap().push(req);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
        }
    });
    (url, seen)
}

fn job() -> ExecJob {
    ExecJob {
        trace: TraceId::from_raw(1),
        plan: Bytes::from_static(&[1]),
        trigger: TriggerKind::InterestDrift,
        protocol: ProtocolId(1),
        market: MarketId(1),
        collateral: AssetId(0),
        debt: AssetId(1),
        flash: FlashProvider::Aave,
        operator_key: Address::ZERO,
        position: PositionKey {
            protocol: ProtocolId(1),
            market: MarketId(1),
            user: Address::repeat_byte(7),
        },
        hint_hash: None,
        backrun_tx: None,
        target_block: 101,
        max_block: 101,
        fee: liq_exec::fee::FeeQuote {
            parent_block: 100,
            next_base_fee: 1_000,
            priority_wei: 10,
            modest_priority_wei: 2,
        },
        auction_bps: 0,
        calldata: Bytes::from_static(&[0xee]),
        gas_limit: 1,
        chain_id: 1,
        slot: 0,
        rpc_verify: true,
    }
}

fn sim(ok: bool) -> Value {
    json!([{"calls": [{
        "status": if ok { "0x1" } else { "0x0" },
        "gasUsed": "0x186a0",
        "returnData": "0x",
        "logs": [],
    }]}])
}

fn answer_ok(req: &Value) -> Value {
    match req["method"].as_str().unwrap() {
        "eth_getBlockByNumber" => json!({"number": "0x64", "timestamp": "0x3e8"}),
        _ => sim(true),
    }
}

fn answer_revert(req: &Value) -> Value {
    match req["method"].as_str().unwrap() {
        "eth_getBlockByNumber" => json!({"number": "0x64", "timestamp": "0x3e8"}),
        _ => sim(false),
    }
}

/// A job the drain could not simulate in process is simulated against the
/// node on the block before its target, as the target block, at the price it
/// will pay, with the Executor's code where it will be sent; it is sized from
/// that simulation, and dropped if it reverts.
#[tokio::test(flavor = "current_thread")]
async fn rpc_verification_sizes_or_drops_the_job() {
    let operator = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let t = GovTarget {
        executor: liq_sim::PLANNED_EXECUTOR,
        code: Some(Bytes::from_static(&[0x60, 0x00])),
    };
    let (url, seen) = node(answer_ok).await;
    let chain = ChainClient::new(&url).unwrap();
    assert_eq!(
        verify(&job(), &chain, &t, operator, 100).await,
        Some(120_000)
    );
    let reqs = seen.lock().unwrap().clone();
    let sim_req = reqs
        .iter()
        .find(|r| r["method"] == "eth_simulateV1")
        .unwrap();
    let block = &sim_req["params"][0]["blockStateCalls"][0];
    assert_eq!(sim_req["params"][1], "0x64");
    assert_eq!(block["blockOverrides"]["number"], "0x65");
    assert_eq!(
        block["blockOverrides"]["time"],
        format!("{:#x}", 1_000 + 12)
    );
    assert_eq!(block["calls"][0]["gasPrice"], format!("{:#x}", 1_010));
    assert!(
        block["stateOverrides"][format!("{:#x}", liq_sim::PLANNED_EXECUTOR)]["code"].is_string()
    );

    let (url, _) = node(answer_revert).await;
    let chain = ChainClient::new(&url).unwrap();
    assert_eq!(verify(&job(), &chain, &t, operator, 100).await, None);
}
