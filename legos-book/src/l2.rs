//! L2 扁平数组账簿：为缓存命中率而生的定价引擎。
//!
//! 布局：买盘 `[(price, qty); LEVELS]` 按价格**降序**、卖盘按价格**升序**，
//! 全部挤在两个连续数组里。一次行情事件的处理流程：
//!
//! 1. 在对应数组的**有效前缀**上做二分查找定位价格档（`O(log LEVELS)`）；
//! 2. 命中则原地加减数量；未命中则用 `copy_within` 平移插入（`O(LEVELS)`，
//!    但 `LEVELS` 是编译期常量且很小，实测就是几条向量化内存搬运指令）；
//! 3. 数量归零的档位同样平移删除，保持数组紧凑。
//!
//! 全程**零堆分配**：没有 `Vec`、没有 `Box`，`apply` 内只有栈上操作。
//! 深度截断策略：数组满时，更差的档位直接丢弃（L2 场景下可接受，
//! 需要全深度请换 `L3MapBook`）。

use legos_core::{EventKind, OrderBook, Side, Tick};

/// L2 扁平账簿。`LEVELS` 为每侧保留的最大档位数（典型 5 / 10）。
#[derive(Clone, Copy, Debug)]
pub struct L2FlatArrayBook<const LEVELS: usize> {
    /// 买盘：价格降序，`bids[0]` 即最优买价。
    bids: [(i64, u64); LEVELS],
    /// 卖盘：价格升序，`asks[0]` 即最优卖价。
    asks: [(i64, u64); LEVELS],
    bid_len: usize,
    ask_len: usize,
}

impl<const LEVELS: usize> L2FlatArrayBook<LEVELS> {
    pub const fn new() -> Self {
        assert!(LEVELS > 0, "LEVELS must be > 0");
        Self {
            bids: [(0, 0); LEVELS],
            asks: [(0, 0); LEVELS],
            bid_len: 0,
            ask_len: 0,
        }
    }

    /// 在有效前缀上二分查找 `price`。
    ///
    /// 买盘降序、卖盘升序，统一用 comparator 适配。返回 `Ok(idx)` 表示命中，
    /// `Err(idx)` 表示应插入的位置。
    #[inline]
    fn find(levels: &[(i64, u64)], len: usize, price: i64, ascending: bool) -> Result<usize, usize> {
        levels[..len].binary_search_by(|&(p, _)| {
            if ascending {
                p.cmp(&price)
            } else {
                price.cmp(&p) // 降序：反转比较
            }
        })
    }

    /// 增减某一档的数量。`delta > 0` 为加，`< 0` 为减。
    ///
    /// 档位不存在且 `delta > 0` 时插入新档；数量减到 0 时删除该档。
    /// 数组满且新档更差时直接丢弃（截断）。
    fn add_qty(&mut self, side: Side, price: i64, delta: i64) {
        let (levels, len) = match side {
            Side::Bid => (&mut self.bids, &mut self.bid_len),
            Side::Ask => (&mut self.asks, &mut self.ask_len),
        };
        let ascending = matches!(side, Side::Ask);
        match Self::find(levels, *len, price, ascending) {
            Ok(idx) => {
                let new_qty = levels[idx].1 as i64 + delta;
                if new_qty <= 0 {
                    // 删除档位：后面的元素整体前移。
                    levels.copy_within(idx + 1..*len, idx);
                    *len -= 1;
                } else {
                    levels[idx].1 = new_qty as u64;
                }
            }
            Err(idx) => {
                if delta <= 0 {
                    return; // 减少不存在的档位：忽略
                }
                if *len == LEVELS {
                    if idx == LEVELS {
                        return; // 比最差档还差：直接丢弃（牺牲极端深度）
                    }
                    // 更优的新档：插入并挤掉最差一档，长度不变。
                    levels.copy_within(idx..LEVELS - 1, idx + 1);
                    levels[idx] = (price, delta as u64);
                } else {
                    // 插入：idx 及之后的元素后移一位。
                    levels.copy_within(idx..*len, idx + 1);
                    levels[idx] = (price, delta as u64);
                    *len += 1;
                }
            }
        }
    }

    /// 当前有效档位数（监控用）。
    pub fn depth(&self) -> (usize, usize) {
        (self.bid_len, self.ask_len)
    }
}

impl<const LEVELS: usize> Default for L2FlatArrayBook<LEVELS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const LEVELS: usize> OrderBook for L2FlatArrayBook<LEVELS> {
    #[inline]
    fn apply(&mut self, ev: &Tick) {
        match ev.kind {
            EventKind::Add => self.add_qty(ev.side, ev.price, ev.qty as i64),
            EventKind::Cancel => self.add_qty(ev.side, ev.price, -(ev.qty as i64)),
            // 成交吃掉盘口流动性：按事件方向扣减对应档位。
            EventKind::Trade => self.add_qty(ev.side, ev.price, -(ev.qty as i64)),
        }
    }

    #[inline]
    fn best_bid(&self) -> Option<(i64, u64)> {
        if self.bid_len > 0 {
            Some(self.bids[0])
        } else {
            None
        }
    }

    #[inline]
    fn best_ask(&self) -> Option<(i64, u64)> {
        if self.ask_len > 0 {
            Some(self.asks[0])
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(side: Side, price: i64, qty: u64) -> Tick {
        Tick {
            symbol_id: 1,
            price,
            qty,
            side,
            kind: EventKind::Add,
            order_id: price as u64,
            ts_ns: 0,
        }
    }

    #[test]
    fn best_bid_ask_and_mid() {
        let mut b = L2FlatArrayBook::<10>::new();
        // 乱序插入，账簿内部应自动排序。
        b.apply(&add(Side::Ask, 101_0000, 50));
        b.apply(&add(Side::Bid, 99_0000, 100));
        b.apply(&add(Side::Ask, 100_5000, 30));
        b.apply(&add(Side::Bid, 99_5000, 70));

        assert_eq!(b.best_bid(), Some((99_5000, 70)));
        assert_eq!(b.best_ask(), Some((100_5000, 30)));
        assert_eq!(b.mid_price(), Some((99_5000 + 100_5000) / 2));
    }

    #[test]
    fn aggregate_same_price_level() {
        let mut b = L2FlatArrayBook::<10>::new();
        b.apply(&add(Side::Bid, 99_0000, 100));
        b.apply(&add(Side::Bid, 99_0000, 25));
        assert_eq!(b.best_bid(), Some((99_0000, 125)));
        assert_eq!(b.depth(), (1, 0));
    }

    #[test]
    fn cancel_and_trade_reduce_levels() {
        let mut b = L2FlatArrayBook::<10>::new();
        b.apply(&add(Side::Ask, 100_0000, 100));
        b.apply(&Tick {
            kind: EventKind::Cancel,
            qty: 40,
            ..add(Side::Ask, 100_0000, 0)
        });
        assert_eq!(b.best_ask(), Some((100_0000, 60)));
        b.apply(&Tick {
            kind: EventKind::Trade,
            qty: 60,
            ..add(Side::Ask, 100_0000, 0)
        });
        assert_eq!(b.best_ask(), None, "数量归零档位应被删除");
    }

    #[test]
    fn depth_truncation_drops_worse_levels() {
        let mut b = L2FlatArrayBook::<3>::new();
        // 卖盘升序：100, 101, 102 占满；103 更差应被丢弃。
        for p in [100_0000, 101_0000, 102_0000, 103_0000] {
            b.apply(&add(Side::Ask, p, 10));
        }
        assert_eq!(b.depth(), (0, 3));
        assert_eq!(b.best_ask(), Some((100_0000, 10)));
        // 更优的 99 仍可插入（顶掉最差的 102）。
        b.apply(&add(Side::Ask, 99_0000, 10));
        assert_eq!(b.best_ask(), Some((99_0000, 10)));
        assert_eq!(b.depth(), (0, 3));
    }

    #[test]
    fn binary_search_positions() {
        // 直接验证二分查找在降序 / 升序下的定位。
        let lv = [(105, 1), (103, 1), (101, 1)];
        assert_eq!(L2FlatArrayBook::<8>::find(&lv, 3, 103, false), Ok(1));
        assert_eq!(L2FlatArrayBook::<8>::find(&lv, 3, 104, false), Err(1));
        assert_eq!(L2FlatArrayBook::<8>::find(&lv, 3, 100, false), Err(3));
        let la = [(101, 1), (103, 1), (105, 1)];
        assert_eq!(L2FlatArrayBook::<8>::find(&la, 3, 104, true), Err(2));
    }

    #[test]
    fn empty_book_has_no_quotes() {
        let b = L2FlatArrayBook::<10>::new();
        assert_eq!(b.best_bid(), None);
        assert_eq!(b.best_ask(), None);
        assert_eq!(b.mid_price(), None);
    }
}
