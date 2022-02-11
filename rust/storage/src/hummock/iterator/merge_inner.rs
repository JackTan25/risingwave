use std::collections::binary_heap::PeekMut;
use std::collections::{BinaryHeap, LinkedList};
use std::ops::Bound;
use std::sync::Arc;
use std::iter::once;

use async_trait::async_trait;

use super::variants::*;
use super::{BoxedHummockIterator, IteratorType};
use crate::hummock::iterator::HummockIterator;
use crate::hummock::memtable::{MemTable, MemTableIteratorBuilder};
use crate::hummock::value::HummockValue;
use crate::hummock::version_cmp::VersionedComparator;
use crate::hummock::HummockResult;

pub struct Node<'a, const DIRECTION: usize>(IteratorType<'a>);

impl<const DIRECTION: usize> PartialEq for Node<'_, DIRECTION> {
    fn eq(&self, other: &Self) -> bool {
        // self.0.key_part() == other.0.key_part() && self.0.epoch_part() == other.0.epoch_part()
        self.0.key_parts() == other.0.key_parts()
    }
}
impl<const DIRECTION: usize> Eq for Node<'_, DIRECTION> {}

impl<const DIRECTION: usize> PartialOrd for Node<'_, DIRECTION> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<const DIRECTION: usize> Ord for Node<'_, DIRECTION> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Note: to implement min-heap by using max-heap internally, the comparing
        // order should be reversed.
        match DIRECTION {
            FORWARD => {
                VersionedComparator::compare_key_parts(other.0.key_parts(), self.0.key_parts())
            }
            BACKWARD => {
                VersionedComparator::compare_key_parts(self.0.key_parts(), other.0.key_parts())
            }
            _ => unreachable!(),
        }
    }
}

/// Iterates on multiple iterators, a.k.a. `MergeIterator`.
pub struct MergeIteratorInner<'a, const DIRECTION: usize, M: MemTable> {
    memtable_iter_builder: MemTableIteratorBuilder<M>,
    /// Invalid or non-initialized iterators.
    unused_iters: LinkedList<BoxedHummockIterator<'a>>,

    /// The heap for merge sort.
    heap: BinaryHeap<Node<'a, DIRECTION>>,

    range: (Bound<Vec<u8>>, Bound<Vec<u8>>),
}

impl<'a, const DIRECTION: usize, M: MemTable> MergeIteratorInner<'a, DIRECTION, M> {
    /// Caller should make sure that `iterators`'s direction is the same as `DIRECTION`.
    pub fn new(
        memtable_iter_builder: MemTableIteratorBuilder<M>,
        iterators: impl IntoIterator<Item = BoxedHummockIterator<'a>>,
        range: (Bound<Vec<u8>>, Bound<Vec<u8>>)
    ) -> Self {
        Self {
            memtable_iter_builder,
            unused_iters: iterators.into_iter().collect(),
            heap: BinaryHeap::new(),
            range
        }
    }

    /// Move all iterators from the `heap` to the linked list.
    fn reset_heap(&mut self) {
        self.unused_iters.extend(
            self.heap
                .drain()
                .filter(|i| i.0.is_sstable_iterator())
                .map(|n| n.0.to_hummock_iterator()),
        );
    }

    /// After some of the iterators in `unused_iterators` are seeked or rewound, call this function
    /// to construct a new heap using the valid ones.
    fn build_heap(&'a mut self) {
        assert!(self.heap.is_empty());

        self.heap = self
            .unused_iters
            .drain_filter(|i| i.is_valid())
            .map(|i| Node(IteratorType::new_sstable_iterator(i)))
            // .chain(once(Node(self.memtable_iter_builder.build())))
            .collect();
        self.heap.push(Node(self.memtable_iter_builder.build()));
    }
}

#[async_trait]
impl<const DIRECTION: usize, M: MemTable> HummockIterator for MergeIteratorInner<'_, DIRECTION, M> {
    async fn next(&mut self) -> HummockResult<()> {
        let mut node = self.heap.peek_mut().expect("no inner iter");

        node.0.next().await?;
        if !node.0.is_valid() {
            // put back to `unused_iters`
            let node = PeekMut::pop(node);
            self.unused_iters.push_back(node.0);
        } else {
            // this will update the heap top
            drop(node);
        }

        Ok(())
    }

    fn key(&self) -> &[u8] {
        self.heap.peek().expect("no inner iter").0.key()
    }

    fn value(&self) -> HummockValue<&[u8]> {
        self.heap.peek().expect("no inner iter").0.value()
    }

    fn is_valid(&self) -> bool {
        self.heap.peek().map_or(false, |n| n.0.is_valid())
    }

    async fn rewind(&mut self) -> HummockResult<()> {
        self.reset_heap();
        futures::future::try_join_all(self.unused_iters.iter_mut().map(|x| x.rewind())).await?;
        self.build_heap();
        Ok(())
    }

    async fn seek(&mut self, key: &[u8]) -> HummockResult<()> {
        self.reset_heap();
        futures::future::try_join_all(self.unused_iters.iter_mut().map(|x| x.seek(key))).await?;
        self.build_heap();
        Ok(())
    }
}
