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

use itertools::Itertools;
use tantivy::{
    postings::SegmentPostings,
    query::{EmptyScorer, EnableScoring, Explanation, Query, Scorer, Weight},
    DocId, DocSet, HasLen, Score, SegmentReader, Term,
};

use crate::webgraph::{
    schema::{Field, FieldEnum},
    NodeID,
};

#[derive(Debug, Clone, bincode::Encode, bincode::Decode)]
pub struct Id2NodeQuery {
    node: NodeID,
    fields: Vec<FieldEnum>,
}

impl Id2NodeQuery {
    pub fn new(node: NodeID, fields: Vec<FieldEnum>) -> Self {
        Self { node, fields }
    }
}

impl Query for Id2NodeQuery {
    fn weight(&self, _: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(Id2NodeWeight {
            node: self.node,
            fields: self.fields.clone(),
        }))
    }
}

struct Id2NodeWeight {
    node: NodeID,
    fields: Vec<FieldEnum>,
}

impl Weight for Id2NodeWeight {
    fn scorer(&self, reader: &SegmentReader, _: Score) -> tantivy::Result<Box<dyn Scorer>> {
        for field in self.fields.iter() {
            let field = reader.schema().get_field(field.name())?;
            let inverted_index = reader.inverted_index(field)?;

            let term = Term::from_field_u128(field, self.node.as_u128());

            if let Some(postings) =
                inverted_index.read_postings(&term, tantivy::schema::IndexRecordOption::Basic)?
            {
                return Ok(Box::new(Id2NodeScorer { postings }));
            }
        }

        Ok(Box::new(EmptyScorer))
    }

    fn explain(&self, _: &SegmentReader, _: DocId) -> tantivy::Result<Explanation> {
        Ok(Explanation::new_with_string(
            format!(
                "Id2Node on [{}]",
                self.fields.iter().map(|f| f.name()).join(", ")
            ),
            0.0,
        ))
    }
}

struct Id2NodeScorer {
    postings: SegmentPostings,
}

impl Scorer for Id2NodeScorer {
    fn score(&mut self) -> tantivy::Score {
        unimplemented!()
    }
}

impl DocSet for Id2NodeScorer {
    fn advance(&mut self) -> tantivy::DocId {
        tantivy::TERMINATED
    }

    fn doc(&self) -> tantivy::DocId {
        self.postings.doc()
    }

    fn size_hint(&self) -> u32 {
        if self.postings.is_empty() {
            0
        } else {
            1
        }
    }
}
