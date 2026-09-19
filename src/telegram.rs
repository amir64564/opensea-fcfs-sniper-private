//! Telegram long-poll control bot (reqwest).
//! Access: TELEGRAM_BOT_PASSWORD required; commands /password and /lock.
//! Implementation split across include! parts (MCP size); behavior matches monolithic src/telegram.rs.
include!("telegram_part1.rs");
include!("telegram_part2.rs");
