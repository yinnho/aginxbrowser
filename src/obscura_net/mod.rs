#![allow(dead_code)]
pub mod client;
pub mod cookies;
pub mod encoding;
pub mod robots;
pub mod blocklist;
#[cfg(feature = "stealth")]
pub mod wreq_client;

pub use client::{
    env_allows_private_network, ObscuraHttpClient, ObscuraNetError,
    Response,
};
pub use cookies::CookieJar;
pub use encoding::{
    decode_non_html, decode_response_with_name, decode_with_label, label_name,
    url_encode_query,
};
pub use robots::RobotsCache;
#[cfg(feature = "stealth")]
pub use wreq_client::{StealthHttpClient, STEALTH_USER_AGENT, parse_tls_fingerprint};
