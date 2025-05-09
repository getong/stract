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

use std::marker::PhantomData;

use anyhow::anyhow;
use itertools::Itertools;
use tantivy::{
    collector::{SegmentCollector, TopNComputer},
    columnar::Column,
    DocId, SegmentOrdinal,
};

use crate::webgraph::{
    doc_address::DocAddress,
    query::{
        document_scorer::{DefaultDocumentScorer, DocumentScorer},
        ColumnFieldFilter, SegmentColumnFieldFilter,
    },
    schema::{Field, FieldEnum},
    EdgeLimit,
};
use crate::{distributed::member::ShardId, webgraph::warmed_column_fields::WarmedColumnFields};

use super::Collector;

/// Buffer to ensure remote has enough docs for deduplication
pub const DEDUPLICATION_BUFFER: usize = 128;

pub trait DeduplicatorDoc
where
    Self: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Ord + Clone,
{
    fn new<S, D>(collector: &TopDocsSegmentCollector<S, D>, doc: DocId) -> Self
    where
        S: DocumentScorer + 'static,
        D: Deduplicator + 'static;
}

pub trait Deduplicator: Clone + Send + Sync {
    type Doc: DeduplicatorDoc;

    fn deduplicate(&self, docs: Vec<(u64, Self::Doc)>) -> Vec<(u64, Self::Doc)>;
}

impl DeduplicatorDoc for DocAddress {
    fn new<S, D>(collector: &TopDocsSegmentCollector<S, D>, doc: DocId) -> Self
    where
        S: DocumentScorer + 'static,
        D: Deduplicator + 'static,
    {
        DocAddress::new(collector.shard_id, collector.segment_ord, doc)
    }
}

#[derive(Clone)]
pub struct NoDeduplicator;

impl Deduplicator for NoDeduplicator {
    type Doc = DocAddress;

    fn deduplicate(&self, docs: Vec<(u64, Self::Doc)>) -> Vec<(u64, Self::Doc)> {
        docs
    }
}

#[derive(
    Clone,
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
)]
pub struct DocAddressWithHost {
    pub address: DocAddress,
    pub host: u128,
}

impl DeduplicatorDoc for DocAddressWithHost {
    fn new<S, D>(collector: &TopDocsSegmentCollector<S, D>, doc: DocId) -> Self
    where
        S: DocumentScorer + 'static,
        D: Deduplicator + 'static,
    {
        let host = collector
            .host_column
            .as_ref()
            .and_then(|col| col.first(doc))
            .ok_or_else(|| anyhow!("ColumnFields must be set to use HostDeduplicator"))
            .unwrap();
        Self {
            address: DocAddress::new(collector.shard_id, collector.segment_ord, doc),
            host,
        }
    }
}

#[derive(Clone)]
pub struct HostDeduplicator;

impl Deduplicator for HostDeduplicator {
    type Doc = DocAddressWithHost;

    fn deduplicate(&self, docs: Vec<(u64, Self::Doc)>) -> Vec<(u64, Self::Doc)> {
        docs.into_iter().unique_by(|(_, doc)| doc.host).collect()
    }
}

pub struct ColumnFields {
    warmed_column_fields: WarmedColumnFields,
    host_field: Option<FieldEnum>,
}

impl ColumnFields {
    pub fn new(warmed_column_fields: WarmedColumnFields) -> Self {
        Self {
            warmed_column_fields,
            host_field: None,
        }
    }

    pub fn with_host_field<F: Field>(self, host_field: F) -> Self {
        Self {
            warmed_column_fields: self.warmed_column_fields,
            host_field: Some(host_field.into()),
        }
    }
}

pub struct TopDocsCollector<S = DefaultDocumentScorer, D = NoDeduplicator> {
    shard_id: Option<ShardId>,
    limit: Option<usize>,
    offset: Option<usize>,
    perform_offset: bool,
    deduplicator: D,
    column_fields: Option<ColumnFields>,
    filter: Option<Box<dyn ColumnFieldFilter>>,
    _phantom: PhantomData<S>,
}

impl<S> From<EdgeLimit> for TopDocsCollector<S, NoDeduplicator> {
    fn from(limit: EdgeLimit) -> Self {
        let mut collector = TopDocsCollector::new().disable_offset();

        match limit {
            EdgeLimit::Unlimited => {}
            EdgeLimit::Limit(limit) => collector = collector.with_limit(limit),
            EdgeLimit::LimitAndOffset { limit, offset } => {
                collector = collector.with_limit(limit);
                collector = collector.with_offset(offset);
            }
        }

        collector
    }
}

impl<S> Default for TopDocsCollector<S, NoDeduplicator> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> TopDocsCollector<S, NoDeduplicator> {
    pub fn new() -> Self {
        Self {
            shard_id: None,
            limit: None,
            offset: None,
            perform_offset: true,
            deduplicator: NoDeduplicator,
            column_fields: None,
            filter: None,
            _phantom: PhantomData,
        }
    }
}

impl<S, D> TopDocsCollector<S, D> {
    #[must_use]
    pub fn with_shard_id(self, shard_id: ShardId) -> Self {
        Self {
            shard_id: Some(shard_id),
            ..self
        }
    }

    #[must_use]
    pub fn with_offset(self, offset: usize) -> Self {
        Self {
            offset: Some(offset),
            ..self
        }
    }

    #[must_use]
    pub fn with_limit(self, limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..self
        }
    }

    #[must_use]
    pub fn enable_offset(self) -> Self {
        Self {
            perform_offset: true,
            ..self
        }
    }

    #[must_use]
    pub fn disable_offset(self) -> Self {
        Self {
            perform_offset: false,
            ..self
        }
    }

    #[must_use]
    pub fn with_deduplicator<D2: Deduplicator>(self, deduplicator: D2) -> TopDocsCollector<S, D2> {
        TopDocsCollector {
            deduplicator,
            shard_id: self.shard_id,
            limit: self.limit,
            offset: self.offset,
            perform_offset: self.perform_offset,
            column_fields: self.column_fields,
            filter: self.filter,
            _phantom: PhantomData,
        }
    }

    #[must_use]
    pub fn with_column_fields(self, warmed_column_fields: WarmedColumnFields) -> Self {
        Self {
            column_fields: Some(ColumnFields::new(warmed_column_fields)),
            ..self
        }
    }

    #[must_use]
    pub fn with_host_field<F: Field>(self, host_field: F) -> Self {
        Self {
            column_fields: Some(self.column_fields.unwrap().with_host_field(host_field)),
            ..self
        }
    }

    #[must_use]
    pub fn with_filter(self, filter: Box<dyn ColumnFieldFilter>) -> Self {
        Self {
            filter: Some(filter),
            ..self
        }
    }
}

impl<S, D> TopDocsCollector<S, D>
where
    D: Deduplicator,
{
    fn computer(&self) -> Computer<D> {
        match (self.offset, self.limit) {
            (Some(offset), Some(limit)) => {
                Computer::TopN(TopNComputer::new(limit + offset + DEDUPLICATION_BUFFER))
            }
            (Some(_), None) => Computer::All(AllComputer::new()),
            (None, Some(limit)) => Computer::TopN(TopNComputer::new(limit + DEDUPLICATION_BUFFER)),
            (None, None) => Computer::All(AllComputer::new()),
        }
    }
}

impl<S: DocumentScorer + 'static, D: Deduplicator + 'static> Collector for TopDocsCollector<S, D> {
    type Fruit = Vec<(u64, <D as Deduplicator>::Doc)>;

    type Child = TopDocsSegmentCollector<S, D>;

    fn for_segment(
        &self,
        segment_ord: SegmentOrdinal,
        segment: &tantivy::SegmentReader,
    ) -> crate::Result<Self::Child> {
        let column_fields = self.column_fields.as_ref().unwrap();

        let scorer = S::for_segment(segment, &column_fields.warmed_column_fields)?;

        let segment_id = segment.segment_id();
        let segment_column_fields = column_fields.warmed_column_fields.segment(&segment_id);

        Ok(TopDocsSegmentCollector {
            shard_id: self.shard_id.unwrap(),
            computer: self.computer(),
            segment_ord,
            scorer,
            host_column: column_fields
                .host_field
                .map(|host_field| segment_column_fields.u128_by_enum(host_field).unwrap()),
            filter: self
                .filter
                .as_ref()
                .map(|f| f.for_segment(segment_column_fields)),
            _deduplicator: PhantomData,
        })
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<<Self::Child as tantivy::collector::SegmentCollector>::Fruit>,
    ) -> crate::Result<Self::Fruit> {
        let mut computer = self.computer();

        let before_deduplication: Vec<_> = segment_fruits.into_iter().flatten().collect();
        let deduplicated = self.deduplicator.deduplicate(before_deduplication);

        for (rank, doc) in deduplicated {
            computer.push(rank, doc);
        }

        let result = computer.harvest();

        if self.perform_offset {
            Ok(result
                .into_iter()
                .skip(self.offset.unwrap_or(0))
                .take(self.limit.unwrap_or(usize::MAX))
                .collect())
        } else {
            Ok(result
                .into_iter()
                .take(
                    // offset is only performed on remote. take buffer to ensure remote has enough docs for deduplication
                    self.limit
                        .unwrap_or(usize::MAX)
                        .saturating_add(DEDUPLICATION_BUFFER),
                )
                .collect())
        }
    }
}

enum Computer<D: Deduplicator> {
    TopN(TopNComputer<u64, <D as Deduplicator>::Doc, false>),
    All(AllComputer<D>),
}

impl<D: Deduplicator> Computer<D> {
    fn push(&mut self, rank: u64, doc: <D as Deduplicator>::Doc) {
        match self {
            Computer::TopN(computer) => computer.push(rank, doc),
            Computer::All(computer) => computer.push(rank, doc),
        }
    }

    fn harvest(self) -> Vec<(u64, <D as Deduplicator>::Doc)> {
        match self {
            Computer::TopN(computer) => computer
                .into_sorted_vec()
                .into_iter()
                .map(|comparable_doc| (comparable_doc.feature, comparable_doc.doc))
                .collect(),
            Computer::All(computer) => computer.harvest(),
        }
    }
}

struct AllComputer<D: Deduplicator> {
    docs: Vec<(u64, <D as Deduplicator>::Doc)>,
}

impl<D: Deduplicator> AllComputer<D> {
    fn new() -> Self {
        Self { docs: Vec::new() }
    }

    fn push(&mut self, rank: u64, doc: <D as Deduplicator>::Doc) {
        self.docs.push((rank, doc));
    }

    fn harvest(self) -> Vec<(u64, <D as Deduplicator>::Doc)> {
        let mut docs = self.docs;
        docs.sort_by(|(rank1, _), (rank2, _)| rank1.cmp(rank2));
        docs
    }
}

pub struct TopDocsSegmentCollector<S: DocumentScorer, D: Deduplicator> {
    shard_id: ShardId,
    computer: Computer<D>,
    segment_ord: SegmentOrdinal,
    scorer: S,
    host_column: Option<Column<u128>>,
    filter: Option<Box<dyn SegmentColumnFieldFilter>>,
    _deduplicator: PhantomData<D>,
}

impl<S: DocumentScorer + 'static, D: Deduplicator + 'static> SegmentCollector
    for TopDocsSegmentCollector<S, D>
{
    type Fruit = Vec<(u64, <D as Deduplicator>::Doc)>;

    fn collect(&mut self, doc: DocId, _: tantivy::Score) {
        if doc == tantivy::TERMINATED {
            return;
        }

        if let Some(filter) = self.filter.as_ref() {
            if filter.should_skip(doc) {
                return;
            }
        }

        let rank = self.scorer.rank(doc);
        self.computer
            .push(rank, <D::Doc as DeduplicatorDoc>::new(self, doc));
    }

    fn harvest(self) -> Self::Fruit {
        self.computer.harvest()
    }
}

#[cfg(test)]
mod tests {
    use crate::webgraph::{
        query::{BacklinksQuery, HostBacklinksQuery},
        Edge, EdgeLimit, Node, Webgraph,
    };

    #[test]
    fn test_simple() {
        let temp_dir = crate::gen_temp_dir().unwrap();
        let mut graph = Webgraph::builder(&temp_dir, 0u64.into()).open().unwrap();

        graph
            .insert(Edge::new_test(
                Node::from("https://A.com/1"),
                Node::from("https://B.com/1"),
            ))
            .unwrap();

        graph.commit().unwrap();

        let res = graph
            .search(&BacklinksQuery::new(Node::from("https://B.com/1").id()))
            .unwrap();

        assert_eq!(res.len(), 1);
        assert!(res[0].from == Node::from("https://A.com/1").id());
    }

    #[test]
    fn test_deduplication() {
        let temp_dir = crate::gen_temp_dir().unwrap();
        let mut graph = Webgraph::builder(&temp_dir, 0u64.into()).open().unwrap();

        graph
            .insert(Edge::new_test(
                Node::from("https://A.com/1"),
                Node::from("https://B.com/1"),
            ))
            .unwrap();
        graph
            .insert(Edge::new_test(
                Node::from("https://A.com/2"),
                Node::from("https://B.com/1"),
            ))
            .unwrap();

        graph.commit().unwrap();

        let res = graph
            .search(&BacklinksQuery::new(Node::from("https://B.com/1").id()))
            .unwrap();

        assert_eq!(res.len(), 2);

        let res = graph
            .search(&HostBacklinksQuery::new(
                Node::from("https://B.com/").into_host().id(),
            ))
            .unwrap();

        assert_eq!(res.len(), 1);
    }

    #[test]
    fn test_deduplication_across_segments() {
        let temp_dir = crate::gen_temp_dir().unwrap();
        let mut graph = Webgraph::builder(&temp_dir, 0u64.into()).open().unwrap();

        graph
            .insert(Edge::new_test(
                Node::from("https://A.com/1"),
                Node::from("https://B.com/1"),
            ))
            .unwrap();
        graph
            .insert(Edge::new_test(
                Node::from("https://A.com/2"),
                Node::from("https://B.com/1"),
            ))
            .unwrap();

        graph.commit().unwrap();

        let res = graph
            .search(&HostBacklinksQuery::new(
                Node::from("https://B.com/").into_host().id(),
            ))
            .unwrap();

        assert_eq!(res.len(), 1);
    }

    #[test]
    fn test_offset_with_deduplication() {
        let temp_dir = crate::gen_temp_dir().unwrap();
        let mut graph = Webgraph::builder(&temp_dir, 0u64.into()).open().unwrap();

        graph
            .insert(Edge {
                from: Node::from("https://A.com/1"),
                to: Node::from("https://B.com/1"),
                sort_score: 1,
                ..Edge::empty()
            })
            .unwrap();
        graph
            .insert(Edge {
                from: Node::from("https://A.com/2"),
                to: Node::from("https://B.com/1"),
                sort_score: 1,
                ..Edge::empty()
            })
            .unwrap();
        graph
            .insert(Edge {
                from: Node::from("https://C.com/1"),
                to: Node::from("https://B.com/1"),
                sort_score: 3,
                ..Edge::empty()
            })
            .unwrap();

        graph.commit().unwrap();

        let res = graph
            .search(
                &HostBacklinksQuery::new(Node::from("https://B.com/").into_host().id()).with_limit(
                    EdgeLimit::LimitAndOffset {
                        limit: 1024,
                        offset: 0,
                    },
                ),
            )
            .unwrap();

        assert_eq!(res.len(), 2);

        let res = graph
            .search(
                &HostBacklinksQuery::new(Node::from("https://B.com/").into_host().id()).with_limit(
                    EdgeLimit::LimitAndOffset {
                        limit: 1,
                        offset: 0,
                    },
                ),
            )
            .unwrap();

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].from, Node::from("https://A.com/").into_host().id());

        let res = graph
            .search(
                &HostBacklinksQuery::new(Node::from("https://B.com/").into_host().id()).with_limit(
                    EdgeLimit::LimitAndOffset {
                        limit: 1,
                        offset: 1,
                    },
                ),
            )
            .unwrap();

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].from, Node::from("https://C.com/").into_host().id());

        let res = graph
            .search(
                &HostBacklinksQuery::new(Node::from("https://B.com/").into_host().id()).with_limit(
                    EdgeLimit::LimitAndOffset {
                        limit: 1,
                        offset: 2,
                    },
                ),
            )
            .unwrap();

        assert_eq!(res.len(), 0);
    }
}
