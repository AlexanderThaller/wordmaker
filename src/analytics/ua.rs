/// What a `User-Agent` header says about the visitor's software.
#[derive(Debug, PartialEq, Eq)]
pub struct UserAgent {
    pub browser: String,
    pub browser_version: String,
    pub os: String,
    pub os_version: String,
    /// `pc`, `smartphone`, `mobilephone`, `appliance`, `crawler` or `UNKNOWN`.
    pub device: String,
    pub is_bot: bool,
}

pub fn parse(user_agent: &str) -> UserAgent {
    let parsed = woothee::parser::Parser::new()
        .parse(user_agent)
        .unwrap_or_default();
    let known = |value: &str| {
        if value.is_empty() {
            woothee::woothee::VALUE_UNKNOWN.to_owned()
        } else {
            value.to_owned()
        }
    };
    UserAgent {
        browser: known(parsed.name),
        browser_version: known(parsed.version),
        os: known(parsed.os),
        os_version: known(&parsed.os_version),
        device: known(parsed.category),
        is_bot: user_agent.trim().is_empty() || parsed.category == "crawler",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsers_and_bots() {
        let chrome = parse(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/129.0.0.0 Safari/537.36",
        );
        assert_eq!(chrome.browser, "Chrome");
        assert_eq!(chrome.browser_version, "129.0.0.0");
        assert_eq!(chrome.os, "Windows 10");
        assert_eq!(chrome.device, "pc");
        assert!(!chrome.is_bot);

        let iphone = parse(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, \
             like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
        );
        assert_eq!(iphone.browser, "Safari");
        assert_eq!(iphone.os, "iPhone");
        assert_eq!(iphone.device, "smartphone");
        assert!(!iphone.is_bot);

        let googlebot =
            parse("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)");
        assert_eq!(googlebot.device, "crawler");
        assert!(googlebot.is_bot);

        let empty = parse("");
        assert_eq!(empty.browser, "UNKNOWN");
        assert!(empty.is_bot);
    }
}
