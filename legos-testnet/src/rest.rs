//! TestnetRestGateway：Binance Testnet 免费下单 REST API（paper-trading 积木）。
//!
//! 实现 [`ExecutionGateway`](legos_core::ExecutionGateway)，把
//! [`OrderIntent`](legos_core::OrderIntent) 发到 Binance **testnet**
//!（虚拟资金），是实盘压力测试的可替换积木。
//!
//! # 认证（绝不硬编码）
//!
//! API key 只从环境变量读取：
//!
//! * `BINANCE_TESTNET_API_KEY`
//! * `BINANCE_TESTNET_API_SECRET`
//!
//! 缺失/为空时 `new` 返回 `Err`（不 panic）。免费申请见
//! README「0 成本实盘验证」章节（testnet.binance.vision，虚拟资金）。
//!
//! # 签名与请求
//!
//! 按 Binance 规则：`signature = HMAC_SHA256(query_string, api_secret)`，
//! 查询串追加 `&signature=`，请求头带 `X-MBX-APIKEY`。
//! `POST {base}/api/v3/order`。
//!
//! # 数量/价格刻度
//!
//! 与 `TestnetWsFeed` / `scripts/fetch_ticks.py` 一致：`1e-8` 单位。
//! `qty: u32`（1e-8 基础货币）→ `"0.5"`；`price: i64` ticks → `"97500.12"`。
//! `price == 0` 的 intent 发 `MARKET` 单；限价单带 `timeInForce=GTC`。
//!
//! # 状态映射
//!
//! Binance `status` → [`AckStatus`](legos_core::AckStatus)：
//! `NEW`→Accepted，`PARTIALLY_FILLED`→PartiallyFilled，`FILLED`→Filled，
//! 其他/错误体（含 `code`）/传输失败 → Rejected（`last_error` 保留原因）。

use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use legos_core::{AckStatus, ExecutionGateway, OrderAck, OrderIntent, Side};
use sha2::Sha256;

/// API key 的环境变量名。
pub const ENV_API_KEY: &str = "BINANCE_TESTNET_API_KEY";
/// API secret 的环境变量名。
pub const ENV_API_SECRET: &str = "BINANCE_TESTNET_API_SECRET";

/// Binance testnet REST 基地址。
pub const TESTNET_BASE_URL: &str = "https://testnet.binance.vision";

/// 数量/价格刻度：1e-8（与 ws.rs 一致）。
const SCALE: f64 = 100_000_000.0;

/// `1e-8` 单位的 u32 → Binance 可接受的小数字符串（去尾零，如 `"0.5"`）。
fn fmt_scaled(v: u32) -> String {
    let s = format!("{:.8}", v as f64 / SCALE);
    let t = s.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() {
        "0".to_string()
    } else {
        t.to_string()
    }
}

/// `1e-8` 单位的 i64 ticks → 小数字符串（整数运算，精确无浮点；
/// i64 价格可达 9e10 量级，远超 u32，不能走 `fmt_scaled`）。
fn fmt_price_ticks(ticks: i64) -> String {
    let neg = ticks < 0;
    let a = ticks.unsigned_abs();
    let int_part = a / 100_000_000;
    let frac_part = a % 100_000_000;
    let s = if frac_part == 0 {
        int_part.to_string()
    } else {
        format!("{int_part}.{}", format!("{frac_part:08}").trim_end_matches('0'))
    };
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Binance Testnet 下单网关（paper trading）。
pub struct TestnetRestGateway {
    api_key: String,
    api_secret: String,
    base_url: String,
    symbol: String,
    symbol_id: u16,
    recv_window_ms: u64,
    /// 已发送订单数。
    pub orders_sent: u64,
    /// 最近一次失败的原因（成功时为 None）。
    last_error: Option<String>,
}

impl TestnetRestGateway {
    /// 从环境变量读取 key/secret 构造网关。
    pub fn new(symbol: &str, symbol_id: u16) -> Result<Self, String> {
        let api_key = env::var(ENV_API_KEY).map_err(|_| {
            format!(
                "缺少环境变量 {ENV_API_KEY}：testnet key 免费申请见 README「0 成本实盘验证」"
            )
        })?;
        let api_secret = env::var(ENV_API_SECRET).map_err(|_| {
            format!(
                "缺少环境变量 {ENV_API_SECRET}：testnet key 免费申请见 README「0 成本实盘验证」"
            )
        })?;
        if api_key.is_empty() || api_secret.is_empty() {
            return Err(format!("{ENV_API_KEY} / {ENV_API_SECRET} 为空"));
        }
        Ok(Self {
            api_key,
            api_secret,
            base_url: TESTNET_BASE_URL.to_string(),
            symbol: symbol.to_uppercase(),
            symbol_id,
            recv_window_ms: 5000,
            orders_sent: 0,
            last_error: None,
        })
    }

    /// 最近一次失败的原因。
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// HMAC-SHA256(query, secret) → 十六进制签名（纯函数，可单测）。
    fn sign(&self, query: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC-SHA256 接受任意长度 key");
        mac.update(query.as_bytes());
        hex_encode(&mac.finalize().into_bytes())
    }

    /// 构造待签名的查询串（纯函数，可单测；不含 signature）。
    fn build_order_query(&self, order: &OrderIntent, timestamp_ms: u64) -> Result<String, String> {
        if order.symbol_id != self.symbol_id {
            return Err("symbol_id 与网关不符".to_string());
        }
        if order.qty == 0 {
            return Err("qty 为 0".to_string());
        }
        let side = match order.side {
            Side::Bid => "BUY",
            Side::Ask => "SELL",
        };
        let mut q = format!("symbol={}&side={side}&type=", self.symbol);
        if order.price == 0 {
            q.push_str(&format!("MARKET&quantity={}", fmt_scaled(order.qty)));
        } else {
            if order.price < 0 {
                return Err("price 为负".to_string());
            }
            q.push_str(&format!(
                "LIMIT&timeInForce=GTC&quantity={}&price={}",
                fmt_scaled(order.qty),
                fmt_price_ticks(order.price)
            ));
        }
        q.push_str(&format!(
            "&timestamp={timestamp_ms}&recvWindow={}",
            self.recv_window_ms
        ));
        Ok(q)
    }

    /// 解析下单响应体 → OrderAck（纯函数，可单测）。
    fn parse_order_response(client_order_id: u64, body: &str) -> OrderAck {
        let v: serde_json::Value =
            match serde_json::from_str(body) {
                Ok(v) => v,
                Err(_) => return OrderAck::rejected(client_order_id),
            };
        // Binance 错误体 {"code":-1102,"msg":"..."} → 拒单
        if v.get("code").is_some() {
            return OrderAck::rejected(client_order_id);
        }
        let status = v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let ack_status = match status {
            "NEW" => AckStatus::Accepted,
            "PARTIALLY_FILLED" => AckStatus::PartiallyFilled,
            "FILLED" => AckStatus::Filled,
            _ => AckStatus::Rejected,
        };
        let exec_qty_f: f64 = v
            .get("executedQty")
            .and_then(|q| q.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let cum_quote_f: f64 = v
            .get("cummulativeQuoteQty")
            .and_then(|q| q.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let filled_qty =
            ((exec_qty_f * SCALE + 0.5).floor().min(u32::MAX as f64).max(0.0)) as u32;
        let avg_price = if exec_qty_f > 0.0 {
            ((cum_quote_f / exec_qty_f * SCALE + 0.5).floor()) as i64
        } else {
            0
        };
        OrderAck {
            client_order_id,
            status: ack_status,
            filled_qty,
            avg_price,
        }
    }
}

impl ExecutionGateway for TestnetRestGateway {
    fn send_order(&mut self, order: &OrderIntent) -> OrderAck {
        let query = match self.build_order_query(order, now_ms()) {
            Ok(q) => q,
            Err(e) => {
                self.last_error = Some(e);
                return OrderAck::rejected(order.client_order_id);
            }
        };
        let sig = self.sign(&query);
        let url = format!("{}/api/v3/order?{}&signature={sig}", self.base_url, query);
        let body = match ureq::post(&url).set("X-MBX-APIKEY", &self.api_key).call() {
            Ok(resp) => match resp.into_string() {
                Ok(b) => b,
                Err(e) => {
                    self.last_error = Some(format!("读取响应失败: {e}"));
                    return OrderAck::rejected(order.client_order_id);
                }
            },
            Err(ureq::Error::Status(code, resp)) => {
                let detail = resp.into_string().unwrap_or_default();
                self.last_error = Some(format!("HTTP {code}: {detail}"));
                // Binance 错误体仍可解析出 code → Rejected
                let ack = Self::parse_order_response(order.client_order_id, &detail);
                return ack;
            }
            Err(e) => {
                self.last_error = Some(format!("请求失败: {e}"));
                return OrderAck::rejected(order.client_order_id);
            }
        };
        self.last_error = None;
        self.orders_sent += 1;
        Self::parse_order_response(order.client_order_id, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway_with_secret(secret: &str) -> TestnetRestGateway {
        TestnetRestGateway {
            api_key: "k".to_string(),
            api_secret: secret.to_string(),
            base_url: TESTNET_BASE_URL.to_string(),
            symbol: "BTCUSDT".to_string(),
            symbol_id: 1,
            recv_window_ms: 5000,
            orders_sent: 0,
            last_error: None,
        }
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_vector() {
        // RFC 4231 Test Case 1: key = 20×0x0b, data = "Hi There"
        let g = gateway_with_secret(&"\x0b".repeat(20));
        assert_eq!(
            g.sign("Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn builds_limit_order_query() {
        let g = gateway_with_secret("s");
        let order = OrderIntent { client_order_id: 7, symbol_id: 1, side: Side::Bid, price: 9_750_012_000_000, qty: 50_000_000, ts_ns: 0 };
        let q = g.build_order_query(&order, 1_727_486_400_000).unwrap();
        assert_eq!(
            q,
            "symbol=BTCUSDT&side=BUY&type=LIMIT&timeInForce=GTC&quantity=0.5&price=97500.12\
             &timestamp=1727486400000&recvWindow=5000"
        );
    }

    #[test]
    fn builds_market_order_query() {
        let g = gateway_with_secret("s");
        let order = OrderIntent { client_order_id: 7, symbol_id: 1, side: Side::Ask, price: 0, qty: 100_000_000, ts_ns: 0 };
        let q = g.build_order_query(&order, 1).unwrap();
        assert!(q.contains("type=MARKET&quantity=1"), "q={q}");
        assert!(q.contains("side=SELL"), "q={q}");
        assert!(!q.contains("price="), "市价单不应带 price: {q}");
    }

    #[test]
    fn rejects_bad_intents_locally() {
        let g = gateway_with_secret("s");
        let wrong_sym = OrderIntent { client_order_id: 1, symbol_id: 2, side: Side::Bid, price: 100, qty: 1, ts_ns: 0 };
        assert!(g.build_order_query(&wrong_sym, 1).is_err());
        let zero_qty = OrderIntent { client_order_id: 1, symbol_id: 1, side: Side::Bid, price: 100, qty: 0, ts_ns: 0 };
        assert!(g.build_order_query(&zero_qty, 1).is_err());
    }

    #[test]
    fn parses_new_and_filled_responses() {
        let new_ack = TestnetRestGateway::parse_order_response(
            7,
            r#"{"symbol":"BTCUSDT","orderId":1,"status":"NEW","executedQty":"0.00000000","cummulativeQuoteQty":"0.00000000"}"#,
        );
        assert_eq!(new_ack.status, AckStatus::Accepted);
        assert_eq!(new_ack.filled_qty, 0);

        let filled = TestnetRestGateway::parse_order_response(
            7,
            r#"{"symbol":"BTCUSDT","orderId":2,"status":"FILLED","executedQty":"0.50000000","cummulativeQuoteQty":"48750.06000000"}"#,
        );
        assert_eq!(filled.status, AckStatus::Filled);
        assert_eq!(filled.filled_qty, 50_000_000);
        assert_eq!(filled.avg_price, 9_750_012_000_000, "48750.06/0.5 = 97500.12");
    }

    #[test]
    fn parses_error_body_as_rejected() {
        let ack = TestnetRestGateway::parse_order_response(
            7,
            r#"{"code":-1102,"msg":"Mandatory parameter 'timestamp' was not sent."}"#,
        );
        assert_eq!(ack.status, AckStatus::Rejected);
        let garbage = TestnetRestGateway::parse_order_response(7, "not json");
        assert_eq!(garbage.status, AckStatus::Rejected);
    }

    #[test]
    fn new_requires_env_vars() {
        // 保存并清空环境变量，测完恢复（本进程内无其他测试依赖它们）。
        let saved_key = env::var(ENV_API_KEY).ok();
        let saved_secret = env::var(ENV_API_SECRET).ok();
        env::remove_var(ENV_API_KEY);
        env::remove_var(ENV_API_SECRET);
        let err = match TestnetRestGateway::new("BTCUSDT", 1) {
            Ok(_) => panic!("缺少环境变量时 new 必须失败"),
            Err(e) => e,
        };
        assert!(err.contains(ENV_API_KEY), "错误信息应指引用户: {err}");
        if let Some(v) = saved_key {
            env::set_var(ENV_API_KEY, v);
        }
        if let Some(v) = saved_secret {
            env::set_var(ENV_API_SECRET, v);
        }
    }
}
