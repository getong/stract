use std::sync::Arc;

use optics::HostRankings;
use rand::seq::SliceRandom;
use stract::{
    bangs::Bangs,
    config::{
        defaults, ApiConfig, ApiThresholds, CollectorConfig, CorrectionConfig, SnippetConfig,
        WidgetsConfig,
    },
    generic_query::TopKeyPhrasesQuery,
    index::Index,
    searcher::{api::ApiSearcher, LocalSearchClient, LocalSearcher, SearchQuery},
    webgraph::Webgraph,
};
use tokio::sync::RwLock;

#[tokio::main]
pub async fn main() {
    let mut index = Index::open("data/index").unwrap();

    let collector_conf = CollectorConfig {
        ..Default::default()
    };

    let config = ApiConfig {
        host: "0.0.0.0:8000".parse().unwrap(),
        prometheus_host: "0.0.0.0:8001".parse().unwrap(),
        management_host: "0.0.0.0:8003".parse().unwrap(),
        crossencoder_model_path: None,
        lambda_model_path: None,
        dual_encoder_model_path: None,
        bangs_path: Some("data/bangs.json".to_string()),
        query_store_db: None,
        gossip_seed_nodes: None,
        gossip_addr: "0.0.0.0:8002".parse().unwrap(),
        collector: collector_conf.clone(),
        thresholds: ApiThresholds::default(),
        widgets: WidgetsConfig {
            thesaurus_paths: vec!["data/english-wordnet-2022-subset.ttl".to_string()],
            calculator_fetch_currencies_exchange: false,
        },
        spell_check: Some(stract::config::ApiSpellCheck {
            path: "data/web_spell".to_string(),
            correction_config: CorrectionConfig::default(),
        }),
        max_concurrent_searches: defaults::Api::max_concurrent_searches(),
        max_similar_hosts: defaults::Api::max_similar_hosts(),
        top_phrases_for_autosuggest: defaults::Api::top_phrases_for_autosuggest(),
    };

    index.inverted_index.set_snippet_config(SnippetConfig {
        num_words_for_lang_detection: Some(250),
        max_considered_words: Some(10_000),
        ..Default::default()
    });

    let searcher = LocalSearcher::builder(Arc::new(RwLock::new(index)))
        .set_collector_config(collector_conf)
        .build();

    let mut queries: Vec<String> = searcher
        .search_generic(TopKeyPhrasesQuery { top_n: 1_000_000 })
        .await
        .unwrap()
        .into_iter()
        .map(|phrase| phrase.text().to_string())
        .collect();
    queries.shuffle(&mut rand::thread_rng());

    let bangs = Bangs::from_path(config.bangs_path.as_ref().unwrap());

    let searcher = stract::searcher::LocalSearchClient::from(searcher);

    let webgraph = Webgraph::open("data/webgraph", 0u64.into()).unwrap();

    let searcher: ApiSearcher<LocalSearchClient, Webgraph> =
        ApiSearcher::new(searcher, None, bangs, config)
            .await
            .with_webgraph(webgraph);

    for query in queries {
        let mut desc = "search '".to_string();
        desc.push_str(&query);
        desc.push('\'');

        println!("{desc}");

        searcher
            .search(&SearchQuery {
                query: query.to_string(),
                host_rankings: Some(HostRankings {
                    liked: vec!["en.wikipedia.org".to_string()],
                    disliked: vec![],
                    blocked: vec![],
                }),
                ..Default::default()
            })
            .await
            .unwrap();
    }
}
