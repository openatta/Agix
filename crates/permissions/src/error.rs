//! 权限闸 / 规则解析错误。

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum ParseRuleError {
    #[error("rule string is empty")]
    Empty,
    #[error("rule has unbalanced parens: {0}")]
    Unbalanced(String),
    #[error("malformed rule: {0}")]
    Malformed(String),
}
