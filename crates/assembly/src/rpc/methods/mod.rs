//! 各命名空间方法的 params 与线格式包装，一个命名空间一个文件。领域结果类型在
//! `session_pool::api`，这里只放线上多出来的那一层（`ping` 的 `pong`、
//! `ignored_options` 之类）。

pub mod daemon;
pub mod schedule;
