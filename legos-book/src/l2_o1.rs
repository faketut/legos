//! L2 直接下标账簿：价格 → 数组下标 O(1) 映射，确定性更新。
//!
//! 布局契约（见 `legos-book/SPEC.md` §1.3）：
//!
//! * 固定档距：合法价格必须满足 `(price - BASE) % TICK == 0`，
//!   且落在 `[BASE, BASE + TICK * SLOTS)` 区间内；
//!   越界 / 不对齐的价格事件**直接忽略**（与 `L2FlatArrayBook` 的
//!   深度截断策略同属"静默丢弃远端流动性"的取舍）；
//! * 买盘卖盘各一个 `[u32; SLOTS]` 数量数组，下标 `s = (price - BASE) / TICK`；
//! * 各配一个 `[u64; WORDS]` 占用位图，用于最优价档位被删时
//!   常数时间找回次优档（买盘向下扫描、卖盘向上扫描，最多扫 `WORDS` 个字）；
//! * `bid_top` / `ask_top` 缓存当前最优档下标，`best_bid` / `best_ask` 为 O(1) 读取。
//!
//! 复杂度（`WORDS = SLOTS / 64` 为编译期常量）：
//!
//! * `apply`：数组下标直接计算，无二分、无 `memmove`；
//!   唯一非常数分支是"删掉当前最优档"时的位图回扫——上界 `WORDS` 次
//!   64 位字操作，**与盘口深度无关**，最坏情况依然确定；
//! * 全程零堆分配，全部栈上。
//!
//! 与 `L2FlatArrayBook` 的取舍：本实现用"固定价格区间 + 固定档距"换取
//! 确定性；价格区间外的流动性不可见（flat 版是深度档数外的流动性不可见）。
//! 网格对齐的价格流下，两者的 `best_bid` / `best_ask` / `mid_price`
//! 逐 tick 一致（见本文件 `equivalence_*` 测试）。

use legos_core::{EventKind, MarketTick, OrderBook, Side};

/// 覆盖的价格档位数（固定）。
const SLOTS: usize = 1024;
/// 占用位图字数 = SLOTS / 64（编译期常量，保证扫描上界）。
const WORDS: usize = SLOTS / 64;

/// L2 直接下标账簿。
///
/// `BASE`：价格区间下界；`TICK`：档距（必须 > 0）。
/// 覆盖区间为 `[BASE, BASE + TICK * 1024)`。
#[derive(Clone, Copy, Debug)]
pub struct L2DirectIndexBook<const BASE: i64, const TICK: i64> {
    /// 买盘数量，下标 `s` 对应价格 `BASE + s * TICK`。
    bids: [u32; SLOTS],
    /// 卖盘数量，下标含义同上。
    asks: [u32; SLOTS],
    /// 买盘占用位图。
    bid_mask: [u64; WORDS],
    /// 卖盘占用位图。
    ask_mask: [u64; WORDS],
    /// 当前最优买档下标（买盘为空时 `None`）。
    bid_top: Option<usize>,
    /// 当前最优卖档下标（卖盘为空时 `None`）。
    ask_top: Option<usize>,
    bid_count: usize,
    ask_count: usize,
}

impl<const BASE: i64, const TICK: i64> L2DirectIndexBook<BASE, TICK> {
    pub const fn new() -> Self {
        assert!(TICK > 0, "TICK must be > 0");
        assert!(SLOTS % 64 == 0, "SLOTS must be a multiple of 64");
        Self {
            bids: [0; SLOTS],
            asks: [0; SLOTS],
            bid_mask: [0; WORDS],
            ask_mask: [0; WORDS],
            bid_top: None,
            ask_top: None,
            bid_count: 0,
            ask_count: 0,
        }
    }

    /// 价格 → 数组下标。越界或不对齐返回 `None`（调用方忽略该事件）。
    #[inline]
    fn slot(price: i64) -> Option<usize> {
        let off = price.wrapping_sub(BASE);
        if off < 0 || off % TICK != 0 {
            return None;
        }
        let idx = (off / TICK) as usize;
        if idx < SLOTS {
            Some(idx)
        } else {
            None
        }
    }

    /// 位图中严格小于 `slot` 的最大已占用下标（买盘找次优）。
    /// 至多扫描 `WORDS` 个字，常数时间。
    #[inline]
    fn prev_occupied(mask: &[u64], slot: usize) -> Option<usize> {
        let (w, b) = (slot / 64, slot % 64);
        // 字内：只看第 b 位以下的位（b == 0 时掩码为 0）。
        let below = mask[w] & ((1u64 << b).wrapping_sub(1));
        if below != 0 {
            return Some(w * 64 + 63 - below.leading_zeros() as usize);
        }
        for wi in (0..w).rev() {
            let m = mask[wi];
            if m != 0 {
                return Some(wi * 64 + 63 - m.leading_zeros() as usize);
            }
        }
        None
    }

    /// 位图中严格大于 `slot` 的最小已占用下标（卖盘找次优）。
    /// 至多扫描 `WORDS` 个字，常数时间。
    #[inline]
    fn next_occupied(mask: &[u64], slot: usize) -> Option<usize> {
        let (w, b) = (slot / 64, slot % 64);
        // 字内：只看第 b 位以上的位（b == 63 时无更高位）。
        let above = if b == 63 {
            0
        } else {
            mask[w] & (u64::MAX << (b + 1))
        };
        if above != 0 {
            return Some(w * 64 + above.trailing_zeros() as usize);
        }
        for wi in w + 1..mask.len() {
            let m = mask[wi];
            if m != 0 {
                return Some(wi * 64 + m.trailing_zeros() as usize);
            }
        }
        None
    }

    /// 增减某一档的数量，语义与 `L2FlatArrayBook::add_qty` 一致：
    /// 命中则加减，归零删档；未命中且 `delta > 0` 则建档；
    /// 未命中且 `delta <= 0` 忽略。越界/不对齐价格直接忽略。
    fn add_qty(&mut self, side: Side, price: i64, delta: i64) {
        let Some(s) = Self::slot(price) else {
            return;
        };
        let (w, b) = (s / 64, s % 64);
        let bit = 1u64 << b;
        match side {
            Side::Bid => {
                let new_qty = self.bids[s] as i64 + delta;
                if new_qty <= 0 {
                    if self.bids[s] != 0 {
                        self.bids[s] = 0;
                        self.bid_mask[w] &= !bit;
                        self.bid_count -= 1;
                        if self.bid_top == Some(s) {
                            // 删掉的是最优档：位图向下回扫找次优（常数上界）。
                            self.bid_top = Self::prev_occupied(&self.bid_mask, s);
                        }
                    }
                } else {
                    let was_empty = self.bids[s] == 0;
                    self.bids[s] = new_qty as u32;
                    if was_empty {
                        self.bid_mask[w] |= bit;
                        self.bid_count += 1;
                        // 买盘最优 = 价格最高 = 下标最大。
                        if self.bid_top.map_or(true, |t| s > t) {
                            self.bid_top = Some(s);
                        }
                    }
                }
            }
            Side::Ask => {
                let new_qty = self.asks[s] as i64 + delta;
                if new_qty <= 0 {
                    if self.asks[s] != 0 {
                        self.asks[s] = 0;
                        self.ask_mask[w] &= !bit;
                        self.ask_count -= 1;
                        if self.ask_top == Some(s) {
                            // 删掉的是最优档：位图向上回扫找次优（常数上界）。
                            self.ask_top = Self::next_occupied(&self.ask_mask, s);
                        }
                    }
                } else {
                    let was_empty = self.asks[s] == 0;
                    self.asks[s] = new_qty as u32;
                    if was_empty {
                        self.ask_mask[w] |= bit;
                        self.ask_count += 1;
                        // 卖盘最优 = 价格最低 = 下标最小。
                        if self.ask_top.map_or(true, |t| s < t) {
                            self.ask_top = Some(s);
                        }
                    }
                }
            }
        }
    }

    /// 当前有效档位数（监控用）。
    pub fn depth(&self) -> (usize, usize) {
        (self.bid_count, self.ask_count)
    }
}

impl<const BASE: i64, const TICK: i64> Default for L2DirectIndexBook<BASE, TICK> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const BASE: i64, const TICK: i64> OrderBook for L2DirectIndexBook<BASE, TICK> {
    #[inline]
    fn apply(&mut self, ev: &MarketTick) {
        match ev.kind() {
            EventKind::Add => self.add_qty(ev.side(), ev.price, ev.qty as i64),
            EventKind::Cancel => self.add_qty(ev.side(), ev.price, -(ev.qty as i64)),
            // 成交吃掉盘口流动性：按事件方向扣减对应档位。
            EventKind::Trade => self.add_qty(ev.side(), ev.price, -(ev.qty as i64)),
        }
    }

    #[inline]
    fn best_bid(&self) -> Option<(i64, u32)> {
        self.bid_top
            .map(|s| (BASE + s as i64 * TICK, self.bids[s]))
    }

    #[inline]
    fn best_ask(&self) -> Option<(i64, u32)> {
        self.ask_top
            .map(|s| (BASE + s as i64 * TICK, self.asks[s]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::L2FlatArrayBook;

    const BASE_T: i64 = 99_0000;
    const TICK_T: i64 = 100;
    type TestBook = L2DirectIndexBook<BASE_T, TICK_T>;

    fn add(side: Side, price: i64, qty: u32) -> MarketTick {
        MarketTick::new(1, price, qty, side, EventKind::Add, price as u32, 0)
    }

    #[test]
    fn best_bid_ask_and_mid() {
        let mut b = TestBook::new();
        // 乱序插入（网格对齐），最优价应正确跟踪。
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
        let mut b = TestBook::new();
        b.apply(&add(Side::Bid, 99_0000, 100));
        b.apply(&add(Side::Bid, 99_0000, 25));
        assert_eq!(b.best_bid(), Some((99_0000, 125)));
        assert_eq!(b.depth(), (1, 0));
    }

    #[test]
    fn cancel_and_trade_reduce_levels() {
        let mut b = TestBook::new();
        b.apply(&add(Side::Ask, 100_0000, 100));
        let cancel = MarketTick::new(1, 100_0000, 40, Side::Ask, EventKind::Cancel, 1, 0);
        b.apply(&cancel);
        assert_eq!(b.best_ask(), Some((100_0000, 60)));
        let trade = MarketTick::new(1, 100_0000, 60, Side::Ask, EventKind::Trade, 2, 0);
        b.apply(&trade);
        assert_eq!(b.best_ask(), None, "数量归零档位应被删除");
    }

    #[test]
    fn best_removal_falls_back_to_next_level() {
        // 删掉最优档：位图回扫必须找到次优档（买盘向下、卖盘向上）。
        let mut b = TestBook::new();
        for p in [99_0000, 99_5000, 100_0000] {
            b.apply(&add(Side::Bid, p, 10));
        }
        for p in [101_0000, 101_5000, 102_0000] {
            b.apply(&add(Side::Ask, p, 10));
        }
        // 逐档删光最优买：100_0000 → 99_5000 → 99_0000 → None。
        for (p, next) in [
            (100_0000, Some((99_5000, 10))),
            (99_5000, Some((99_0000, 10))),
            (99_0000, None),
        ] {
            let cancel = MarketTick::new(1, p, 10, Side::Bid, EventKind::Cancel, 1, 0);
            b.apply(&cancel);
            assert_eq!(b.best_bid(), next, "删除买盘 {p} 后次优应为 {next:?}");
        }
        // 卖盘同理：101_0000 → 101_5000 → 102_0000 → None。
        for (p, next) in [
            (101_0000, Some((101_5000, 10))),
            (101_5000, Some((102_0000, 10))),
            (102_0000, None),
        ] {
            let cancel = MarketTick::new(1, p, 10, Side::Ask, EventKind::Cancel, 1, 0);
            b.apply(&cancel);
            assert_eq!(b.best_ask(), next, "删除卖盘 {p} 后次优应为 {next:?}");
        }
        assert_eq!(b.depth(), (0, 0));
    }

    #[test]
    fn out_of_range_and_misaligned_prices_ignored() {
        let mut b = TestBook::new();
        // 低于 BASE / 高于区间上界 / 不对齐档距：全部忽略，不得 panic。
        b.apply(&add(Side::Bid, 98_9999, 10)); // < BASE
        b.apply(&add(Side::Bid, 99_0050, 10)); // 不对齐 TICK=100
        b.apply(&add(Side::Ask, 99_0000 + 100 * 1024, 10)); // == 上界外
        b.apply(&add(Side::Ask, 200_0000, 10)); // 远超上界
        assert_eq!(b.best_bid(), None);
        assert_eq!(b.best_ask(), None);
        assert_eq!(b.depth(), (0, 0));
        // 边界档可用：BASE 本身与最后一个槽位。
        b.apply(&add(Side::Bid, 99_0000, 7));
        b.apply(&add(Side::Ask, 99_0000 + 100 * 1023, 9));
        assert_eq!(b.best_bid(), Some((99_0000, 7)));
        assert_eq!(b.best_ask(), Some((99_0000 + 100 * 1023, 9)));
    }

    #[test]
    fn slot_mapping_edges() {
        assert_eq!(TestBook::slot(99_0000), Some(0));
        assert_eq!(TestBook::slot(99_0000 + 100 * 1023), Some(1023));
        assert_eq!(TestBook::slot(99_0000 - 100), None);
        assert_eq!(TestBook::slot(99_0000 + 100 * 1024), None);
        assert_eq!(TestBook::slot(99_0050), None, "不对齐档距");
    }

    /// 确定性伪随机流：两种实现的最优报价 / 深度必须逐 tick 一致。
    /// 价格限制在 8 档以内（≤ LEVELS=10，无截断），深度也应完全一致。
    #[test]
    fn equivalence_with_flat_book_on_grid_stream() {
        let mut flat = L2FlatArrayBook::<10>::new();
        let mut o1 = TestBook::new();
        let mut rng = 0x1234_5678u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for i in 0..20_000u32 {
            let r = next();
            let side = if r & 1 == 0 { Side::Bid } else { Side::Ask };
            let slot = 100 + (r >> 1) % 8;
            let price = BASE_T + slot as i64 * TICK_T;
            let kind = match (r >> 7) % 10 {
                0..=6 => EventKind::Add,
                7..=8 => EventKind::Cancel,
                _ => EventKind::Trade,
            };
            let t = MarketTick::new(
                1,
                price,
                1 + (r >> 13) as u32 % 20,
                side,
                kind,
                i,
                i as u64,
            );
            flat.apply(&t);
            o1.apply(&t);
            assert_eq!(flat.best_bid(), o1.best_bid(), "tick {i} best_bid 不一致");
            assert_eq!(flat.best_ask(), o1.best_ask(), "tick {i} best_ask 不一致");
            assert_eq!(flat.depth(), o1.depth(), "tick {i} depth 不一致");
        }
    }

    /// 40 档 > LEVELS=10：flat 版截断远端档位，最优报价仍必须一致。
    #[test]
    fn best_quotes_match_flat_book_under_truncation() {
        let mut flat = L2FlatArrayBook::<10>::new();
        let mut o1 = TestBook::new();
        let mut rng = 0xABCD_EFu64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for i in 0..20_000u32 {
            let r = next();
            let side = if r & 1 == 0 { Side::Bid } else { Side::Ask };
            let slot = 100 + (r >> 1) % 40;
            let price = BASE_T + slot as i64 * TICK_T;
            let kind = match (r >> 7) % 10 {
                0..=6 => EventKind::Add,
                7..=8 => EventKind::Cancel,
                _ => EventKind::Trade,
            };
            let t = MarketTick::new(
                1,
                price,
                1 + (r >> 13) as u32 % 20,
                side,
                kind,
                i,
                i as u64,
            );
            flat.apply(&t);
            o1.apply(&t);
            assert_eq!(flat.best_bid(), o1.best_bid(), "tick {i} best_bid 不一致");
            assert_eq!(flat.best_ask(), o1.best_ask(), "tick {i} best_ask 不一致");
        }
    }

    /// 最坏序列等价性：下标 0 反复插入/删除（flat 版每次 memmove 满数组）。
    #[test]
    fn worst_case_stream_matches_flat_book() {
        let mut flat = L2FlatArrayBook::<10>::new();
        let mut o1 = TestBook::new();
        for k in 0..10u32 {
            let t = add(Side::Bid, 100_0000 + k as i64 * 100, 10);
            flat.apply(&t);
            o1.apply(&t);
        }
        for r in 0..1000u32 {
            let p = if r % 2 == 0 { 100_9500 } else { 100_9400 };
            let a = MarketTick::new(1, p, 10, Side::Bid, EventKind::Add, r, 0);
            flat.apply(&a);
            o1.apply(&a);
            assert_eq!(flat.best_bid(), o1.best_bid(), "round {r} add 后不一致");
            let c = MarketTick::new(1, p, 10, Side::Bid, EventKind::Cancel, r, 0);
            flat.apply(&c);
            o1.apply(&c);
            assert_eq!(flat.best_bid(), o1.best_bid(), "round {r} cancel 后不一致");
        }
    }

    #[test]
    fn empty_book_has_no_quotes() {
        let b = TestBook::new();
        assert_eq!(b.best_bid(), None);
        assert_eq!(b.best_ask(), None);
        assert_eq!(b.mid_price(), None);
    }
}
