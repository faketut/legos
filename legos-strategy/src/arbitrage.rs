//! 跨场所套利策略：捕捉两个交易场所之间的价差。
//!
//! 接线方式（编译期织入，无运行时开销）：
//!
//! ```text
//! 场所 A feed → 账簿 A ─┬─→ on_tick(场所 A 的 bid/ask) ─→ OrderIntent
//! 场所 B feed → 账簿 B ─┘
//!        （每 tick 先调 update_venue_b 同步场所 B 快照）
//! ```
//!
//! 触发条件（`threshold_ticks` 为价差阈值，覆盖手续费/滑点后仍有利可图）：
//!
//! * `bid_A > ask_B + threshold` → 在 B 以 `ask_B` 买入（意图的 `price=ask_B`）；
//! * `bid_B > ask_A + threshold` → 在 A 以 `ask_A` 买入。
//!
//! 发出的只是**买入腿**的意图；卖出腿的对冲由执行层 / 组合管理负责
//! （最小可用语义，文档化说明）。

use legos_core::{OrderIntent, Side, TradingStrategy};

pub struct ArbitrageStrategy {
    threshold_ticks: i64,
    order_qty: u32,
    symbol_id: u16,
    next_id: u64,
    venue_b_bid: Option<i64>,
    venue_b_ask: Option<i64>,
}

impl ArbitrageStrategy {
    pub fn new(symbol_id: u16, threshold_ticks: i64, order_qty: u32) -> Self {
        Self {
            threshold_ticks,
            order_qty,
            symbol_id,
            next_id: 0,
            venue_b_bid: None,
            venue_b_ask: None,
        }
    }

    /// 同步场所 B 的最新最优报价（每 tick 调用一次）。
    pub fn update_venue_b(&mut self, bid: Option<(i64, u32)>, ask: Option<(i64, u32)>) {
        self.venue_b_bid = bid.map(|(p, _)| p);
        self.venue_b_ask = ask.map(|(p, _)| p);
    }

    fn emit(&mut self, price: i64) -> OrderIntent {
        self.next_id += 1;
        OrderIntent {
            client_order_id: self.next_id,
            symbol_id: self.symbol_id,
            side: Side::Bid, // 买入腿
            price,
            qty: self.order_qty,
            ts_ns: 0, // 管线统一打戳
        }
    }
}

impl TradingStrategy for ArbitrageStrategy {
    /// `bid`/`ask` 为场所 A 的报价。
    fn on_tick(
        &mut self,
        bid: Option<(i64, u32)>,
        ask: Option<(i64, u32)>,
        _mid: Option<i64>,
    ) -> Option<OrderIntent> {
        let (a_bid, a_ask) = (bid?.0, ask?.0);
        let (b_bid, b_ask) = (self.venue_b_bid?, self.venue_b_ask?);
        if a_bid > b_ask + self.threshold_ticks {
            return Some(self.emit(b_ask)); // A 出价更高：在 B 买
        }
        if b_bid > a_ask + self.threshold_ticks {
            return Some(self.emit(a_ask)); // B 出价更高：在 A 买
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cross_venue_spread() {
        let mut s = ArbitrageStrategy::new(1, 10, 100);
        // 场所 B: bid=999_000 ask=1_001_000
        s.update_venue_b(Some((999_000, 50)), Some((1_001_000, 50)));
        // 场所 A: bid=1_002_000（> 1_001_000+10）→ 在 B 以 1_001_000 买入
        let o = s
            .on_tick(Some((1_002_000, 40)), Some((1_003_000, 40)), None)
            .unwrap();
        assert_eq!((o.side, o.price, o.qty), (Side::Bid, 1_001_000, 100));
    }

    #[test]
    fn no_signal_when_spread_below_threshold() {
        let mut s = ArbitrageStrategy::new(1, 10, 100);
        s.update_venue_b(Some((999_000, 50)), Some((1_001_000, 50)));
        // 价差 1_001_005 - 1_001_000 = 5 < 10：无信号
        assert_eq!(
            s.on_tick(Some((1_001_005, 40)), Some((1_002_000, 40)), None),
            None
        );
    }

    #[test]
    fn no_signal_without_venue_b_snapshot() {
        let mut s = ArbitrageStrategy::new(1, 0, 100);
        assert_eq!(
            s.on_tick(Some((2_000_000, 1)), Some((2_001_000, 1)), None),
            None,
            "场所 B 快照缺失时不交易"
        );
    }
}
