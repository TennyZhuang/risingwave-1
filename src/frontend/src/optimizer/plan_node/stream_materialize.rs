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

use std::assert_matches::assert_matches;

use fixedbitset::FixedBitSet;
use itertools::Itertools;
use pretty_xmlish::{Pretty, XmlNode};
use risingwave_common::catalog::{ColumnCatalog, ConflictBehavior, TableId};
use risingwave_common::error::Result;
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_common::util::sort_util::{ColumnOrder, OrderType};
use risingwave_pb::stream_plan::stream_node::PbNodeBody;

use super::derive::derive_columns;
use super::stream::prelude::*;
use super::stream::StreamPlanRef;
use super::utils::{childless_record, Distill};
use super::{reorganize_elements_id, ExprRewritable, PlanRef, PlanTreeNodeUnary, StreamNode};
use crate::catalog::table_catalog::{CreateType, TableCatalog, TableType, TableVersion};
use crate::catalog::FragmentId;
use crate::optimizer::plan_node::derive::derive_pk;
use crate::optimizer::plan_node::expr_visitable::ExprVisitable;
use crate::optimizer::plan_node::generic::GenericPlanRef;
use crate::optimizer::plan_node::{PlanBase, PlanNodeMeta};
use crate::optimizer::property::{Cardinality, Distribution, Order, RequiredDist};
use crate::stream_fragmenter::BuildFragmentGraphState;

/// Materializes a stream.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamMaterialize {
    pub base: PlanBase<Stream>,
    /// Child of Materialize plan
    input: PlanRef,
    table: TableCatalog,
}

impl StreamMaterialize {
    #[must_use]
    pub fn new(input: PlanRef, table: TableCatalog) -> Self {
        let base = PlanBase::new_stream(
            input.ctx(),
            input.schema().clone(),
            Some(table.stream_key.clone()),
            input.functional_dependency().clone(),
            input.distribution().clone(),
            input.append_only(),
            input.emit_on_window_close(),
            input.watermark_columns().clone(),
        );
        Self { base, input, table }
    }

    /// Create a materialize node, for `MATERIALIZED VIEW` and `INDEX`.
    ///
    /// When creating index, `TableType` should be `Index`. Then, materialize will distribute keys
    /// using `user_distributed_by`.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        input: PlanRef,
        name: String,
        user_distributed_by: RequiredDist,
        user_order_by: Order,
        user_cols: FixedBitSet,
        out_names: Vec<String>,
        definition: String,
        table_type: TableType,
        cardinality: Cardinality,
    ) -> Result<Self> {
        let input = Self::rewrite_input(input, user_distributed_by, table_type)?;
        // the hidden column name might refer some expr id
        let input = reorganize_elements_id(input);
        let columns = derive_columns(input.schema(), out_names, &user_cols)?;

        let table = Self::derive_table_catalog(
            input.clone(),
            name,
            user_order_by,
            columns,
            definition,
            ConflictBehavior::NoCheck,
            None,
            None,
            table_type,
            None,
            cardinality,
        )?;

        Ok(Self::new(input, table))
    }

    /// Create a materialize node, for `TABLE`.
    ///
    /// Different from `create`, the `columns` are passed in directly, instead of being derived from
    /// the input. So the column IDs are preserved from the SQL columns binding step and will be
    /// consistent with the source node and DML node.
    #[allow(clippy::too_many_arguments)]
    pub fn create_for_table(
        input: PlanRef,
        name: String,
        user_distributed_by: RequiredDist,
        user_order_by: Order,
        columns: Vec<ColumnCatalog>,
        definition: String,
        conflict_behavior: ConflictBehavior,
        pk_column_indices: Vec<usize>,
        row_id_index: Option<usize>,
        version: Option<TableVersion>,
    ) -> Result<Self> {
        let input = Self::rewrite_input(input, user_distributed_by, TableType::Table)?;

        let table = Self::derive_table_catalog(
            input.clone(),
            name,
            user_order_by,
            columns,
            definition,
            conflict_behavior,
            Some(pk_column_indices),
            row_id_index,
            TableType::Table,
            version,
            Cardinality::unknown(), // unknown cardinality for tables
        )?;

        Ok(Self::new(input, table))
    }

    /// Rewrite the input to satisfy the required distribution if necessary, according to the type.
    fn rewrite_input(
        input: PlanRef,
        user_distributed_by: RequiredDist,
        table_type: TableType,
    ) -> Result<PlanRef> {
        let required_dist = match input.distribution() {
            Distribution::Single => RequiredDist::single(),
            _ => match table_type {
                TableType::Table => {
                    assert_matches!(user_distributed_by, RequiredDist::ShardByKey(_));
                    user_distributed_by
                }
                TableType::MaterializedView => {
                    assert_matches!(user_distributed_by, RequiredDist::Any);
                    // ensure the same pk will not shuffle to different node
                    let required_dist =
                        RequiredDist::shard_by_key(input.schema().len(), input.expect_stream_key());

                    // If the input is a stream join, enforce the stream key as the materialized
                    // view distribution key to avoid slow backfilling caused by
                    // data skew of the dimension table join key.
                    // See <https://github.com/risingwavelabs/risingwave/issues/12824> for more information.
                    let is_stream_join = matches!(input.as_stream_hash_join(), Some(_join))
                        || matches!(input.as_stream_temporal_join(), Some(_join))
                        || matches!(input.as_stream_delta_join(), Some(_join));

                    if is_stream_join {
                        return Ok(required_dist.enforce(input, &Order::any()));
                    }

                    required_dist
                }
                TableType::Index => {
                    assert_matches!(
                        user_distributed_by,
                        RequiredDist::PhysicalDist(Distribution::HashShard(_))
                    );
                    user_distributed_by
                }
                TableType::Internal => unreachable!(),
            },
        };

        required_dist.enforce_if_not_satisfies(input, &Order::any())
    }

    /// Derive the table catalog with the given arguments.
    ///
    /// - The caller must ensure the validity of the given `columns`.
    /// - The `rewritten_input` should be generated by `rewrite_input`.
    #[allow(clippy::too_many_arguments)]
    fn derive_table_catalog(
        rewritten_input: PlanRef,
        name: String,
        user_order_by: Order,
        columns: Vec<ColumnCatalog>,
        definition: String,
        conflict_behavior: ConflictBehavior,
        pk_column_indices: Option<Vec<usize>>, // Is some when create table
        row_id_index: Option<usize>,
        table_type: TableType,
        version: Option<TableVersion>,
        cardinality: Cardinality,
    ) -> Result<TableCatalog> {
        let input = rewritten_input;

        let value_indices = (0..columns.len()).collect_vec();
        let distribution_key = input.distribution().dist_column_indices().to_vec();
        let properties = input.ctx().with_options().internal_table_subset(); // TODO: remove this
        let append_only = input.append_only();
        let watermark_columns = input.watermark_columns().clone();

        let (table_pk, stream_key) = if let Some(pk_column_indices) = pk_column_indices {
            let table_pk = pk_column_indices
                .iter()
                .map(|idx| ColumnOrder::new(*idx, OrderType::ascending()))
                .collect();
            // No order by for create table, so stream key is identical to table pk.
            (table_pk, pk_column_indices)
        } else {
            derive_pk(input, user_order_by, &columns)
        };
        // assert: `stream_key` is a subset of `table_pk`

        let read_prefix_len_hint = table_pk.len();
        Ok(TableCatalog {
            id: TableId::placeholder(),
            associated_source_id: None,
            name,
            columns,
            pk: table_pk,
            stream_key,
            distribution_key,
            table_type,
            append_only,
            owner: risingwave_common::catalog::DEFAULT_SUPER_USER_ID,
            properties,
            // TODO(zehua): replace it with FragmentId::placeholder()
            fragment_id: FragmentId::MAX - 1,
            dml_fragment_id: None,
            vnode_col_index: None,
            row_id_index,
            value_indices,
            definition,
            conflict_behavior,
            read_prefix_len_hint,
            version,
            watermark_columns,
            dist_key_in_pk: vec![],
            cardinality,
            created_at_epoch: None,
            initialized_at_epoch: None,
            cleaned_by_watermark: false,
            create_type: CreateType::Foreground, // Will be updated in the handler itself.
            description: None,
            incoming_sinks: vec![],
        })
    }

    /// Get a reference to the stream materialize's table.
    #[must_use]
    pub fn table(&self) -> &TableCatalog {
        &self.table
    }

    pub fn name(&self) -> &str {
        self.table.name()
    }
}

impl Distill for StreamMaterialize {
    fn distill<'a>(&self) -> XmlNode<'a> {
        let table = self.table();

        let column_names = (table.columns.iter())
            .map(|col| col.name_with_hidden().to_string())
            .map(Pretty::from)
            .collect();

        let stream_key = (table.stream_key.iter())
            .map(|&k| table.columns[k].name().to_string())
            .map(Pretty::from)
            .collect();

        let pk_columns = (table.pk.iter())
            .map(|o| table.columns[o.column_index].name().to_string())
            .map(Pretty::from)
            .collect();
        let mut vec = Vec::with_capacity(5);
        vec.push(("columns", Pretty::Array(column_names)));
        vec.push(("stream_key", Pretty::Array(stream_key)));
        vec.push(("pk_columns", Pretty::Array(pk_columns)));
        let pk_conflict_behavior = self.table.conflict_behavior().debug_to_string();

        vec.push(("pk_conflict", Pretty::from(pk_conflict_behavior)));

        let watermark_columns = &self.base.watermark_columns();
        if self.base.watermark_columns().count_ones(..) > 0 {
            let watermark_column_names = watermark_columns
                .ones()
                .map(|i| table.columns()[i].name_with_hidden().to_string())
                .map(Pretty::from)
                .collect();
            vec.push(("watermark_columns", Pretty::Array(watermark_column_names)));
        };
        childless_record("StreamMaterialize", vec)
    }
}

impl PlanTreeNodeUnary for StreamMaterialize {
    fn input(&self) -> PlanRef {
        self.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        let new = Self::new(input, self.table().clone());
        new.base
            .schema()
            .fields
            .iter()
            .zip_eq_fast(self.base.schema().fields.iter())
            .for_each(|(a, b)| {
                assert_eq!(a.data_type, b.data_type);
                assert_eq!(a.type_name, b.type_name);
                assert_eq!(a.sub_fields, b.sub_fields);
            });
        assert_eq!(new.plan_base().stream_key(), self.plan_base().stream_key());
        new
    }
}

impl_plan_tree_node_for_unary! { StreamMaterialize }

impl StreamNode for StreamMaterialize {
    fn to_stream_prost_body(&self, _state: &mut BuildFragmentGraphState) -> PbNodeBody {
        use risingwave_pb::stream_plan::*;

        PbNodeBody::Materialize(MaterializeNode {
            // We don't need table id for materialize node in frontend. The id will be generated on
            // meta catalog service.
            table_id: 0,
            column_orders: self
                .table()
                .pk()
                .iter()
                .map(ColumnOrder::to_protobuf)
                .collect(),
            table: Some(self.table().to_internal_table_prost()),
        })
    }
}

impl ExprRewritable for StreamMaterialize {}

impl ExprVisitable for StreamMaterialize {}
