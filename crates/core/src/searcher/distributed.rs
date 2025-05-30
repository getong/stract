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

use crate::{
    distributed::{
        cluster::Cluster,
        member::{LiveIndexState, Service},
        sonic::{
            self,
            replication::{
                AllShardsSelector, RandomReplicaSelector, RemoteClient, ReplicatedClient,
                ReusableClientManager, ReusableShardedClient, Shard, ShardIdentifier,
                ShardedClient, SpecificShardSelector,
            },
        },
    },
    entrypoint::{
        entity_search_server, live_index::LiveIndexService, search_server::{self, RetrieveReq, SearchService}
    },
    generic_query::{self, Collector},
    inverted_index::{RetrievedWebpage, ShardId, WebpagePointer},
    ranking::pipeline::{PrecisionRankingWebpage, RecallRankingWebpage},
    Result,
};

use std::{collections::HashMap, sync::Arc};

use fnv::FnvHashMap;
use futures::{future::join_all, stream::FuturesUnordered, StreamExt};
use itertools::Itertools;
use std::future::Future;
use thiserror::Error;
use tokio::sync::Mutex;

use super::{InitialWebsiteResult, LocalSearcher, SearchQuery};

const CLIENT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to get search result")]
    SearchFailed,

    #[error("Query cannot be empty")]
    EmptyQuery,

    #[error("Webpage not found")]
    WebpageNotFound,
}

pub trait SearchClient {
    fn search_initial(
        &self,
        query: &SearchQuery,
    ) -> impl Future<Output = Vec<InitialSearchResultShard>> + Send;

    fn retrieve_webpages(
        &self,
        top_websites: &[ScoredWebpagePointer],
        query: &str,
    ) -> impl Future<Output = Vec<PrecisionRankingWebpage>> + Send;

    fn search_initial_generic<Q>(
        &self,
        query: Q,
    ) -> impl Future<Output = Result<<Q::Collector as generic_query::Collector>::Fruit>> + Send
    where
        Q: search_server::Query,
        
        Result<
            <Q::Collector as generic_query::Collector>::Fruit,
            search_server::EncodedError,
        >: From<<Q as sonic::service::Message<SearchService>>::Response>,
        <<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit:
            From<<Q::Collector as generic_query::Collector>::Fruit>;

    fn retrieve_generic<Q>(
        &self,
        query: Q,
        fruit: <Q::Collector as generic_query::Collector>::Fruit,
    ) -> impl Future<Output = Result<Vec<Q::IntermediateOutput>>> + Send
    where
        Q: search_server::Query,
        <Q::Collector as generic_query::Collector>::Fruit: Clone,
        Result<Q::IntermediateOutput, search_server::EncodedError>:
            From<
                <<Q as search_server::Query>::RetrieveReq as sonic::service::Message<
                    SearchService,
                >>::Response,
            >;

    fn search_generic<Q>(&self, query: Q) -> impl Future<Output = Result<Q::Output>> + Send
    where
        Self: Send + Sync,
        Q: search_server::Query,
        Result<
        <Q::Collector as generic_query::Collector>::Fruit,
        search_server::EncodedError,
    >: From<<Q as sonic::service::Message<SearchService>>::Response>,
    <<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit:
        From<<Q::Collector as generic_query::Collector>::Fruit>,
        <Q::Collector as generic_query::Collector>::Fruit: Clone,
        Result<Q::IntermediateOutput, search_server::EncodedError>:
            From<
                <<Q as search_server::Query>::RetrieveReq as sonic::service::Message<
                    SearchService,
                >>::Response,
    >
    {
        async {
            let fruit = self.search_initial_generic(query.clone()).await?;
            let res = self.retrieve_generic(query, fruit).await?;
            let output = Q::merge_results(res);
            Ok(output)
        }
    }

    fn batch_search_initial_generic<Q>(
        &self,
        queries: Vec<Q>,
    ) -> impl Future<Output = Result<Vec<<Q::Collector as generic_query::Collector>::Fruit>>> + Send
    where
        Q: search_server::Query,
        
        Result<<<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit, search_server::EncodedError>:
            From<<Q as sonic::service::Message<SearchService>>::Response>;

    fn batch_retrieve_generic<Q>(
        &self,
        queries: Vec<(Q, <Q::Collector as generic_query::Collector>::Fruit)>,
    ) -> impl Future<Output = Result<Vec<Vec<Q::IntermediateOutput>>>> + Send
    where
        Q: search_server::Query,
        Result<Q::IntermediateOutput, search_server::EncodedError>: From<
            <<Q as search_server::Query>::RetrieveReq as sonic::service::Message<
                SearchService,
            >>::Response,
        >,
        <Q::Collector as generic_query::Collector>::Fruit: Clone;

    fn batch_search_generic<Q>(&self, queries: Vec<Q>) -> impl Future<Output = Result<Vec<Q::Output>>> + Send
        where
            Q: search_server::Query,
            Self: Send + Sync,
            Result<<<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit, search_server::EncodedError>:
                From<<Q as sonic::service::Message<SearchService>>::Response>,
            Result<Q::IntermediateOutput, search_server::EncodedError>: From<
                <<Q as search_server::Query>::RetrieveReq as sonic::service::Message<
                    SearchService,
                >>::Response,
            >,
            <Q::Collector as generic_query::Collector>::Fruit: Clone
        {
            async {
                let res = self.batch_search_initial_generic(queries.clone()).await?;
                let res = self
                    .batch_retrieve_generic(queries.into_iter().zip(res).collect())
                    .await?;
                Ok(res.into_iter().map(|v| Q::merge_results(v)).collect())
            }
        }
}

#[derive(Clone, Debug)]
pub struct ScoredWebpagePointer {
    pub website: RecallRankingWebpage,
    pub shard: ShardId,
}

impl ScoredWebpagePointer {
    pub fn as_ranking(&self) -> &RecallRankingWebpage {
        &self.website
    }

    pub fn as_ranking_mut(&mut self) -> &mut RecallRankingWebpage {
        &mut self.website
    }
}

impl ShardIdentifier for ShardId {}

#[derive(Debug)]
pub struct InitialSearchResultShard {
    pub local_result: InitialWebsiteResult,
    pub shard: ShardId,
}

impl ReusableClientManager for SearchService {
    const CLIENT_REFRESH_INTERVAL: std::time::Duration = CLIENT_REFRESH_INTERVAL;

    type Service = SearchService;
    type ShardId = ShardId;

    async fn new_client(cluster: &Cluster) -> ShardedClient<Self::Service, Self::ShardId> {
        let mut shards = HashMap::new();
        for member in cluster.members().await {
            if let Service::Searcher { host, shard } = member.service {
                shards.entry(shard).or_insert_with(Vec::new).push(host);
            } else if let Service::LiveIndex { search_host, shard, state, .. } = member.service {
                if state == LiveIndexState::Ready {
                    shards.entry(shard).or_insert_with(Vec::new).push(search_host);
                }
            }
        }

        let mut shard_clients = Vec::new();

        for (id, replicas) in shards {
            let replicated =
                ReplicatedClient::new(replicas.into_iter().map(RemoteClient::new).collect());
            let shard = Shard::new(id, replicated);
            shard_clients.push(shard);
        }

        ShardedClient::new(shard_clients)
    }
}

impl ReusableClientManager for entity_search_server::SearchService {
    const CLIENT_REFRESH_INTERVAL: std::time::Duration = CLIENT_REFRESH_INTERVAL;

    type Service = entity_search_server::SearchService;
    type ShardId = ();

    async fn new_client(cluster: &Cluster) -> ShardedClient<Self::Service, Self::ShardId> {
        let mut replicas = Vec::new();
        for member in cluster.members().await {
            if let Service::EntitySearcher { host } = member.service {
                replicas.push(RemoteClient::new(host));
            }
        }

        let rep = ReplicatedClient::new(replicas);

        if !rep.is_empty() {
            ShardedClient::new(vec![Shard::new((), rep)])
        } else {
            ShardedClient::new(vec![])
        }
    }
}

impl ReusableClientManager for LiveIndexService {
    const CLIENT_REFRESH_INTERVAL: std::time::Duration = CLIENT_REFRESH_INTERVAL;

    type Service = LiveIndexService;

    type ShardId = ShardId;

    async fn new_client(cluster: &Cluster) -> ShardedClient<Self::Service, Self::ShardId> {
        let mut shards = HashMap::new();
        for member in cluster.members().await {
            if let Service::LiveIndex { host, shard, state, .. } = member.service {
                if state == LiveIndexState::Ready {
                    shards.entry(shard).or_insert_with(Vec::new).push(host);
                }
            }
        }

        let mut shard_clients = Vec::new();

        for (id, replicas) in shards {
            let replicated =
                ReplicatedClient::new(replicas.into_iter().map(RemoteClient::new).collect());
            let shard = Shard::new(id, replicated);
            shard_clients.push(shard);
        }

        ShardedClient::new(shard_clients)
    }
}

/// A searcher that runs the search on a remote cluster.
pub struct DistributedSearcher {
    client: Mutex<ReusableShardedClient<SearchService>>,
}

impl DistributedSearcher {
    pub async fn new(cluster: Arc<Cluster>) -> Self {
        Self::from_client(ReusableShardedClient::new(cluster.clone()).await)
    }

    pub fn from_client(client: ReusableShardedClient<SearchService>) -> Self {
        Self {
            client: Mutex::new(client),
        }
    }

    async fn conn(&self) -> Arc<ShardedClient<SearchService, ShardId>> {
        self.client.lock().await.conn().await
    }

    async fn retrieve_webpages_from_shard(
        &self,
        shard: ShardId,
        client: &ShardedClient<SearchService, ShardId>,
        query: &str,
        pointers: Vec<(usize, WebpagePointer)>,
    ) -> Vec<(usize, RetrievedWebpage)> {
        let (idxs, pointers): (Vec<usize>, Vec<WebpagePointer>) = pointers.into_iter().unzip();

        match client
            .send(
                search_server::RetrieveWebsites {
                    websites: pointers,
                    query: query.to_string(),
                },
                &SpecificShardSelector(shard),
                &RandomReplicaSelector,
            )
            .await
        {
            Ok(v) => v
                .into_iter()
                .flatten()
                .flat_map(|(_, v)| v.into_iter().map(|(_, v)| v))
                .flatten()
                .flatten()
                .zip_eq(idxs)
                .map(|(v, i)| (i, v))
                .collect(),
            _ => vec![],
        }
    }
}

impl SearchClient for DistributedSearcher {
    async fn search_initial(&self, query: &SearchQuery) -> Vec<InitialSearchResultShard> {
        let client = self.conn().await;
        let mut results = Vec::new();

        if let Ok(res) = client
            .send(
                search_server::Search {
                    query: query.clone(),
                },
                &AllShardsSelector,
                &RandomReplicaSelector,
            )
            .await
        {
            for (shard_id, mut res) in res.into_iter().flatten() {
                if let Some((_, Some(res))) = res.pop() {
                    results.push(InitialSearchResultShard {
                        local_result: res,
                        shard: shard_id,
                    });
                }
            }
        }

        results
    }

    async fn retrieve_webpages(
        &self,
        top_websites: &[ScoredWebpagePointer],
        query: &str,
    ) -> Vec<PrecisionRankingWebpage> {
        let mut rankings = FnvHashMap::default();
        let mut pointers: HashMap<_, Vec<_>> = HashMap::new();

        for (i, pointer) in top_websites.iter().enumerate() {
            pointers
                .entry(pointer.shard)
                .or_default()
                .push((i, pointer.website.pointer().clone()));

            rankings.insert(i, pointer.website.clone());
        }

        let client = self.conn().await;
        let mut futures = Vec::new();
        for (shard, pointers) in pointers {
            futures.push(self.retrieve_webpages_from_shard(shard, &client, query, pointers));
        }

        let mut retrieved_webpages = Vec::new();
        for pages in join_all(futures).await {
            for (i, page) in pages {
                retrieved_webpages
                    .push((i, PrecisionRankingWebpage::new(page, rankings[&i].clone())));
            }
        }

        debug_assert_eq!(retrieved_webpages.len(), top_websites.len());

        retrieved_webpages.sort_by(|(a, _), (b, _)| a.cmp(b));
        retrieved_webpages.into_iter().map(|(_, v)| v).collect()
    }

    async fn search_initial_generic<Q>(
        &self,
        query: Q,
    ) -> Result<<Q::Collector as generic_query::Collector>::Fruit>
    where
        Q: search_server::Query,
        Result<
            <Q::Collector as generic_query::Collector>::Fruit,
            search_server::EncodedError,
        >: From<<Q as sonic::service::Message<SearchService>>::Response>,
        <<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit:
            From<<Q::Collector as generic_query::Collector>::Fruit> {
        let collector = query.coordinator_collector();

        let res = self.conn().await
            .send(query, &AllShardsSelector, &RandomReplicaSelector)
            .await?;

        let fruits: Vec<<<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit> = res
            .into_iter()
            .flatten()
            .flat_map(|(_, reps)| reps)
            .filter_map(|(_, rep)| {
                Result::<
                    <Q::Collector as generic_query::Collector>::Fruit,
                    search_server::EncodedError,
                >::from(rep)
                .ok()
            })
            .map(|fruit| {
                <<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit::from(fruit)
            })
            .collect();

        collector
            .merge_fruits(fruits)
            .map_err(|_| anyhow::anyhow!("failed to merge fruits"))
    }
    
    async fn retrieve_generic<Q>(
        &self,
        query: Q,
        fruit: <Q::Collector as generic_query::Collector>::Fruit,
    ) -> Result<Vec<Q::IntermediateOutput>>
    where
        Q: search_server::Query,
        <Q::Collector as generic_query::Collector>::Fruit: Clone,
        Result<Q::IntermediateOutput, search_server::EncodedError>:
            From<
                <<Q as search_server::Query>::RetrieveReq as sonic::service::Message<
                    SearchService,
                >>::Response,
            >
    {
        let conn = self.conn().await;
        let mut results = FuturesUnordered::new();
        for shard in conn.shards() {
            let fruit = query.filter_fruit_shards(*shard.id(), fruit.clone());
            let req = Q::RetrieveReq::new(query.clone(), fruit);
            results.push(shard.replicas().send(req, &RandomReplicaSelector));
        }
        let mut res = Vec::new();

        while let Some(shard_res) = results.next().await {
            if let Ok(shard_res) = shard_res {
                res.push(shard_res);
            }
        }

        Ok(res
            .into_iter()
            .flatten()
            .filter_map(|(_, res)| {
                Result::<Q::IntermediateOutput, search_server::EncodedError>::from(res).ok()
            })
            .collect())
    }
    
    async fn batch_search_initial_generic<Q>(
        &self,
        queries: Vec<Q>,
    ) -> Result<Vec<<Q::Collector as generic_query::Collector>::Fruit>>
    where
        Q: search_server::Query,
        Result<<<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit, search_server::EncodedError>:
            From<<Q as sonic::service::Message<SearchService>>::Response>,
        {
            let res = self
                .conn()
                .await
                .batch_send(&queries, &AllShardsSelector, &RandomReplicaSelector)
                .await?;
    
            let mut fruits = Vec::with_capacity(queries.len());
    
            for _ in 0..queries.len() {
                fruits.push(Vec::new());
            }
    
            for (_, replica_results) in res.into_iter() {
                debug_assert_eq!(replica_results.len(), 1);
    
                for (_, shard_results) in replica_results.into_iter() {
                    for (i, shard_result) in shard_results.into_iter().enumerate() {
                        if let Ok(shard_result) =
                            Result::<_, search_server::EncodedError>::from(shard_result)
                        {
                            fruits[i].push(shard_result);
                        }
                    }
                }
            }
    
            queries
                .iter()
                .zip_eq(fruits.into_iter())
                .map(|(query, shard_fruits)| query.coordinator_collector().merge_fruits(shard_fruits))
                .collect::<Result<Vec<_>, _>>()
    }
    
    async fn batch_retrieve_generic<Q>(
        &self,
        queries: Vec<(Q, <Q::Collector as generic_query::Collector>::Fruit)>,
    ) -> Result<Vec<Vec<Q::IntermediateOutput>>>
    where
        Q: search_server::Query,
        Result<Q::IntermediateOutput, search_server::EncodedError>: From<
            <<Q as search_server::Query>::RetrieveReq as sonic::service::Message<
                SearchService,
            >>::Response,
        >,
        <Q::Collector as generic_query::Collector>::Fruit: Clone,
    {
        let conn = self.conn().await;
        let mut results = FuturesUnordered::new();

        for shard in conn.shards() {
            let retrieve_requests: Vec<_> = queries
                .iter()
                .map(|(query, fruit)| {
                    let fruit = query.filter_fruit_shards(*shard.id(), fruit.clone());
                    Q::RetrieveReq::new(query.clone(), fruit)
                })
                .collect();

            results.push(async move {
                let retrieve_requests = retrieve_requests; // move lifetime
                shard
                    .replicas()
                    .batch_send(&retrieve_requests, &RandomReplicaSelector)
                    .await
            });
        }

        let mut res = Vec::new();

        for _ in 0..queries.len() {
            res.push(Vec::new());
        }

        while let Some(shard_res) = results.next().await {
            for (_, shard_res) in shard_res? {
                assert_eq!(shard_res.len(), queries.len());

                for (i, query_res) in shard_res.into_iter().enumerate() {
                    res[i].push(
                        <Result<Q::IntermediateOutput, search_server::EncodedError>>::from(
                            query_res,
                        )
                        .map_err(|e| anyhow::anyhow!("{e}"))?,
                    );
                }
            }
        }

        Ok(res)
    }

    
}

/// This should only be used for testing and benchmarks.
pub struct LocalSearchClient(LocalSearcher);
impl From<LocalSearcher> for LocalSearchClient {
    fn from(searcher: LocalSearcher) -> Self {
        Self(searcher)
    }
}

impl SearchClient for LocalSearchClient {
    async fn search_initial(&self, query: &SearchQuery) -> Vec<InitialSearchResultShard> {
        let res = self.0.search_initial(query, true).await.unwrap();

        vec![InitialSearchResultShard {
            local_result: res,
            shard: ShardId::Backbone(0),
        }]
    }

    async fn retrieve_webpages(
        &self,
        top_websites: &[ScoredWebpagePointer],
        query: &str,
    ) -> Vec<PrecisionRankingWebpage> {
        let pointers = top_websites
            .iter()
            .map(|p| p.website.pointer().clone())
            .collect::<Vec<_>>();

        let res = self
            .0
            .retrieve_websites(&pointers, query)
            .await
            .unwrap()
            .into_iter()
            .zip(top_websites.iter().map(|p| p.website.clone()))
            .map(|(ret, ran)| PrecisionRankingWebpage::new(ret, ran))
            .collect::<Vec<_>>();

        res
    }

    async fn search_initial_generic<Q>(
        &self,
        query: Q,
    ) -> Result<<Q::Collector as generic_query::Collector>::Fruit>
    where
        Q: search_server::Query + 'static,
        Result<
            <Q::Collector as generic_query::Collector>::Fruit,
            search_server::EncodedError,
        >: From<<Q as sonic::service::Message<SearchService>>::Response>,
        <<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit:
            From<<Q::Collector as generic_query::Collector>::Fruit> {
        self.0.search_initial_generic(query).await
    }
    
    async fn retrieve_generic<Q>(
        &self,
        query: Q,
        fruit: <Q::Collector as generic_query::Collector>::Fruit,
    ) -> Result<Vec<Q::IntermediateOutput>>
    where
        Q: search_server::Query,
        <Q::Collector as generic_query::Collector>::Fruit: Clone,
        Result<Q::IntermediateOutput, search_server::EncodedError>:
            From<
                <<Q as search_server::Query>::RetrieveReq as sonic::service::Message<
                    SearchService,
                >>::Response,
            > {
        Ok(vec![self.0.retrieve_generic(query, fruit).await?])
    }
    
    async fn batch_search_initial_generic<Q>(
        &self,
        queries: Vec<Q>,
    ) -> Result<Vec<<Q::Collector as generic_query::Collector>::Fruit>>
    where
        Q: search_server::Query,
        Result<<<Q::Collector as generic_query::Collector>::Child as tantivy::collector::SegmentCollector>::Fruit, search_server::EncodedError>:
            From<<Q as sonic::service::Message<SearchService>>::Response> {
        let mut res = Vec::new();

        for query in queries {
            res.push(self.0.search_initial_generic(query).await?);
        }

        Ok(res)
    }
    
    async fn batch_retrieve_generic<Q>(
        &self,
        queries: Vec<(Q, <Q::Collector as generic_query::Collector>::Fruit)>,
    ) -> Result<Vec<Vec<Q::IntermediateOutput>>>
    where
        Q: search_server::Query,
        Result<Q::IntermediateOutput, search_server::EncodedError>: From<
            <<Q as search_server::Query>::RetrieveReq as sonic::service::Message<
                SearchService,
            >>::Response,
        >,
        <Q::Collector as generic_query::Collector>::Fruit: Clone,
       {
        let mut res = Vec::new();

        for (query, fruit) in queries {
            res.push(vec![self.0.retrieve_generic(query, fruit).await?]);
        }

        Ok(res)
    }
}
