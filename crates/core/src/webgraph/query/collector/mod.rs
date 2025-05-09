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

mod fast_count;
mod first_doc;
mod group_exact;
mod group_sketch;
pub mod top_docs;

pub use fast_count::{FastCountCollector, FastCountValue};
pub use first_doc::FirstDocCollector;
pub use group_exact::GroupExactCollector;
pub use group_sketch::GroupSketchCollector;
pub use top_docs::{HostDeduplicator, TopDocsCollector};

use tantivy::{
    collector::{Fruit, SegmentCollector},
    query::Weight,
    SegmentOrdinal, SegmentReader,
};

pub trait Collector: Sync + Send {
    type Fruit: Fruit;
    type Child: SegmentCollector;

    fn for_segment(
        &self,
        segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> crate::Result<Self::Child>;

    fn merge_fruits(
        &self,
        segment_fruits: Vec<<Self::Child as SegmentCollector>::Fruit>,
    ) -> crate::Result<Self::Fruit>;

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        segment_ord: u32,
        reader: &SegmentReader,
    ) -> crate::Result<<Self::Child as SegmentCollector>::Fruit> {
        let mut segment_collector = self.for_segment(segment_ord, reader)?;

        weight.for_each_no_score(reader, &mut |docs| {
            segment_collector.collect_block(docs);
        })?;

        Ok(segment_collector.harvest())
    }
}

pub struct TantivyCollector<'a, T>(&'a T);

impl<'a, T> From<&'a T> for TantivyCollector<'a, T>
where
    T: Collector,
{
    fn from(collector: &'a T) -> Self {
        TantivyCollector(collector)
    }
}

impl<T> tantivy::collector::Collector for TantivyCollector<'_, T>
where
    T: Collector,
{
    type Fruit = T::Fruit;

    type Child = T::Child;

    fn for_segment(
        &self,
        segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        T::for_segment(self.0, segment_local_id, segment)
            .map_err(|e| tantivy::TantivyError::InternalError(e.to_string()))
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<<Self::Child as tantivy::collector::SegmentCollector>::Fruit>,
    ) -> tantivy::Result<Self::Fruit> {
        T::merge_fruits(self.0, segment_fruits)
            .map_err(|e| tantivy::TantivyError::InternalError(e.to_string()))
    }

    fn collect_segment(
        &self,
        weight: &dyn tantivy::query::Weight,
        segment_ord: u32,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as tantivy::collector::SegmentCollector>::Fruit> {
        T::collect_segment(self.0, weight, segment_ord, reader)
            .map_err(|e| tantivy::TantivyError::InternalError(e.to_string()))
    }
}
