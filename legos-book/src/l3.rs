//! L3 订单树账簿：精确到订单 ID 的深度账簿。
//!
//! 维护两套结构：
//!
//! * `orders: BTreeMap<u64, Order>` — 订单 ID → 订单明细（精确的排队位置分析）；
//! * `bid_qty / ask_qty: BTreeMap<i64, u64>` — 价格 → 该档总挂单量（快速取最优）。
//!
//! 事件语义：
//!
//! * `Add`：按 `order_id` 登记订单，价格档累加；
//! * `Cancel`：按 `order_id` 找到原订单，全额撤销并从价格档扣减；
//! * `Trade`：成交吃掉流动性，按事件方向从最优档开始扣减（L3 的简化处理：
//!   不追踪每笔成交到底吃掉了哪些订单 ID，只维护档位总量；如需精确归因，
//!   可在 `orders` 上按价格时间优先级遍历——此处为保持代码简洁采用总量法）。
//!
//! 注意：`BTreeMap` 会堆分配。本积木定位是**深度研究 / 排队分析**，
//! 不在极致热路径上；要零堆分配请用 [`crate::L2FlatArrayBook`]。

use std::collections::BTreeMap;

use legos_core::{EventKind, Order, OrderBook, Side, Tick};

#[derive(Debug, Default)]
pub struct L3MapBook {
    orders: BTreeMap<u64, Order>,
    bid_qty: BTreeMap<i64, u64>,
    ask_qty: BTreeMap<i64, u64>,
}

impl L3MapBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前挂单总数（监控用）。
    pub fn order_count(&self) -> usize {
        self.orders.len()
    }

    fn level_mut(&mut self, side: Side) -> &mut BTreeMap<i64, u64> {
        match side {
            Side::Bid => &mut self.bid_qty,
            Side::Ask => &mut self.ask_qty,
        }
    }

    fn add_level_qty(&mut self, side: Side, price: i64, delta: i64) {
        let map = self.level_mut(side);
        let entry = map.entry(price).or_insert(0);
        let new = *entry as i64 + delta;
        if new <= 0 {
            map.remove(&price);
        } else {
            *entry = new as u64;
        }
    }

    /// 从最优档开始扣减 `qty`（成交场景），跨档继续。
    fn take_liquidity(&mut self, side: Side, mut qty: u64) {
        while qty > 0 {
            let best = match side {
                Side::Bid => self.bid_qty.iter().next_back().map(|(&p, &q)| (p, q)),
                Side::Ask => self.ask_qty.iter().next().map(|(&p, &q)| (p, q)),
            };
            let (price, available) = match best {
                Some(x) => x,
                None => break,
            };
            let take = qty.min(available);
            self.add_level_qty(side, price, -(take as i64));
            qty -= take;
        }
    }
}

impl OrderBook for L3MapBook {
    fn apply(&mut self, ev: &Tick) {
        match ev.kind {
            EventKind::Add => {
                let order = Order::from(ev);
                self.orders.insert(ev.order_id, order);
                self.add_level_qty(ev.side, ev.price, ev.qty as i64);
            }
            EventKind::Cancel => {
                if let Some(order) = self.orders.remove(&ev.order_id) {
                    self.add_level_qty(order.side, order.price, -(order.qty as i64));
                }
            }
            EventKind::Trade => {
                // 成交方向：买方主动吃卖盘 / 卖方主动吃买盘。
                let resting = match ev.side {
                    Side::Bid => Side::Ask,
                    Side::Ask => Side::Bid,
                };
                self.take_liquidity(resting, ev.qty);
            }
        }
    }

    fn best_bid(&self) -> Option<(i64, u64)> {
        self.bid_qty.iter().next_back().map(|(&p, &q)| (p, q))
    }

    fn best_ask(&self) -> Option<(i64, u64)> {
        self.ask_qty.iter().next().map(|(&p, &q)| (p, q))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(order_id: u64, side: Side, price: i64, qty: u64) -> Tick {
        Tick {
            symbol_id: 1,
            price,
            qty,
            side,
            kind: EventKind::Add,
            order_id,
            ts_ns: 0,
        }
    }

    #[test]
    fn add_cancel_by_order_id() {
        let mut b = L3MapBook::new();
        b.apply(&add(1, Side::Bid, 99_0000, 100));
        b.apply(&add(2, Side::Bid, 99_0000, 50));
        b.apply(&add(3, Side::Ask, 100_0000, 70));
        assert_eq!(b.best_bid(), Some((99_0000, 150)));
        assert_eq!(b.best_ask(), Some((100_0000, 70)));
        assert_eq!(b.order_count(), 3);

        // 按订单 ID 撤销其中一笔：同价档总量应精确扣减。
        b.apply(&Tick {
            kind: EventKind::Cancel,
            order_id: 1,
            ..add(0, Side::Bid, 0, 0)
        });
        assert_eq!(b.best_bid(), Some((99_0000, 50)));
        assert_eq!(b.order_count(), 2);

        // 撤销不存在的订单 ID：静默忽略。
        b.apply(&Tick {
            kind: EventKind::Cancel,
            order_id: 999,
            ..add(0, Side::Bid, 0, 0)
        });
        assert_eq!(b.best_bid(), Some((99_0000, 50)));
    }

    #[test]
    fn trade_takes_liquidity_across_levels() {
        let mut b = L3MapBook::new();
        b.apply(&add(1, Side::Ask, 100_0000, 30));
        b.apply(&add(2, Side::Ask, 100_5000, 50));
        // 买方主动成交 60：吃掉 100_0000 整档 + 100_5000 的 30。
        b.apply(&Tick {
            kind: EventKind::Trade,
            side: Side::Bid,
            qty: 60,
            ..add(0, Side::Bid, 0, 0)
        });
        assert_eq!(b.best_ask(), Some((100_5000, 20)));
    }

    #[test]
    fn mid_price_from_trees() {
        let mut b = L3MapBook::new();
        assert_eq!(b.mid_price(), None);
        b.apply(&add(1, Side::Bid, 99_0000, 10));
        b.apply(&add(2, Side::Ask, 101_0000, 10));
        assert_eq!(b.mid_price(), Some(100_0000));
    }
}
