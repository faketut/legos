//! legos-book: 定价账簿 —— 实时重组市场盘口的两块可替换积木。
//!
//! * [`L2FlatArrayBook`]: 固定大小连续扁平数组，**零堆分配**，二分查找定位档位、
//!   `copy_within` 平移插入。热数据常驻 L1/L2 缓存，为高频抢单而生；
//!   代价是牺牲极端深度（超出 `LEVELS` 的档位直接丢弃）。
//! * [`L3MapBook`]: 按订单 ID 维护精确订单树的深度账簿，适合订单排队队列分析。

mod l2;
mod l3;

pub use l2::L2FlatArrayBook;
pub use l3::L3MapBook;
