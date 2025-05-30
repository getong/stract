// Stract is an open source web search engine.
// Copyright (C) 2024 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::{
    ops::Deref,
    panic,
    sync::{Arc, Mutex},
    time::Duration,
};

use dashmap::DashMap;
use url::Url;

use crate::{config::CrawlerConfig, crawler};

use super::{encoded_body, Site};

const RETRY_ROBOTSTXT_UNREACHABLE: bool = false;

#[derive(Debug)]
enum Lookup<T> {
    Found(T),
    /// 404
    Unavailable,
    /// 5xx (and other network errors)
    Unreachable,
}

struct CheckedRobotsTxt {
    robots: Lookup<robotstxt::Robots>,
    last_check: std::time::Instant,
}

impl CheckedRobotsTxt {
    fn new(robots: Lookup<robotstxt::Robots>) -> Self {
        Self {
            robots,
            last_check: std::time::Instant::now(),
        }
    }
}

impl CheckedRobotsTxt {
    fn is_expired(&self, expiration: &Duration) -> bool {
        match self.robots {
            Lookup::Found(_) | Lookup::Unavailable => self.last_check.elapsed() >= *expiration,
            Lookup::Unreachable => false,
        }
    }
}

struct InnerRobotsTxtManager {
    cache: DashMap<Site, CheckedRobotsTxt>,
    last_prune: Arc<Mutex<std::time::Instant>>,
    client: reqwest::Client,
    cache_expiration: Duration,
    user_agent: String,
    min_crawl_delay: Duration,
    max_crawl_delay: Duration,
}

impl InnerRobotsTxtManager {
    fn new(config: &CrawlerConfig) -> Self {
        let client = crawler::robot_client::reqwest_client(config).unwrap();
        let cache_expiration = Duration::from_secs(config.robots_txt_cache_sec);
        let user_agent = config.user_agent.token.clone();
        let min_crawl_delay = Duration::from_millis(config.min_crawl_delay_ms);
        let max_crawl_delay = Duration::from_millis(config.max_crawl_delay_ms);

        Self {
            client,
            cache_expiration,
            last_prune: Arc::new(Mutex::new(std::time::Instant::now())),
            cache: DashMap::new(),
            user_agent: user_agent.to_string(),
            min_crawl_delay,
            max_crawl_delay,
        }
    }

    async fn is_allowed(&self, url: &Url) -> bool {
        match &self.get(url).await.robots {
            Lookup::Found(robots_txt) => robots_txt.is_allowed(url),
            Lookup::Unavailable => true,
            Lookup::Unreachable => false,
        }
    }

    async fn crawl_delay(&self, url: &Url) -> Option<Duration> {
        match &self.get(url).await.robots {
            Lookup::Found(robots_txt) => robots_txt.crawl_delay(),
            Lookup::Unavailable | Lookup::Unreachable => None,
        }
    }

    async fn sitemaps(&self, url: &Url) -> Vec<Url> {
        match &self.get(url).await.robots {
            Lookup::Found(robots_txt) => robots_txt
                .sitemaps()
                .iter()
                .filter_map(|s| Url::parse(s).ok())
                .collect(),
            Lookup::Unavailable | Lookup::Unreachable => vec![],
        }
    }

    async fn fetch_robots_txt_from_url(&self, url: &str) -> Lookup<robotstxt::Robots> {
        let res = match super::robot_client::RequestBuilder::new(self.client.get(url))
            .timeout(Duration::from_secs(60))
            .send()
            .await
        {
            Ok(res) => {
                if res.status() != reqwest::StatusCode::OK {
                    match res.status() {
                        reqwest::StatusCode::NOT_FOUND => return Lookup::Unavailable,
                        _ => return Lookup::Unreachable,
                    }
                }

                let body = match encoded_body(res).await {
                    Ok(body) => body,
                    Err(_) => return Lookup::Unreachable,
                };

                let self_user_agent = self.user_agent.clone();
                match panic::catch_unwind(|| robotstxt::Robots::parse(&self_user_agent, &body)) {
                    Ok(Ok(r)) => Lookup::Found(r),
                    _ => Lookup::Unreachable,
                }
            }
            Err(_) => Lookup::Unreachable,
        };

        tokio::time::sleep(self.min_crawl_delay).await;

        res
    }

    async fn fetch_robots_txt_without_retry(&self, site: &Site) -> Lookup<robotstxt::Robots> {
        match self
            .fetch_robots_txt_from_url(&format!("https://{}/robots.txt", site.0))
            .await
        {
            Lookup::Unreachable => {
                self.fetch_robots_txt_from_url(&format!("http://{}/robots.txt", site.0))
                    .await
            }
            Lookup::Unavailable => {
                match self
                    .fetch_robots_txt_from_url(&format!("http://{}/robots.txt", site.0))
                    .await
                {
                    Lookup::Found(robots_txt) => Lookup::Found(robots_txt),
                    Lookup::Unreachable => Lookup::Unavailable,
                    Lookup::Unavailable => Lookup::Unavailable,
                }
            }
            Lookup::Found(robots_txt) => Lookup::Found(robots_txt),
        }
    }

    async fn fetch_robots_txt_with_retry(&self, site: &Site) -> Lookup<robotstxt::Robots> {
        for _ in 0..3 {
            match self.fetch_robots_txt_without_retry(site).await {
                Lookup::Found(robots_txt) => return Lookup::Found(robots_txt),
                Lookup::Unavailable => return Lookup::Unavailable,
                Lookup::Unreachable => {}
            }

            tokio::time::sleep(self.max_crawl_delay).await;
        }

        Lookup::Unreachable
    }

    async fn fetch_robots_txt(&self, site: &Site) -> CheckedRobotsTxt {
        if RETRY_ROBOTSTXT_UNREACHABLE {
            CheckedRobotsTxt::new(self.fetch_robots_txt_with_retry(site).await)
        } else {
            CheckedRobotsTxt::new(self.fetch_robots_txt_without_retry(site).await)
        }
    }

    fn maybe_prune(&self) {
        if self.last_prune.lock().unwrap().elapsed() < Duration::from_secs(60) {
            return;
        }

        self.cache
            .retain(|_, v| !v.is_expired(&self.cache_expiration));

        *self.last_prune.lock().unwrap() = std::time::Instant::now();
    }

    async fn get(&self, url: &Url) -> impl Deref<Target = CheckedRobotsTxt> + '_ {
        self.maybe_prune();
        let site = Site(url.host_str().unwrap_or_default().to_string());

        let cache_should_update = self
            .cache
            .get_mut(&site)
            .map(|v| v.is_expired(&self.cache_expiration))
            .unwrap_or(true);

        if cache_should_update {
            self.cache
                .insert(site.clone(), self.fetch_robots_txt(&site).await);
        }

        self.cache.get(&site).unwrap()
    }

    #[cfg(test)]
    fn insert(&self, site: String, robots_txt: robotstxt::Robots) {
        self.cache
            .insert(Site(site), CheckedRobotsTxt::new(Lookup::Found(robots_txt)));
    }
}
pub struct RobotsTxtManager {
    inner: Arc<InnerRobotsTxtManager>,
}

impl Clone for RobotsTxtManager {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl RobotsTxtManager {
    pub fn new(config: &CrawlerConfig) -> Self {
        Self {
            inner: Arc::new(InnerRobotsTxtManager::new(config)),
        }
    }
}

impl RobotsTxtManager {
    pub async fn is_allowed(&self, url: &Url) -> bool {
        self.inner.is_allowed(url).await
    }

    pub async fn crawl_delay(&self, url: &Url) -> Option<Duration> {
        self.inner.crawl_delay(url).await
    }

    pub async fn sitemaps(&self, url: &Url) -> Vec<Url> {
        self.inner.sitemaps(url).await
    }

    #[cfg(test)]
    pub fn insert(&self, site: String, robots_txt: robotstxt::Robots) {
        self.inner.insert(site, robots_txt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type RobotsTxt = robotstxt::Robots;

    #[test]
    fn simple() {
        let ua_token = "StractBot";
        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-agent: StractBot
            Disallow: /test"#,
        )
        .unwrap();

        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test").unwrap()));
        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/example").unwrap()));
    }

    #[test]
    fn lowercase() {
        let ua_token = "StractBot";
        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-agent: stractbot
            Disallow: /test"#,
        )
        .unwrap();

        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test").unwrap()));
        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/example").unwrap()));
    }

    #[test]
    fn test_extra_newline() {
        let ua_token = "StractBot";
        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-agent: StractBot


            Disallow: /test"#,
        )
        .unwrap();

        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test").unwrap()));
        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/example").unwrap()));
    }

    #[test]
    fn test_multiple_agents() {
        let ua_token = "StractBot";

        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-Agent: GoogleBot
User-Agent: StractBot
Disallow: /

User-Agent: *
Allow: /"#,
        )
        .unwrap();

        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test").unwrap()));

        let ua_token = "StractBot";

        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-Agent: GoogleBot, StractBot
Disallow: /

User-Agent: *
Allow: /"#,
        )
        .unwrap();

        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test").unwrap()));
    }

    #[test]
    fn test_sitemap() {
        let ua_token = "StractBot";
        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-agent: *
Disallow: /test

Sitemap: http://example.com/sitemap.xml"#,
        )
        .unwrap();

        assert_eq!(robots_txt.sitemaps(), &["http://example.com/sitemap.xml"]);

        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-agent: *
Disallow: /test

SiTeMaP: http://example.com/sitemap.xml"#,
        )
        .unwrap();

        assert_eq!(robots_txt.sitemaps(), &["http://example.com/sitemap.xml"]);
    }

    #[test]
    fn wildcard() {
        let ua_token = "StractBot";

        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-agent: StractBot
Disallow: /test/*
"#,
        )
        .unwrap();

        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test/").unwrap()));
        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test/foo").unwrap()));
        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test/foo/bar").unwrap()));
        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/test").unwrap()));
        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/testfoo").unwrap()));

        let robots_txt = RobotsTxt::parse(
            ua_token,
            r#"User-agent: StractBot
    Disallow: /test/*/bar
    "#,
        )
        .unwrap();

        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/test/").unwrap()));
        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/test/foo").unwrap()));
        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test/foo/bar").unwrap()));
        assert!(!robots_txt.is_allowed(&Url::parse("http://example.com/test/foo/baz/bar").unwrap()));
        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/test").unwrap()));
        assert!(robots_txt.is_allowed(&Url::parse("http://example.com/testfoo").unwrap()));
    }

    #[test]
    fn test_unreachable_robots_never_updated() {
        let checked = CheckedRobotsTxt::new(Lookup::Unreachable);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!checked.is_expired(&Duration::from_millis(10)));
    }
}
