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

use risingwave_common::error::Result;
use risingwave_common::types::DataType;
use risingwave_pb::batch_plan::plan_node::NodeBody;
use risingwave_pb::batch_plan::SortAggNode;
use risingwave_pb::expr::ExprNode;

use super::generic::{self, GenericPlanRef, PlanAggCall};
use super::{ExprRewritable, PlanBase, PlanRef, PlanTreeNodeUnary, ToBatchPb, ToDistributedBatch};
use crate::expr::{Expr, ExprImpl, ExprRewriter, InputRef};
use crate::optimizer::plan_node::ToLocalBatch;
use crate::optimizer::property::{Distribution, Order, RequiredDist};
use crate::utils::ColIndexMappingRewriteExt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BatchSortAgg {
    pub base: PlanBase,
    logical: generic::Agg<PlanRef>,
    input_order: Order,
}

impl BatchSortAgg {
    pub fn new(logical: generic::Agg<PlanRef>) -> Self {
        let base = PlanBase::new_logical_with_core(&logical);
        let ctx = base.ctx;
        let input = logical.input.clone();
        let input_dist = input.distribution();
        let dist = match input_dist {
            Distribution::HashShard(_) | Distribution::UpstreamHashShard(_, _) => logical
                .i2o_col_mapping()
                .rewrite_provided_distribution(input_dist),
            d => d.clone(),
        };
        let input_order = Order {
            column_orders: input
                .order()
                .column_orders
                .iter()
                .filter(|o| logical.group_key.iter().any(|g_k| *g_k == o.column_index))
                .cloned()
                .collect(),
        };

        assert_eq!(input_order.column_orders.len(), logical.group_key.len());

        let order = logical
            .i2o_col_mapping()
            .rewrite_provided_order(&input_order);

        let base = PlanBase::new_batch(ctx, base.schema, dist, order);

        BatchSortAgg {
            base,
            logical,
            input_order,
        }
    }

    pub fn agg_calls(&self) -> &[PlanAggCall] {
        &self.logical.agg_calls
    }

    pub fn group_key(&self) -> &[usize] {
        &self.logical.group_key
    }
}

impl fmt::Display for BatchSortAgg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.logical.fmt_with_name(f, "BatchSortAgg")
    }
}

impl PlanTreeNodeUnary for BatchSortAgg {
    fn input(&self) -> PlanRef {
        self.logical.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        let mut logical = self.logical.clone();
        logical.input = input;
        Self::new(logical)
    }
}
impl_plan_tree_node_for_unary! { BatchSortAgg }

impl ToDistributedBatch for BatchSortAgg {
    fn to_distributed(&self) -> Result<PlanRef> {
        let new_input = self.input().to_distributed_with_required(
            &self.input_order,
            &RequiredDist::shard_by_key(self.input().schema().len(), self.group_key()),
        )?;
        Ok(self.clone_with_input(new_input).into())
    }
}

impl ToBatchPb for BatchSortAgg {
    fn to_batch_prost_body(&self) -> NodeBody {
        NodeBody::SortAgg(SortAggNode {
            agg_calls: self
                .agg_calls()
                .iter()
                .map(PlanAggCall::to_protobuf)
                .collect(),
            group_key: self
                .group_key()
                .iter()
                .map(|idx| ExprImpl::InputRef(Box::new(InputRef::new(*idx, DataType::Int32))))
                .map(|expr| expr.to_expr_proto())
                .collect::<Vec<ExprNode>>(),
        })
    }
}

impl ToLocalBatch for BatchSortAgg {
    fn to_local(&self) -> Result<PlanRef> {
        let new_input = self.input().to_local()?;

        let new_input =
            RequiredDist::single().enforce_if_not_satisfies(new_input, self.input().order())?;

        Ok(self.clone_with_input(new_input).into())
    }
}

impl ExprRewritable for BatchSortAgg {
    fn has_rewritable_expr(&self) -> bool {
        true
    }

    fn rewrite_exprs(&self, r: &mut dyn ExprRewriter) -> PlanRef {
        let mut new_logical = self.logical.clone();
        new_logical.rewrite_exprs(r);
        Self::new(new_logical).into()
    }
}
