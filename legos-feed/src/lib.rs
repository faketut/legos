//! legos-feed: 行情接入 —— 系统的「耳朵」。
//!
//! * [`CsvFileFeed`]: 回测积木，从本地 CSV 文件逐行解析 [`Tick`](legos_core::Tick)；
//!   仓库自带 `data/sample_ticks.csv` 示例数据；
//! * [`NativeItchParser`]: 实盘积木，解析 NASDAQ ITCH 5.0 二进制流
//!   （`'A'` Add Order / `'E'` Executed / `'X'` Cancel / `'D'` Delete），
//!   输出统一的 `Tick`，其余消息类型见文档中的类型表。

mod csv;
mod itch;

pub use csv::CsvFileFeed;
pub use itch::NativeItchParser;
