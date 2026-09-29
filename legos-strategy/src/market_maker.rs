//! 高频做市策略：围绕中间价做双边报价。
//!
//! 参数：
//!
//! * `spread_ticks`: 买卖报价总价差（tick）；
//! * `skew_ticks`: 整体偏移（>0 上移报价，表达看多 / 库存偏空时的纠偏）；
//! * `order_qty`: 每笔报价数量。
//!
//! `on_tick` 每次被调用时轮流报出一边（买→卖→买…），因为 trait 每次只返回
//! 一个 `OrderIntent`。实盘中可把返回的 intent 直接送风控/网关，
//! 下一 tick 再报另一边——两 tick 即完成一次双边刷新。

use legos_core::{OrderIntent, Side, TradingStrategy};

pub struct MarketMakerStrategy {
    spread_ticks: i64,
    skew_ticks: i64,
    order_qty: u32,
    symbol_id: u16,
    next_id: u64,
    quote_bid_next: bool,
}

impl MarketMakerStrategy {
    pub fn new(symbol_id: u16, spread_ticks: i64, skew_ticks: i64, order_qty: u32) -> Self {
        Self {
            spread_ticks,
            skew_ticks,
            order_qty,
            symbol_id,
            next_id: 0,
            quote_bid_next: true,
        }
    }
}

impl TradingStrategy for MarketMakerStrategy {
    fn on_tick(
        &mut self,
        _bid: Option<(i64, u32)>,
        _ask: Option<(i64, u32)>,
        mid: Option<i64>,
    ) -> Option<OrderIntent> {
        let mid = mid?;
        let half = self.spread_ticks / 2;
        let (side, price) = if self.quote_bid_next {
            (Side::Bid, mid - half + self.skew_ticks)
        } else {
            (Side::Ask, mid + half + self.skew_ticks)
        };
        self.quote_bid_next = !self.quote_bid_next;
        self.next_id += 1;
        Some(OrderIntent {
            client_order_id: self.next_id,
            symbol_id: self.symbol_id,
            side,
            price,
            qty: self.order_qty,
            // ts_ns 由管线在送风控前统一打戳。
            ts_ns: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_around_mid_with_spread_and_skew() {
        let mut s = MarketMakerStrategy::new(1, 100, 10, 50);
        // mid=1_000_000, spread=100 → half=50, skew=+10
        let b = s.on_tick(None, None, Some(1_000_000)).unwrap();
        assert_eq!((b.side, b.price, b.qty), (Side::Bid, 1_000_000 - 50 + 10, 50));
        let a = s.on_tick(None, None, Some(1_000_000)).unwrap();
        assert_eq!((a.side, a.price, a.qty), (Side::Ask, 1_000_000 + 50 + 10, 50));
        assert_ne!(b.client_order_id, a.client_order_id);
    }

    #[test]
    fn no_mid_no_quote() {
        let mut s = MarketMakerStrategy::new(1, 100, 0, 50);
        assert_eq!(s.on_tick(Some((1, 1)), None, None), None);
    }

    #[test]
    fn zero_spread_quotes_at_mid() {
        let mut s = MarketMakerStrategy::new(1, 0, 0, 10);
        let q = s.on_tick(None, None, Some(500)).unwrap();
        assert_eq!(q.price, 500);
    }
}
