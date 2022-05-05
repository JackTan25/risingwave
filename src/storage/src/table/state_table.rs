// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
#![allow(dead_code)]
use std::cmp::Ordering;
use std::sync::Arc;

use itertools::Itertools;
use risingwave_common::array::Row;
use risingwave_common::catalog::ColumnDesc;
use risingwave_common::error::RwError;
use risingwave_common::util::ordered::{serialize_pk, OrderedRowSerializer};
use risingwave_common::util::sort_util::OrderType;

use super::cell_based_table::{CellBasedTable, CellBasedTableRowIter};
use super::mem_table::{MemTable, RowOp};
use super::TableIter;
use crate::error::{StorageError, StorageResult};
use crate::monitor::StateStoreMetrics;
use crate::{Keyspace, StateStore};

/// `StateTable` is the interface accessing relational data in KV(`StateStore`) with encoding
#[derive(Clone)]
pub struct StateTable<S: StateStore> {
    keyspace: Keyspace<S>,

    column_descs: Vec<ColumnDesc>,
    // /// Ordering of primary key (for assertion)
    order_types: Vec<OrderType>,

    /// buffer key/values
    mem_table: MemTable,

    /// Relation layer
    cell_based_table: CellBasedTable<S>,
}
impl<S: StateStore> StateTable<S> {
    pub fn new(
        keyspace: Keyspace<S>,
        column_descs: Vec<ColumnDesc>,
        order_types: Vec<OrderType>,
    ) -> Self {
        let cell_based_keyspace = keyspace.clone();
        let cell_based_column_descs = column_descs.clone();
        Self {
            keyspace,
            column_descs,
            order_types: order_types.clone(),
            mem_table: MemTable::new(),
            cell_based_table: CellBasedTable::new(
                cell_based_keyspace,
                cell_based_column_descs,
                Some(OrderedRowSerializer::new(order_types)),
                Arc::new(StateStoreMetrics::unused()),
            ),
        }
    }

    /// read methods
    pub async fn get_row(&self, pk: &Row, epoch: u64) -> StorageResult<Option<Row>> {
        let mem_table_res = self.mem_table.get_row(pk).map_err(err)?;
        match mem_table_res {
            Some(row_op) => match row_op {
                RowOp::Insert(row) => Ok(Some(row.clone())),
                RowOp::Delete(_) => Ok(None),
                RowOp::Update((_, new_row)) => Ok(Some(new_row.clone())),
            },
            None => self.cell_based_table.get_row(pk, epoch).await,
        }
    }

    /// write methods
    pub fn insert(&mut self, pk: Row, value: Row) -> StorageResult<()> {
        assert_eq!(self.order_types.len(), pk.size());
        self.mem_table.insert(pk, value)?;
        Ok(())
    }

    pub fn delete(&mut self, pk: Row, old_value: Row) -> StorageResult<()> {
        assert_eq!(self.order_types.len(), pk.size());
        self.mem_table.delete(pk, old_value)?;
        Ok(())
    }

    pub fn update(&mut self, _pk: Row, _old_value: Row, _new_value: Row) -> StorageResult<()> {
        todo!()
    }

    pub async fn commit(&mut self, new_epoch: u64) -> StorageResult<()> {
        let mem_table = std::mem::take(&mut self.mem_table).into_parts();
        self.cell_based_table
            .batch_write_rows(mem_table, new_epoch)
            .await?;
        Ok(())
    }

    pub async fn commit_with_value_meta(&mut self, new_epoch: u64) -> StorageResult<()> {
        let mem_table = std::mem::take(&mut self.mem_table).into_parts();
        self.cell_based_table
            .batch_write_rows_with_value_meta(mem_table, new_epoch)
            .await?;
        Ok(())
    }

    pub async fn iter(&self, epoch: u64) -> StorageResult<StateTableRowIter<S>> {
        let mem_table_vec = self
            .mem_table
            .buffer
            .iter()
            .map(|(row, row_op)| (row.clone(), row_op.clone()))
            .collect_vec();
        StateTableRowIter::new(
            self.keyspace.clone(),
            self.column_descs.clone(),
            Arc::new(mem_table_vec),
            self.order_types.clone(),
            epoch,
            Arc::new(StateStoreMetrics::unused()),
        )
        .await
    }
}

type MemTableItem = (Row, RowOp);
pub struct StateTableRowIter<S: StateStore> {
    cell_based_row_iter: CellBasedTableRowIter<S>,
    mem_table_vec: Arc<Vec<MemTableItem>>,
    mem_table_idx: usize,
    order_types: Vec<OrderType>,
    keyspace: Keyspace<S>,
}

impl<S: StateStore> StateTableRowIter<S> {
    async fn new(
        keyspace: Keyspace<S>,
        table_descs: Vec<ColumnDesc>,
        mem_table_vec: Arc<Vec<MemTableItem>>,
        order_types: Vec<OrderType>,
        epoch: u64,
        stats: Arc<StateStoreMetrics>,
    ) -> StorageResult<StateTableRowIter<S>> {
        let cell_based_row_iter =
            CellBasedTableRowIter::new(keyspace.clone(), table_descs, epoch, stats).await?;
        let iter = Self {
            cell_based_row_iter,
            mem_table_vec,
            mem_table_idx: 0,
            order_types,
            keyspace,
        };

        Ok(iter)
    }

    fn mem_table_current_item(&self) -> Option<&MemTableItem> {
        self.mem_table_vec.get(self.mem_table_idx)
    }

}

#[async_trait::async_trait]
impl<S: StateStore> TableIter for StateTableRowIter<S> {
    async fn next(&mut self) -> StorageResult<Option<Row>> {
        let cell_based_item = self.cell_based_row_iter.next_with_pk().await?;
        let mem_table_cur_item = self.mem_table_vec.get(self.mem_table_idx);
        match (cell_based_item, mem_table_cur_item) {
            (None, None) => {
                return Ok(None);
            }
            (Some((_, row)), None) => {
                return Ok(Some(row));
            }
            (None, Some((_, row_op))) => {
                self.mem_table_idx += 1;
                match row_op {
                    RowOp::Insert(row) => return Ok(Some(row.clone())),
                    RowOp::Delete(_) => {
                        return Ok(None);
                    }
                    RowOp::Update((_, new_row)) => return Ok(Some(new_row.clone())),
                }
            }
            (Some((cell_based_pk, cell_based_row)), Some((mem_table_pk, mem_table_row_op))) => {
                let pk_serializer = OrderedRowSerializer::new(self.order_types.clone());
                let mem_table_pk_bytes =
                    serialize_pk(mem_table_pk, &pk_serializer).map_err(err)?;
                let mem_table_pk_with_prefix = self.keyspace.prefixed_key(mem_table_pk_bytes);
                match cell_based_pk.cmp(&mem_table_pk_with_prefix) {
                    Ordering::Less => {
                        return Ok(Some(cell_based_row));
                    }
                    Ordering::Greater => {
                        self.mem_table_idx += 1;
                        match mem_table_row_op {
                            RowOp::Insert(row) => return Ok(Some(row.clone())),
                            RowOp::Delete(_) => {
                                return Ok(None);
                            }
                            RowOp::Update((_, new_row)) => return Ok(Some(new_row.clone())),
                        }
                    }
                    Ordering::Equal => {
                        self.mem_table_idx += 1;
                        match mem_table_row_op {
                            RowOp::Insert(row) => {
                                return Ok(Some(row.clone()));
                            }
                            RowOp::Delete(_) => {
                                return Ok(None);
                            }
                            RowOp::Update((_, new_row)) => {
                                return Ok(Some(new_row.clone()));
                            }
                        }
                    }
                }
            }
        }
    }

    async fn next_with_pk(&mut self) -> StorageResult<Option<(Vec<u8>, Row)>> {
        unimplemented!()
    }
}

fn err(rw: impl Into<RwError>) -> StorageError {
    StorageError::StateTable(rw.into())
}

#[cfg(test)]
mod tests {
    use risingwave_common::catalog::ColumnId;
    use risingwave_common::types::DataType;
    use risingwave_common::util::sort_util::OrderType;

    use super::*;
    use crate::memory::MemoryStateStore;

    #[tokio::test]
    async fn test_state_table() -> StorageResult<()> {
        let state_store = MemoryStateStore::new();
        let keyspace = Keyspace::executor_root(state_store.clone(), 0x42);
        let column_descs = vec![
            ColumnDesc::unnamed(ColumnId::from(0), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(1), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(2), DataType::Int32),
        ];
        let order_types = vec![OrderType::Ascending];
        let mut state_table = StateTable::new(keyspace.clone(), column_descs, order_types);
        let mut epoch: u64 = 0;
        state_table
            .insert(
                Row(vec![Some(1_i32.into())]),
                Row(vec![
                    Some(1_i32.into()),
                    Some(11_i32.into()),
                    Some(111_i32.into()),
                ]),
            )
            .unwrap();
        state_table
            .insert(
                Row(vec![Some(2_i32.into())]),
                Row(vec![
                    Some(2_i32.into()),
                    Some(22_i32.into()),
                    Some(222_i32.into()),
                ]),
            )
            .unwrap();
        state_table
            .insert(
                Row(vec![Some(3_i32.into())]),
                Row(vec![
                    Some(3_i32.into()),
                    Some(33_i32.into()),
                    Some(333_i32.into()),
                ]),
            )
            .unwrap();

        // test read visibility
        let row1 = state_table
            .get_row(&Row(vec![Some(1_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(
            row1,
            Some(Row(vec![
                Some(1_i32.into()),
                Some(11_i32.into()),
                Some(111_i32.into())
            ]))
        );

        let row2 = state_table
            .get_row(&Row(vec![Some(2_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(
            row2,
            Some(Row(vec![
                Some(2_i32.into()),
                Some(22_i32.into()),
                Some(222_i32.into())
            ]))
        );

        state_table
            .delete(
                Row(vec![Some(2_i32.into())]),
                Row(vec![
                    Some(2_i32.into()),
                    Some(22_i32.into()),
                    Some(222_i32.into()),
                ]),
            )
            .unwrap();

        let row2_delete = state_table
            .get_row(&Row(vec![Some(2_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(row2_delete, None);

        state_table.commit(epoch).await.unwrap();

        let row2_delete_commit = state_table
            .get_row(&Row(vec![Some(2_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(row2_delete_commit, None);

        epoch += 1;
        state_table
            .delete(
                Row(vec![Some(3_i32.into())]),
                Row(vec![
                    Some(3_i32.into()),
                    Some(33_i32.into()),
                    Some(333_i32.into()),
                ]),
            )
            .unwrap();

        state_table
            .insert(
                Row(vec![Some(4_i32.into())]),
                Row(vec![Some(4_i32.into()), None, None]),
            )
            .unwrap();
        state_table
            .insert(Row(vec![Some(5_i32.into())]), Row(vec![None, None, None]))
            .unwrap();

        let row4 = state_table
            .get_row(&Row(vec![Some(4_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(row4, Some(Row(vec![Some(4_i32.into()), None, None])));

        let row5 = state_table
            .get_row(&Row(vec![Some(5_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(row5, Some(Row(vec![None, None, None])));

        let non_exist_row = state_table
            .get_row(&Row(vec![Some(0_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(non_exist_row, None);

        state_table
            .delete(
                Row(vec![Some(4_i32.into())]),
                Row(vec![Some(4_i32.into()), None, None]),
            )
            .unwrap();

        state_table.commit(epoch).await.unwrap();

        let row3_delete = state_table
            .get_row(&Row(vec![Some(3_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(row3_delete, None);

        let row4_delete = state_table
            .get_row(&Row(vec![Some(4_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(row4_delete, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_state_table_update_insert() -> StorageResult<()> {
        let state_store = MemoryStateStore::new();
        let keyspace = Keyspace::executor_root(state_store.clone(), 0x42);
        let column_descs = vec![
            ColumnDesc::unnamed(ColumnId::from(0), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(1), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(2), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(4), DataType::Int32),
        ];
        let order_types = vec![OrderType::Ascending];
        let mut state_table = StateTable::new(keyspace.clone(), column_descs, order_types);
        let mut epoch: u64 = 0;
        state_table
            .insert(
                Row(vec![Some(6_i32.into())]),
                Row(vec![
                    Some(6_i32.into()),
                    Some(66_i32.into()),
                    Some(666_i32.into()),
                    Some(6666_i32.into()),
                ]),
            )
            .unwrap();

        state_table
            .insert(
                Row(vec![Some(7_i32.into())]),
                Row(vec![Some(7_i32.into()), None, Some(777_i32.into()), None]),
            )
            .unwrap();
        state_table.commit(epoch).await.unwrap();

        epoch += 1;
        state_table
            .delete(
                Row(vec![Some(6_i32.into())]),
                Row(vec![
                    Some(6_i32.into()),
                    Some(66_i32.into()),
                    Some(666_i32.into()),
                    Some(6666_i32.into()),
                ]),
            )
            .unwrap();
        state_table
            .insert(
                Row(vec![Some(6_i32.into())]),
                Row(vec![
                    Some(6666_i32.into()),
                    None,
                    None,
                    Some(6666_i32.into()),
                ]),
            )
            .unwrap();

        state_table
            .delete(
                Row(vec![Some(7_i32.into())]),
                Row(vec![Some(7_i32.into()), None, Some(777_i32.into()), None]),
            )
            .unwrap();
        state_table
            .insert(
                Row(vec![Some(7_i32.into())]),
                Row(vec![None, Some(77_i32.into()), Some(7777_i32.into()), None]),
            )
            .unwrap();
        let row6 = state_table
            .get_row(&Row(vec![Some(6_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(
            row6,
            Some(Row(vec![
                Some(6666_i32.into()),
                None,
                None,
                Some(6666_i32.into())
            ]))
        );

        let row7 = state_table
            .get_row(&Row(vec![Some(7_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(
            row7,
            Some(Row(vec![
                None,
                Some(77_i32.into()),
                Some(7777_i32.into()),
                None
            ]))
        );

        state_table.commit(epoch).await.unwrap();

        let row6_commit = state_table
            .get_row(&Row(vec![Some(6_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(
            row6_commit,
            Some(Row(vec![
                Some(6666_i32.into()),
                None,
                None,
                Some(6666_i32.into())
            ]))
        );
        let row7_commit = state_table
            .get_row(&Row(vec![Some(7_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(
            row7_commit,
            Some(Row(vec![
                None,
                Some(77_i32.into()),
                Some(7777_i32.into()),
                None
            ]))
        );

        epoch += 1;

        state_table
            .insert(
                Row(vec![Some(1_i32.into())]),
                Row(vec![
                    Some(1_i32.into()),
                    Some(2_i32.into()),
                    Some(3_i32.into()),
                    Some(4_i32.into()),
                ]),
            )
            .unwrap();
        state_table.commit(epoch).await.unwrap();
        // one epoch: delete (1, 2, 3, 4), insert (5, 6, 7, None), delete(5, 6, 7, None)
        state_table
            .delete(
                Row(vec![Some(1_i32.into())]),
                Row(vec![
                    Some(1_i32.into()),
                    Some(2_i32.into()),
                    Some(3_i32.into()),
                    Some(4_i32.into()),
                ]),
            )
            .unwrap();
        state_table
            .insert(
                Row(vec![Some(1_i32.into())]),
                Row(vec![
                    Some(5_i32.into()),
                    Some(6_i32.into()),
                    Some(7_i32.into()),
                    None,
                ]),
            )
            .unwrap();
        state_table
            .delete(
                Row(vec![Some(1_i32.into())]),
                Row(vec![
                    Some(5_i32.into()),
                    Some(6_i32.into()),
                    Some(7_i32.into()),
                    None,
                ]),
            )
            .unwrap();

        let row1 = state_table
            .get_row(&Row(vec![Some(1_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(row1, None);
        state_table.commit(epoch).await.unwrap();

        let row1_commit = state_table
            .get_row(&Row(vec![Some(1_i32.into())]), epoch)
            .await
            .unwrap();
        assert_eq!(row1_commit, None);
        Ok(())
    }
}
