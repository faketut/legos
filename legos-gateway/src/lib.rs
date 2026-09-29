//! legos-gateway: 执行网关 —— 订单落地的最后一公里。
//!
//! * [`FixProtocolGateway`]: 实盘积木，构造 FIX 4.4 `NewOrderSingle`
//!   报文并经 TCP 发送到券商 / 交易所柜台；连接失败**永不 panic**，
//!   一律返回 `AckStatus::Rejected` 回执；
//! * [`SimulatedExchange`]: 回测积木，内存撮合：维护各品种最优买卖报价，
//!   按市价 / 可成交限价即时撮合，带固定滑点模型，返回 Fill 回执。

mod fix;
mod sim;

pub use fix::FixProtocolGateway;
pub use sim::SimulatedExchange;
