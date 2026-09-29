//! legos-core: 「Legos」交易系统的共享地基与**契约中心**。
//!
//! 本 crate 是全系统的 Spec-Driven Development 锚点：
//!
//! * **数据契约**：`MarketTick` 固定 32 字节、`#[repr(C)]`，字段偏移与
//!   bit 位全部显式声明（见 [`MarketTick`] 文档与 `legos-core/SPEC.md`），
//!   拒绝隐式填充与堆分配；
//! * **行为契约**：[`TrackedOrder`] 实现订单生命周期状态机，非法转换在
//!   类型/逻辑层直接拦截；
//! * **Code-as-Spec**：六大组件 trait 只声明签名（关联类型、输入输出），
//!   Rust 编译器做裁判，检查实现是否符合契约。
//!
//! 设计约束：热路径**零动态分发**（trait + 泛型单态化，无 `dyn`）、
//! **零堆分配**（核心类型全部定长 `Copy`）。
//!
//! 价格统一用 **整数 tick** 表示（例如 1 tick = 0.0001 美元，由各 feed 在解析时
//! 换算），避免浮点数在热路径上的不确定性；**全系统数量字段统一为 `u32`**。

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
// 数据契约：MarketTick（32 字节定长）
// ---------------------------------------------------------------------------

/// MarketTick：系统内流转的最小行情事件。**数据契约见 `legos-core/SPEC.md`**。
///
/// 内存布局（`#[repr(C)]`，共 32 字节，对齐 8；偏移量由单测锁定）：
/// ```text
/// 偏移  大小  字段
/// 0     8     price: i64       整数价格（tick）
/// 8     8     ts_ns: u64       纳秒时间戳
/// 16    4     qty: u32         数量
/// 20    2     symbol_id: u16   品种编号
/// 22    2     _pad0: u16       显式填充（对齐 order_id）
/// 24    4     order_id: u32    交易所订单号
/// 28    1     flags: u8        bit0=方向(0=Bid,1=Ask)；bit1-2=种类(0=Add,1=Cancel,2=Trade)；bit3-7=保留
/// 29    3     _pad1: [u8; 3]   显式填充至 32 字节
/// ```
///
/// 填充字段私有且构造时清零——**拒绝隐式填充**：任何对齐空隙都显式可见。
/// 可直接塞进无锁环形队列、共享内存，或在栈上传递，全程不触碰堆。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct MarketTick {
    pub price: i64,
    pub ts_ns: u64,
    pub qty: u32,
    pub symbol_id: u16,
    _pad0: u16,
    pub order_id: u32,
    pub flags: u8,
    _pad1: [u8; 3],
}

impl MarketTick {
    /// flags bit 布局（契约，见 SPEC.md）。
    pub const SIDE_BIT: u8 = 0;
    pub const KIND_SHIFT: u8 = 1;
    pub const KIND_MASK: u8 = 0b110;
    /// 锁定的总大小。
    pub const SIZE: usize = 32;

    /// 唯一构造入口：保证 flags 合法、填充清零。
    pub fn new(
        symbol_id: u16,
        price: i64,
        qty: u32,
        side: Side,
        kind: EventKind,
        order_id: u32,
        ts_ns: u64,
    ) -> Self {
        let flags = (side as u8) | ((kind as u8) << Self::KIND_SHIFT);
        debug_assert!(flags & !0b111 == 0, "保留位必须为 0");
        Self {
            price,
            ts_ns,
            qty,
            symbol_id,
            _pad0: 0,
            order_id,
            flags,
            _pad1: [0; 3],
        }
    }

    /// bit0：0=Bid，1=Ask。
    #[inline]
    pub fn side(&self) -> Side {
        if self.flags & 0b1 == 0 {
            Side::Bid
        } else {
            Side::Ask
        }
    }

    /// bit1-2：0=Add，1=Cancel，2=Trade（3 为保留值，永不产生）。
    #[inline]
    pub fn kind(&self) -> EventKind {
        match (self.flags >> Self::KIND_SHIFT) & 0b11 {
            0 => EventKind::Add,
            1 => EventKind::Cancel,
            _ => EventKind::Trade,
        }
    }

    /// 保留位必须为 0（损坏数据检测用）。
    #[inline]
    pub fn reserved_bits_clear(&self) -> bool {
        self.flags & !0b111 == 0
    }
}

/// Order：账簿内部使用的定长订单表示（L3 账簿按订单 ID 建树时使用）。
///
/// `order_id` 保留 `u64`（L3 为深度分析场景，由 `MarketTick` 的 u32 零扩展而来）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Order {
    pub order_id: u64,
    pub symbol_id: u16,
    pub side: Side,
    pub price: i64,
    pub qty: u32,
    pub ts_ns: u64,
}

impl From<&MarketTick> for Order {
    fn from(t: &MarketTick) -> Self {
        Order {
            order_id: t.order_id as u64,
            symbol_id: t.symbol_id,
            side: t.side(),
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
    pub symbol_id: u16,
    pub side: Side,
    /// 限价（tick）；0 表示市价。
    pub price: i64,
    pub qty: u32,
    /// 纳秒时间戳（管线在风控前统一打戳，供 `HardLimitRisk` 做速率统计）。
    pub ts_ns: u64,
}

/// 风控拒绝原因码（`&'static str`，契约枚举见 `legos-risk/SPEC.md`）。
pub const REJECT_NOTIONAL_EXCEEDED: &str = "notional_exceeded";
pub const REJECT_QTY_EXCEEDED: &str = "qty_exceeded";
pub const REJECT_RATE_EXCEEDED: &str = "rate_exceeded";

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
    pub filled_qty: u32,
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
// 行为契约：订单生命周期状态机
// ---------------------------------------------------------------------------

/// 订单生命周期状态（状态机定义见 `legos-core/SPEC.md`）。
///
/// ```text
/// Created → PendingNew ─┬─→ Accepted ─┬─→ PartiallyFilled ─→ Filled
///    │                  │             │        └→ Canceled
///    └→ Rejected        ├─→ Filled ───┘  (网关立即成交，无需先 Accepted)
///                       ├─→ PartiallyFilled
///                       └─→ Rejected
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderState {
    /// 策略刚产生意图，尚未送风控。
    Created,
    /// 通过风控，已送往网关，等待回执。
    PendingNew,
    /// 网关接受（已挂单，等待后续成交/撤销）。
    Accepted,
    /// 部分成交。
    PartiallyFilled,
    /// 全部成交（终态）。
    Filled,
    /// 已撤销（终态）。
    Canceled,
    /// 被拒绝（风控拦截 / 网关失败，终态）。
    Rejected,
}

/// 非法状态转换（类型层拦截的证据）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IllegalTransition {
    pub from: OrderState,
    pub to: OrderState,
}

/// 跟踪单个订单生命周期的小状态机（`Copy`，零堆分配）。
///
/// 同步管线中每个 `OrderIntent` 配一个 `TrackedOrder`，随事件推进；
/// 任何非法转换返回 `Err(IllegalTransition)` 而不是静默通过。
#[derive(Clone, Copy, Debug)]
pub struct TrackedOrder {
    pub client_order_id: u64,
    state: OrderState,
}

impl TrackedOrder {
    /// 新订单诞生于 `Created`。
    pub fn new(client_order_id: u64) -> Self {
        Self {
            client_order_id,
            state: OrderState::Created,
        }
    }

    pub fn state(&self) -> OrderState {
        self.state
    }

    /// 是否终态（Filled / Canceled / Rejected）。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            OrderState::Filled | OrderState::Canceled | OrderState::Rejected
        )
    }

    /// 合法转换表（与 SPEC.md 中的状态机严格一致）：
    /// - Created → PendingNew, Rejected
    /// - PendingNew → Accepted, PartiallyFilled, Filled, Rejected
    ///   （网关可立即成交，无需先回 Accepted）
    /// - Accepted → PartiallyFilled, Filled, Canceled
    /// - PartiallyFilled → PartiallyFilled, Filled, Canceled
    /// - 终态 → 无出边（任何转换均为非法）
    pub fn advance(&mut self, to: OrderState) -> Result<(), IllegalTransition> {
        let ok = matches!(
            (self.state, to),
            (OrderState::Created, OrderState::PendingNew)
                | (OrderState::Created, OrderState::Rejected)
                | (OrderState::PendingNew, OrderState::Accepted)
                | (OrderState::PendingNew, OrderState::PartiallyFilled)
                | (OrderState::PendingNew, OrderState::Filled)
                | (OrderState::PendingNew, OrderState::Rejected)
                | (OrderState::Accepted, OrderState::PartiallyFilled)
                | (OrderState::Accepted, OrderState::Filled)
                | (OrderState::Accepted, OrderState::Canceled)
                | (OrderState::PartiallyFilled, OrderState::PartiallyFilled)
                | (OrderState::PartiallyFilled, OrderState::Filled)
                | (OrderState::PartiallyFilled, OrderState::Canceled)
        );
        if ok {
            self.state = to;
            Ok(())
        } else {
            Err(IllegalTransition {
                from: self.state,
                to,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// 组件 Trait：六大积木块的插槽（Code-as-Spec：只声明契约，不含算法）
// ---------------------------------------------------------------------------

/// 行情接入积木：`CsvFileFeed`（回测）/ `NativeItchParser`（实盘）实现它。
///
/// 契约：返回的每个 `MarketTick` 必须符合 32 字节数据契约；
/// 两种 feed 对同一逻辑事件产生的 tick 在除 `ts_ns`/`order_id` 来源外必须一致
/// （见 `legos-feed/SPEC.md` 的一致性要求）。
pub trait MarketDataFeed {
    /// 返回下一个行情事件；`None` 表示当前无数据（文件 feed 到达 EOF 时
    /// 为永久结束，实盘 feed 则只是暂时无包）。
    fn next_event(&mut self) -> Option<MarketTick>;
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
    /// 把一个 `MarketTick` 事件应用到账簿上。
    fn apply(&mut self, ev: &MarketTick);
    /// 最优买价 `(price, qty)`。
    fn best_bid(&self) -> Option<(i64, u32)>;
    /// 最优卖价 `(price, qty)`。
    fn best_ask(&self) -> Option<(i64, u32)>;
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
    /// 每个账簿更新后调用一次；返回 `Some(intent)` 即产生一个交易意图
    /// （调用方为其创建 `TrackedOrder`，从 `Created` 开始跟踪）。
    fn on_tick(
        &mut self,
        bid: Option<(i64, u32)>,
        ask: Option<(i64, u32)>,
        mid: Option<i64>,
    ) -> Option<OrderIntent>;
}

/// 事前风控积木：`PassThroughRisk`（回测零开销）/ `HardLimitRisk`（实盘）实现它。
///
/// 契约：返回 `Err(reason)` 时 `reason` 必须是 SPEC 中枚举的原因码之一
/// （`REJECT_*` 常量），调用方据此做监控与告警。
pub trait PreTradeRisk {
    /// 返回 `Ok(())` 放行并推进 `Created → PendingNew`；
    /// `Err(&'static str)` 拦截并推进 `Created → Rejected`。
    fn check_order(&mut self, order: &OrderIntent) -> Result<(), &'static str>;
}

/// 执行网关积木：`FixProtocolGateway`（实盘）/ `SimulatedExchange`（回测）实现它。
pub trait ExecutionGateway {
    /// 发送订单并返回回执；实现**不得 panic**，失败用
    /// `AckStatus::Rejected` 表达。调用方按回执推进状态机：
    /// `PendingNew → Accepted / Filled / PartiallyFilled / Rejected`。
    fn send_order(&mut self, order: &OrderIntent) -> OrderAck;

    /// 每 tick 的最新报价通知（默认空实现）。
    ///
    /// `SimulatedExchange` 重写它来刷新内存撮合盘口；FIX 等实盘网关忽略即可。
    fn on_quote(
        &mut self,
        _symbol_id: u16,
        _bid: Option<(i64, u32)>,
        _ask: Option<(i64, u32)>,
    ) {}
}

// ---------------------------------------------------------------------------
// 单测（Spec-Based Testing：契约即测试）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, size_of};
    use std::ptr::addr_of;

    #[test]
    fn market_tick_data_contract() {
        // 1) 大小 / 对齐锁死
        assert_eq!(size_of::<MarketTick>(), MarketTick::SIZE);
        assert_eq!(size_of::<MarketTick>(), 32);
        assert_eq!(align_of::<MarketTick>(), 8);

        // 2) 字段偏移锁死（拒绝隐式填充：每个空隙都显式命名）
        let t = MarketTick::new(1, 100, 10, Side::Bid, EventKind::Add, 7, 99);
        let base = &t as *const _ as usize;
        // SAFETY: 仅做偏移计算，不解引用。
        unsafe {
            assert_eq!(addr_of!(t.price) as usize - base, 0);
            assert_eq!(addr_of!(t.ts_ns) as usize - base, 8);
            assert_eq!(addr_of!(t.qty) as usize - base, 16);
            assert_eq!(addr_of!(t.symbol_id) as usize - base, 20);
            assert_eq!(addr_of!(t.order_id) as usize - base, 24);
            assert_eq!(addr_of!(t.flags) as usize - base, 28);
        }

        // 3) Copy 语义
        let u = t;
        assert_eq!(t, u);
    }

    #[test]
    fn flags_bitmask_contract() {
        let combos = [
            (Side::Bid, EventKind::Add, 0b000),
            (Side::Ask, EventKind::Add, 0b001),
            (Side::Bid, EventKind::Cancel, 0b010),
            (Side::Ask, EventKind::Cancel, 0b011),
            (Side::Bid, EventKind::Trade, 0b100),
            (Side::Ask, EventKind::Trade, 0b101),
        ];
        for (side, kind, bits) in combos {
            let t = MarketTick::new(1, 0, 0, side, kind, 0, 0);
            assert_eq!(t.flags & 0b111, bits, "flags bit 位契约");
            assert_eq!(t.side(), side);
            assert_eq!(t.kind(), kind);
            assert!(t.reserved_bits_clear());
        }
        // 填充字节清零（杜绝未初始化字节泄漏进共享内存 / 网络）
        let t = MarketTick::new(9, -5, 3, Side::Ask, EventKind::Trade, 11, 13);
        let bytes: [u8; 32] = unsafe { std::mem::transmute(t) };
        assert_eq!(&bytes[22..24], &[0, 0]);
        assert_eq!(&bytes[29..32], &[0, 0, 0]);
    }

    #[test]
    fn order_state_machine_valid_paths() {
        // 完整生命周期：Created → PendingNew → Accepted → PartiallyFilled → Filled
        let mut o = TrackedOrder::new(1);
        assert_eq!(o.state(), OrderState::Created);
        assert!(!o.is_terminal());
        for s in [
            OrderState::PendingNew,
            OrderState::Accepted,
            OrderState::PartiallyFilled,
            OrderState::Filled,
        ] {
            o.advance(s).unwrap();
        }
        assert!(o.is_terminal());

        // 风控拒绝路径
        let mut o = TrackedOrder::new(2);
        o.advance(OrderState::Rejected).unwrap();
        assert!(o.is_terminal());

        // 撤销路径
        let mut o = TrackedOrder::new(3);
        o.advance(OrderState::PendingNew).unwrap();
        o.advance(OrderState::Accepted).unwrap();
        o.advance(OrderState::Canceled).unwrap();
        assert!(o.is_terminal());
    }

    #[test]
    fn order_state_machine_bombardment() {
        // 穷举 7×7=49 种转换：合法的必须通过，非法的必须拦截。
        let all = [
            OrderState::Created,
            OrderState::PendingNew,
            OrderState::Accepted,
            OrderState::PartiallyFilled,
            OrderState::Filled,
            OrderState::Canceled,
            OrderState::Rejected,
        ];
        let legal = [
            (OrderState::Created, OrderState::PendingNew),
            (OrderState::Created, OrderState::Rejected),
            (OrderState::PendingNew, OrderState::Accepted),
            (OrderState::PendingNew, OrderState::PartiallyFilled),
            (OrderState::PendingNew, OrderState::Filled),
            (OrderState::PendingNew, OrderState::Rejected),
            (OrderState::Accepted, OrderState::PartiallyFilled),
            (OrderState::Accepted, OrderState::Filled),
            (OrderState::Accepted, OrderState::Canceled),
            (OrderState::PartiallyFilled, OrderState::PartiallyFilled),
            (OrderState::PartiallyFilled, OrderState::Filled),
            (OrderState::PartiallyFilled, OrderState::Canceled),
        ];
        for &from in &all {
            for &to in &all {
                // 把状态机拨到 from（Created 出发，只走合法边）
                let mut o = TrackedOrder::new(0);
                if from != OrderState::Created {
                    // 找一条到 from 的合法路径
                    let path: &[OrderState] = match from {
                        OrderState::PendingNew => &[OrderState::PendingNew],
                        OrderState::Accepted => {
                            &[OrderState::PendingNew, OrderState::Accepted]
                        }
                        OrderState::PartiallyFilled => &[
                            OrderState::PendingNew,
                            OrderState::Accepted,
                            OrderState::PartiallyFilled,
                        ],
                        OrderState::Filled => &[
                            OrderState::PendingNew,
                            OrderState::Accepted,
                            OrderState::Filled,
                        ],
                        OrderState::Canceled => &[
                            OrderState::PendingNew,
                            OrderState::Accepted,
                            OrderState::Canceled,
                        ],
                        OrderState::Rejected => &[OrderState::Rejected],
                        OrderState::Created => &[],
                    };
                    for s in path {
                        o.advance(*s).unwrap();
                    }
                }
                assert_eq!(o.state(), from);
                let res = o.advance(to);
                if legal.contains(&(from, to)) {
                    assert!(res.is_ok(), "{from:?} → {to:?} 应合法");
                    assert_eq!(o.state(), to);
                } else {
                    let err = res.unwrap_err();
                    assert_eq!(err.from, from);
                    assert_eq!(err.to, to);
                    assert_eq!(o.state(), from, "非法转换不得改变状态");
                    // 核心拦截用例：Filled 的订单再次被撤单必须失败
                    if from == OrderState::Filled && to == OrderState::Canceled {
                        assert!(o.is_terminal());
                    }
                }
            }
        }
    }

    #[test]
    fn side_parsing() {
        assert_eq!(Side::from_byte(b'B'), Some(Side::Bid));
        assert_eq!(Side::from_byte(b'S'), Some(Side::Ask));
        assert_eq!(Side::from_byte(b'1'), Some(Side::Bid));
        assert_eq!(Side::from_byte(b'2'), Some(Side::Ask));
        assert_eq!(Side::from_byte(b'X'), None);
    }

    /// Trait 默认方法的契约测试（Mock 实现）。
    struct OneSidedBook;
    impl OrderBook for OneSidedBook {
        fn apply(&mut self, _ev: &MarketTick) {}
        fn best_bid(&self) -> Option<(i64, u32)> {
            Some((100, 10))
        }
        fn best_ask(&self) -> Option<(i64, u32)> {
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

    #[test]
    fn reject_reason_codes_are_documented() {
        // 原因码契约：非空、小写下划线风格、互不相同
        for r in [
            REJECT_NOTIONAL_EXCEEDED,
            REJECT_QTY_EXCEEDED,
            REJECT_RATE_EXCEEDED,
        ] {
            assert!(!r.is_empty());
            assert!(r.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
        assert_ne!(REJECT_NOTIONAL_EXCEEDED, REJECT_QTY_EXCEEDED);
        assert_ne!(REJECT_QTY_EXCEEDED, REJECT_RATE_EXCEEDED);
    }
}
