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
        is_bot: user_agent.trim().is_empty()
            || parsed.category == "crawler"
            || names_a_bot(user_agent),
    }
}

/// Words in the user agents of bots the parser does not know, such as
/// Bytespider, Applebot or headless browsers.
const BOT_WORDS: &[&str] = &[
    "bot",
    "crawl",
    "spider",
    "slurp",
    "headless",
    "facebookexternalhit",
];

/// Phones whose names contain a bot word.
const NOT_BOTS: &[&str] = &["cubot"];

fn names_a_bot(user_agent: &str) -> bool {
    let mut agent = user_agent.to_lowercase();
    for name in NOT_BOTS {
        agent = agent.replace(name, "");
    }
    BOT_WORDS.iter().any(|word| agent.contains(word))
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

        for bot in [
            "Mozilla/5.0 (compatible; GPTBot/1.2; +https://openai.com/gptbot)",
            "Mozilla/5.0 (Linux; Android 5.0) AppleWebKit/537.36 (KHTML, like Gecko) Mobile \
             Safari/537.36 (compatible; Bytespider; spider-feedback@bytedance.com)",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like \
             Gecko) Version/13.1.1 Safari/605.1.15 (Applebot/0.1; +http://www.apple.com/go/applebot)",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
             HeadlessChrome/129.0.0.0 Safari/537.36",
        ] {
            assert!(parse(bot).is_bot, "{bot}");
        }
        let cubot = parse(
            "Mozilla/5.0 (Linux; Android 10; CUBOT X30) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/120.0 Mobile Safari/537.36",
        );
        assert!(!cubot.is_bot, "a phone, not a bot");

        let empty = parse("");
        assert_eq!(empty.browser, "UNKNOWN");
        assert!(empty.is_bot);
    }
}
