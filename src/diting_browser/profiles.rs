//! UA / platform fingerprint pool, ported from upstream `profiles.rs`
//! (browser Phase 1 batch 1).
//!
//! One entry is a *consistent* persona: the UA string, `navigator.platform`,
//! and the userAgentData platform/version triple all describe the same
//! device. Our `bootstrap.js` derives `navigator.platform` from the UA string
//! itself (`__obscuraPlatformFromUA`, the Baidu Wenku mismatch fix), so on
//! this side the pool is only consumed for its UA strings — but the table
//! keeps the platform columns both for upstream parity and as the source of
//! truth the bootstrap derivation is checked against.

/// One fingerprint persona. `user_agent` drives every request; the platform
/// fields are the persona contract the UA implies — bootstrap derives the
/// live values from the UA at runtime, tests cross-check them here, and
/// nothing else reads them yet.
#[allow(dead_code)]
pub struct BrowserProfile {
    pub user_agent: &'static str,
    pub platform: &'static str,
    pub ua_platform: &'static str,
    pub ua_platform_version: &'static str,
}

pub static PROFILES: &[BrowserProfile] = &[
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "10.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "10.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "15.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "15.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "13.6.7",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.4.1",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.5.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.6.0",
    },
];

/// Product default: the stable macOS Chrome 145 persona this service has
/// always presented when no override is set (index 6 in the pool). Upstream's
/// `select_profile` defaults to `PROFILES[0]` (Windows); flipping our default
/// would change the UA every un-pinned deployment sends, so we pin ours.
pub const DEFAULT_PROFILE_INDEX: usize = 6;

pub fn random_profile() -> &'static BrowserProfile {
    let idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize)
        % PROFILES.len();
    &PROFILES[idx]
}

/// Pick the profile for a new browser context.
///
/// The default is a single stable profile. Cycling through different browser
/// identities from one address is itself a bot signal (a real address maps to
/// a stable device), and the rotated profile does not yet carry a matching
/// TLS or timezone fingerprint, so rotation is opt-in:
///   OBSCURA_PROFILE=<index>   pin a specific profile from PROFILES
///   OBSCURA_ROTATE_PROFILE=1  pick a random profile per context
pub fn select_profile() -> &'static BrowserProfile {
    if let Some(idx) = std::env::var("OBSCURA_PROFILE")
        .ok()
        .as_deref()
        .map(str::trim)
        .and_then(|s| s.parse::<usize>().ok())
    {
        if idx < PROFILES.len() {
            return &PROFILES[idx];
        }
    }
    if env_enabled("OBSCURA_ROTATE_PROFILE") {
        return random_profile();
    }
    &PROFILES[DEFAULT_PROFILE_INDEX]
}

fn env_enabled(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_consistent_personas() {
        assert!(PROFILES.len() >= 8);
        for p in PROFILES {
            assert!(p.user_agent.contains("Chrome/"), "{}", p.user_agent);
            // UA OS token and platform triple must describe the same device.
            if p.user_agent.contains("Windows NT") {
                assert_eq!(p.platform, "Win32");
                assert_eq!(p.ua_platform, "Windows");
            } else if p.user_agent.contains("Macintosh") {
                assert_eq!(p.platform, "MacIntel");
                assert_eq!(p.ua_platform, "macOS");
            } else {
                panic!("unknown OS token in {}", p.user_agent);
            }
        }
    }

    #[test]
    fn default_profile_is_the_product_default_macos_chrome() {
        let p = select_profile();
        // Un-pinned select keeps the persona every AGINXBROWSER_UA-unset
        // deployment has always presented: macOS Chrome 145.
        assert!(p.user_agent.starts_with("Mozilla/5.0 (Macintosh"));
        assert!(p.user_agent.contains("Chrome/145"));
        assert_eq!(p.platform, "MacIntel");
    }

    #[test]
    fn random_profile_stays_inside_the_pool() {
        for _ in 0..50 {
            let p = random_profile();
            assert!(PROFILES
                .iter()
                .any(|c| c.user_agent == p.user_agent));
        }
    }
}
