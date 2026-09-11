//! Token 估算、上下文窗口、成本表、会话用量累计。
//! 估算走 `rupi_core::estimate_tokens`；provider `usage` 用来校准比例。

use rupi_core::estimate_tokens;

/// 默认上下文窗口（无模型命中时）。
pub const DEFAULT_CONTEXT_WINDOW: usize = 128_000;
pub const DEFAULT_RESERVE_TOKENS: usize = 16_384;
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 20_000;

/// 模型价目：每 1M token 的 USD（input / output）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelRates {
    pub input_per_m: f64,
    pub output_per_m: f64,
    pub context_window: usize,
}

impl ModelRates {
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64) * self.input_per_m / 1_000_000.0
            + (output_tokens as f64) * self.output_per_m / 1_000_000.0
    }
}

/// 按模型名（大小写不敏感、子串）查价目与窗口。
pub fn rates_for(model: &str) -> ModelRates {
    let m = model.to_ascii_lowercase();
    if m.contains("claude-opus") || m.contains("claude-4-opus") {
        ModelRates {
            input_per_m: 15.0,
            output_per_m: 75.0,
            context_window: 200_000,
        }
    } else if m.contains("claude-sonnet") || m.contains("claude-4") || m.contains("claude-3-5") {
        ModelRates {
            input_per_m: 3.0,
            output_per_m: 15.0,
            context_window: 200_000,
        }
    } else if m.contains("claude-haiku") || m.contains("claude-3-haiku") {
        ModelRates {
            input_per_m: 0.80,
            output_per_m: 4.0,
            context_window: 200_000,
        }
    } else if m.contains("claude") {
        ModelRates {
            input_per_m: 3.0,
            output_per_m: 15.0,
            context_window: 200_000,
        }
    } else if m.contains("gemini") && (m.contains("pro") || m.contains("2.5")) {
        ModelRates {
            input_per_m: 1.25,
            output_per_m: 10.0,
            context_window: 1_048_576,
        }
    } else if m.contains("gemini") {
        ModelRates {
            input_per_m: 0.10,
            output_per_m: 0.40,
            context_window: 1_048_576,
        }
    } else if m.contains("gpt-4o-mini") || m.contains("o4-mini") {
        ModelRates {
            input_per_m: 0.15,
            output_per_m: 0.60,
            context_window: 128_000,
        }
    } else if m.contains("gpt-4.1") || m.contains("gpt-5") {
        ModelRates {
            input_per_m: 2.0,
            output_per_m: 8.0,
            context_window: 1_047_576,
        }
    } else if m.contains("gpt-4o") {
        ModelRates {
            input_per_m: 2.50,
            output_per_m: 10.0,
            context_window: 128_000,
        }
    } else {
        ModelRates {
            input_per_m: 1.0,
            output_per_m: 5.0,
            context_window: DEFAULT_CONTEXT_WINDOW,
        }
    }
}

pub fn context_window_for(model: &str) -> usize {
    rates_for(model).context_window
}

/// 会话级用量：累计 provider 回报，并用估算校准后续 context%。
#[derive(Debug, Clone)]
pub struct TokenMeter {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_input: u64,
    pub last_output: u64,
    pub last_estimate: u64,
    /// provider_input / estimate；无校准为 1.0。
    pub ratio: f64,
    pub context_window: usize,
}

impl Default for TokenMeter {
    fn default() -> Self {
        Self {
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            last_input: 0,
            last_output: 0,
            last_estimate: 0,
            ratio: 1.0,
            context_window: DEFAULT_CONTEXT_WINDOW,
        }
    }
}

impl TokenMeter {
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        let window = context_window_for(&model);
        Self {
            context_window: window,
            model,
            ..Self::default()
        }
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        let model = model.into();
        self.context_window = context_window_for(&model);
        self.model = model;
    }

    pub fn set_context_window(&mut self, window: usize) {
        if window > 0 {
            self.context_window = window;
        }
    }

    /// 记下本轮 provider usage，并用本轮送出估算校准。
    pub fn note_usage(&mut self, input: u64, output: u64, estimated_input: u64) {
        self.last_input = input;
        self.last_output = output;
        self.last_estimate = estimated_input;
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.output_tokens = self.output_tokens.saturating_add(output);
        if estimated_input > 0 && input > 0 {
            let r = input as f64 / estimated_input as f64;
            if r.is_finite() && r > 0.1 && r < 10.0 {
                self.ratio = r;
            }
        }
    }

    pub fn calibrate(&self, raw_estimate: u64) -> u64 {
        ((raw_estimate as f64) * self.ratio).round() as u64
    }

    pub fn estimate_text(&self, text: &str) -> u64 {
        self.calibrate(estimate_tokens(text))
    }

    pub fn cost_usd(&self) -> f64 {
        rates_for(&self.model).cost(self.input_tokens, self.output_tokens)
    }

    pub fn context_pct(&self, context_tokens: u64) -> f64 {
        if self.context_window == 0 {
            return 0.0;
        }
        (context_tokens as f64) * 100.0 / (self.context_window as f64)
    }

    /// 状态栏：`↑1.2k ↓340 12% $0.004`
    pub fn footer(&self, context_tokens: u64) -> String {
        format!(
            "↑{} ↓{} {:.0}% ${}",
            fmt_tokens(self.input_tokens),
            fmt_tokens(self.output_tokens),
            self.context_pct(context_tokens),
            fmt_usd(self.cost_usd())
        )
    }
}

pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn fmt_usd(v: f64) -> String {
    if v < 0.01 {
        format!("{v:.4}")
    } else if v < 10.0 {
        format!("{v:.3}")
    } else {
        format!("{v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_match_common_models() {
        assert_eq!(context_window_for("claude-sonnet-4-5"), 200_000);
        assert_eq!(context_window_for("gpt-4o-mini"), 128_000);
        assert!(rates_for("gpt-4o-mini").cost(1_000_000, 0) > 0.1);
        assert!(rates_for("gpt-4o-mini").cost(1_000_000, 0) < 0.2);
    }

    #[test]
    fn meter_calibrates_and_formats() {
        let mut m = TokenMeter::new("gpt-4o-mini");
        m.note_usage(80, 20, 40);
        assert!((m.ratio - 2.0).abs() < 1e-9);
        assert_eq!(m.calibrate(10), 20);
        assert_eq!(m.input_tokens, 80);
        assert_eq!(m.output_tokens, 20);
        let line = m.footer(12_800);
        assert!(line.contains('↑'), "{line}");
        assert!(line.contains('↓'), "{line}");
        assert!(line.contains('%'), "{line}");
        assert!(line.contains('$'), "{line}");
        assert_eq!(fmt_tokens(1500), "1.5k");
        assert_eq!(fmt_tokens(42), "42");
    }
}
