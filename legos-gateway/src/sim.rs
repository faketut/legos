//! 模拟交易所（回测积木）：本地内存撮合。
//!
//! 每个品种维护一组最新最优买卖报价（由外部行情驱动，通过
//! [`SimulatedExchange::update_quote`] 灌入）。`send_order` 规则：
//!
//! * **市价单**（`price == 0`）：按对手价立即成交；
//! * **限价单**：买价 `>=` 卖一价（或卖价 `<=` 买一价）时立即成交，
//!   否则作为挂单 `Accepted`（不建完整订单簿——回测撮合的最小可用语义）；
//! * 成交价 = 触及的对手价 ± `slippage_ticks`（滑点模型）；
//! * 数量超过对手盘时部分成交（`PartiallyFilled`），并扣减该档剩余量。
//!
//! 报价表用固定 64 槽位数组实现，**零堆分配**。
//! `send_order` 永不 panic：未知品种 / 零数量一律返回 `Rejected` 回执。

use legos_core::{AckStatus, ExecutionGateway, OrderAck, OrderIntent, Side};

/// 单个品种的模拟报价。
#[derive(Clone, Copy, Debug, Default)]
struct SimQuote {
    bid: i64,
    bid_qty: u32,
    ask: i64,
    ask_qty: u32,
    valid: bool,
}

const MAX_SYMBOLS: usize = 64;

/// 内存撮合模拟器。
pub struct SimulatedExchange {
    quotes: [Option<(u16, SimQuote)>; MAX_SYMBOLS],
    /// 每笔成交的固定滑点（tick），买单加、卖单减。
    slippage_ticks: i64,
    pub fills: u64,
    pub rejected: u64,
}

impl SimulatedExchange {
    pub fn new(slippage_ticks: i64) -> Self {
        Self {
            quotes: [None; MAX_SYMBOLS],
            slippage_ticks,
            fills: 0,
            rejected: 0,
        }
    }

    /// 灌入某品种最新最优买卖报价（通常由账簿的 best_bid/best_ask 驱动）。
    pub fn update_quote(&mut self, symbol_id: u16, bid: (i64, u32), ask: (i64, u32)) {
        let q = SimQuote {
            bid: bid.0,
            bid_qty: bid.1,
            ask: ask.0,
            ask_qty: ask.1,
            valid: true,
        };
        if let Some(slot) = self.quotes.iter_mut().find(|s| s.map(|(id, _)| id) == Some(symbol_id))
        {
            *slot = Some((symbol_id, q));
            return;
        }
        if let Some(slot) = self.quotes.iter_mut().find(|s| s.is_none()) {
            *slot = Some((symbol_id, q));
        }
        // 槽位用尽：静默忽略（回测品种数远小于 64）。
    }

    fn quote(&self, symbol_id: u16) -> Option<SimQuote> {
        self.quotes
            .iter()
            .flatten()
            .find(|(id, _)| *id == symbol_id)
            .map(|(_, q)| *q)
            .filter(|q| q.valid)
    }

    fn quote_mut(&mut self, symbol_id: u16) -> Option<&mut SimQuote> {
        self.quotes
            .iter_mut()
            .flatten()
            .find(|(id, _)| *id == symbol_id)
            .map(|(_, q)| q)
            .filter(|q| q.valid)
    }
}

impl ExecutionGateway for SimulatedExchange {
    fn on_quote(&mut self, symbol_id: u16, bid: Option<(i64, u32)>, ask: Option<(i64, u32)>) {
        if let (Some(b), Some(a)) = (bid, ask) {
            self.update_quote(symbol_id, b, a);
        }
    }

    fn send_order(&mut self, order: &OrderIntent) -> OrderAck {
        let quote = match self.quote(order.symbol_id) {
            Some(q) => q,
            None => {
                self.rejected += 1;
                return OrderAck::rejected(order.client_order_id);
            }
        };
        if order.qty == 0 {
            self.rejected += 1;
            return OrderAck::rejected(order.client_order_id);
        }

        // 对手盘价/量。
        let (touch_price, touch_qty) = match order.side {
            Side::Bid => (quote.ask, quote.ask_qty),
            Side::Ask => (quote.bid, quote.bid_qty),
        };
        // 是否可成交：市价恒可；限价需交叉。
        let marketable = order.price == 0
            || match order.side {
                Side::Bid => order.price >= touch_price,
                Side::Ask => order.price <= touch_price,
            };
        if !marketable || touch_qty == 0 {
            return OrderAck {
                client_order_id: order.client_order_id,
                status: AckStatus::Accepted, // 挂单，未成交
                filled_qty: 0,
                avg_price: 0,
            };
        }

        let fill_qty = order.qty.min(touch_qty);
        let fill_price = match order.side {
            Side::Bid => touch_price + self.slippage_ticks,
            Side::Ask => touch_price - self.slippage_ticks,
        };
        // 扣减对手盘剩余量。
        if let Some(q) = self.quote_mut(order.symbol_id) {
            match order.side {
                Side::Bid => q.ask_qty -= fill_qty,
                Side::Ask => q.bid_qty -= fill_qty,
            }
        }
        self.fills += 1;
        OrderAck {
            client_order_id: order.client_order_id,
            status: if fill_qty == order.qty {
                AckStatus::Filled
            } else {
                AckStatus::PartiallyFilled
            },
            filled_qty: fill_qty,
            avg_price: fill_price,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buy_limit(price: i64, qty: u32) -> OrderIntent {
        OrderIntent {
            client_order_id: 1,
            symbol_id: 1,
            side: Side::Bid,
            price,
            qty,
            ts_ns: 0,
        }
    }

    fn ex() -> SimulatedExchange {
        let mut e = SimulatedExchange::new(2); // 滑点 2 tick
        e.update_quote(1, (100_0000, 500), (100_1000, 400));
        e
    }

    #[test]
    fn crossing_limit_buy_fills_with_slippage() {
        let mut e = ex();
        let ack = e.send_order(&buy_limit(100_1000, 100));
        assert_eq!(ack.status, AckStatus::Filled);
        assert_eq!(ack.filled_qty, 100);
        assert_eq!(ack.avg_price, 100_1000 + 2, "买单成交价 = 卖一 + 滑点");
        assert_eq!(e.fills, 1);
    }

    #[test]
    fn non_crossing_limit_rests_as_accepted() {
        let mut e = ex();
        let ack = e.send_order(&buy_limit(99_0000, 100)); // 低于卖一
        assert_eq!(ack.status, AckStatus::Accepted);
        assert_eq!(ack.filled_qty, 0);
    }

    #[test]
    fn market_order_fills_at_touch() {
        let mut e = ex();
        let mut o = buy_limit(0, 50); // 市价
        let ack = e.send_order(&o);
        assert_eq!(ack.status, AckStatus::Filled);
        assert_eq!(ack.avg_price, 100_1000 + 2);
        o.side = Side::Ask;
        o.client_order_id = 2;
        let ack2 = e.send_order(&o);
        assert_eq!(ack2.avg_price, 100_0000 - 2, "卖单成交价 = 买一 - 滑点");
    }

    #[test]
    fn partial_fill_when_qty_exceeds_touch() {
        let mut e = ex();
        let ack = e.send_order(&buy_limit(100_1000, 1000)); // 对手只有 400
        assert_eq!(ack.status, AckStatus::PartiallyFilled);
        assert_eq!(ack.filled_qty, 400);
        // 盘口被吃空后，再来一单应挂单（无对手量）
        let ack2 = e.send_order(&buy_limit(100_1000, 10));
        assert_eq!(ack2.status, AckStatus::Accepted);
        assert_eq!(ack2.filled_qty, 0);
    }

    #[test]
    fn unknown_symbol_rejected() {
        let mut e = ex();
        let mut o = buy_limit(100_1000, 10);
        o.symbol_id = 999;
        let ack = e.send_order(&o);
        assert_eq!(ack.status, AckStatus::Rejected);
    }
}
