use arrow::array::{
    make_array, Array, ArrayBuilder, ArrayData, ArrayDataBuilder, ArrayRef, BooleanBuilder,
    LargeStringArray, ListBuilder, NullArray, OffsetSizeTrait, PrimitiveArray, StringArray,
    StringBuilder, StringDictionaryBuilder,
};

use apache_avro::schema::RecordSchema;
use apache_avro::{
    schema::{Schema as AvroSchema, SchemaKind},
    types::Value as AvroValue,
    AvroResult, Error as AvroError,
};
use arrow::array::{BinaryArray, FixedSizeBinaryArray, GenericListArray};
use arrow::buffer::{Buffer, MutableBuffer};
use arrow::datatypes::{
    ArrowDictionaryKeyType, ArrowNumericType, ArrowPrimitiveType, DataType, Date32Type, Date64Type,
    Field, Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, Schema,
    Time32MillisecondType, Time32SecondType, Time64MicrosecondType, Time64NanosecondType, TimeUnit,
    TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType, UInt16Type, UInt32Type, UInt64Type, UInt8Type,
};
use arrow::datatypes::{Fields, SchemaRef};
use arrow::error::ArrowError;
use arrow::error::ArrowError::SchemaError;
use arrow::error::Result as ArrowResult;
use arrow::record_batch::RecordBatch;
use arrow::util::bit_util;
use arroyo_rpc::schema_resolver::SchemaResolver;
use datafusion::common::Result as DFResult;
use datafusion::error::DataFusionError;
use num_traits::NumCast;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Mutex;

type RecordSlice<'a> = &'a [&'a Vec<(String, AvroValue)>];

pub(crate) struct AvroDecoder {
    pub(crate) buffer: Vec<(u32, AvroValue)>,
    pub(crate) lookup: HashMap<u32, BTreeMap<String, usize>>,
    pub(crate) schema: SchemaRef,

    pub(crate) schema_registry: Arc<Mutex<HashMap<u32, AvroSchema>>>,
    pub(crate) schema_resolver: Arc<dyn SchemaResolver + Sync>,
}

pub fn schema_lookup(schema: AvroSchema) -> DFResult<BTreeMap<String, usize>> {
    match schema {
        AvroSchema::Record(RecordSchema {
            fields, mut lookup, ..
        }) => {
            for field in fields {
                child_schema_lookup(&field.name, &field.schema, &mut lookup)?;
            }
            Ok(lookup)
        }
        _ => Err(DataFusionError::ArrowError(
            SchemaError("expected avro schema to be a record".to_string()),
            None,
        )),
    }
}

fn child_schema_lookup<'b>(
    parent_field_name: &str,
    schema: &AvroSchema,
    schema_lookup: &'b mut BTreeMap<String, usize>,
) -> DFResult<&'b BTreeMap<String, usize>> {
    match schema {
        AvroSchema::Union(us) => {
            let has_nullable = us
                .find_schema_with_known_schemata::<apache_avro::Schema>(
                    &AvroValue::Null,
                    None,
                    &None,
                )
                .is_some();
            let sub_schemas = us.variants();
            if has_nullable && sub_schemas.len() == 2 {
                if let Some(sub_schema) =
                    sub_schemas.iter().find(|&s| !matches!(s, AvroSchema::Null))
                {
                    child_schema_lookup(parent_field_name, sub_schema, schema_lookup)?;
                }
            }
        }
        AvroSchema::Record(RecordSchema { fields, lookup, .. }) => {
            lookup.iter().for_each(|(field_name, pos)| {
                schema_lookup.insert(format!("{}.{}", parent_field_name, field_name), *pos);
            });

            for field in fields {
                let sub_parent_field_name = format!("{}.{}", parent_field_name, field.name);
                child_schema_lookup(&sub_parent_field_name, &field.schema, schema_lookup)?;
            }
        }
        AvroSchema::Array(schema) => {
            let sub_parent_field_name = format!("{}.element", parent_field_name);
            child_schema_lookup(&sub_parent_field_name, schema, schema_lookup)?;
        }
        _ => (),
    }
    Ok(schema_lookup)
}

impl AvroDecoder {
    pub fn append_value(&mut self, value: (u32, AvroValue)) {
        self.buffer.push(value)
    }

    pub fn flush(&mut self) -> ArrowResult<Option<RecordBatch>> {
        Ok(None)
    }
}

impl AvroDecoder {
    /// Read the next batch of records
    pub fn next_batch(&mut self, _batch_size: usize) -> Option<ArrowResult<RecordBatch>> {
        // 将错误转移到avro decode上
        let rows_result = self
            .buffer
            .iter()
            .map(|(_, value)| match value {
                //Ok(Value::Record(v)) => Ok(v),
                //Err(e) => Err(ArrowError::ParseError(format!(
                //"Failed to parse avro value: {e:?}"
                //))),
                //other => Err(ArrowError::ParseError(format!(
                //"Row needs to be of type object, got: {other:?}"
                //))),
                AvroValue::Record(v) => v,
                other => panic!("Row needs to be of type object, got: {other:?}"),
            })
            .collect::<Vec<&Vec<(String, AvroValue)>>>();
        let rows = rows_result;

        //let rows = match rows_result {
        //// Return error early
        //Err(e) => return Some(Err(e)),
        //// No rows: return None early
        //Ok(rows) if rows.is_empty() => return None,
        //Ok(rows) => rows,
        //};

        //let rows = rows.iter().collect::<Vec<&Vec<(String, Value)>>>();
        let arrays = self.build_struct_array(&rows, "", self.schema.fields());
        let projected_fields = self.schema.fields().clone();
        let projected_schema = Arc::new(Schema::new(projected_fields));
        Some(arrays.and_then(|arr| RecordBatch::try_new(projected_schema, arr)))
    }

    fn build_boolean_array(&self, rows: RecordSlice, col_name: &str) -> ArrayRef {
        let mut builder = BooleanBuilder::with_capacity(rows.len());
        for row in rows {
            if let Some(value) = self.field_lookup(col_name, row) {
                if let Some(boolean) = resolve_boolean(value) {
                    builder.append_value(boolean)
                } else {
                    builder.append_null();
                }
            } else {
                builder.append_null();
            }
        }
        Arc::new(builder.finish())
    }

    fn build_primitive_array<T>(&self, rows: RecordSlice, col_name: &str) -> ArrayRef
    where
        T: ArrowNumericType + Resolver,
        T::Native: num_traits::cast::NumCast,
    {
        Arc::new(
            rows.iter()
                .map(|row| {
                    self.field_lookup(col_name, row)
                        .and_then(|value| resolve_item::<T>(value))
                })
                .collect::<PrimitiveArray<T>>(),
        )
    }

    #[inline(always)]
    fn build_string_dictionary_builder<T>(&self, row_len: usize) -> StringDictionaryBuilder<T>
    where
        T: ArrowPrimitiveType + ArrowDictionaryKeyType,
    {
        StringDictionaryBuilder::with_capacity(row_len, row_len, row_len)
    }

    fn build_wrapped_list_array(
        &self,
        rows: RecordSlice,
        col_name: &str,
        key_type: &DataType,
    ) -> ArrowResult<ArrayRef> {
        match *key_type {
            DataType::Int8 => {
                let dtype =
                    DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Utf8));
                self.list_array_string_array_builder::<Int8Type>(&dtype, col_name, rows)
            }
            DataType::Int16 => {
                let dtype =
                    DataType::Dictionary(Box::new(DataType::Int16), Box::new(DataType::Utf8));
                self.list_array_string_array_builder::<Int16Type>(&dtype, col_name, rows)
            }
            DataType::Int32 => {
                let dtype =
                    DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8));
                self.list_array_string_array_builder::<Int32Type>(&dtype, col_name, rows)
            }
            DataType::Int64 => {
                let dtype =
                    DataType::Dictionary(Box::new(DataType::Int64), Box::new(DataType::Utf8));
                self.list_array_string_array_builder::<Int64Type>(&dtype, col_name, rows)
            }
            DataType::UInt8 => {
                let dtype =
                    DataType::Dictionary(Box::new(DataType::UInt8), Box::new(DataType::Utf8));
                self.list_array_string_array_builder::<UInt8Type>(&dtype, col_name, rows)
            }
            DataType::UInt16 => {
                let dtype =
                    DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8));
                self.list_array_string_array_builder::<UInt16Type>(&dtype, col_name, rows)
            }
            DataType::UInt32 => {
                let dtype =
                    DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8));
                self.list_array_string_array_builder::<UInt32Type>(&dtype, col_name, rows)
            }
            DataType::UInt64 => {
                let dtype =
                    DataType::Dictionary(Box::new(DataType::UInt64), Box::new(DataType::Utf8));
                self.list_array_string_array_builder::<UInt64Type>(&dtype, col_name, rows)
            }
            ref e => Err(SchemaError(format!(
                "Data type is currently not supported for dictionaries in list : {e:?}"
            ))),
        }
    }

    #[inline(always)]
    fn list_array_string_array_builder<D>(
        &self,
        data_type: &DataType,
        col_name: &str,
        rows: RecordSlice,
    ) -> ArrowResult<ArrayRef>
    where
        D: ArrowPrimitiveType + ArrowDictionaryKeyType,
    {
        let mut builder: Box<dyn ArrayBuilder> = match data_type {
            DataType::Utf8 => {
                let values_builder = StringBuilder::with_capacity(rows.len(), 5);
                Box::new(ListBuilder::new(values_builder))
            }
            DataType::Dictionary(_, _) => {
                let values_builder = self.build_string_dictionary_builder::<D>(rows.len() * 5);
                Box::new(ListBuilder::new(values_builder))
            }
            e => {
                return Err(SchemaError(format!(
                    "Nested list data builder type is not supported: {e:?}"
                )))
            }
        };

        for row in rows {
            if let Some(value) = self.field_lookup(col_name, row) {
                let value = maybe_resolve_union(value);
                // value can be an array or a scalar
                let vals: Vec<Option<String>> = if let AvroValue::String(v) = value {
                    vec![Some(v.to_string())]
                } else if let AvroValue::Array(n) = value {
                    n.iter()
                        .map(resolve_string)
                        .collect::<ArrowResult<Vec<Option<String>>>>()?
                        .into_iter()
                        .collect::<Vec<Option<String>>>()
                } else if let AvroValue::Null = value {
                    vec![None]
                } else if !matches!(value, AvroValue::Record(_)) {
                    vec![resolve_string(value)?]
                } else {
                    return Err(SchemaError(
                        "Only scalars are currently supported in Avro arrays".to_string(),
                    ));
                };

                // TODO: ARROW-10335: APIs of dictionary arrays and others are different. Unify
                // them.
                match data_type {
                    DataType::Utf8 => {
                        let builder = builder
                            .as_any_mut()
                            .downcast_mut::<ListBuilder<StringBuilder>>()
                            .ok_or_else(||ArrowError::SchemaError(
                                "Cast failed for ListBuilder<StringBuilder> during nested data parsing".to_string(),
                            ))?;
                        for val in vals {
                            if let Some(v) = val {
                                builder.values().append_value(&v)
                            } else {
                                builder.values().append_null()
                            };
                        }

                        // Append to the list
                        builder.append(true);
                    }
                    DataType::Dictionary(_, _) => {
                        let builder = builder.as_any_mut().downcast_mut::<ListBuilder<StringDictionaryBuilder<D>>>().ok_or_else(||ArrowError::SchemaError(
                            "Cast failed for ListBuilder<StringDictionaryBuilder> during nested data parsing".to_string(),
                        ))?;
                        for val in vals {
                            if let Some(v) = val {
                                let _ = builder.values().append(&v)?;
                            } else {
                                builder.values().append_null()
                            };
                        }

                        // Append to the list
                        builder.append(true);
                    }
                    e => {
                        return Err(SchemaError(format!(
                            "Nested list data builder type is not supported: {e:?}"
                        )))
                    }
                }
            }
        }

        Ok(builder.finish() as ArrayRef)
    }

    #[inline(always)]
    fn build_dictionary_array<T>(&self, rows: RecordSlice, col_name: &str) -> ArrowResult<ArrayRef>
    where
        T::Native: num_traits::cast::NumCast,
        T: ArrowPrimitiveType + ArrowDictionaryKeyType,
    {
        let mut builder: StringDictionaryBuilder<T> =
            self.build_string_dictionary_builder(rows.len());
        for row in rows {
            if let Some(value) = self.field_lookup(col_name, row) {
                if let Ok(Some(str_v)) = resolve_string(value) {
                    builder.append(str_v).map(drop)?
                } else {
                    builder.append_null()
                }
            } else {
                builder.append_null()
            }
        }
        Ok(Arc::new(builder.finish()) as ArrayRef)
    }

    #[inline(always)]
    fn build_string_dictionary_array(
        &self,
        rows: RecordSlice,
        col_name: &str,
        key_type: &DataType,
        value_type: &DataType,
    ) -> ArrowResult<ArrayRef> {
        if let DataType::Utf8 = *value_type {
            match *key_type {
                DataType::Int8 => self.build_dictionary_array::<Int8Type>(rows, col_name),
                DataType::Int16 => self.build_dictionary_array::<Int16Type>(rows, col_name),
                DataType::Int32 => self.build_dictionary_array::<Int32Type>(rows, col_name),
                DataType::Int64 => self.build_dictionary_array::<Int64Type>(rows, col_name),
                DataType::UInt8 => self.build_dictionary_array::<UInt8Type>(rows, col_name),
                DataType::UInt16 => self.build_dictionary_array::<UInt16Type>(rows, col_name),
                DataType::UInt32 => self.build_dictionary_array::<UInt32Type>(rows, col_name),
                DataType::UInt64 => self.build_dictionary_array::<UInt64Type>(rows, col_name),
                _ => Err(ArrowError::SchemaError(
                    "unsupported dictionary key type".to_string(),
                )),
            }
        } else {
            Err(ArrowError::SchemaError(
                "dictionary types other than UTF-8 not yet supported".to_string(),
            ))
        }
    }

    /// Build a nested GenericListArray from a list of unnested `Value`s
    fn build_nested_list_array<OffsetSize: OffsetSizeTrait>(
        &self,
        parent_field_name: &str,
        rows: &[&AvroValue],
        list_field: &Field,
    ) -> ArrowResult<ArrayRef> {
        // build list offsets
        let mut cur_offset = OffsetSize::zero();
        let list_len = rows.len();
        let num_list_bytes = bit_util::ceil(list_len, 8);
        let mut offsets = Vec::with_capacity(list_len + 1);
        let mut list_nulls = MutableBuffer::from_len_zeroed(num_list_bytes);
        offsets.push(cur_offset);
        rows.iter().enumerate().for_each(|(i, v)| {
            // TODO: unboxing Union(Array(Union(...))) should probably be done earlier
            let v = maybe_resolve_union(v);
            if let AvroValue::Array(a) = v {
                cur_offset += OffsetSize::from_usize(a.len()).unwrap();
                bit_util::set_bit(&mut list_nulls, i);
            } else if let AvroValue::Null = v {
                // value is null, not incremented
            } else {
                cur_offset += OffsetSize::one();
            }
            offsets.push(cur_offset);
        });
        let valid_len = cur_offset.to_usize().unwrap();
        let array_data = match list_field.data_type() {
            DataType::Null => NullArray::new(valid_len).into_data(),
            DataType::Boolean => {
                let num_bytes = bit_util::ceil(valid_len, 8);
                let mut bool_values = MutableBuffer::from_len_zeroed(num_bytes);
                let mut bool_nulls = MutableBuffer::new(num_bytes).with_bitset(num_bytes, true);
                let mut curr_index = 0;
                rows.iter().for_each(|v| {
                    if let AvroValue::Array(vs) = v {
                        vs.iter().for_each(|value| {
                            if let AvroValue::Boolean(child) = value {
                                // if valid boolean, append value
                                if *child {
                                    bit_util::set_bit(&mut bool_values, curr_index);
                                }
                            } else {
                                // null slot
                                bit_util::unset_bit(&mut bool_nulls, curr_index);
                            }
                            curr_index += 1;
                        });
                    }
                });
                ArrayData::builder(list_field.data_type().clone())
                    .len(valid_len)
                    .add_buffer(bool_values.into())
                    .null_bit_buffer(Some(bool_nulls.into()))
                    .build()
                    .unwrap()
            }
            DataType::Int8 => self.read_primitive_list_values::<Int8Type>(rows),
            DataType::Int16 => self.read_primitive_list_values::<Int16Type>(rows),
            DataType::Int32 => self.read_primitive_list_values::<Int32Type>(rows),
            DataType::Int64 => self.read_primitive_list_values::<Int64Type>(rows),
            DataType::UInt8 => self.read_primitive_list_values::<UInt8Type>(rows),
            DataType::UInt16 => self.read_primitive_list_values::<UInt16Type>(rows),
            DataType::UInt32 => self.read_primitive_list_values::<UInt32Type>(rows),
            DataType::UInt64 => self.read_primitive_list_values::<UInt64Type>(rows),
            DataType::Float16 => {
                return Err(ArrowError::SchemaError("Float16 not supported".to_string()))
            }
            DataType::Float32 => self.read_primitive_list_values::<Float32Type>(rows),
            DataType::Float64 => self.read_primitive_list_values::<Float64Type>(rows),
            DataType::Timestamp(_, _)
            | DataType::Date32
            | DataType::Date64
            | DataType::Time32(_)
            | DataType::Time64(_) => {
                return Err(ArrowError::SchemaError(
                    "Temporal types are not yet supported, see ARROW-4803".to_string(),
                ))
            }
            DataType::Utf8 => flatten_string_values(rows)
                .into_iter()
                .collect::<StringArray>()
                .into_data(),
            DataType::LargeUtf8 => flatten_string_values(rows)
                .into_iter()
                .collect::<LargeStringArray>()
                .into_data(),
            DataType::List(field) => {
                let child = self.build_nested_list_array::<i32>(
                    parent_field_name,
                    &flatten_values(rows),
                    field,
                )?;
                child.to_data()
            }
            DataType::LargeList(field) => {
                let child = self.build_nested_list_array::<i64>(
                    parent_field_name,
                    &flatten_values(rows),
                    field,
                )?;
                child.to_data()
            }
            DataType::Struct(fields) => {
                // extract list values, with non-lists converted to Value::Null
                let array_item_count = rows
                    .iter()
                    .map(|row| match maybe_resolve_union(row) {
                        AvroValue::Array(values) => values.len(),
                        _ => 1,
                    })
                    .sum();
                let num_bytes = bit_util::ceil(array_item_count, 8);
                let mut null_buffer = MutableBuffer::from_len_zeroed(num_bytes);
                let mut struct_index = 0;
                let null_struct_array = vec![("null".to_string(), AvroValue::Null)];
                let rows: Vec<&Vec<(String, AvroValue)>> = rows
                    .iter()
                    .map(|v| maybe_resolve_union(v))
                    .flat_map(|row| {
                        if let AvroValue::Array(values) = row {
                            values
                                .iter()
                                .map(maybe_resolve_union)
                                .map(|v| match v {
                                    AvroValue::Record(record) => {
                                        bit_util::set_bit(&mut null_buffer, struct_index);
                                        struct_index += 1;
                                        record
                                    }
                                    AvroValue::Null => {
                                        struct_index += 1;
                                        &null_struct_array
                                    }
                                    other => panic!("expected Record, got {other:?}"),
                                })
                                .collect::<Vec<&Vec<(String, AvroValue)>>>()
                        } else {
                            struct_index += 1;
                            vec![&null_struct_array]
                        }
                    })
                    .collect();

                let sub_parent_field_name = format!("{}.{}", parent_field_name, list_field.name());
                let arrays = self.build_struct_array(&rows, &sub_parent_field_name, fields)?;
                let data_type = DataType::Struct(fields.clone());
                ArrayDataBuilder::new(data_type)
                    .len(rows.len())
                    .null_bit_buffer(Some(null_buffer.into()))
                    .child_data(arrays.into_iter().map(|a| a.to_data()).collect())
                    .build()
                    .unwrap()
            }
            datatype => {
                return Err(ArrowError::SchemaError(format!(
                    "Nested list of {datatype:?} not supported"
                )));
            }
        };
        // build list
        let list_data = ArrayData::builder(DataType::List(Arc::new(list_field.clone())))
            .len(list_len)
            .add_buffer(Buffer::from_slice_ref(&offsets))
            .add_child_data(array_data)
            .null_bit_buffer(Some(list_nulls.into()))
            .build()
            .unwrap();
        Ok(Arc::new(GenericListArray::<OffsetSize>::from(list_data)))
    }

    /// Builds the child values of a `StructArray`, falling short of constructing the StructArray.
    /// The function does not construct the StructArray as some callers would want the child arrays.
    ///
    /// *Note*: The function is recursive, and will read nested structs.
    ///
    /// If `projection` is not empty, then all values are returned. The first level of projection
    /// occurs at the `RecordBatch` level. No further projection currently occurs, but would be
    /// useful if plucking values from a struct, e.g. getting `a.b.c.e` from `a.b.c.{d, e}`.
    fn build_struct_array(
        &self,
        rows: RecordSlice,
        parent_field_name: &str,
        struct_fields: &Fields,
    ) -> ArrowResult<Vec<ArrayRef>> {
        let arrays: ArrowResult<Vec<ArrayRef>> = struct_fields
            .iter()
            .map(|field| {
                let field_path = if parent_field_name.is_empty() {
                    field.name().to_string()
                } else {
                    format!("{}.{}", parent_field_name, field.name())
                };
                let arr = match field.data_type() {
                    DataType::Null => Arc::new(NullArray::new(rows.len())) as ArrayRef,
                    DataType::Boolean => self.build_boolean_array(rows, &field_path),
                    DataType::Float64 => {
                        self.build_primitive_array::<Float64Type>(rows, &field_path)
                    }
                    DataType::Float32 => {
                        self.build_primitive_array::<Float32Type>(rows, &field_path)
                    }
                    DataType::Int64 => self.build_primitive_array::<Int64Type>(rows, &field_path),
                    DataType::Int32 => self.build_primitive_array::<Int32Type>(rows, &field_path),
                    DataType::Int16 => self.build_primitive_array::<Int16Type>(rows, &field_path),
                    DataType::Int8 => self.build_primitive_array::<Int8Type>(rows, &field_path),
                    DataType::UInt64 => self.build_primitive_array::<UInt64Type>(rows, &field_path),
                    DataType::UInt32 => self.build_primitive_array::<UInt32Type>(rows, &field_path),
                    DataType::UInt16 => self.build_primitive_array::<UInt16Type>(rows, &field_path),
                    DataType::UInt8 => self.build_primitive_array::<UInt8Type>(rows, &field_path),
                    // TODO: this is incomplete
                    DataType::Timestamp(unit, _) => match unit {
                        TimeUnit::Second => {
                            self.build_primitive_array::<TimestampSecondType>(rows, &field_path)
                        }
                        TimeUnit::Microsecond => self
                            .build_primitive_array::<TimestampMicrosecondType>(rows, &field_path),
                        TimeUnit::Millisecond => self
                            .build_primitive_array::<TimestampMillisecondType>(rows, &field_path),
                        TimeUnit::Nanosecond => {
                            self.build_primitive_array::<TimestampNanosecondType>(rows, &field_path)
                        }
                    },
                    DataType::Date64 => self.build_primitive_array::<Date64Type>(rows, &field_path),
                    DataType::Date32 => self.build_primitive_array::<Date32Type>(rows, &field_path),
                    DataType::Time64(unit) => {
                        match unit {
                            TimeUnit::Microsecond => self
                                .build_primitive_array::<Time64MicrosecondType>(rows, &field_path),
                            TimeUnit::Nanosecond => self
                                .build_primitive_array::<Time64NanosecondType>(rows, &field_path),
                            t => {
                                return Err(ArrowError::SchemaError(format!(
                                    "TimeUnit {t:?} not supported with Time64"
                                )))
                            }
                        }
                    }
                    DataType::Time32(unit) => match unit {
                        TimeUnit::Second => {
                            self.build_primitive_array::<Time32SecondType>(rows, &field_path)
                        }
                        TimeUnit::Millisecond => {
                            self.build_primitive_array::<Time32MillisecondType>(rows, &field_path)
                        }
                        t => {
                            return Err(ArrowError::SchemaError(format!(
                                "TimeUnit {t:?} not supported with Time32"
                            )))
                        }
                    },
                    DataType::Utf8 | DataType::LargeUtf8 => Arc::new(
                        rows.iter()
                            .map(|row| {
                                let maybe_value = self.field_lookup(&field_path, row);
                                match maybe_value {
                                    None => Ok(None),
                                    Some(v) => resolve_string(v),
                                }
                            })
                            .collect::<ArrowResult<StringArray>>()?,
                    ) as ArrayRef,
                    DataType::Binary | DataType::LargeBinary => Arc::new(
                        rows.iter()
                            .map(|row| {
                                let maybe_value = self.field_lookup(&field_path, row);
                                maybe_value.and_then(resolve_bytes)
                            })
                            .collect::<BinaryArray>(),
                    ) as ArrayRef,
                    DataType::FixedSizeBinary(ref size) => {
                        Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                            rows.iter().map(|row| {
                                let maybe_value = self.field_lookup(&field_path, row);
                                maybe_value.and_then(|v| resolve_fixed(v, *size as usize))
                            }),
                            *size,
                        )?) as ArrayRef
                    }
                    DataType::List(ref list_field) => {
                        match list_field.data_type() {
                            DataType::Dictionary(ref key_ty, _) => {
                                self.build_wrapped_list_array(rows, &field_path, key_ty)?
                            }
                            _ => {
                                // extract rows by name
                                let extracted_rows = rows
                                    .iter()
                                    .map(|row| {
                                        self.field_lookup(&field_path, row)
                                            .unwrap_or(&AvroValue::Null)
                                    })
                                    .collect::<Vec<&AvroValue>>();
                                self.build_nested_list_array::<i32>(
                                    &field_path,
                                    &extracted_rows,
                                    list_field,
                                )?
                            }
                        }
                    }
                    DataType::Dictionary(ref key_ty, ref val_ty) => {
                        self.build_string_dictionary_array(rows, &field_path, key_ty, val_ty)?
                    }
                    DataType::Struct(fields) => {
                        let len = rows.len();
                        let num_bytes = bit_util::ceil(len, 8);
                        let mut null_buffer = MutableBuffer::from_len_zeroed(num_bytes);
                        let empty_vec = vec![];
                        let struct_rows = rows
                            .iter()
                            .enumerate()
                            .map(|(i, row)| (i, self.field_lookup(&field_path, row)))
                            .map(|(i, v)| {
                                let v = v.map(maybe_resolve_union);
                                match v {
                                    Some(AvroValue::Record(value)) => {
                                        bit_util::set_bit(&mut null_buffer, i);
                                        value
                                    }
                                    None | Some(AvroValue::Null) => &empty_vec,
                                    other => {
                                        panic!("expected struct got {other:?}");
                                    }
                                }
                            })
                            .collect::<Vec<&Vec<(String, AvroValue)>>>();
                        let arrays = self.build_struct_array(&struct_rows, &field_path, fields)?;
                        // construct a struct array's data in order to set null buffer
                        let data_type = DataType::Struct(fields.clone());
                        let data = ArrayDataBuilder::new(data_type)
                            .len(len)
                            .null_bit_buffer(Some(null_buffer.into()))
                            .child_data(arrays.into_iter().map(|a| a.to_data()).collect())
                            .build()?;
                        make_array(data)
                    }
                    _ => {
                        return Err(ArrowError::SchemaError(format!(
                            "type {:?} not supported",
                            field.data_type()
                        )))
                    }
                };
                Ok(arr)
            })
            .collect();
        arrays
    }

    /// Read the primitive list's values into ArrayData
    fn read_primitive_list_values<T>(&self, rows: &[&AvroValue]) -> ArrayData
    where
        T: ArrowPrimitiveType + ArrowNumericType,
        T::Native: num_traits::cast::NumCast,
    {
        let values = rows
            .iter()
            .flat_map(|row| {
                let row = maybe_resolve_union(row);
                if let AvroValue::Array(values) = row {
                    values
                        .iter()
                        .map(resolve_item::<T>)
                        .collect::<Vec<Option<T::Native>>>()
                } else if let Some(f) = resolve_item::<T>(row) {
                    vec![Some(f)]
                } else {
                    vec![]
                }
            })
            .collect::<Vec<Option<T::Native>>>();
        let array = values.iter().collect::<PrimitiveArray<T>>();
        array.to_data()
    }

    fn field_lookup<'b>(
        &self,
        _name: &str,
        _row: &'b [(String, AvroValue)],
    ) -> Option<&'b AvroValue> {
        // TODO: 需要专门处理
        None
        //self.schema_lookup
        //.get(name)
        //.and_then(|i| row.get(*i))
        //.map(|o| &o.1)
    }
}

/// Flattens a list of Avro values, by flattening lists, and treating all other values as
/// single-value lists.
/// This is used to read into nested lists (list of list, list of struct) and non-dictionary lists.
#[inline]
fn flatten_values<'a>(values: &[&'a AvroValue]) -> Vec<&'a AvroValue> {
    values
        .iter()
        .flat_map(|row| {
            let v = maybe_resolve_union(row);
            if let AvroValue::Array(values) = v {
                values.iter().collect()
            } else {
                // we interpret a scalar as a single-value list to minimize data loss
                vec![v]
            }
        })
        .collect()
}

/// Flattens a list into string values, dropping Value::Null in the process.
/// This is useful for interpreting any Avro array as string, dropping nulls.
/// See `value_as_string`.
#[inline]
fn flatten_string_values(values: &[&AvroValue]) -> Vec<Option<String>> {
    values
        .iter()
        .flat_map(|row| {
            let row = maybe_resolve_union(row);
            if let AvroValue::Array(values) = row {
                values
                    .iter()
                    .map(|s| resolve_string(s).ok().flatten())
                    .collect::<Vec<Option<_>>>()
            } else if let AvroValue::Null = row {
                vec![]
            } else {
                vec![resolve_string(row).ok().flatten()]
            }
        })
        .collect::<Vec<Option<_>>>()
}

/// Reads an Avro value as a string, regardless of its type.
/// This is useful if the expected datatype is a string, in which case we preserve
/// all the values regardless of they type.
fn resolve_string(v: &AvroValue) -> ArrowResult<Option<String>> {
    let v = if let AvroValue::Union(_, b) = v { b } else { v };
    match v {
        AvroValue::String(s) => Ok(Some(s.clone())),
        AvroValue::Bytes(bytes) => String::from_utf8(bytes.to_vec())
            .map_err(AvroError::ConvertToUtf8)
            .map(Some),
        AvroValue::Enum(_, s) => Ok(Some(s.clone())),
        AvroValue::Null => Ok(None),
        other => Err(AvroError::GetString(other.into())),
    }
    .map_err(|e| SchemaError(format!("expected resolvable string : {e:?}")))
}

fn resolve_u8(v: &AvroValue) -> AvroResult<u8> {
    let int = match v {
        AvroValue::Int(n) => Ok(AvroValue::Int(*n)),
        AvroValue::Long(n) => Ok(AvroValue::Int(*n as i32)),
        other => Err(AvroError::GetU8(other.into())),
    }?;
    if let AvroValue::Int(n) = int {
        if n >= 0 && n <= std::convert::From::from(u8::MAX) {
            return Ok(n as u8);
        }
    }

    Err(AvroError::GetU8(int.into()))
}

fn resolve_bytes(v: &AvroValue) -> Option<Vec<u8>> {
    let v = if let AvroValue::Union(_, b) = v { b } else { v };
    match v {
        AvroValue::Bytes(_) => Ok(v.clone()),
        AvroValue::String(s) => Ok(AvroValue::Bytes(s.clone().into_bytes())),
        AvroValue::Array(items) => Ok(AvroValue::Bytes(
            items
                .iter()
                .map(resolve_u8)
                .collect::<Result<Vec<_>, _>>()
                .ok()?,
        )),
        other => Err(AvroError::GetBytes(other.into())),
    }
    .ok()
    .and_then(|v| match v {
        AvroValue::Bytes(s) => Some(s),
        _ => None,
    })
}

fn resolve_fixed(v: &AvroValue, size: usize) -> Option<Vec<u8>> {
    let v = if let AvroValue::Union(_, b) = v { b } else { v };
    match v {
        AvroValue::Fixed(n, bytes) => {
            if *n == size {
                Some(bytes.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn resolve_boolean(value: &AvroValue) -> Option<bool> {
    let v = if let AvroValue::Union(_, b) = value {
        b
    } else {
        value
    };
    match v {
        AvroValue::Boolean(boolean) => Some(*boolean),
        _ => None,
    }
}

trait Resolver: ArrowPrimitiveType {
    fn resolve(value: &AvroValue) -> Option<Self::Native>;
}

fn resolve_item<T: Resolver>(value: &AvroValue) -> Option<T::Native> {
    T::resolve(value)
}

fn maybe_resolve_union(value: &AvroValue) -> &AvroValue {
    if SchemaKind::from(value) == SchemaKind::Union {
        // Pull out the Union, and attempt to resolve against it.
        match value {
            AvroValue::Union(_, b) => b,
            _ => unreachable!(),
        }
    } else {
        value
    }
}

impl<N> Resolver for N
where
    N: ArrowNumericType,
    N::Native: num_traits::cast::NumCast,
{
    fn resolve(value: &AvroValue) -> Option<Self::Native> {
        let value = maybe_resolve_union(value);
        match value {
            AvroValue::Int(i) | AvroValue::TimeMillis(i) | AvroValue::Date(i) => NumCast::from(*i),
            AvroValue::Long(l)
            | AvroValue::TimeMicros(l)
            | AvroValue::TimestampMillis(l)
            | AvroValue::TimestampMicros(l) => NumCast::from(*l),
            AvroValue::Float(f) => NumCast::from(*f),
            AvroValue::Double(f) => NumCast::from(*f),
            AvroValue::Duration(_d) => unimplemented!(), // shenanigans type
            AvroValue::Null => None,
            _ => unreachable!(),
        }
    }
}
