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
// along with this program.  If not, see <https://www.gnu.org/license

use super::{prelude::Job, DhtConn};

/// A mapper is the specific computation to be run on the graph.
pub trait Mapper: bincode::Encode + bincode::Decode + Send + Sync + Clone {
    type Job: Job<Mapper = Self>;

    fn map(
        &self,
        job: Self::Job,
        worker: &<<Self as Mapper>::Job as Job>::Worker,
        dht: &DhtConn<<<Self as Mapper>::Job as Job>::DhtTables>,
    );
}
