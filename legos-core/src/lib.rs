//! legos-core: 「Legos」交易系统的共享地基。
//!
//! 本 crate 只包含**定长、`Copy` 的数据结构**与**组件 Trait 定义**，不包含任何
//! 具体实现。设计约束：
//!
//! * 热路径上**零动态分发**：所有组件边界都是 `trait` + 泛型，单态化后直接内联，
//!   没有 `dyn`、没有虚函数指针跳转。
//! * 热路径上**零堆分配**：`Tick` / `Order` / `OrderIntent` / `OrderAck` 全部是
//!   固定大小的 `Copy` 结构体，可以在栈上、数组里、共享内存里自由搬运。
//!
//! 价格统一用 **整数 tick** 表示（例如 1 tick = 0.0001 美元，由各 feed 在解析时
//! 换算），避免浮点数在热路径上的不确定性。

// ---------------------------------------------------------------------------
// 基础枚举
// ---------------------------------------------------------------------------

/// 买卖方向。`repr(u8)` 保证单字节、定长。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Side {
    Bid = 0,
    Ask = 1,
}

impl Side {
    /// ITCH / FIX 风格的单字符方向码转 `Side`。
    pub fn from_byte(b: u8) -> Option<Side> {
        match b {
            b'B' | b'1' => Some(Side::Bid),
            b'S' | b'2' => Some(Side::Ask),
            _ => None,
        }
    }
}

/// 行情事件种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EventKind {
    /// 新增订单（挂单）
    Add = 0,
    /// 撤销 / 删除订单
    Cancel = 1,
    /// 成交（吃掉盘口流动性）
    Trade = 2,
}

// ---------------------------------------------------------------------------
// 核心数据结构：Tick / Order
// ---------------------------------------------------------------------------

/// Tick：系统内流转的最小行情事件。
///
/// 定长 `Copy`（见下方单测对大小的断言），可直接塞进无锁环形队列、
/// 共享内存，或在栈上传递，全程不触碰堆。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Tick {
    /// 品种编号（由 feed 映射，例如 ITCH 的 stock-locate）。
    pub symbol_id: u32,
    /// 整数价格（tick）。
    pub price: i64,
    /// 数量（股数 / 合约数）。
    pub qty: u64,
    /// 方向。
    pub side: Side,
    /// 事件种类。
    pub kind: EventKind,
    /// 交易所订单号（Add/Cancel 携带；Trade 可为 0）。
    pub order_id: u64,
    /// 纳秒时间戳。
    pub ts_ns: u64,
}

/// Order：账簿内部使用的定长订单表示（L3 账簿按订单 ID 建树时使用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Order {
    pub order_id: u64,
    pub symbol_id: u32,
    pub side: Side,
    pub price: i64,
    pub qty: u64,
    pub ts_ns: u64,
}

impl From<&Tick> for Order {
    fn from(t: &Tick) -> Self {
        Order {
            order_id: t.order_id,
            symbol_id: t.symbol_id,
            side: t.side,
            price: t.price,
            qty: t.qty,
            ts_ns: t.ts_ns,
        }
    }
}

// ---------------------------------------------------------------------------
// 策略 / 风控 / 执行之间流转的类型
// ---------------------------------------------------------------------------

/// OrderIntent：策略输出的**交易意图**，定长 `Copy`。
///
/// `price == 0` 约定为市价单（按对手价立即成交），否则为限价单。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct OrderIntent {
    pub client_order_id: u64,
    pub symbol_id: u32,
    pub side: Side,
    /// 限价（tick）；0 表示市价。
    pub price: i64,
    pub qty: u64,
    /// 纳秒时间戳（管线在风控前统一打戳，供 `HardLimitRisk` 做速率统计）。
    pub ts_ns: u64,
}

/// 风控拒绝原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskRejectReason {
    /// 名义金额超限（`price * qty`）。
    NotionalExceeded,
    /// 单笔数量超限。
    QtyExceeded,
    /// 单位时间订单数超限（撤单率 / 下单率保护）。
    RateExceeded,
}

/// 风控拒绝：小而 `Copy`，热路径上用 `Result` 返回，无异常、无堆分配。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RiskReject {
    pub reason: RiskRejectReason,
}

/// 执行回执状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AckStatus {
    Accepted = 0,
    Filled = 1,
    PartiallyFilled = 2,
    /// 被拒绝（风控 / 网关连接失败等）。注意：网关实现**永不 panic**，
    /// 失败一律以 `Rejected` 回执表达。
    Rejected = 3,
}

/// OrderAck：执行网关返回的定长 `Copy` 回执。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct OrderAck {
    pub client_order_id: u64,
    pub status: AckStatus,
    pub filled_qty: u64,
    /// 平均成交价（tick）；未成交时为 0。
    pub avg_price: i64,
}

impl OrderAck {
    pub fn rejected(client_order_id: u64) -> Self {
        OrderAck {
            client_order_id,
            status: AckStatus::Rejected,
            filled_qty: 0,
            avg_price: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// 组件 Trait：六大积木块的插槽
// ---------------------------------------------------------------------------

/// 行情接入积木：`CsvFileFeed`（回测）/ `NativeItchParser`（实盘）实现它。
pub trait MarketDataFeed {
    /// 返回下一个行情事件；`None` 表示当前无数据（文件 feed 到达 EOF 时
    /// 为永久结束，实盘 feed 则只是暂时无包）。
    fn next_event(&mut self) -> Option<Tick>;
}

/// 无锁数据总线积木：`SpscRingBuffer` / `SharedMemoryBus` 实现它。
///
/// `push`/`pop` 都只取 `&self`：内部用原子操作同步，天然 `Sync`，
/// 生产者与消费者线程可直接共享引用。
pub trait MessageBus {
    type Item: Copy;
    /// 队列满时返回 `false`（调用方负责先 drain），永不阻塞、永不分配。
    fn push(&self, item: Self::Item) -> bool;
    fn pop(&self) -> Option<Self::Item>;
}

/// 定价账簿积木：`L2FlatArrayBook` / `L3MapBook` 实现它。
pub trait OrderBook {
    /// 把一个 `Tick` 事件应用到账簿上。
    fn apply(&mut self, ev: &Tick);
    /// 最优买价 `(price, qty)`。
    fn best_bid(&self) -> Option<(i64, u64)>;
    /// 最优卖价 `(price, qty)`。
    fn best_ask(&self) -> Option<(i64, u64)>;
    /// 中间价（买卖一档均值）；任一方向缺失时返回 `None`。
    fn mid_price(&self) -> Option<i64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((b, _)), Some((a, _))) => Some((b + a) / 2),
            _ => None,
        }
    }
}

/// 策略积木：`MarketMakerStrategy` / `ArbitrageStrategy` /
/// `PythonBindingStrategy` 实现它。
pub trait TradingStrategy {
    /// 每个账簿更新后调用一次；返回 `Some(intent)` 即产生一个交易意图。
    fn on_tick(
        &mut self,
        bid: Option<(i64, u64)>,
        ask: Option<(i64, u64)>,
        mid: Option<i64>,
    ) -> Option<OrderIntent>;
}

/// 事前风控积木：`PassThroughRisk`（回测零开销）/ `HardLimitRisk`（实盘）实现它。
pub trait PreTradeRisk {
    /// 返回 `Ok(())` 放行；`Err(RiskReject)` 拦截。
    fn check(&mut self, order: &OrderIntent) -> Result<(), RiskReject>;
}

/// 执行网关积木：`FixProtocolGateway`（实盘）/ `SimulatedExchange`（回测）实现它。
pub trait ExecutionGateway {
    /// 发送订单并返回回执；实现**不得 panic**，失败用
    /// `AckStatus::Rejected` 表达。
    fn send_order(&mut self, order: &OrderIntent) -> OrderAck;

    /// 每 tick 的最新报价通知（默认空实现）。
    ///
    /// `SimulatedExchange` 重写它来刷新内存撮合盘口；FIX 等实盘网关忽略即可。
    fn on_quote(&mut self, _symbol_id: u32, _bid: Option<(i64, u64)>, _ask: Option<(i64, u64)>) {}
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, size_of};

    #[test]
    fn core_types_are_small_fixed_and_copy() {
        // 定长断言：改动结构体字段时若大小变化，这里会第一时间提醒。
        assert_eq!(size_of::<Tick>(), 48);
        assert_eq!(size_of::<Order>(), 40);
        assert_eq!(size_of::<OrderIntent>(), 40);
        assert_eq!(size_of::<OrderAck>(), 32);
        assert!(align_of::<Tick>() <= 8);

        // Copy 语义：按值搬运后原值仍可用。
        let t = Tick {
            symbol_id: 1,
            price: 100_5000,
            qty: 100,
            side: Side::Bid,
            kind: EventKind::Add,
            order_id: 7,
            ts_ns: 1,
        };
        let u = t;
        assert_eq!(t, u);
        let o = Order::from(&t);
        assert_eq!(o.order_id, 7);
        assert_eq!(o.price, 100_5000);
    }

    #[test]
    fn side_parsing() {
        assert_eq!(Side::from_byte(b'B'), Some(Side::Bid));
        assert_eq!(Side::from_byte(b'S'), Some(Side::Ask));
        assert_eq!(Side::from_byte(b'1'), Some(Side::Bid));
        assert_eq!(Side::from_byte(b'2'), Some(Side::Ask));
        assert_eq!(Side::from_byte(b'X'), None);
    }

    /// 验证 `OrderBook::mid_price` 默认实现的边界行为。
    struct OneSidedBook;
    impl OrderBook for OneSidedBook {
        fn apply(&mut self, _ev: &Tick) {}
        fn best_bid(&self) -> Option<(i64, u64)> {
            Some((100, 10))
        }
        fn best_ask(&self) -> Option<(i64, u64)> {
            None
        }
    }

    #[test]
    fn mid_price_needs_both_sides() {
        assert_eq!(OneSidedBook.mid_price(), None);
    }

    #[test]
    fn reject_ack_constructor() {
        let ack = OrderAck::rejected(42);
        assert_eq!(ack.status, AckStatus::Rejected);
        assert_eq!(ack.client_order_id, 42);
        assert_eq!(ack.filled_qty, 0);
    }
}
