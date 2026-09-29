//! TestnetWsFeed：Binance Testnet 免费行情 WebSocket 流（实盘验证积木）。
//!
//! 实现 [`MarketDataFeed`](legos_core::MarketDataFeed)，把 Binance
//! `@trade` 流的消息转成 [`MarketTick`](legos_core::MarketTick)。
//! 行情流**无需 API key**；只有下单（`TestnetRestGateway`）需要 testnet key。
//!
//! # 数据刻度契约（与 scripts/fetch_ticks.py 严格一致）
//!
//! * `price`：`1 tick = 1e-8` 计价货币（如 USDT），即
//!   `price_ticks = floor(price_f64 * 1e8 + 0.5)`（非负值统一用此舍入）；
//! * `qty`：`1 = 1e-8` 基础货币，`qty = floor(qty_f64 * 1e8 + 0.5)`，
//!   超出 `u32` 范围时**饱和**为 `u32::MAX`（文档化取舍）；
//! * `side`：取 Binance `m` 字段（buyer is maker）；`m=true` 表示买方挂单
//!   被吃 → `Bid`（与 ITCH「side = 被吃掉的挂单方向」语义一致）；
//! * `ts_ns`：`T`（毫秒）× 1e6；`order_id`：`t`（trade id）截断为 u32。
//!
//! # 连接语义
//!
//! * `connect` 时立即建连；读线程阻塞读 WS，`next_event` 用
//!   `recv_timeout(1s)` 取消息——超时返回 `None`（暂时无数据），
//!   与 `MarketDataFeed`「`None` = 暂时无数据」契约一致；
//! * 断线时读线程退出，`next_event` 感知到 `Disconnected` 后**下次调用
//!   自动重连**（`reconnects` 计数）；
//! * JSON 解析失败的消息跳过（`parse_errors` 计数），不中断流。

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use legos_core::{MarketDataFeed, MarketTick, Side};
use tungstenite::Message;

/// 价格/数量刻度：1e-8。
pub const PRICE_SCALE: f64 = 100_000_000.0;
pub const QTY_SCALE: f64 = 100_000_000.0;

/// Binance `@trade` 流消息 → `MarketTick`（纯函数，可单测）。
///
/// 期望的 JSON 形如：
/// `{"e":"trade","s":"BTCUSDT","t":12345,"p":"97500.12","q":"0.5",
///   "T":1727486400000,"m":true}`
pub fn parse_trade_message(symbol_id: u16, text: &str) -> Option<MarketTick> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("e")?.as_str()? != "trade" {
        return None;
    }
    let price_f: f64 = v.get("p")?.as_str()?.parse().ok()?;
    let qty_f: f64 = v.get("q")?.as_str()?.parse().ok()?;
    let is_buyer_maker = v.get("m")?.as_bool()?;
    let trade_id = v.get("t")?.as_u64()?;
    let ts_ms = v.get("T")?.as_u64()?;

    if !price_f.is_finite() || price_f <= 0.0 {
        return None;
    }
    if !qty_f.is_finite() || qty_f < 0.0 {
        return None;
    }
    // 非负值统一 floor(x + 0.5)，与 scripts/fetch_ticks.py 的舍入一致。
    let price = (price_f * PRICE_SCALE + 0.5).floor() as i64;
    let qty = ((qty_f * QTY_SCALE + 0.5).floor().min(u32::MAX as f64)) as u32;
    let ts_ns = ts_ms.checked_mul(1_000_000)?;
    let side = if is_buyer_maker { Side::Bid } else { Side::Ask };

    Some(MarketTick::new(
        symbol_id,
        price,
        qty,
        side,
        legos_core::EventKind::Trade,
        trade_id as u32, // 截断说明见模块文档
        ts_ns,
    ))
}

/// Binance Testnet 免费 WS 行情 feed。
pub struct TestnetWsFeed {
    url: String,
    symbol_id: u16,
    rx: Option<mpsc::Receiver<String>>,
    /// 成功解析的消息数。
    pub messages: u64,
    /// 解析失败跳过的消息数。
    pub parse_errors: u64,
    /// 重连次数。
    pub reconnects: u64,
}

impl TestnetWsFeed {
    /// 连接任意 Binance 兼容的 `@trade` WS 地址。
    pub fn connect(url: &str, symbol_id: u16) -> Self {
        let mut f = Self {
            url: url.to_string(),
            symbol_id,
            rx: None,
            messages: 0,
            parse_errors: 0,
            reconnects: 0,
        };
        f.ensure_connected();
        f
    }

    /// Binance **testnet** 现货 trade 流：`wss://testnet.binance.vision/ws/<symbol>@trade`
    ///（`symbol` 如 `"BTCUSDT"`，大小写不敏感）。行情流无需 key。
    pub fn binance_testnet(symbol: &str, symbol_id: u16) -> Self {
        Self::connect(
            &format!(
                "wss://testnet.binance.vision/ws/{}@trade",
                symbol.to_lowercase()
            ),
            symbol_id,
        )
    }

    /// Binance **主网**公开 trade 流（同样免费、无需 key；注意部分地区
    /// 对 api.binance.com 有地理限制，testnet 也可能受限）。
    pub fn binance_mainnet(symbol: &str, symbol_id: u16) -> Self {
        Self::connect(
            &format!(
                "wss://stream.binance.com:9443/ws/{}@trade",
                symbol.to_lowercase()
            ),
            symbol_id,
        )
    }

    fn ensure_connected(&mut self) {
        if self.rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let url = self.url.clone();
        // 读线程：阻塞读 WS，文本消息转发给主线程；任何错误即退出，
        // 主线程感知 Disconnected 后重连。
        thread::spawn(move || {
            let (mut socket, _) = match tungstenite::connect(&url) {
                Ok(x) => x,
                Err(_) => return,
            };
            loop {
                match socket.read() {
                    Ok(Message::Text(t)) => {
                        let s: &str = &t;
                        if tx.send(s.to_string()).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {} // ping/pong/binary：忽略
                    Err(_) => break,
                }
            }
        });
        self.rx = Some(rx);
    }
}

impl MarketDataFeed for TestnetWsFeed {
    fn next_event(&mut self) -> Option<MarketTick> {
        self.ensure_connected();
        let rx = self.rx.as_ref().expect("ensure_connected 后 rx 必存在");
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(text) => match parse_trade_message(self.symbol_id, &text) {
                    Some(t) => {
                        self.messages += 1;
                        return Some(t);
                    }
                    None => {
                        self.parse_errors += 1;
                        continue; // 坏消息跳过
                    }
                },
                Err(RecvTimeoutError::Timeout) => return None, // 暂时无数据
                Err(RecvTimeoutError::Disconnected) => {
                    self.rx = None;
                    self.reconnects += 1;
                    return None; // 下次调用自动重连
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legos_core::EventKind;

    fn trade_json(m: bool) -> String {
        format!(
            r#"{{"e":"trade","E":1727486400001,"s":"BTCUSDT","t":424242,"p":"97500.12","q":"0.5","T":1727486400000,"m":{m},"M":true}}"#
        )
    }

    #[test]
    fn parses_buyer_maker_trade() {
        // m=true：买方是 maker（挂单被吃）→ side=Bid
        let t = parse_trade_message(1, &trade_json(true)).unwrap();
        assert_eq!(t.symbol_id, 1);
        assert_eq!(t.side(), Side::Bid);
        assert_eq!(t.kind(), EventKind::Trade);
        assert_eq!(t.price, 9_750_012_000_000, "97500.12 * 1e8");
        assert_eq!(t.qty, 50_000_000, "0.5 * 1e8");
        assert_eq!(t.ts_ns, 1_727_486_400_000_000_000);
        assert_eq!(t.order_id, 424242);
        assert_eq!(std::mem::size_of_val(&t), 32);
        assert!(t.reserved_bits_clear());
    }

    #[test]
    fn parses_seller_maker_trade() {
        // m=false：卖方是 maker → side=Ask
        let t = parse_trade_message(1, &trade_json(false)).unwrap();
        assert_eq!(t.side(), Side::Ask);
    }

    #[test]
    fn rejects_malformed_messages() {
        assert!(parse_trade_message(1, "not json").is_none());
        assert!(parse_trade_message(1, r#"{"e":"kline"}"#).is_none());
        assert!(parse_trade_message(1, r#"{"e":"trade","p":"abc","q":"1","m":true,"t":1,"T":1}"#).is_none());
        assert!(parse_trade_message(1, r#"{"e":"trade","p":"-5","q":"1","m":true,"t":1,"T":1}"#).is_none());
    }

    #[test]
    fn qty_saturates_at_u32_max() {
        // 100 BTC = 1e10 > u32::MAX：饱和而非回绕
        let j = r#"{"e":"trade","p":"97500.12","q":"100","m":true,"t":1,"T":1}"#;
        let t = parse_trade_message(1, j).unwrap();
        assert_eq!(t.qty, u32::MAX);
    }
}
