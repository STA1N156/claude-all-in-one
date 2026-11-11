//! `agent.v1.AgentService/Run` 的请求构造与响应解析。
//!
//! 字段号全部来自 `PROTOCOL-agent-run.md` §3(2026-08-06 对本机 3.14.27 抓真包逐字节解码)。
//!
//! ## 读字段号时最容易栽的一跤
//!
//! 文档 §3.1 写的 `1.1` / `1.2` / `1.9`,**最前面那个 `1` 是 Connect 帧 payload 的顶层
//! field 1**(帧类型 oneof),不是 message 名。也就是说 `1.9`(当前模型)的真实路径是
//! 「帧 payload 的 field 1 → 它内部的 field 9」,**不是**顶层 field 9。按字面写会整个请求作废。
//!
//! ## 哪些字段是必需的:文档答不了,只能实测
//!
//! 文档 §0 的三级验证(原样重放 / 变异重放 / 结构合成)用的都是抓包实物的**完整** 3 帧,
//! **没做过任何删字段、删帧的最小化实验**。所以「哪些能省」在文档里是空白。
//! 因此本模块把可疑分节做成 [`RunShape`] 开关,e2e 时二分,而不是拍脑袋写死。

use serde_json::{json, Value};

use sha2::{Digest, Sha256};

use crate::protobuf::{Reader, Value as PbValue, Writer};

mod request;
mod tools;
mod response;
mod exec_frames;
mod trailer;
pub use self::request::*;
pub use self::tools::*;
pub use self::response::*;
pub use self::exec_frames::*;
pub use self::trailer::*;

/// 影子模式测试向量:官方 CLI 两轮抓包(2026-08-23)里响应尾部的真实续轮
/// 描述符帧(顶层 `.3`;已去 connect 帧头,flag=0 未压缩)。
/// turn1(新会话)= 4 个 blob 引用;turn2(resume)= 6 个(多出的 2 个是 turn1
/// 产生的新 blob)。chat.rs 的影子状态机测试也复用这份样本。
#[cfg(test)]
pub(crate) mod descriptor_samples {
    const TURN1_HEX: &str = concat!(
        "1a93040a2083ccb7a56d69b4e802e6d3e5bd95f480c74e90509a99c0e7e86199a253420e9d0a209d3e7d77",
        "8fe69cee10f80bc46cc6b488b3e9647c6c685c405bfcdaf853a81b190a200e854779b77b9c42c1cce78cb4",
        "4f5a20a0bdebf3e7bff700f02275e9382909a90a20eba17aceecd9f9f0cc20147089bef4c5e54395096e6d",
        "d791abe4aa0722be1ec62aaf0208938a011080d00f1aa40208938a011080d00f1a240a0d73797374656d5f",
        "70726f6d7074120d53797374656d2070726f6d707418ea0320a20f1a200a05746f6f6c731210546f6f6c",
        "20646566696e6974696f6e7318ea3820eae2011a140a0572756c6573120552756c657318c01420ea511a",
        "170a06736b696c6c731206536b696c6c7318fd2720cf9f011a200a036d637012134d435020262064796e",
        "616d696320746f6f6c73189e0720ee1c1a270a097375626167656e747312145375626167656e74206465",
        "66696e6974696f6e7318df0520f8161a340a1773756d6d6172697a65645f636f6e766572736174696f6e",
        "121753756d6d6172697a656420636f6e766572736174696f6e20001a220a0c636f6e766572736174696f",
        "6e120c436f6e766572736174696f6e18850420931042202079b7f951d09870cf25df5f3fad51d63d0c7d",
        "f18f0382782a0964568a7eacae4a1566696c653a2f2f2f746d702f636c692d70726f62655002b2010363",
        "6c69d001b0cd9fe68234da010d417369612f5368616e67686169",
    );
    const TURN2_HEX: &str = concat!(
        "1af9040a2083ccb7a56d69b4e802e6d3e5bd95f480c74e90509a99c0e7e86199a253420e9d0a209d3e7d77",
        "8fe69cee10f80bc46cc6b488b3e9647c6c685c405bfcdaf853a81b190a200e854779b77b9c42c1cce78cb4",
        "4f5a20a0bdebf3e7bff700f02275e9382909a90a20eba17aceecd9f9f0cc20147089bef4c5e54395096e6d",
        "d791abe4aa0722be1ec60a2049efdd84a6f34b5a5627262f8be867f55519b8fdd912c8d9a09974eec1b18b",
        "bc0a20055fd9c055dde4c3ceaa81df885f49e5a601f8a1339952d96042ecc669e610cb2aaf0208b98b0110",
        "80d00f1aa40208b98b011080d00f1a240a0d73797374656d5f70726f6d7074120d53797374656d2070726f",
        "6d707418ea0320a20f1a200a05746f6f6c731210546f6f6c20646566696e6974696f6e7318ea3820eae201",
        "1a140a0572756c6573120552756c657318c01420ea511a170a06736b696c6c731206536b696c6c7318fd27",
        "20cf9f011a200a036d637012134d435020262064796e616d696320746f6f6c73189e0720ee1c1a270a0973",
        "75626167656e747312145375626167656e7420646566696e6974696f6e7318df0520f8161a340a177375",
        "6d6d6172697a65645f636f6e766572736174696f6e121753756d6d6172697a656420636f6e7665727361",
        "74696f6e20001a220a0c636f6e766572736174696f6e120c436f6e766572736174696f6e18ab0520a513",
        "42202079b7f951d09870cf25df5f3fad51d63d0c7df18f0382782a0964568a7eacae42208ef11f326b346f",
        "31a60f7404fc4e2fae4e3e43335adeada5207242c3e5d0f34a4a1566696c653a2f2f2f746d702f636c69",
        "2d70726f62655002b20103636c69d001b0cd9fe68234da010d417369612f5368616e67686169",
    );

    pub(crate) fn hex_to_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("样本必须是合法 hex"))
            .collect()
    }

    /// turn1(新会话)响应尾部的描述符帧 payload:4 个 blob 引用。
    pub(crate) fn turn1() -> Vec<u8> {
        hex_to_bytes(TURN1_HEX)
    }

    /// turn2(resume)响应尾部的描述符帧 payload:6 个 blob 引用。
    pub(crate) fn turn2() -> Vec<u8> {
        hex_to_bytes(TURN2_HEX)
    }
}

#[cfg(test)]
pub(crate) mod tests;
