use std::convert::TryFrom;

use bytes::{Buf, BufMut};
use risingwave_pb::data::DataType as ProstDataType;
use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, Result, RwError};
mod native_type;

mod scalar_impl;
use std::fmt::Debug;

pub use native_type::*;
use risingwave_pb::data::data_type::TypeName;
pub use scalar_impl::*;

mod decimal;
pub mod interval;

mod ordered_float;
pub use decimal::Decimal;
pub use interval::*;
pub use ordered_float::IntoOrdered;
use paste::paste;

use crate::array::{ArrayBuilderImpl, PrimitiveArrayItemType, StructValue};

pub type OrderedF32 = ordered_float::OrderedFloat<f32>;
pub type OrderedF64 = ordered_float::OrderedFloat<f64>;

// TODO(eric): rename to `DataType` once migration done
#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
pub enum DataTypeKind {
    Boolean,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
    Decimal { prec: u32, scale: u32 },
    Date,
    Char,
    Varchar,
    Time,
    Timestamp,
    Timestampz,
    Interval,
    Struct,
}

/// Number of bytes of one element in array of [`DataType`].
pub enum DataSize {
    /// For types with fixed size, e.g. int, float.
    Fixed(usize),
    /// For types with variable size, e.g. string.
    Variable,
}

impl From<&ProstDataType> for DataTypeKind {
    fn from(proto: &ProstDataType) -> DataTypeKind {
        match proto.get_type_name().expect("unsupported type") {
            TypeName::Int16 => DataTypeKind::Int16,
            TypeName::Int32 => DataTypeKind::Int32,
            TypeName::Int64 => DataTypeKind::Int64,
            TypeName::Float => DataTypeKind::Float32,
            TypeName::Double => DataTypeKind::Float64,
            TypeName::Boolean => DataTypeKind::Boolean,
            TypeName::Char => DataTypeKind::Char,
            TypeName::Varchar => DataTypeKind::Varchar,
            TypeName::Date => DataTypeKind::Date,
            TypeName::Time => DataTypeKind::Time,
            TypeName::Timestamp => DataTypeKind::Timestamp,
            TypeName::Timestampz => DataTypeKind::Timestampz,
            TypeName::Decimal => DataTypeKind::Decimal {
                prec: proto.precision,
                scale: proto.scale,
            },
            TypeName::Interval => DataTypeKind::Interval,
            TypeName::Symbol => DataTypeKind::Varchar,
            TypeName::Struct => DataTypeKind::Struct,
        }
    }
}

impl DataTypeKind {
    pub fn create_array_builder(&self, capacity: usize) -> Result<ArrayBuilderImpl> {
        use crate::array::*;
        Ok(match self {
            DataTypeKind::Boolean => BoolArrayBuilder::new(capacity)?.into(),
            DataTypeKind::Int16 => PrimitiveArrayBuilder::<i16>::new(capacity)?.into(),
            DataTypeKind::Int32 => PrimitiveArrayBuilder::<i32>::new(capacity)?.into(),
            DataTypeKind::Int64 => PrimitiveArrayBuilder::<i64>::new(capacity)?.into(),
            DataTypeKind::Float32 => PrimitiveArrayBuilder::<OrderedF32>::new(capacity)?.into(),
            DataTypeKind::Float64 => PrimitiveArrayBuilder::<OrderedF64>::new(capacity)?.into(),
            DataTypeKind::Decimal { prec: _, scale: _ } => {
                DecimalArrayBuilder::new(capacity)?.into()
            }
            DataTypeKind::Date => PrimitiveArrayBuilder::<i32>::new(capacity)?.into(),
            DataTypeKind::Char | DataTypeKind::Varchar => Utf8ArrayBuilder::new(capacity)?.into(),
            DataTypeKind::Time => PrimitiveArrayBuilder::<i64>::new(capacity)?.into(),
            DataTypeKind::Timestamp => PrimitiveArrayBuilder::<i64>::new(capacity)?.into(),
            DataTypeKind::Timestampz => PrimitiveArrayBuilder::<i64>::new(capacity)?.into(),
            DataTypeKind::Interval => IntervalArrayBuilder::new(capacity)?.into(),
            DataTypeKind::Struct => StructArrayBuilder::new(capacity)?.into(),
        })
    }

    fn prost_type_name(&self) -> TypeName {
        match self {
            DataTypeKind::Int16 => TypeName::Int16,
            DataTypeKind::Int32 => TypeName::Int32,
            DataTypeKind::Int64 => TypeName::Int64,
            DataTypeKind::Float32 => TypeName::Float,
            DataTypeKind::Float64 => TypeName::Double,
            DataTypeKind::Boolean => TypeName::Boolean,
            DataTypeKind::Char => TypeName::Char,
            DataTypeKind::Varchar => TypeName::Varchar,
            DataTypeKind::Date => TypeName::Date,
            DataTypeKind::Time => TypeName::Time,
            DataTypeKind::Timestamp => TypeName::Timestamp,
            DataTypeKind::Timestampz => TypeName::Timestampz,
            DataTypeKind::Decimal { .. } => TypeName::Decimal,
            DataTypeKind::Interval => TypeName::Interval,
            DataTypeKind::Struct => TypeName::Struct,
        }
    }

    pub fn to_protobuf(&self) -> Result<ProstDataType> {
        Ok(ProstDataType {
            type_name: self.prost_type_name() as i32,
            is_nullable: true,
            ..Default::default()
        })
    }

    pub fn data_size(&self) -> DataSize {
        use std::mem::size_of;
        match self {
            DataTypeKind::Boolean => DataSize::Variable,
            DataTypeKind::Int16 => DataSize::Fixed(size_of::<i16>()),
            DataTypeKind::Int32 => DataSize::Fixed(size_of::<i32>()),
            DataTypeKind::Int64 => DataSize::Fixed(size_of::<i64>()),
            DataTypeKind::Float32 => DataSize::Fixed(size_of::<OrderedF32>()),
            DataTypeKind::Float64 => DataSize::Fixed(size_of::<OrderedF64>()),
            DataTypeKind::Decimal { .. } => DataSize::Fixed(16),
            DataTypeKind::Char => DataSize::Variable,
            DataTypeKind::Varchar => DataSize::Variable,
            DataTypeKind::Date => DataSize::Fixed(size_of::<i32>()),
            DataTypeKind::Time => DataSize::Fixed(size_of::<i64>()),
            DataTypeKind::Timestamp => DataSize::Fixed(size_of::<i64>()),
            DataTypeKind::Timestampz => DataSize::Fixed(size_of::<i64>()),
            DataTypeKind::Interval => DataSize::Variable,
            DataTypeKind::Struct => DataSize::Variable,
        }
    }

    pub fn decimal_default() -> DataTypeKind {
        DataTypeKind::Decimal { prec: 20, scale: 6 }
    }
}

/// `Scalar` is a trait over all possible owned types in the evaluation
/// framework.
///
/// `Scalar` is reciprocal to `ScalarRef`. Use `as_scalar_ref` to get a
/// reference which has the same lifetime as `self`.
pub trait Scalar:
    std::fmt::Debug
    + Send
    + Sync
    + 'static
    + Clone
    + std::fmt::Debug
    + TryFrom<ScalarImpl, Error = RwError>
    + Into<ScalarImpl>
{
    /// Type for reference of `Scalar`
    type ScalarRefType<'a>: ScalarRef<'a, ScalarType = Self> + 'a
    where
        Self: 'a;

    /// Get a reference to current scalar.
    fn as_scalar_ref(&self) -> Self::ScalarRefType<'_>;

    fn to_scalar_value(self) -> ScalarImpl;
}

/// Convert an `Option<Scalar>` to corresponding `Option<ScalarRef>`.
pub fn option_as_scalar_ref<S: Scalar>(scalar: &Option<S>) -> Option<S::ScalarRefType<'_>> {
    scalar.as_ref().map(|x| x.as_scalar_ref())
}

/// Convert an `Option<ScalarRef>` to corresponding `Option<Scalar>`.
pub fn option_to_owned_scalar<S: Scalar>(scalar: &Option<S::ScalarRefType<'_>>) -> Option<S> {
    scalar.map(|x| x.to_owned_scalar())
}

/// `ScalarRef` is a trait over all possible references in the evaluation
/// framework.
///
/// `ScalarRef` is reciprocal to `Scalar`. Use `to_owned_scalar` to get an
/// owned scalar.
pub trait ScalarRef<'a>:
    Copy + std::fmt::Debug + 'a + TryFrom<ScalarRefImpl<'a>, Error = RwError> + Into<ScalarRefImpl<'a>>
{
    /// `ScalarType` is the owned type of current `ScalarRef`.
  #[rustfmt::skip]
  // rustfmt will incorrectly remove GAT lifetime.
  type ScalarType: Scalar<ScalarRefType<'a> = Self>;

    /// Convert `ScalarRef` to an owned scalar.
    fn to_owned_scalar(&self) -> Self::ScalarType;
}

/// `for_all_scalar_variants` includes all variants of our scalar types. If you added a new scalar
/// type inside the project, be sure to add a variant here.
///
/// Every tuple has four elements, where
/// `{ enum variant name, function suffix name, scalar type, scalar ref type }`
#[macro_export]
macro_rules! for_all_scalar_variants {
  ($macro:tt $(, $x:tt)*) => {
    $macro! {
      [$($x),*],
      { Int16, int16, i16, i16 },
      { Int32, int32, i32, i32 },
      { Int64, int64, i64, i64 },
      { Float32, float32, OrderedF32, OrderedF32 },
      { Float64, float64, OrderedF64, OrderedF64 },
      { Utf8, utf8, String, &'scalar str },
      { Bool, bool, bool, bool },
      { Decimal, decimal, Decimal, Decimal  },
      { Interval, interval, IntervalUnit, IntervalUnit },
      { Struct, struct, StructValue, StructValue }
    }
  };
}

/// Define `ScalarImpl` and `ScalarRefImpl` with macro.
macro_rules! scalar_impl_enum {
  ([], $( { $variant_name:ident, $suffix_name:ident, $scalar:ty, $scalar_ref:ty } ),*) => {
    /// `ScalarImpl` embeds all possible scalars in the evaluation framework.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub enum ScalarImpl {
      $( $variant_name($scalar) ),*
    }

    /// `ScalarRefImpl` embeds all possible scalar references in the evaluation
    /// framework.
    #[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub enum ScalarRefImpl<'scalar> {
      $( $variant_name($scalar_ref) ),*
    }
  };
}

for_all_scalar_variants! { scalar_impl_enum }

pub type Datum = Option<ScalarImpl>;
pub type DatumRef<'a> = Option<ScalarRefImpl<'a>>;

// TODO: specify `NULL FIRST` or `NULL LAST`.
pub fn serialize_datum_ref_into(
    datum_ref: &DatumRef,
    serializer: &mut memcomparable::Serializer<impl BufMut>,
) -> memcomparable::Result<()> {
    if let Some(datum_ref) = datum_ref {
        1u8.serialize(&mut *serializer)?;
        datum_ref.serialize(serializer)?;
    } else {
        0u8.serialize(serializer)?;
    }
    Ok(())
}

pub fn serialize_datum_ref_not_null_into(
    datum_ref: &DatumRef,
    serializer: &mut memcomparable::Serializer<impl BufMut>,
) -> memcomparable::Result<()> {
    datum_ref
        .as_ref()
        .expect("datum cannot be null")
        .serialize(serializer)
}

// TODO(MrCroxx): turn Datum into a struct, and impl ser/de as its member functions.
// TODO: specify `NULL FIRST` or `NULL LAST`.
pub fn serialize_datum_into(
    datum: &Datum,
    serializer: &mut memcomparable::Serializer<impl BufMut>,
) -> memcomparable::Result<()> {
    if let Some(datum) = datum {
        1u8.serialize(&mut *serializer)?;
        datum.serialize(serializer)?;
    } else {
        0u8.serialize(serializer)?;
    }
    Ok(())
}

// TODO(MrCroxx): turn Datum into a struct, and impl ser/de as its member functions.
pub fn serialize_datum_not_null_into(
    datum: &Datum,
    serializer: &mut memcomparable::Serializer<impl BufMut>,
) -> memcomparable::Result<()> {
    datum
        .as_ref()
        .expect("datum cannot be null")
        .serialize(serializer)
}

// TODO(MrCroxx): turn Datum into a struct, and impl ser/de as its member functions.
pub fn deserialize_datum_from(
    ty: &DataTypeKind,
    deserializer: &mut memcomparable::Deserializer<impl Buf>,
) -> memcomparable::Result<Datum> {
    let null_tag = u8::deserialize(&mut *deserializer)?;
    match null_tag {
        0 => Ok(None),
        1 => Ok(Some(ScalarImpl::deserialize(*ty, deserializer)?)),
        _ => Err(memcomparable::Error::InvalidTagEncoding(null_tag as _)),
    }
}

// TODO(MrCroxx): turn Datum into a struct, and impl ser/de as its member functions.
pub fn deserialize_datum_not_null_from(
    ty: DataTypeKind,
    deserializer: &mut memcomparable::Deserializer<impl Buf>,
) -> memcomparable::Result<Datum> {
    Ok(Some(ScalarImpl::deserialize(ty, deserializer)?))
}

/// This trait is to implement `to_owned_datum` for `Option<ScalarImpl>`
pub trait ToOwnedDatum {
    /// implement `to_owned_datum` for `DatumRef` to convert to `Datum`
    fn to_owned_datum(self) -> Datum;
}

impl ToOwnedDatum for DatumRef<'_> {
    fn to_owned_datum(self) -> Datum {
        self.map(ScalarRefImpl::into_scalar_impl)
    }
}

/// `for_all_native_types` includes all native variants of our scalar types.
#[macro_export]
macro_rules! for_all_native_types {
($macro:tt $(, $x:tt)*) => {
    $macro! {
      [$($x),*],
      { i16, Int16 },
      { i32, Int32 },
      { i64, Int64 },
      { $crate::types::OrderedF32, Float32 },
      { $crate::types::OrderedF64, Float64 }
    }
  };
}

/// `impl_convert` implements several conversions for `Scalar`.
/// * `Scalar <-> ScalarImpl` with `From` and `TryFrom` trait.
/// * `ScalarRef <-> ScalarRefImpl` with `From` and `TryFrom` trait.
/// * `&ScalarImpl -> &Scalar` with `impl.as_int16()`.
/// * `ScalarImpl -> Scalar` with `impl.into_int16()`.
macro_rules! impl_convert {
  ([], $( { $variant_name:ident, $suffix_name:ident, $scalar:ty, $scalar_ref:ty } ),*) => {
    $(
      impl From<$scalar> for ScalarImpl {
        fn from(val: $scalar) -> Self {
          ScalarImpl::$variant_name(val)
        }
      }

      impl TryFrom<ScalarImpl> for $scalar {
        type Error = RwError;

        fn try_from(val: ScalarImpl) -> Result<Self> {
          match val {
            ScalarImpl::$variant_name(scalar) => Ok(scalar),
            other_scalar => Err(ErrorCode::InternalError(
              format!("cannot convert ScalarImpl::{} to concrete type", other_scalar.get_ident())
            ).into())
          }
        }
      }

      impl <'scalar> From<$scalar_ref> for ScalarRefImpl<'scalar> {
        fn from(val: $scalar_ref) -> Self {
          ScalarRefImpl::$variant_name(val)
        }
      }

      impl <'scalar> TryFrom<ScalarRefImpl<'scalar>> for $scalar_ref {
        type Error = RwError;

        fn try_from(val: ScalarRefImpl<'scalar>) -> Result<Self> {
          match val {
            ScalarRefImpl::$variant_name(scalar_ref) => Ok(scalar_ref),
            other_scalar => Err(ErrorCode::InternalError(
              format!("cannot convert ScalarRefImpl::{} to concrete type", other_scalar.get_ident())
            ).into())
          }
        }
      }

      paste! {
        impl ScalarImpl {
          pub fn [<as_ $suffix_name>](&self) -> &$scalar {
            match self {
              Self::$variant_name(ref scalar) => scalar,
              other_scalar => panic!("cannot convert ScalarImpl::{} to concrete type", other_scalar.get_ident())
            }
          }

          pub fn [<into_ $suffix_name>](self) -> $scalar {
            match self {
              Self::$variant_name(scalar) => scalar,
              other_scalar =>  panic!("cannot convert ScalarImpl::{} to concrete type", other_scalar.get_ident())
            }
          }
        }

        impl <'scalar> ScalarRefImpl<'scalar> {
          // Note that this conversion consume self.
          pub fn [<into_ $suffix_name>](self) -> $scalar_ref {
            match self {
              Self::$variant_name(inner) => inner,
              other_scalar => panic!("cannot convert ScalarRefImpl::{} to concrete type", other_scalar.get_ident())
            }
          }
        }
      }
    )*
  };
}

for_all_scalar_variants! { impl_convert }

// Implement `From<raw float>` for `ScalarImpl` manually/
impl From<f32> for ScalarImpl {
    fn from(f: f32) -> Self {
        Self::Float32(f.into())
    }
}
impl From<f64> for ScalarImpl {
    fn from(f: f64) -> Self {
        Self::Float64(f.into())
    }
}

macro_rules! impl_scalar_impl_ref_conversion {
 ([], $( { $variant_name:ident, $suffix_name:ident, $scalar:ty, $scalar_ref:ty } ),*) => {
   impl ScalarImpl {
     /// Converts [`ScalarImpl`] to [`ScalarRefImpl`]
     pub fn as_scalar_ref_impl(&self) -> ScalarRefImpl<'_> {
       match self {
         $(
           Self::$variant_name(inner) => ScalarRefImpl::<'_>::$variant_name(inner.as_scalar_ref())
         ), *
       }
     }
   }

   impl<'a> ScalarRefImpl<'a> {
     /// Converts [`ScalarRefImpl`] to [`ScalarImpl`]
     pub fn into_scalar_impl(self) -> ScalarImpl {
       match self {
         $(
           Self::$variant_name(inner) => ScalarImpl::$variant_name(inner.to_owned_scalar())
         ), *
       }
     }
   }
};
}

for_all_scalar_variants! { impl_scalar_impl_ref_conversion }

// FIXME: should implement Hash and Eq all by deriving
// TODO: may take type information into consideration later
#[allow(clippy::derive_hash_xor_eq)]
impl std::hash::Hash for ScalarImpl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        macro_rules! impl_all_hash {
      ([$self:ident], $({ $variant_type:ty, $scalar_type:ident } ),*) => {
        match $self {
          $( Self::$scalar_type(inner) => {
            inner.hash_wrapper(state);
          }, )*
          Self::Bool(b) => b.hash(state),
          Self::Utf8(s) => s.hash(state),
          Self::Decimal(decimal) => decimal.hash(state),
          Self::Interval(interval) => interval.hash(state),
          Self::Struct(v) => v.hash(state),
        }
      };
    }
        for_all_native_types! { impl_all_hash, self }
    }
}

impl ToString for ScalarImpl {
    fn to_string(&self) -> String {
        macro_rules! impl_to_string {
      ([], $( { $variant_name:ident, $suffix_name:ident, $scalar:ty, $scalar_ref:ty } ),*) => {
        match self {
          $( Self::$variant_name(ref inner) => {
            inner.to_string()
          }, )*
        }
      }
    }

        for_all_scalar_variants! { impl_to_string }
    }
}

impl ToString for ScalarRefImpl<'_> {
    fn to_string(&self) -> String {
        macro_rules! impl_to_string {
      ([], $( { $variant_name:ident, $suffix_name:ident, $scalar:ty, $scalar_ref:ty } ),*) => {
        match self {
          $( Self::$variant_name(inner) => {
            inner.to_string()
          }, )*
        }
      }
    }

        for_all_scalar_variants! { impl_to_string }
    }
}

impl ScalarRefImpl<'_> {
    /// Serialize the scalar.
    pub fn serialize(
        &self,
        ser: &mut memcomparable::Serializer<impl BufMut>,
    ) -> memcomparable::Result<()> {
        match self {
            &Self::Int16(v) => v.serialize(ser)?,
            &Self::Int32(v) => v.serialize(ser)?,
            &Self::Int64(v) => v.serialize(ser)?,
            &Self::Float32(v) => v.serialize(ser)?,
            &Self::Float64(v) => v.serialize(ser)?,
            &Self::Utf8(v) => v.serialize(ser)?,
            &Self::Bool(v) => v.serialize(ser)?,
            &Self::Decimal(v) => ser.serialize_decimal(v.mantissa(), v.scale() as u8)?,
            Self::Interval(v) => v.serialize(ser)?,
            Self::Struct(v) => v.serialize(ser)?,
        };
        Ok(())
    }
}

impl ScalarImpl {
    /// Serialize the scalar.
    pub fn serialize(
        &self,
        ser: &mut memcomparable::Serializer<impl BufMut>,
    ) -> memcomparable::Result<()> {
        self.as_scalar_ref_impl().serialize(ser)
    }

    /// Deserialize the scalar.
    pub fn deserialize(
        ty: DataTypeKind,
        de: &mut memcomparable::Deserializer<impl Buf>,
    ) -> memcomparable::Result<Self> {
        use DataTypeKind as Ty;
        Ok(match ty {
            Ty::Int16 => Self::Int16(i16::deserialize(de)?),
            Ty::Int32 => Self::Int32(i32::deserialize(de)?),
            Ty::Int64 => Self::Int64(i64::deserialize(de)?),
            Ty::Float32 => Self::Float32(f32::deserialize(de)?.into()),
            Ty::Float64 => Self::Float64(f64::deserialize(de)?.into()),
            Ty::Char | Ty::Varchar => Self::Utf8(String::deserialize(de)?),
            Ty::Boolean => Self::Bool(bool::deserialize(de)?),
            Ty::Decimal { .. } => Self::Decimal({
                let (mantissa, scale) = de.deserialize_decimal()?;
                Decimal::from_i128_with_scale(mantissa, scale as u32)
            }),
            Ty::Interval => Self::Interval(IntervalUnit::deserialize(de)?),
            Ty::Time | Ty::Timestamp | Ty::Timestampz => Self::Int64(i64::deserialize(de)?),
            Ty::Date => Self::Int32(i32::deserialize(de)?),
            Ty::Struct => Self::Struct(StructValue::deserialize(de)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Neg;

    use itertools::Itertools;
    use rand::thread_rng;

    use super::*;

    fn serialize_datum_not_null_into_vec(data: i64) -> Vec<u8> {
        let mut serializer = memcomparable::Serializer::new(vec![]);
        serialize_datum_not_null_into(&Some(ScalarImpl::Int64(data)), &mut serializer).unwrap();
        serializer.into_inner()
    }

    #[test]
    fn test_memcomparable() {
        let memcmp_minus_1 = serialize_datum_not_null_into_vec(-1);
        let memcmp_3874 = serialize_datum_not_null_into_vec(3874);
        let memcmp_45745 = serialize_datum_not_null_into_vec(45745);
        let memcmp_21234 = serialize_datum_not_null_into_vec(21234);
        assert!(memcmp_3874 < memcmp_45745);
        assert!(memcmp_3874 < memcmp_21234);
        assert!(memcmp_21234 < memcmp_45745);

        assert!(memcmp_minus_1 < memcmp_3874);
        assert!(memcmp_minus_1 < memcmp_21234);
        assert!(memcmp_minus_1 < memcmp_45745);
    }

    #[test]
    fn test_issue_2057_ordered_float_memcomparable() {
        use num_traits::*;
        use rand::seq::SliceRandom;

        fn serialize(f: OrderedF32) -> Vec<u8> {
            let mut serializer = memcomparable::Serializer::new(vec![]);
            serialize_datum_not_null_into(&Some(f.into()), &mut serializer).unwrap();
            serializer.into_inner()
        }

        fn deserialize(data: Vec<u8>) -> OrderedF32 {
            let mut deserializer = memcomparable::Deserializer::new(data.as_slice());
            let datum =
                deserialize_datum_not_null_from(DataTypeKind::Float32, &mut deserializer).unwrap();
            datum.unwrap().try_into().unwrap()
        }

        let floats = vec![
            // -inf
            OrderedF32::neg_infinity(),
            // -1
            OrderedF32::one().neg(),
            // 0, -0 should be treated the same
            OrderedF32::zero(),
            OrderedF32::neg_zero(),
            OrderedF32::zero(),
            // 1
            OrderedF32::one(),
            // inf
            OrderedF32::infinity(),
            // nan, -nan should be treated the same
            OrderedF32::nan(),
            OrderedF32::nan().neg(),
            OrderedF32::nan(),
        ];
        assert!(floats.is_sorted());

        let mut floats_clone = floats.clone();
        floats_clone.shuffle(&mut thread_rng());
        floats_clone.sort();
        assert_eq!(floats, floats_clone);

        let memcomparables = floats.clone().into_iter().map(serialize).collect_vec();
        assert!(memcomparables.is_sorted());

        let decoded_floats = memcomparables.into_iter().map(deserialize).collect_vec();
        assert!(decoded_floats.is_sorted());
        assert_eq!(floats, decoded_floats);
    }
}
