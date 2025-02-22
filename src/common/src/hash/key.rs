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

//! This module contains implementation for hash key serialization for
//! hash-agg, hash-join, and perhaps other hash-based operators.
//!
//! There may be multiple columns in one row being combined and encoded into
//! one single hash key.
//! For example, `SELECT sum(t.a) FROM t GROUP BY t.b, t.c`, the hash keys
//! are encoded from both `t.b` and `t.c`. If `t.b="abc"` and `t.c=1`, the hashkey may be
//! encoded in certain format of `("abc", 1)`.

use std::convert::TryInto;
use std::default::Default;
use std::fmt::Debug;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{Cursor, Read};
use std::marker::PhantomData;

use chrono::{Datelike, Timelike};
use derivative::Derivative;
use fixedbitset::FixedBitSet;
use smallbitset::Set64;
use static_assertions::const_assert_eq;

use crate::array::serial_array::Serial;
use crate::array::{
    Array, ArrayBuilder, ArrayBuilderImpl, ArrayError, ArrayImpl, ArrayResult, DataChunk, JsonbRef,
    ListRef, StructRef,
};
use crate::collection::estimate_size::EstimateSize;
use crate::row::{OwnedRow, RowDeserializer};
use crate::types::num256::Int256Ref;
use crate::types::{DataType, Date, Decimal, ScalarRef, Time, Timestamp, F32, F64};
use crate::util::hash_util::{Crc32FastBuilder, XxHash64Builder};
use crate::util::iter_util::ZipEqFast;
use crate::util::value_encoding::{deserialize_datum, serialize_datum_into};

/// This is determined by the stack based data structure we use,
/// `StackNullBitmap`, which can store 64 bits at most.
pub static MAX_GROUP_KEYS_ON_STACK: usize = 64;

/// Null bitmap on heap.
/// We use this for the **edge case** where group key sizes are larger than 64.
/// This is because group key null bits cannot fit into a u64 on the stack
/// if they exceed 64 bits.
/// NOTE(kwannoel): This is not really optimized as it is an edge case.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq)]
pub struct HeapNullBitmap {
    inner: FixedBitSet,
}

impl HeapNullBitmap {
    fn with_capacity(n: usize) -> Self {
        HeapNullBitmap {
            inner: FixedBitSet::with_capacity(n),
        }
    }
}

/// Null Bitmap on stack.
/// This is specialized for the common case where group keys (<= 64).
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq)]
pub struct StackNullBitmap {
    inner: Set64,
}

const_assert_eq!(
    std::mem::size_of::<StackNullBitmap>(),
    std::mem::size_of::<u64>()
);

const_assert_eq!(
    std::mem::size_of::<HeapNullBitmap>(),
    std::mem::size_of::<usize>() * 4,
);

/// We use a trait for `NullBitmap` so we can parameterize structs on it.
/// This is because `NullBitmap` is used often, and we want it to occupy
/// the minimal stack space.
///
/// ### Example
/// ```rust
/// use risingwave_common::hash::{NullBitmap, StackNullBitmap};
/// struct A<B: NullBitmap> {
///     null_bitmap: B,
/// }
/// ```
///
/// Then `A<StackNullBitmap>` occupies 64 bytes,
/// and in cases which require it,
/// `A<HeapNullBitmap>` will occupy 4 * usize bytes (on 64 bit arch that would be 256 bytes).
pub trait NullBitmap: EstimateSize + Clone + PartialEq + Debug + Send + Sync + 'static {
    fn empty() -> Self;

    fn is_empty(&self) -> bool;

    fn set_true(&mut self, idx: usize);

    fn contains(&self, x: usize) -> bool;

    fn is_subset(&self, other: &Self) -> bool;

    fn from_bool_vec<T: AsRef<[bool]> + IntoIterator<Item = bool>>(value: T) -> Self;
}

impl NullBitmap for StackNullBitmap {
    fn empty() -> Self {
        StackNullBitmap {
            inner: Set64::empty(),
        }
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn set_true(&mut self, idx: usize) {
        self.inner.add_inplace(idx);
    }

    fn contains(&self, x: usize) -> bool {
        self.inner.contains(x)
    }

    fn is_subset(&self, other: &Self) -> bool {
        other.inner.contains_all(self.inner)
    }

    fn from_bool_vec<T: AsRef<[bool]> + IntoIterator<Item = bool>>(value: T) -> Self {
        value.into()
    }
}

impl NullBitmap for HeapNullBitmap {
    fn empty() -> Self {
        HeapNullBitmap {
            inner: FixedBitSet::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn set_true(&mut self, idx: usize) {
        self.inner.grow(idx + 1);
        self.inner.insert(idx)
    }

    fn contains(&self, x: usize) -> bool {
        self.inner.contains(x)
    }

    fn is_subset(&self, other: &Self) -> bool {
        self.inner.is_subset(&other.inner)
    }

    fn from_bool_vec<T: AsRef<[bool]> + IntoIterator<Item = bool>>(value: T) -> Self {
        value.into()
    }
}

impl EstimateSize for StackNullBitmap {
    fn estimated_heap_size(&self) -> usize {
        0
    }
}

impl EstimateSize for HeapNullBitmap {
    fn estimated_heap_size(&self) -> usize {
        self.inner.estimated_heap_size()
    }
}

impl<T: AsRef<[bool]> + IntoIterator<Item = bool>> From<T> for StackNullBitmap {
    fn from(value: T) -> Self {
        let mut bitmap = StackNullBitmap::empty();
        for (idx, is_true) in value.into_iter().enumerate() {
            if is_true {
                bitmap.set_true(idx);
            }
        }
        bitmap
    }
}

impl<T: AsRef<[bool]> + IntoIterator<Item = bool>> From<T> for HeapNullBitmap {
    fn from(value: T) -> Self {
        let mut bitmap = HeapNullBitmap::with_capacity(value.as_ref().len());
        for (idx, is_true) in value.into_iter().enumerate() {
            if is_true {
                bitmap.set_true(idx);
            }
        }
        bitmap
    }
}

/// A wrapper for u64 hash result. Generic over the hasher.
#[derive(Derivative)]
#[derivative(Default, Clone, Copy, Debug, PartialEq)]
pub struct HashCode<T: 'static + BuildHasher> {
    value: u64,
    #[derivative(Debug = "ignore")]
    _phantom: PhantomData<&'static T>,
}

impl<T: BuildHasher> From<u64> for HashCode<T> {
    fn from(hash_code: u64) -> Self {
        Self {
            value: hash_code,
            _phantom: PhantomData,
        }
    }
}

impl<T: BuildHasher> HashCode<T> {
    pub fn value(self) -> u64 {
        self.value
    }
}

/// Hash code from the `Crc32` hasher. Used for hash-shuffle exchange.
pub type Crc32HashCode = HashCode<Crc32FastBuilder>;
/// Hash code from the `XxHash64` hasher. Used for in-memory hash map cache.
pub type XxHash64HashCode = HashCode<XxHash64Builder>;

pub trait HashKeySerializer {
    type K: HashKey;
    fn from_hash_code(hash_code: XxHash64HashCode, estimated_key_size: usize) -> Self;
    fn append<'a, D: HashKeySerDe<'a>>(&mut self, data: Option<D>);
    fn into_hash_key(self) -> Self::K;
}

pub trait HashKeyDeserializer {
    type K: HashKey;
    fn from_hash_key(hash_key: Self::K) -> Self;
    fn deserialize<'a, D: HashKeySerDe<'a>>(&'a mut self) -> ArrayResult<Option<D>>;
}

/// Trait for value types that can be serialized to or deserialized from hash keys.
///
/// Note that this trait is more like a marker suggesting that types that implement it can be
/// encoded into the hash key. The actual encoding/decoding method is not limited to
/// [`HashKeySerDe`]'s fixed-size implementation.
pub trait HashKeySerDe<'a>: ScalarRef<'a> {
    type S: AsRef<[u8]>;
    fn serialize(self) -> Self::S;
    fn deserialize<R: Read>(source: &mut R) -> Self;

    fn read_fixed_size_bytes<R: Read, const N: usize>(source: &mut R) -> [u8; N] {
        let mut buffer: [u8; N] = [0u8; N];
        source
            .read_exact(&mut buffer)
            .expect("Failed to read fixed size serialized key!");
        buffer
    }
}

/// Trait for different kinds of hash keys.
///
/// Current comparison implementation treats `null == null`. This is consistent with postgresql's
/// group by implementation, but not join. In pg's join implementation, `null != null`, and the join
/// executor should take care of this.
pub trait HashKey:
    EstimateSize + Clone + Debug + Hash + Eq + Sized + Send + Sync + 'static
{
    type Bitmap: NullBitmap;
    type S: HashKeySerializer<K = Self>;

    fn build(column_idxes: &[usize], data_chunk: &DataChunk) -> ArrayResult<Vec<Self>> {
        let hash_codes = data_chunk.get_hash_values(column_idxes, XxHash64Builder);
        Ok(Self::build_from_hash_code(
            column_idxes,
            data_chunk,
            hash_codes,
        ))
    }

    fn build_from_hash_code(
        column_idxes: &[usize],
        data_chunk: &DataChunk,
        hash_codes: Vec<XxHash64HashCode>,
    ) -> Vec<Self> {
        let estimated_key_size = data_chunk.estimate_value_encoding_size(column_idxes);
        // Construct serializers for each row.
        let mut serializers: Vec<Self::S> = hash_codes
            .into_iter()
            .map(|hashcode| Self::S::from_hash_code(hashcode, estimated_key_size))
            .collect();

        for column_idx in column_idxes {
            data_chunk
                .column_at(*column_idx)
                .array_ref()
                .serialize_to_hash_key(&mut serializers[..]);
        }

        serializers
            .into_iter()
            .map(Self::S::into_hash_key)
            .collect()
    }

    fn deserialize(&self, data_types: &[DataType]) -> ArrayResult<OwnedRow>;

    fn deserialize_to_builders(
        &self,
        array_builders: &mut [ArrayBuilderImpl],
        data_types: &[DataType],
    ) -> ArrayResult<()>;

    fn null_bitmap(&self) -> &Self::Bitmap;

    fn has_null(&self) -> bool {
        !self.null_bitmap().is_empty()
    }
}

/// Designed for hash keys with at most `N` serialized bytes.
///
/// See [`crate::hash::calc_hash_key_kind`]
#[derive(Clone, Debug)]
pub struct FixedSizeKey<const N: usize, B = StackNullBitmap> {
    key: [u8; N],
    hash_code: u64,
    null_bitmap: B,
}

/// Designed for hash keys which can't be represented by [`FixedSizeKey`].
///
/// See [`crate::hash::calc_hash_key_kind`]
#[derive(Clone, Debug)]
pub struct SerializedKey<B = StackNullBitmap> {
    // Key encoding.
    key: Vec<u8>,
    hash_code: u64,
    null_bitmap: B,
}

impl<const N: usize, B: NullBitmap> EstimateSize for FixedSizeKey<N, B> {
    fn estimated_heap_size(&self) -> usize {
        self.null_bitmap.estimated_heap_size()
    }
}

impl<const N: usize, B: NullBitmap> PartialEq for FixedSizeKey<N, B> {
    fn eq(&self, other: &Self) -> bool {
        (self.key == other.key) && (self.null_bitmap == other.null_bitmap)
    }
}

impl<const N: usize, B: NullBitmap> Eq for FixedSizeKey<N, B> {}

impl<const N: usize, B: NullBitmap> Hash for FixedSizeKey<N, B> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash_code)
    }
}

impl<B: NullBitmap> EstimateSize for SerializedKey<B> {
    fn estimated_heap_size(&self) -> usize {
        self.key.estimated_heap_size() + self.null_bitmap.estimated_heap_size()
    }
}

impl<B: NullBitmap> PartialEq for SerializedKey<B> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<B: NullBitmap> Eq for SerializedKey<B> {}

impl<B: NullBitmap> Hash for SerializedKey<B> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash_code)
    }
}

/// A special hasher designed for [`HashKey`], which stores a hash key from `HashKey::hash()` and
/// outputs it on `finish()`.
///
/// We need this because we compute hash keys in vectorized fashion, and we store them in this
/// hasher.
///
/// WARN: This should ONLY be used along with [`HashKey`].
#[derive(Default)]
pub struct PrecomputedHasher {
    hash_code: u64,
}

impl Hasher for PrecomputedHasher {
    fn finish(&self) -> u64 {
        self.hash_code
    }

    fn write_u64(&mut self, i: u64) {
        assert_eq!(self.hash_code, 0);
        self.hash_code = i;
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("must writes from HashKey with write_u64")
    }
}

#[derive(Default, Clone)]
pub struct PrecomputedBuildHasher;

impl BuildHasher for PrecomputedBuildHasher {
    type Hasher = PrecomputedHasher;

    fn build_hasher(&self) -> Self::Hasher {
        PrecomputedHasher::default()
    }
}

pub type Key8<B = StackNullBitmap> = FixedSizeKey<1, B>;
pub type Key16<B = StackNullBitmap> = FixedSizeKey<2, B>;
pub type Key32<B = StackNullBitmap> = FixedSizeKey<4, B>;
pub type Key64<B = StackNullBitmap> = FixedSizeKey<8, B>;
pub type Key128<B = StackNullBitmap> = FixedSizeKey<16, B>;
pub type Key256<B = StackNullBitmap> = FixedSizeKey<32, B>;
pub type KeySerialized<B = StackNullBitmap> = SerializedKey<B>;

impl HashKeySerDe<'_> for bool {
    type S = [u8; 1];

    fn serialize(self) -> Self::S {
        if self {
            [1u8; 1]
        } else {
            [0u8; 1]
        }
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 1>(source);
        value[0] == 1u8
    }
}

impl HashKeySerDe<'_> for i16 {
    type S = [u8; 2];

    fn serialize(self) -> Self::S {
        self.to_ne_bytes()
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 2>(source);
        Self::from_ne_bytes(value)
    }
}

impl HashKeySerDe<'_> for i32 {
    type S = [u8; 4];

    fn serialize(self) -> Self::S {
        self.to_ne_bytes()
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 4>(source);
        Self::from_ne_bytes(value)
    }
}

impl HashKeySerDe<'_> for i64 {
    type S = [u8; 8];

    fn serialize(self) -> Self::S {
        self.to_ne_bytes()
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 8>(source);
        Self::from_ne_bytes(value)
    }
}

impl HashKeySerDe<'_> for F32 {
    type S = [u8; 4];

    fn serialize(self) -> Self::S {
        self.normalized().to_ne_bytes()
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 4>(source);
        f32::from_ne_bytes(value).into()
    }
}

impl HashKeySerDe<'_> for F64 {
    type S = [u8; 8];

    fn serialize(self) -> Self::S {
        self.normalized().to_ne_bytes()
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 8>(source);
        f64::from_ne_bytes(value).into()
    }
}

impl HashKeySerDe<'_> for Decimal {
    type S = [u8; 16];

    fn serialize(self) -> Self::S {
        Decimal::unordered_serialize(&self.normalize())
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 16>(source);
        Self::unordered_deserialize(value)
    }
}

impl<'a> HashKeySerDe<'a> for &'a str {
    type S = Vec<u8>;

    /// This should never be called
    fn serialize(self) -> Self::S {
        panic!("Should not serialize str for hash!")
    }

    /// This should never be called
    fn deserialize<R: Read>(_source: &mut R) -> Self {
        panic!("Should not serialize str for hash!")
    }
}

impl<'a> HashKeySerDe<'a> for Int256Ref<'a> {
    type S = [u8; 32];

    fn serialize(self) -> Self::S {
        unimplemented!("HashKeySerDe cannot be implemented for non-primitive types")
    }

    fn deserialize<R: Read>(_source: &mut R) -> Self {
        unimplemented!("HashKeySerDe cannot be implemented for non-primitive types")
    }
}

/// Same as str.
impl<'a> HashKeySerDe<'a> for &'a [u8] {
    type S = Vec<u8>;

    /// This should never be called
    fn serialize(self) -> Self::S {
        panic!("Should not serialize bytes for hash!")
    }

    /// This should never be called
    fn deserialize<R: Read>(_source: &mut R) -> Self {
        panic!("Should not serialize bytes for hash!")
    }
}

impl HashKeySerDe<'_> for Date {
    type S = [u8; 4];

    fn serialize(self) -> Self::S {
        let mut ret = [0; 4];
        ret[0..4].copy_from_slice(&self.0.num_days_from_ce().to_ne_bytes());

        ret
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 4>(source);
        let days = i32::from_ne_bytes(value[0..4].try_into().unwrap());
        Date::with_days(days).unwrap()
    }
}

impl HashKeySerDe<'_> for Timestamp {
    type S = [u8; 12];

    fn serialize(self) -> Self::S {
        let mut ret = [0; 12];
        ret[0..8].copy_from_slice(&self.0.timestamp().to_ne_bytes());
        ret[8..12].copy_from_slice(&self.0.timestamp_subsec_nanos().to_ne_bytes());

        ret
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 12>(source);
        let secs = i64::from_ne_bytes(value[0..8].try_into().unwrap());
        let nsecs = u32::from_ne_bytes(value[8..12].try_into().unwrap());
        Timestamp::with_secs_nsecs(secs, nsecs).unwrap()
    }
}

impl HashKeySerDe<'_> for Time {
    type S = [u8; 8];

    fn serialize(self) -> Self::S {
        let mut ret = [0; 8];
        ret[0..4].copy_from_slice(&self.0.num_seconds_from_midnight().to_ne_bytes());
        ret[4..8].copy_from_slice(&self.0.nanosecond().to_ne_bytes());

        ret
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        let value = Self::read_fixed_size_bytes::<R, 8>(source);
        let secs = u32::from_ne_bytes(value[0..4].try_into().unwrap());
        let nano = u32::from_ne_bytes(value[4..8].try_into().unwrap());
        Time::with_secs_nano(secs, nano).unwrap()
    }
}

impl<'a> HashKeySerDe<'a> for JsonbRef<'a> {
    type S = Vec<u8>;

    /// This should never be called
    fn serialize(self) -> Self::S {
        todo!()
    }

    /// This should never be called
    fn deserialize<R: Read>(_source: &mut R) -> Self {
        todo!()
    }
}

impl<'a> HashKeySerDe<'a> for Serial {
    type S = <i64 as HashKeySerDe<'a>>::S;

    fn serialize(self) -> Self::S {
        self.into_inner().serialize()
    }

    fn deserialize<R: Read>(source: &mut R) -> Self {
        i64::deserialize(source).into()
    }
}

impl<'a> HashKeySerDe<'a> for StructRef<'a> {
    type S = Vec<u8>;

    /// This should never be called
    fn serialize(self) -> Self::S {
        todo!()
    }

    /// This should never be called
    fn deserialize<R: Read>(_source: &mut R) -> Self {
        todo!()
    }
}

impl<'a> HashKeySerDe<'a> for ListRef<'a> {
    type S = Vec<u8>;

    /// This should never be called
    fn serialize(self) -> Self::S {
        todo!()
    }

    /// This should never be called
    fn deserialize<R: Read>(_source: &mut R) -> Self {
        todo!()
    }
}

pub struct FixedSizeKeySerializer<const N: usize, B: NullBitmap> {
    buffer: [u8; N],
    null_bitmap: B,
    null_bitmap_idx: usize,
    data_len: usize,
    hash_code: u64,
}

impl<const N: usize, B: NullBitmap> FixedSizeKeySerializer<N, B> {
    fn left_size(&self) -> usize {
        N - self.data_len
    }
}

impl<const N: usize, B: NullBitmap> HashKeySerializer for FixedSizeKeySerializer<N, B> {
    type K = FixedSizeKey<N, B>;

    /// We already know the estimated key size statically, no need
    /// to use runtime parameter: `estimated_key_size`.
    fn from_hash_code(hash_code: XxHash64HashCode, _estimated_key_size: usize) -> Self {
        Self {
            buffer: [0u8; N],
            null_bitmap: NullBitmap::empty(),
            null_bitmap_idx: 0,
            data_len: 0,
            hash_code: hash_code.value(),
        }
    }

    fn append<'a, D: HashKeySerDe<'a>>(&mut self, data: Option<D>) {
        assert!(self.null_bitmap_idx < 8);
        match data {
            Some(v) => {
                let data = v.serialize();
                let ret = data.as_ref();
                assert!(self.left_size() >= ret.len());
                self.buffer[self.data_len..(self.data_len + ret.len())].copy_from_slice(ret);
                self.data_len += ret.len();
            }
            None => {
                self.null_bitmap.set_true(self.null_bitmap_idx);
            }
        };
        self.null_bitmap_idx += 1;
    }

    fn into_hash_key(self) -> Self::K {
        FixedSizeKey::<N, B> {
            hash_code: self.hash_code,
            key: self.buffer,
            null_bitmap: self.null_bitmap,
        }
    }
}

pub struct FixedSizeKeyDeserializer<const N: usize, B: NullBitmap> {
    cursor: Cursor<[u8; N]>,
    null_bitmap: B,
    null_bitmap_idx: usize,
}

impl<const N: usize, B: NullBitmap> HashKeyDeserializer for FixedSizeKeyDeserializer<N, B> {
    type K = FixedSizeKey<N, B>;

    fn from_hash_key(hash_key: Self::K) -> Self {
        Self {
            cursor: Cursor::new(hash_key.key),
            null_bitmap: hash_key.null_bitmap,
            null_bitmap_idx: 0,
        }
    }

    fn deserialize<'a, D: HashKeySerDe<'a>>(&mut self) -> ArrayResult<Option<D>> {
        ensure!(self.null_bitmap_idx < 8);
        let is_null = self.null_bitmap.contains(self.null_bitmap_idx);
        self.null_bitmap_idx += 1;
        if is_null {
            Ok(None)
        } else {
            let value = D::deserialize(&mut self.cursor);
            Ok(Some(value))
        }
    }
}

pub struct SerializedKeySerializer<B: NullBitmap> {
    buffer: Vec<u8>,
    hash_code: u64,
    null_bitmap: B,
    null_bitmap_idx: usize,
}

impl<B: NullBitmap> HashKeySerializer for SerializedKeySerializer<B> {
    type K = SerializedKey<B>;

    fn from_hash_code(hash_code: XxHash64HashCode, estimated_value_encoding_size: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(estimated_value_encoding_size),
            hash_code: hash_code.value(),
            null_bitmap: NullBitmap::empty(),
            null_bitmap_idx: 0,
        }
    }

    fn append<'a, D: HashKeySerDe<'a>>(&mut self, data: Option<D>) {
        match data {
            Some(v) => {
                serialize_datum_into(&Some(v.to_owned_scalar().into()), &mut self.buffer);
            }
            None => {
                serialize_datum_into(&None, &mut self.buffer);
                self.null_bitmap.set_true(self.null_bitmap_idx);
            }
        }
        self.null_bitmap_idx += 1;
    }

    fn into_hash_key(self) -> SerializedKey<B> {
        SerializedKey {
            key: self.buffer,
            hash_code: self.hash_code,
            null_bitmap: self.null_bitmap,
        }
    }
}

fn serialize_array_to_hash_key<'a, A, S>(array: &'a A, serializers: &mut [S])
where
    A: Array,
    A::RefItem<'a>: HashKeySerDe<'a>,
    S: HashKeySerializer,
{
    for (item, serializer) in array.iter().zip_eq_fast(serializers.iter_mut()) {
        serializer.append(item);
    }
}

fn deserialize_array_element_from_hash_key<'a, A, S>(
    builder: &'a mut A,
    deserializer: &'a mut S,
) -> ArrayResult<()>
where
    A: ArrayBuilder,
    <<A as ArrayBuilder>::ArrayType as Array>::RefItem<'a>: HashKeySerDe<'a>,
    S: HashKeyDeserializer,
{
    builder.append(deserializer.deserialize()?);
    Ok(())
}

impl ArrayImpl {
    fn serialize_to_hash_key<S: HashKeySerializer>(&self, serializers: &mut [S]) {
        macro_rules! impl_all_serialize_to_hash_key {
            ($({ $variant_name:ident, $suffix_name:ident, $array:ty, $builder:ty } ),*) => {
                match self {
                    $( Self::$variant_name(inner) => serialize_array_to_hash_key(inner, serializers), )*
                }
            };
        }
        for_all_variants! { impl_all_serialize_to_hash_key }
    }
}

impl ArrayBuilderImpl {
    fn deserialize_from_hash_key<S: HashKeyDeserializer>(
        &mut self,
        deserializer: &mut S,
    ) -> ArrayResult<()> {
        macro_rules! impl_all_deserialize_from_hash_key {
            ($({ $variant_name:ident, $suffix_name:ident, $array:ty, $builder:ty } ),*) => {
                match self {
                    $( Self::$variant_name(inner) => deserialize_array_element_from_hash_key(inner, deserializer), )*
                }
            };
        }
        for_all_variants! { impl_all_deserialize_from_hash_key }
    }
}

impl<const N: usize, B: NullBitmap> HashKey for FixedSizeKey<N, B> {
    type Bitmap = B;
    type S = FixedSizeKeySerializer<N, B>;

    fn deserialize(&self, data_types: &[DataType]) -> ArrayResult<OwnedRow> {
        // TODO: directly deserialize to Row
        let mut builders: Vec<_> = data_types
            .iter()
            .map(|dt| dt.create_array_builder(1))
            .collect();

        self.deserialize_to_builders(&mut builders, data_types)?;
        Ok(OwnedRow::new(
            builders
                .into_iter()
                .map(|builder| builder.finish().to_datum())
                .collect(),
        ))
    }

    fn deserialize_to_builders(
        &self,
        array_builders: &mut [ArrayBuilderImpl],
        _data_types: &[DataType],
    ) -> ArrayResult<()> {
        let mut deserializer = FixedSizeKeyDeserializer::<N, B>::from_hash_key(self.clone());
        for array_builder in array_builders.iter_mut() {
            array_builder.deserialize_from_hash_key(&mut deserializer)?;
        }
        Ok(())
    }

    fn null_bitmap(&self) -> &Self::Bitmap {
        &self.null_bitmap
    }
}

impl<B: NullBitmap> HashKey for SerializedKey<B> {
    type Bitmap = B;
    type S = SerializedKeySerializer<B>;

    fn deserialize(&self, data_types: &[DataType]) -> ArrayResult<OwnedRow> {
        RowDeserializer::new(data_types)
            .deserialize(self.key.as_slice())
            .map_err(ArrayError::internal)
    }

    fn deserialize_to_builders(
        &self,
        array_builders: &mut [ArrayBuilderImpl],
        data_types: &[DataType],
    ) -> ArrayResult<()> {
        let mut key_buffer = self.key.as_slice();
        for (datum_result, array_builder) in data_types
            .iter()
            .map(|ty| deserialize_datum(&mut key_buffer, ty))
            .zip_eq_fast(array_builders.iter_mut())
        {
            array_builder.append_datum(&datum_result.map_err(ArrayError::internal)?);
        }
        Ok(())
    }

    fn null_bitmap(&self) -> &Self::Bitmap {
        &self.null_bitmap
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;

    use itertools::Itertools;

    use super::*;
    use crate::array;
    use crate::array::column::Column;
    use crate::array::{
        BoolArray, DataChunk, DataChunkTestExt, DateArray, DecimalArray, F32Array, F64Array,
        I16Array, I32Array, I32ArrayBuilder, I64Array, TimeArray, TimestampArray, Utf8Array,
    };
    use crate::hash::{
        HashKey, Key128, Key16, Key256, Key32, Key64, KeySerialized, PrecomputedBuildHasher,
    };
    use crate::test_utils::rand_array::seed_rand_array_ref;
    use crate::types::Datum;

    #[derive(Hash, PartialEq, Eq)]
    struct Row(Vec<Datum>);

    fn generate_random_data_chunk() -> (DataChunk, Vec<DataType>) {
        let capacity = 128;
        let seed = 10244021u64;
        let columns = vec![
            Column::new(seed_rand_array_ref::<BoolArray>(capacity, seed, 0.5)),
            Column::new(seed_rand_array_ref::<I16Array>(capacity, seed + 1, 0.5)),
            Column::new(seed_rand_array_ref::<I32Array>(capacity, seed + 2, 0.5)),
            Column::new(seed_rand_array_ref::<I64Array>(capacity, seed + 3, 0.5)),
            Column::new(seed_rand_array_ref::<F32Array>(capacity, seed + 4, 0.5)),
            Column::new(seed_rand_array_ref::<F64Array>(capacity, seed + 5, 0.5)),
            Column::new(seed_rand_array_ref::<DecimalArray>(capacity, seed + 6, 0.5)),
            Column::new(seed_rand_array_ref::<Utf8Array>(capacity, seed + 7, 0.5)),
            Column::new(seed_rand_array_ref::<DateArray>(capacity, seed + 8, 0.5)),
            Column::new(seed_rand_array_ref::<TimeArray>(capacity, seed + 9, 0.5)),
            Column::new(seed_rand_array_ref::<TimestampArray>(
                capacity,
                seed + 10,
                0.5,
            )),
        ];
        let types = vec![
            DataType::Boolean,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::Float32,
            DataType::Float64,
            DataType::Decimal,
            DataType::Varchar,
            DataType::Date,
            DataType::Time,
            DataType::Timestamp,
        ];

        (DataChunk::new(columns, capacity), types)
    }

    fn do_test_serialize<K: HashKey, F>(column_indexes: Vec<usize>, data_gen: F)
    where
        F: FnOnce() -> DataChunk,
    {
        let data = data_gen();

        let mut actual_row_id_mapping = HashMap::<usize, Vec<usize>>::with_capacity(100);
        {
            let mut fast_hash_map =
                HashMap::<K, Vec<usize>, PrecomputedBuildHasher>::with_capacity_and_hasher(
                    100,
                    PrecomputedBuildHasher,
                );
            let keys =
                K::build(column_indexes.as_slice(), &data).expect("Failed to build hash keys");

            for (row_id, key) in keys.into_iter().enumerate() {
                let row_ids = fast_hash_map.entry(key).or_default();
                row_ids.push(row_id);
            }

            for row_ids in fast_hash_map.values() {
                for row_id in row_ids {
                    actual_row_id_mapping.insert(*row_id, row_ids.clone());
                }
            }
        }

        let mut expected_row_id_mapping = HashMap::<usize, Vec<usize>>::with_capacity(100);
        {
            let mut normal_hash_map: HashMap<Row, Vec<usize>> = HashMap::with_capacity(100);
            for row_idx in 0..data.capacity() {
                let row = column_indexes
                    .iter()
                    .map(|col_idx| data.column_at(*col_idx))
                    .map(|col| col.array_ref().datum_at(row_idx))
                    .collect::<Vec<Datum>>();

                normal_hash_map.entry(Row(row)).or_default().push(row_idx);
            }

            for row_ids in normal_hash_map.values() {
                for row_id in row_ids {
                    expected_row_id_mapping.insert(*row_id, row_ids.clone());
                }
            }
        }

        assert_eq!(expected_row_id_mapping, actual_row_id_mapping);
    }

    fn do_test_deserialize<K: HashKey, F>(column_indexes: Vec<usize>, data_gen: F)
    where
        F: FnOnce() -> (DataChunk, Vec<DataType>),
    {
        let (data, types) = data_gen();
        let keys = K::build(column_indexes.as_slice(), &data).expect("Failed to build hash keys");

        let mut array_builders = column_indexes
            .iter()
            .map(|idx| data.columns()[*idx].array_ref().create_builder(1024))
            .collect::<Vec<ArrayBuilderImpl>>();
        let key_types: Vec<_> = column_indexes
            .iter()
            .map(|idx| types[*idx].clone())
            .collect();

        keys.into_iter()
            .try_for_each(|k| k.deserialize_to_builders(&mut array_builders[..], &key_types))
            .expect("Failed to deserialize!");

        let result_arrays = array_builders
            .into_iter()
            .map(|array_builder| array_builder.finish())
            .collect::<Vec<ArrayImpl>>();

        for (ret_idx, col_idx) in column_indexes.iter().enumerate() {
            assert_eq!(
                data.columns()[*col_idx].array_ref(),
                &result_arrays[ret_idx]
            );
        }
    }

    fn do_test<K: HashKey, F>(column_indexes: Vec<usize>, data_gen: F)
    where
        F: FnOnce() -> (DataChunk, Vec<DataType>),
    {
        let (data, types) = data_gen();

        let data1 = data.clone();
        do_test_serialize::<K, _>(column_indexes.clone(), move || data1);
        do_test_deserialize::<K, _>(column_indexes, move || (data, types));
    }

    #[test]
    fn test_two_bytes_hash_key() {
        do_test::<Key16, _>(vec![1], generate_random_data_chunk);
    }

    #[test]
    fn test_four_bytes_hash_key() {
        do_test::<Key32, _>(vec![0, 1], generate_random_data_chunk);
        do_test::<Key32, _>(vec![2], generate_random_data_chunk);
        do_test::<Key32, _>(vec![4], generate_random_data_chunk);
    }

    #[test]
    fn test_eight_bytes_hash_key() {
        do_test::<Key64, _>(vec![1, 2], generate_random_data_chunk);
        do_test::<Key64, _>(vec![0, 1, 2], generate_random_data_chunk);
        do_test::<Key64, _>(vec![3], generate_random_data_chunk);
        do_test::<Key64, _>(vec![5], generate_random_data_chunk);
    }

    #[test]
    fn test_128_bits_hash_key() {
        do_test::<Key128, _>(vec![3, 5], generate_random_data_chunk);
        do_test::<Key128, _>(vec![6], generate_random_data_chunk);
    }

    #[test]
    fn test_256_bits_hash_key() {
        do_test::<Key256, _>(vec![3, 5, 6], generate_random_data_chunk);
        do_test::<Key256, _>(vec![3, 6], generate_random_data_chunk);
    }

    #[test]
    fn test_var_length_hash_key() {
        do_test::<KeySerialized, _>(vec![0, 7], generate_random_data_chunk);
    }

    fn generate_decimal_test_data() -> (DataChunk, Vec<DataType>) {
        let columns = vec![array! { DecimalArray, [
            Some(Decimal::from_str("1.2").unwrap()),
            None,
            Some(Decimal::from_str("1.200").unwrap()),
            Some(Decimal::from_str("0.00").unwrap()),
            Some(Decimal::from_str("0.0").unwrap())
        ]}
        .into()];
        let types = vec![DataType::Decimal];

        (DataChunk::new(columns, 5), types)
    }

    #[test]
    fn test_decimal_hash_key_serialization() {
        do_test::<Key128, _>(vec![0], generate_decimal_test_data);
    }

    // Simple test to ensure a row <None, Some(2)> will be serialized and restored
    // losslessly.
    #[test]
    fn test_simple_hash_key_nullable_serde() {
        let keys = Key64::build(
            &[0, 1],
            &DataChunk::from_pretty(
                "i i
                 1 .
                 . 2",
            ),
        )
        .unwrap();

        let mut array_builders = [0, 1]
            .iter()
            .map(|_| ArrayBuilderImpl::Int32(I32ArrayBuilder::new(2)))
            .collect::<Vec<_>>();

        keys.into_iter().for_each(|k: Key64| {
            k.deserialize_to_builders(&mut array_builders[..], &[DataType::Int32, DataType::Int32])
                .unwrap()
        });

        let array = array_builders.pop().unwrap().finish();
        let i32_vec = array
            .iter()
            .map(|opt| opt.map(|s| s.into_int32()))
            .collect_vec();
        assert_eq!(i32_vec, vec![None, Some(2)]);
    }
}
