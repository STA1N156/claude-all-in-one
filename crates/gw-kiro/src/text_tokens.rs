//! 文本 token 估算 —— 🔵 搬自旧 `src/token.rs::count_tokens` + `stream.rs::estimate_tokens`。
//!
//! 纯本地字符级估算,**零网络**:cache_sim 用 [`count_tokens`] 给每条消息估 token
//! (公共前缀命中量),流式收尾用 [`OutputTally`] 累计字符、按模型分词器折算 completion
//! (旧口径 [`legacy_output_tokens`] 并行保留,`cache_io_correction` 关闭时计费用)。
//!
//! 注意与本 crate 的 [`crate::token`](auth 刷新)同名概念不同:那是 OAuth refreshToken
//! 刷新,本模块是计费用的文本 token 估算,故另起 `text_tokens` 名避免混淆。
//!
//! 计算规则(逐字节对齐旧代码,保证 cache_sim 跨项目同口径):
//! - 非西文字符(中日韩等)按 4 字符单位、西文按 1;4 单位 = 1 token;
//! - 再按档位非线性放大(<100 ×1.5 … ≥800 ×1.0),贴合真实 tokenizer 短文本高开销。

pub use gw_core::text_tokens::{count_request_tokens, count_tokens};

/// 模型所用的分词器代际。
///
/// Kiro 对 Claude 模型不下发 tokenUsageEvent,输出 token 只能按字符估算;不同代际每字符
/// token 数不同(新分词器切代码/英文切得更碎),必须按模型选系数,见 [`OutputTally::tokens`]。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tokenizer {
    /// opus-4.7 起的新分词器。opus-4.8 / opus-5 / sonnet-5 / opus-5.5 / sonnet-5.5 实测同一套;
    /// 权威表之外的模型(多为更新的模型)也按它算。
    #[default]
    Current,
    /// 4.6 及更早。opus-4.6 实测;4.5 系与 haiku-4.5 按代际推定。
    Legacy,
}

impl Tokenizer {
    /// 每字符 token 数:(ASCII, 中日韩文字, 其它非 ASCII)。
    ///
    /// 2026-10-06 实测标定:同一个号发「附带一段文本、只回复 OK」,contextUsage 的差值
    /// 就是这段文本的真实 token 数。样本取自自家公开仓库(英文文档、TS/Rust 源码、工具调用
    /// 风格 JSON、中文文档)和一段 1 万字中文:
    /// - 新分词器 ASCII:英文文档 0.375、TS 源码 0.42、工具 JSON 0.43,取 0.39(生产输出约
    ///   六成是英文推理摘要、四成是代码/工具参数,取偏低一侧,宁少勿多);
    /// - 旧分词器(opus-4.6)同一段 TS 源码只有新分词器的 0.755 倍,ASCII 取 0.30;
    /// - 中文两代差不到 2%:1.22~1.27,取 1.2;
    /// - 其它非 ASCII(全角标点、拉丁扩展、符号)按 1。
    ///
    /// 8 段样本上误差在 ±11% 以内。旧公式(英文 4 字符/token、中文 1.5 字/token、逐帧取整)
    /// 在同样样本上低估 23%~48%。
    const fn rates(self) -> (f64, f64, f64) {
        match self {
            Tokenizer::Current => (0.39, 1.2, 1.0),
            Tokenizer::Legacy => (0.30, 1.2, 1.0),
        }
    }
}

/// 中日韩文字(汉字含扩展区与兼容区、假名、谚文音节)。全角标点不在内,按「其它」计。
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{20000}'..='\u{2FA1F}'
    )
}

/// 一次响应的输出字符累计(推理摘要 + 正文 + 工具参数),收尾时按分词器折算成 token。
///
/// 只累计字符、最后一次性折算:逐帧各自取整会让每帧多算最多 1 个 token,
/// 生产上流式小帧把整体抬高了约 16%,且与真实分词无关。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputTally {
    ascii: u64,
    cjk: u64,
    other: u64,
}

impl OutputTally {
    pub fn add(&mut self, text: &str) {
        for c in text.chars() {
            if c.is_ascii() {
                self.ascii += 1;
            } else if is_cjk(c) {
                self.cjk += 1;
            } else {
                self.other += 1;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ascii == 0 && self.cjk == 0 && self.other == 0
    }

    /// 折算成 token(四舍五入;有字符时至少 1,空为 0)。
    pub fn tokens(&self, tokenizer: Tokenizer) -> i64 {
        if self.is_empty() {
            return 0;
        }
        let (a, c, o) = tokenizer.rates();
        let t = self.ascii as f64 * a + self.cjk as f64 * c + self.other as f64 * o;
        (t.round() as i64).max(1)
    }
}

/// 旧口径的输出 token 估算(中文 ~1.5 字/token、其余 ~4 字符/token,至少 1),逐帧调用累加。
///
/// 🔵 搬自旧 `stream.rs::estimate_tokens`。实测比真实分词少算约 37%(见 [`Tokenizer::rates`]),
/// 只在 `cache_io_correction` 关闭时用于计费,保证开关关闭时账单与改动前逐字节一致。
pub fn legacy_output_tokens(text: &str) -> i64 {
    let mut chinese_count: i64 = 0;
    let mut other_count: i64 = 0;
    for c in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&c) {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;
    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_output_estimate_unchanged() {
        assert_eq!(legacy_output_tokens(""), 1, "至少 1");
        assert_eq!(legacy_output_tokens("hi"), 1); // (2+3)/4 = 1
        // 3 个中文 → (3*2+2)/3 = 2;8 个英文 → (8+3)/4 = 2
        assert_eq!(legacy_output_tokens("你好吗"), 2);
        assert_eq!(legacy_output_tokens("abcdefgh"), 2);
    }

    #[test]
    fn tally_empty_is_zero_and_nonempty_at_least_one() {
        let mut t = OutputTally::default();
        assert!(t.is_empty());
        assert_eq!(t.tokens(Tokenizer::Current), 0);
        t.add("");
        assert!(t.is_empty(), "空串不计");
        t.add("a");
        assert_eq!(t.tokens(Tokenizer::Current), 1, "有字符至少 1");
    }

    #[test]
    fn tally_rates_by_char_class_and_tokenizer() {
        let mut t = OutputTally::default();
        t.add(&"a".repeat(1000)); // ASCII
        t.add(&"字".repeat(100)); // 汉字
        t.add(&"。".repeat(10)); // 全角标点 → 其它
        // 新:1000×0.39 + 100×1.2 + 10×1 = 520;旧:1000×0.30 + 120 + 10 = 430
        assert_eq!(t.tokens(Tokenizer::Current), 520);
        assert_eq!(t.tokens(Tokenizer::Legacy), 430);
    }

    #[test]
    fn tally_kana_hangul_count_as_cjk() {
        let mut t = OutputTally::default();
        t.add("かな한글");
        assert_eq!(t.tokens(Tokenizer::Current), 5); // 4 × 1.2 = 4.8 → 5
    }

    #[test]
    fn tally_is_split_invariant() {
        // 逐帧累计与整段一次累计结果相同(旧公式逐帧取整会多算)。
        let text = "Let me check the file. 我先看一下文件。fn main() {}";
        let mut whole = OutputTally::default();
        whole.add(text);
        let mut framed = OutputTally::default();
        for ch in text.chars() {
            framed.add(&ch.to_string());
        }
        assert_eq!(whole, framed);
    }
}
