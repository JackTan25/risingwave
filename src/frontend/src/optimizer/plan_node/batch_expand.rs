// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt;

use itertools::Itertools;
use risingwave_common::error::Result;
use risingwave_pb::batch_plan::expand_node::Subset;
use risingwave_pb::batch_plan::plan_node::NodeBody;
use risingwave_pb::batch_plan::ExpandNode;

use super::{generic, ExprRewritable};
use crate::optimizer::plan_node::{
    PlanBase, PlanTreeNodeUnary, ToBatchPb, ToDistributedBatch, ToLocalBatch,
};
use crate::optimizer::property::{Distribution, Order};
use crate::optimizer::PlanRef;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BatchExpand {
    pub base: PlanBase,
    logical: generic::Expand<PlanRef>,
}

impl BatchExpand {
    pub fn new(logical: generic::Expand<PlanRef>) -> Self {
        let base = PlanBase::new_logical_with_core(&logical);
        let ctx = base.ctx;
        let dist = match logical.input.distribution() {
            Distribution::Single => Distribution::Single,
            Distribution::SomeShard
            | Distribution::HashShard(_)
            | Distribution::UpstreamHashShard(_, _) => Distribution::SomeShard,
            Distribution::Broadcast => unreachable!(),
        };
        let base = PlanBase::new_batch(ctx, base.schema, dist, Order::any());
        BatchExpand { base, logical }
    }

    pub fn column_subsets(&self) -> &[Vec<usize>] {
        &self.logical.column_subsets
    }
}

impl fmt::Display for BatchExpand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.logical.fmt_with_name(f, "BatchExpand")
    }
}

impl PlanTreeNodeUnary for BatchExpand {
    fn input(&self) -> PlanRef {
        self.logical.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        let mut logical = self.logical.clone();
        logical.input = input;
        Self::new(logical)
    }
}

impl_plan_tree_node_for_unary! { BatchExpand }

impl ToDistributedBatch for BatchExpand {
    fn to_distributed(&self) -> Result<PlanRef> {
        let new_input = self.input().to_distributed()?;
        Ok(self.clone_with_input(new_input).into())
    }
}

impl ToBatchPb for BatchExpand {
    fn to_batch_prost_body(&self) -> NodeBody {
        NodeBody::Expand(ExpandNode {
            column_subsets: self
                .column_subsets()
                .iter()
                .map(|subset| subset_to_protobuf(subset))
                .collect_vec(),
        })
    }
}

fn subset_to_protobuf(subset: &[usize]) -> Subset {
    let column_indices = subset.iter().map(|key| *key as u32).collect_vec();
    Subset { column_indices }
}

impl ToLocalBatch for BatchExpand {
    fn to_local(&self) -> Result<PlanRef> {
        let new_input = self.input().to_local()?;
        Ok(self.clone_with_input(new_input).into())
    }
}

impl ExprRewritable for BatchExpand {}
