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
// along with this program.  If not, see <https://www.gnu.org/licenses/>

use std::path::Path;

use super::Webgraph;
use crate::{ampc::dht::ShardId, Result};

pub struct WebgraphBuilder {
    path: Box<Path>,
    shard_id: ShardId,
}

impl WebgraphBuilder {
    pub fn new<P: AsRef<Path>>(path: P, shard_id: ShardId) -> Self {
        Self {
            path: path.as_ref().into(),
            shard_id,
        }
    }

    pub fn open(self) -> Result<Webgraph> {
        Webgraph::open(self.path, self.shard_id)
    }
}
