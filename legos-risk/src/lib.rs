//! legos-risk: 事前风控 —— 下单前最后一道闸门。
//!
//! * [`PassThroughRisk`]: 回测积木，`check` 恒返回 `Ok(())`，
//!   单态化后整个调用被优化为空，**零开销**；
//! * [`HardLimitRisk`]: 实盘积木，检查单笔名义金额、单笔数量、
//!   单位时间订单数（乌龙指 / 流氓循环保护）。
//!
//! 两块积木实现同一个 [`PreTradeRisk`](legos_core::PreTradeRisk) trait，
//! 管线里切换只需改一个泛型参数。

use legos_core::{OrderIntent, PreTradeRisk, RiskReject, RiskRejectReason};

/// 回测用直通风控：永不拦截。
///
/// 因为是具体类型 + `#[inline]`，在 release 构建中 `check` 调用会被
/// 完全内联并消除，对热路径真正做到零开销（对比 `dyn PreTradeRisk`
/// 的虚函数调用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct PassThroughRisk;

impl PreTradeRisk for PassThroughRisk {
    #[inline]
    fn check(&mut self, _order: &OrderIntent) -> Result<(), RiskReject> {
        Ok(())
    }
}

/// 实盘用硬限额风控。
///
/// * `max_notional`: 单笔名义金额上限（`|price| * qty`，tick 单位），防乌龙指；
/// * `max_qty`: 单笔数量上限；
/// * `max_orders_per_sec`: 每秒订单数上限，防流氓循环 / 失控策略。
///
/// 速率统计用固定 1 秒滑动窗口实现（两个 `u64` 字段，零堆分配）。
/// 调用方（管线）负责在 `check` 前把 `OrderIntent.ts_ns` 打上当前时间戳。
#[derive(Debug, Clone, Copy)]
pub struct HardLimitRisk {
    pub max_notional: i64,
    pub max_qty: u64,
    pub max_orders_per_sec: u64,
    window_start_ns: u64,
    orders_in_window: u64,
}

impl HardLimitRisk {
    pub fn new(max_notional: i64, max_qty: u64, max_orders_per_sec: u64) -> Self {
        Self {
            max_notional,
            max_qty,
            max_orders_per_sec,
            window_start_ns: 0,
            orders_in_window: 0,
        }
    }

    /// 当前窗口内已放行的订单数（监控用）。
    pub fn orders_in_window(&self) -> u64 {
        self.orders_in_window
    }
}

impl PreTradeRisk for HardLimitRisk {
    fn check(&mut self, order: &OrderIntent) -> Result<(), RiskReject> {
        // 1 秒窗口滚动：时间戳前进超过 1 秒则重置计数。
        if order
            .ts_ns
            .wrapping_sub(self.window_start_ns)
            >= 1_000_000_000
        {
            self.window_start_ns = order.ts_ns;
            self.orders_in_window = 0;
        }

        // 名义金额用 i128 计算，杜绝溢出误判。
        let notional = (order.price as i128).abs() * (order.qty as i128);
        if notional > self.max_notional as i128 {
            return Err(RiskReject {
                reason: RiskRejectReason::NotionalExceeded,
            });
        }
        if order.qty > self.max_qty {
            return Err(RiskReject {
                reason: RiskRejectReason::QtyExceeded,
            });
        }
        if self.orders_in_window >= self.max_orders_per_sec {
            return Err(RiskReject {
                reason: RiskRejectReason::RateExceeded,
            });
        }

        self.orders_in_window += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legos_core::Side;

    fn intent(price: i64, qty: u64, ts_ns: u64) -> OrderIntent {
        OrderIntent {
            client_order_id: 1,
            symbol_id: 1,
            side: Side::Bid,
            price,
            qty,
            ts_ns,
        }
    }

    #[test]
    fn pass_through_never_rejects() {
        let mut r = PassThroughRisk;
        // 即使是离谱的订单也放行：回测场景追求零开销。
        let absurd = intent(i64::MAX, u64::MAX, 0);
        assert!(r.check(&absurd).is_ok());
    }

    #[test]
    fn rejects_notional_and_qty() {
        let mut r = HardLimitRisk::new(1_000_000, 500, 100);
        // 名义金额 2000*1000=2_000_000 > 1_000_000
        let bad = intent(2000, 1000, 0);
        assert_eq!(
            r.check(&bad).unwrap_err().reason,
            RiskRejectReason::NotionalExceeded
        );
        // 数量超限
        let bad_qty = intent(100, 600, 0);
        assert_eq!(
            r.check(&bad_qty).unwrap_err().reason,
            RiskRejectReason::QtyExceeded
        );
        // 合法订单放行
        assert!(r.check(&intent(100, 10, 0)).is_ok());
    }

    #[test]
    fn rate_limit_and_window_reset() {
        let mut r = HardLimitRisk::new(i64::MAX, u64::MAX, 3);
        for _ in 0..3 {
            assert!(r.check(&intent(100, 1, 0)).is_ok());
        }
        // 第 4 笔：同 1 秒窗口内超限
        assert_eq!(
            r.check(&intent(100, 1, 500_000_000)).unwrap_err().reason,
            RiskRejectReason::RateExceeded
        );
        // 时间推进超过 1 秒：窗口重置，重新放行
        assert!(r.check(&intent(100, 1, 1_000_000_001)).is_ok());
        assert_eq!(r.orders_in_window(), 1);
    }

    #[test]
    fn notional_uses_i128_no_overflow() {
        let mut r = HardLimitRisk::new(i64::MAX, u64::MAX, u64::MAX);
        // price*qty 远超 i64，但 i128 下比较仍正确（上限 i64::MAX 会拒掉它）。
        let huge = intent(i64::MAX, 2, 0);
        assert_eq!(
            r.check(&huge).unwrap_err().reason,
            RiskRejectReason::NotionalExceeded
        );
    }
}
