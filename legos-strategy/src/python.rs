//! Python 绑定策略（中低频研究积木）。
//!
//! # 两种形态（同一积木名，编译期切换）
//!
//! * **默认构建（stub）**：`PythonBindingStrategy::new()` 直接可用，
//!   `on_tick` 恒返回 `None`。零开销、零依赖，`cargo build` 开箱即用；
//! * **`python` feature**：`cargo build -p legos-strategy --features python`
//!   启用真正的 CPython 嵌入（`pyo3` 0.22，需要 Python 3.8+ 开发环境）。
//!   此时 `PythonBindingStrategy::load(path, func)` 从 Python 文件加载
//!   模型函数，每 tick 调用一次。
//!
//! Python 侧函数签名约定：
//!
//! ```python
//! def predict(bid: int | None, ask: int | None, mid: int | None):
//!     """返回 (side, price, qty)，side 为 "B"/"S"；无信号返回 None。"""
//!     ...
//! ```
//!
//! 注意：嵌入解释器 + GIL 的调用开销在微秒量级，只适合中低频研究场景，
//! 这正是把它做成可替换积木的原因——高频场景请换 `MarketMakerStrategy`。

use legos_core::{OrderIntent, TradingStrategy};

// ---------------------------------------------------------------------------
// 真实实现：--features python
// ---------------------------------------------------------------------------

#[cfg(feature = "python")]
mod real {
    use super::*;
    use pyo3::prelude::*;
    use pyo3::types::{PyDict, PyModule};

    /// 嵌入 CPython 的策略：每 tick 调用 Python 的 `predict` 函数。
    pub struct PythonBindingStrategy {
        predict: Py<PyAny>,
        symbol_id: u32,
        next_id: u64,
    }

    impl PythonBindingStrategy {
        /// 从 `path` 的 Python 文件加载名为 `func` 的函数。
        ///
        /// ```no_run
        /// # #[cfg(feature = "python")]
        /// # {
        /// use legos_strategy::PythonBindingStrategy;
        /// let s = PythonBindingStrategy::load("model.py", "predict", 1).unwrap();
        /// # }
        /// ```
        pub fn load(path: &str, func: &str, symbol_id: u32) -> PyResult<Self> {
            Python::with_gil(|py| {
                let code = std::fs::read_to_string(path).map_err(|e| {
                    PyErr::new::<pyo3::exceptions::PyIOError, _>(format!("读取模型文件失败: {e}"))
                })?;
                // pyo3 0.22 的 Bound API（gil-refs 已默认关闭）。
                let module = PyModule::from_code_bound(py, &code, path, "legos_model")?;
                let predict: Py<PyAny> = module.getattr(func)?.into();
                Ok(Self {
                    predict,
                    symbol_id,
                    next_id: 0,
                })
            })
        }

        fn call_predict(
            &self,
            bid: Option<(i64, u64)>,
            ask: Option<(i64, u64)>,
            mid: Option<i64>,
        ) -> Option<(u8, i64, u64)> {
            Python::with_gil(|py| {
                let kwargs = PyDict::new_bound(py);
                kwargs.set_item("bid", bid.map(|(p, _)| p)).ok()?;
                kwargs.set_item("ask", ask.map(|(p, _)| p)).ok()?;
                kwargs.set_item("mid", mid).ok()?;
                let ret = self.predict.call_bound(py, (), Some(&kwargs)).ok()?;
                if ret.is_none(py) {
                    return None;
                }
                let (side, price, qty): (String, i64, u64) = ret.extract(py).ok()?;
                let side = match side.as_str() {
                    "B" => b'B',
                    "S" => b'S',
                    _ => return None,
                };
                Some((side, price, qty))
            })
        }
    }

    impl TradingStrategy for PythonBindingStrategy {
        fn on_tick(
            &mut self,
            bid: Option<(i64, u64)>,
            ask: Option<(i64, u64)>,
            mid: Option<i64>,
        ) -> Option<OrderIntent> {
            let (s, price, qty) = self.call_predict(bid, ask, mid)?;
            self.next_id += 1;
            Some(OrderIntent {
                client_order_id: self.next_id,
                symbol_id: self.symbol_id,
                side: legos_core::Side::from_byte(s)?,
                price,
                qty,
                ts_ns: 0,
            })
        }
    }
}

#[cfg(feature = "python")]
pub use real::PythonBindingStrategy;

// ---------------------------------------------------------------------------
// 默认 stub：无 python feature 时的同名零开销占位
// ---------------------------------------------------------------------------

/// Stub：在未启用 `python` feature 时提供同名类型，保证管线代码
/// 无需改动即可编译；`on_tick` 恒返回 `None`。
///
/// 启用真实嵌入：`cargo build -p legos-strategy --features python`。
#[cfg(not(feature = "python"))]
#[derive(Debug, Default, Clone, Copy)]
pub struct PythonBindingStrategy {
    _private: (),
}

#[cfg(not(feature = "python"))]
impl PythonBindingStrategy {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(not(feature = "python"))]
impl TradingStrategy for PythonBindingStrategy {
    fn on_tick(
        &mut self,
        _bid: Option<(i64, u64)>,
        _ask: Option<(i64, u64)>,
        _mid: Option<i64>,
    ) -> Option<OrderIntent> {
        // stub：不产生任何交易意图。启用 python feature 后替换为真实实现。
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_compiles_and_stays_quiet() {
        // 默认构建下 stub 必须能参与泛型管线且永不下单。
        let mut s = PythonBindingStrategy::new();
        assert_eq!(s.on_tick(Some((1, 1)), Some((2, 1)), Some(1)), None);
    }
}
