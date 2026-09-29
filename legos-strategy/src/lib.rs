//! legos-strategy: 策略逻辑 —— 系统的「大脑」。
//!
//! * [`MarketMakerStrategy`]: 高频做市，围绕中间价按 `spread`/`skew` 双边报价；
//! * [`ArbitrageStrategy`]: 跨场所套利，持有另一场所的报价快照，捕捉价差；
//! * [`PythonBindingStrategy`]: 中低频研究积木。**默认构建是同名 stub**
//!  （`on_tick` 返回 `None`，零开销）；启用 `python` feature 后替换为
//!   真正嵌入 CPython 解释器的实现（见模块文档）。

mod arbitrage;
mod market_maker;
mod python;

pub use arbitrage::ArbitrageStrategy;
pub use market_maker::MarketMakerStrategy;
pub use python::PythonBindingStrategy;
