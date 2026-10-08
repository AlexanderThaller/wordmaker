//! The numbers behind the admin dashboard, read back from `page_views`.

use chrono::{
    Days,
    NaiveDate,
    Utc,
};
use clickhouse::Row;
use serde::{
    Deserialize,
    Serialize,
};

/// Entries in each top list.
const TOP: u32 = 10;

/// Page views of one site over the last `days` UTC days, today included.
#[derive(Debug, Serialize)]
pub struct Report {
    pub site: String,
    pub days: u32,
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// Successful page views by people.
    pub views: u64,
    /// Visitors summed over the days: a visitor ID only lasts a day, so
    /// one person on three days counts three times.
    pub visitor_days: u64,
    /// Page views by crawlers and other bots, not counted anywhere else.
    pub bot_views: u64,
    /// One entry per day, oldest first, days without views included.
    pub daily: Vec<Day>,
    pub pages: Vec<Count>,
    pub referrers: Vec<Count>,
    pub countries: Vec<Count>,
    pub browsers: Vec<Count>,
    pub systems: Vec<Count>,
    pub devices: Vec<Count>,
    /// Paths that answered 404, e.g. broken links.
    pub not_found: Vec<Count>,
    /// Bots by name, as far as their user agent tells.
    pub bots: Vec<Count>,
    /// Types of the `GeoIP` databases in use, e.g. `DBIP-City-Lite`, which
    /// decide the attribution their licenses ask for.
    pub geo_databases: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Row)]
pub struct Day {
    pub date: String,
    pub views: u64,
    pub visitors: u64,
}

#[derive(Debug, Serialize, Deserialize, Row)]
pub struct Count {
    pub name: String,
    pub views: u64,
    pub visitors: u64,
}

#[derive(Debug, Deserialize, Row)]
struct Totals {
    views: u64,
    bot_views: u64,
}

/// Rows of this site in the period; `?` are the site and the first day.
const PERIOD: &str = "site = ? AND timestamp >= toDateTime64(?, 3, 'UTC')";
/// Page views that count: by people, of pages that exist.
const HUMAN: &str = "NOT is_bot AND status = 200";
/// A bot's name from its user agent: the first word with `bot`, `crawler`
/// or the like in it (`GPTBot` in `…compatible; GPTBot/1.2…`), else what
/// the user agent parser called it. Read at query time, so it also names
/// bots recorded before this existed. `??` is a literal `?` to the client,
/// which reads a single one as a query argument.
const BOT_NAME: &str = "multiIf(user_agent = '', 'No user agent', (extract(user_agent, \
                        '(??i)([a-z0-9_-]*(??:bot|crawler|spider|slurp)[a-z0-9_-]*\
                        |headlesschrome|facebookexternalhit)') AS bot_name) != '', bot_name, \
                        browser IN ('', 'UNKNOWN', 'misc crawler'), 'Other', browser)";

pub async fn report(client: &clickhouse::Client, site: &str, days: u32) -> anyhow::Result<Report> {
    let to = Utc::now().date_naive();
    let from = to - Days::new(u64::from(days.max(1) - 1));
    let since = from.to_string();

    let totals = client
        .query(&format!(
            "SELECT countIf({HUMAN}) AS views, countIf(is_bot) AS bot_views FROM page_views WHERE \
             {PERIOD}"
        ))
        .bind(site)
        .bind(&since)
        .fetch_one::<Totals>();
    let daily = client
        .query(&format!(
            "SELECT toString(toDate(timestamp)) AS date, count() AS views, uniq(visitor_id) AS \
             visitors FROM page_views WHERE {PERIOD} AND {HUMAN} GROUP BY date ORDER BY date"
        ))
        .bind(site)
        .bind(&since)
        .fetch_all::<Day>();
    let referrer_host = url::Url::parse(site)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default();
    let top = |column: &str, filter: &str| {
        client
            .query(&format!(
                "SELECT {column} AS name, count() AS views, uniq(visitor_id) AS visitors FROM \
                 page_views WHERE {PERIOD} AND {filter} GROUP BY name ORDER BY views DESC, name \
                 LIMIT {TOP}"
            ))
            .bind(site)
            .bind(&since)
            .fetch_all::<Count>()
    };
    let external = format!(
        "{HUMAN} AND referrer_host != '' AND referrer_host != {}",
        quote(&referrer_host)
    );
    let (totals, daily, pages, referrers, countries, browsers, systems, devices, not_found, bots) =
        tokio::try_join!(
            totals,
            daily,
            top("path", HUMAN),
            top("referrer_host", &external),
            top("if(country = '', 'Unknown', country)", HUMAN),
            top("browser", HUMAN),
            top("os", HUMAN),
            top("device", HUMAN),
            top("path", "NOT is_bot AND status = 404"),
            top(BOT_NAME, "is_bot"),
        )?;

    Ok(Report {
        site: site.to_owned(),
        days,
        from,
        to,
        views: totals.views,
        visitor_days: daily.iter().map(|day| day.visitors).sum(),
        bot_views: totals.bot_views,
        daily: fill(from, to, daily),
        pages,
        referrers,
        countries,
        browsers,
        systems,
        devices,
        not_found,
        bots,
        geo_databases: Vec::new(),
    })
}

/// A string literal for `ClickHouse` SQL.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Every day from `from` to `to`, with zeros where nothing was recorded.
fn fill(from: NaiveDate, to: NaiveDate, recorded: Vec<Day>) -> Vec<Day> {
    let mut recorded = recorded.into_iter().peekable();
    from.iter_days()
        .take_while(|day| *day <= to)
        .map(|day| {
            let day = day.to_string();
            recorded
                .next_if(|recorded| recorded.date == day)
                .unwrap_or(Day {
                    date: day,
                    views: 0,
                    visitors: 0,
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_missing_days() {
        let day = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let filled = fill(
            day("2026-09-29"),
            day("2026-10-02"),
            vec![Day {
                date: "2026-09-30".into(),
                views: 5,
                visitors: 2,
            }],
        );
        let views: Vec<_> = filled.iter().map(|d| (d.date.as_str(), d.views)).collect();
        assert_eq!(
            views,
            [
                ("2026-09-29", 0),
                ("2026-09-30", 5),
                ("2026-10-01", 0),
                ("2026-10-02", 0)
            ]
        );
    }

    #[test]
    fn quotes_strings() {
        assert_eq!(quote("site.example"), "'site.example'");
        assert_eq!(quote(r"a'b\c"), r"'a\'b\\c'");
    }
}
