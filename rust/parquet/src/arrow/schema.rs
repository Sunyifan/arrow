// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Provides API for converting parquet schema to arrow schema and vice versa.
//!
//! The main interfaces for converting parquet schema to arrow schema  are
//! `parquet_to_arrow_schema`, `parquet_to_arrow_schema_by_columns` and
//! `parquet_to_arrow_field`.
//!
//! The interfaces for converting arrow schema to parquet schema is coming.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use arrow::datatypes::{DataType, DateUnit, Field, Schema, TimeUnit};
use arrow::ipc::writer;

use crate::basic::{LogicalType, Repetition, Type as PhysicalType};
use crate::errors::{ParquetError::ArrowError, Result};
use crate::file::{metadata::KeyValue, properties::WriterProperties};
use crate::schema::types::{ColumnDescriptor, SchemaDescriptor, Type, TypePtr};

/// Convert Parquet schema to Arrow schema including optional metadata.
/// Attempts to decode any existing Arrow shcema metadata, falling back
/// to converting the Parquet schema column-wise
pub fn parquet_to_arrow_schema(
    parquet_schema: &SchemaDescriptor,
    key_value_metadata: &Option<Vec<KeyValue>>,
) -> Result<Schema> {
    let mut metadata = parse_key_value_metadata(key_value_metadata).unwrap_or_default();
    let arrow_schema_metadata = metadata
        .remove(super::ARROW_SCHEMA_META_KEY)
        .map(|encoded| get_arrow_schema_from_metadata(&encoded));

    match arrow_schema_metadata {
        Some(Some(schema)) => Ok(schema),
        _ => parquet_to_arrow_schema_by_columns(
            parquet_schema,
            0..parquet_schema.columns().len(),
            key_value_metadata,
        ),
    }
}

/// Convert parquet schema to arrow schema including optional metadata,
/// only preserving some root columns.
/// This is useful if we have columns `a.b`, `a.c.e` and `a.d`,
/// and want `a` with all its child fields
pub fn parquet_to_arrow_schema_by_root_columns<T>(
    parquet_schema: &SchemaDescriptor,
    column_indices: T,
    key_value_metadata: &Option<Vec<KeyValue>>,
) -> Result<Schema>
where
    T: IntoIterator<Item = usize>,
{
    // Reconstruct the index ranges of the parent columns
    // An Arrow struct gets represented by 1+ columns based on how many child fields the
    // struct has. This means that getting fields 1 and 2 might return the struct twice,
    // if field 1 is the struct having say 3 fields, and field 2 is a primitive.
    //
    // The below gets the parent columns, and counts the number of child fields in each parent,
    // such that we would end up with:
    // - field 1 - columns: [0, 1, 2]
    // - field 2 - columns: [3]
    let mut parent_columns = vec![];
    let mut curr_name = "";
    let mut prev_name = "";
    let mut indices = vec![];
    (0..(parquet_schema.num_columns())).for_each(|i| {
        let p_type = parquet_schema.get_column_root(i);
        curr_name = p_type.get_basic_info().name();
        if prev_name.is_empty() {
            // first index
            indices.push(i);
            prev_name = curr_name;
        } else if curr_name != prev_name {
            prev_name = curr_name;
            parent_columns.push((curr_name.to_string(), indices.clone()));
            indices = vec![i];
        } else {
            indices.push(i);
        }
    });
    // push the last column if indices has values
    if !indices.is_empty() {
        parent_columns.push((curr_name.to_string(), indices));
    }

    // gather the required leaf columns
    let leaf_columns = column_indices
        .into_iter()
        .flat_map(|i| parent_columns[i].1.clone());

    parquet_to_arrow_schema_by_columns(parquet_schema, leaf_columns, key_value_metadata)
}

/// Convert parquet schema to arrow schema including optional metadata,
/// only preserving some leaf columns.
pub fn parquet_to_arrow_schema_by_columns<T>(
    parquet_schema: &SchemaDescriptor,
    column_indices: T,
    key_value_metadata: &Option<Vec<KeyValue>>,
) -> Result<Schema>
where
    T: IntoIterator<Item = usize>,
{
    let mut metadata = parse_key_value_metadata(key_value_metadata).unwrap_or_default();
    let arrow_schema_metadata = metadata
        .remove(super::ARROW_SCHEMA_META_KEY)
        .map(|encoded| get_arrow_schema_from_metadata(&encoded))
        .unwrap_or_default();

    // add the Arrow metadata to the Parquet metadata
    if let Some(arrow_schema) = &arrow_schema_metadata {
        arrow_schema.metadata().iter().for_each(|(k, v)| {
            metadata.insert(k.clone(), v.clone());
        });
    }

    let mut base_nodes = Vec::new();
    let mut base_nodes_set = HashSet::new();
    let mut leaves = HashSet::new();

    enum FieldType<'a> {
        Parquet(&'a Type),
        Arrow(Field),
    }

    for c in column_indices {
        let column = parquet_schema.column(c);
        let name = column.name();

        if let Some(field) = arrow_schema_metadata
            .as_ref()
            .and_then(|schema| schema.field_with_name(name).ok().cloned())
        {
            base_nodes.push(FieldType::Arrow(field));
        } else {
            let column = column.self_type() as *const Type;
            let root = parquet_schema.get_column_root(c);
            let root_raw_ptr = root as *const Type;

            leaves.insert(column);
            if !base_nodes_set.contains(&root_raw_ptr) {
                base_nodes.push(FieldType::Parquet(root));
                base_nodes_set.insert(root_raw_ptr);
            }
        }
    }

    base_nodes
        .into_iter()
        .map(|t| match t {
            FieldType::Parquet(t) => ParquetTypeConverter::new(t, &leaves).to_field(),
            FieldType::Arrow(f) => Ok(Some(f)),
        })
        .collect::<Result<Vec<Option<Field>>>>()
        .map(|result| result.into_iter().filter_map(|f| f).collect::<Vec<Field>>())
        .map(|fields| Schema::new_with_metadata(fields, metadata))
}

/// Try to convert Arrow schema metadata into a schema
fn get_arrow_schema_from_metadata(encoded_meta: &str) -> Option<Schema> {
    let decoded = base64::decode(encoded_meta);
    match decoded {
        Ok(bytes) => {
            let slice = if bytes[0..4] == [255u8; 4] {
                &bytes[8..]
            } else {
                bytes.as_slice()
            };
            let message = arrow::ipc::get_root_as_message(slice);
            message
                .header_as_schema()
                .map(arrow::ipc::convert::fb_to_schema)
        }
        Err(err) => {
            // The C++ implementation returns an error if the schema can't be parsed.
            // To prevent this, we explicitly log this, then compute the schema without the metadata
            eprintln!(
                "Unable to decode the encoded schema stored in {}, {:?}",
                super::ARROW_SCHEMA_META_KEY,
                err
            );
            None
        }
    }
}

/// Encodes the Arrow schema into the IPC format, and base64 encodes it
fn encode_arrow_schema(schema: &Schema) -> String {
    let options = writer::IpcWriteOptions::default();
    let mut serialized_schema = arrow::ipc::writer::schema_to_bytes(&schema, &options);

    // manually prepending the length to the schema as arrow uses the legacy IPC format
    // TODO: change after addressing ARROW-9777
    let schema_len = serialized_schema.ipc_message.len();
    let mut len_prefix_schema = Vec::with_capacity(schema_len + 8);
    len_prefix_schema.append(&mut vec![255u8, 255, 255, 255]);
    len_prefix_schema.append((schema_len as u32).to_le_bytes().to_vec().as_mut());
    len_prefix_schema.append(&mut serialized_schema.ipc_message);

    base64::encode(&len_prefix_schema)
}

/// Mutates writer metadata by storing the encoded Arrow schema.
/// If there is an existing Arrow schema metadata, it is replaced.
pub(crate) fn add_encoded_arrow_schema_to_metadata(
    schema: &Schema,
    props: &mut WriterProperties,
) {
    let encoded = encode_arrow_schema(schema);

    let schema_kv = KeyValue {
        key: super::ARROW_SCHEMA_META_KEY.to_string(),
        value: Some(encoded),
    };

    let mut meta = props.key_value_metadata.clone().unwrap_or_default();
    // check if ARROW:schema exists, and overwrite it
    let schema_meta = meta
        .iter()
        .enumerate()
        .find(|(_, kv)| kv.key.as_str() == super::ARROW_SCHEMA_META_KEY);
    match schema_meta {
        Some((i, _)) => {
            meta.remove(i);
            meta.push(schema_kv);
        }
        None => {
            meta.push(schema_kv);
        }
    }
    props.key_value_metadata = Some(meta);
}

/// Convert arrow schema to parquet schema
pub fn arrow_to_parquet_schema(schema: &Schema) -> Result<SchemaDescriptor> {
    let fields: Result<Vec<TypePtr>> = schema
        .fields()
        .iter()
        .map(|field| arrow_to_parquet_type(field).map(Rc::new))
        .collect();
    let group = Type::group_type_builder("arrow_schema")
        .with_fields(&mut fields?)
        .build()?;
    Ok(SchemaDescriptor::new(Rc::new(group)))
}

fn parse_key_value_metadata(
    key_value_metadata: &Option<Vec<KeyValue>>,
) -> Option<HashMap<String, String>> {
    match key_value_metadata {
        Some(key_values) => {
            let map: HashMap<String, String> = key_values
                .iter()
                .filter_map(|kv| {
                    kv.value
                        .as_ref()
                        .map(|value| (kv.key.clone(), value.clone()))
                })
                .collect();

            if map.is_empty() {
                None
            } else {
                Some(map)
            }
        }
        None => None,
    }
}

/// Convert parquet column schema to arrow field.
pub fn parquet_to_arrow_field(parquet_column: &ColumnDescriptor) -> Result<Field> {
    let schema = parquet_column.self_type();

    let mut leaves = HashSet::new();
    leaves.insert(parquet_column.self_type() as *const Type);

    ParquetTypeConverter::new(schema, &leaves)
        .to_field()
        .map(|opt| opt.unwrap())
}

/// Convert an arrow field to a parquet `Type`
fn arrow_to_parquet_type(field: &Field) -> Result<Type> {
    let name = field.name().as_str();
    let repetition = if field.is_nullable() {
        Repetition::OPTIONAL
    } else {
        Repetition::REQUIRED
    };
    // create type from field
    match field.data_type() {
        DataType::Null => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::NONE)
            .with_repetition(repetition)
            .build(),
        DataType::Boolean => Type::primitive_type_builder(name, PhysicalType::BOOLEAN)
            .with_repetition(repetition)
            .build(),
        DataType::Int8 => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::INT_8)
            .with_repetition(repetition)
            .build(),
        DataType::Int16 => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::INT_16)
            .with_repetition(repetition)
            .build(),
        DataType::Int32 => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_repetition(repetition)
            .build(),
        DataType::Int64 => Type::primitive_type_builder(name, PhysicalType::INT64)
            .with_repetition(repetition)
            .build(),
        DataType::UInt8 => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::UINT_8)
            .with_repetition(repetition)
            .build(),
        DataType::UInt16 => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::UINT_16)
            .with_repetition(repetition)
            .build(),
        DataType::UInt32 => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::UINT_32)
            .with_repetition(repetition)
            .build(),
        DataType::UInt64 => Type::primitive_type_builder(name, PhysicalType::INT64)
            .with_logical_type(LogicalType::UINT_64)
            .with_repetition(repetition)
            .build(),
        DataType::Float16 => Err(ArrowError("Float16 arrays not supported".to_string())),
        DataType::Float32 => Type::primitive_type_builder(name, PhysicalType::FLOAT)
            .with_repetition(repetition)
            .build(),
        DataType::Float64 => Type::primitive_type_builder(name, PhysicalType::DOUBLE)
            .with_repetition(repetition)
            .build(),
        DataType::Timestamp(time_unit, _) => {
            Type::primitive_type_builder(name, PhysicalType::INT64)
                .with_logical_type(match time_unit {
                    TimeUnit::Second => LogicalType::TIMESTAMP_MILLIS,
                    TimeUnit::Millisecond => LogicalType::TIMESTAMP_MILLIS,
                    TimeUnit::Microsecond => LogicalType::TIMESTAMP_MICROS,
                    TimeUnit::Nanosecond => LogicalType::TIMESTAMP_MICROS,
                })
                .with_repetition(repetition)
                .build()
        }
        DataType::Date32(_) => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::DATE)
            .with_repetition(repetition)
            .build(),
        DataType::Date64(_) => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::DATE)
            .with_repetition(repetition)
            .build(),
        DataType::Time32(_) => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_logical_type(LogicalType::TIME_MILLIS)
            .with_repetition(repetition)
            .build(),
        DataType::Time64(_) => Type::primitive_type_builder(name, PhysicalType::INT64)
            .with_logical_type(LogicalType::TIME_MICROS)
            .with_repetition(repetition)
            .build(),
        DataType::Duration(_) => Err(ArrowError(
            "Converting Duration to parquet not supported".to_string(),
        )),
        DataType::Interval(_) => {
            Type::primitive_type_builder(name, PhysicalType::FIXED_LEN_BYTE_ARRAY)
                .with_logical_type(LogicalType::INTERVAL)
                .with_repetition(repetition)
                .with_length(12)
                .build()
        }
        DataType::Binary | DataType::LargeBinary => {
            Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
                .with_repetition(repetition)
                .build()
        }
        DataType::FixedSizeBinary(length) => {
            Type::primitive_type_builder(name, PhysicalType::FIXED_LEN_BYTE_ARRAY)
                .with_repetition(repetition)
                .with_length(*length)
                .build()
        }
        DataType::Utf8 | DataType::LargeUtf8 => {
            Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
                .with_logical_type(LogicalType::UTF8)
                .with_repetition(repetition)
                .build()
        }
        DataType::List(f) | DataType::FixedSizeList(f, _) | DataType::LargeList(f) => {
            Type::group_type_builder(name)
                .with_fields(&mut vec![Rc::new(
                    Type::group_type_builder("list")
                        .with_fields(&mut vec![Rc::new(arrow_to_parquet_type(f)?)])
                        .with_repetition(Repetition::REPEATED)
                        .build()?,
                )])
                .with_logical_type(LogicalType::LIST)
                .with_repetition(Repetition::REQUIRED)
                .build()
        }
        DataType::Struct(fields) => {
            if fields.is_empty() {
                return Err(ArrowError(
                    "Parquet does not support writing empty structs".to_string(),
                ));
            }
            // recursively convert children to types/nodes
            let fields: Result<Vec<TypePtr>> = fields
                .iter()
                .map(|f| arrow_to_parquet_type(f).map(Rc::new))
                .collect();
            Type::group_type_builder(name)
                .with_fields(&mut fields?)
                .with_repetition(repetition)
                .build()
        }
        DataType::Union(_) => unimplemented!("See ARROW-8817."),
        DataType::Dictionary(_, ref value) => {
            // Dictionary encoding not handled at the schema level
            let dict_field = Field::new(name, *value.clone(), field.is_nullable());
            arrow_to_parquet_type(&dict_field)
        }
    }
}
/// This struct is used to group methods and data structures used to convert parquet
/// schema together.
struct ParquetTypeConverter<'a> {
    schema: &'a Type,
    /// This is the columns that need to be converted to arrow schema.
    columns_to_convert: &'a HashSet<*const Type>,
}

impl<'a> ParquetTypeConverter<'a> {
    fn new(schema: &'a Type, columns_to_convert: &'a HashSet<*const Type>) -> Self {
        Self {
            schema,
            columns_to_convert,
        }
    }

    fn clone_with_schema(&self, other: &'a Type) -> Self {
        Self {
            schema: other,
            columns_to_convert: self.columns_to_convert,
        }
    }
}

impl ParquetTypeConverter<'_> {
    // Public interfaces.

    /// Converts parquet schema to arrow data type.
    ///
    /// This function discards schema name.
    ///
    /// If this schema is a primitive type and not included in the leaves, the result is
    /// Ok(None).
    ///
    /// If this schema is a group type and none of its children is reserved in the
    /// conversion, the result is Ok(None).
    fn to_data_type(&self) -> Result<Option<DataType>> {
        match self.schema {
            Type::PrimitiveType { .. } => self.to_primitive_type(),
            Type::GroupType { .. } => self.to_group_type(),
        }
    }

    /// Converts parquet schema to arrow field.
    ///
    /// This method is roughly the same as
    /// [`to_data_type`](`ParquetTypeConverter::to_data_type`), except it reserves schema
    /// name.
    fn to_field(&self) -> Result<Option<Field>> {
        self.to_data_type().map(|opt| {
            opt.map(|dt| Field::new(self.schema.name(), dt, self.is_nullable()))
        })
    }

    // Utility functions.

    /// Checks whether this schema is nullable.
    fn is_nullable(&self) -> bool {
        let basic_info = self.schema.get_basic_info();
        if basic_info.has_repetition() {
            match basic_info.repetition() {
                Repetition::OPTIONAL => true,
                Repetition::REPEATED => true,
                Repetition::REQUIRED => false,
            }
        } else {
            false
        }
    }

    fn is_repeated(&self) -> bool {
        let basic_info = self.schema.get_basic_info();

        basic_info.has_repetition() && basic_info.repetition() == Repetition::REPEATED
    }

    fn is_self_included(&self) -> bool {
        self.columns_to_convert
            .contains(&(self.schema as *const Type))
    }

    // Functions for primitive types.

    /// Entry point for converting parquet primitive type to arrow type.
    ///
    /// This function takes care of repetition.
    fn to_primitive_type(&self) -> Result<Option<DataType>> {
        if self.is_self_included() {
            self.to_primitive_type_inner().map(|dt| {
                if self.is_repeated() {
                    Some(DataType::List(Box::new(Field::new(
                        self.schema.name(),
                        dt,
                        self.is_nullable(),
                    ))))
                } else {
                    Some(dt)
                }
            })
        } else {
            Ok(None)
        }
    }

    /// Converting parquet primitive type to arrow data type.
    fn to_primitive_type_inner(&self) -> Result<DataType> {
        match self.schema.get_physical_type() {
            PhysicalType::BOOLEAN => Ok(DataType::Boolean),
            PhysicalType::INT32 => self.from_int32(),
            PhysicalType::INT64 => self.from_int64(),
            PhysicalType::INT96 => Ok(DataType::Timestamp(TimeUnit::Nanosecond, None)),
            PhysicalType::FLOAT => Ok(DataType::Float32),
            PhysicalType::DOUBLE => Ok(DataType::Float64),
            PhysicalType::BYTE_ARRAY => self.from_byte_array(),
            PhysicalType::FIXED_LEN_BYTE_ARRAY => self.from_fixed_len_byte_array(),
        }
    }

    fn from_int32(&self) -> Result<DataType> {
        match self.schema.get_basic_info().logical_type() {
            LogicalType::NONE => Ok(DataType::Int32),
            LogicalType::UINT_8 => Ok(DataType::UInt8),
            LogicalType::UINT_16 => Ok(DataType::UInt16),
            LogicalType::UINT_32 => Ok(DataType::UInt32),
            LogicalType::INT_8 => Ok(DataType::Int8),
            LogicalType::INT_16 => Ok(DataType::Int16),
            LogicalType::INT_32 => Ok(DataType::Int32),
            LogicalType::DATE => Ok(DataType::Date32(DateUnit::Day)),
            LogicalType::TIME_MILLIS => Ok(DataType::Time32(TimeUnit::Millisecond)),
            other => Err(ArrowError(format!(
                "Unable to convert parquet INT32 logical type {}",
                other
            ))),
        }
    }

    fn from_int64(&self) -> Result<DataType> {
        match self.schema.get_basic_info().logical_type() {
            LogicalType::NONE => Ok(DataType::Int64),
            LogicalType::INT_64 => Ok(DataType::Int64),
            LogicalType::UINT_64 => Ok(DataType::UInt64),
            LogicalType::TIME_MICROS => Ok(DataType::Time64(TimeUnit::Microsecond)),
            LogicalType::TIMESTAMP_MILLIS => {
                Ok(DataType::Timestamp(TimeUnit::Millisecond, None))
            }
            LogicalType::TIMESTAMP_MICROS => {
                Ok(DataType::Timestamp(TimeUnit::Microsecond, None))
            }
            other => Err(ArrowError(format!(
                "Unable to convert parquet INT64 logical type {}",
                other
            ))),
        }
    }

    fn from_fixed_len_byte_array(&self) -> Result<DataType> {
        let byte_width = match self.schema {
            Type::PrimitiveType {
                ref type_length, ..
            } => *type_length,
            _ => {
                return Err(ArrowError(
                    "Expected a physical type, not a group type".to_string(),
                ))
            }
        };

        Ok(DataType::FixedSizeBinary(byte_width))
    }

    fn from_byte_array(&self) -> Result<DataType> {
        match self.schema.get_basic_info().logical_type() {
            LogicalType::NONE => Ok(DataType::Binary),
            LogicalType::UTF8 => Ok(DataType::Utf8),
            other => Err(ArrowError(format!(
                "Unable to convert parquet BYTE_ARRAY logical type {}",
                other
            ))),
        }
    }

    // Functions for group types.

    /// Entry point for converting parquet group type.
    ///
    /// This function takes care of logical type and repetition.
    fn to_group_type(&self) -> Result<Option<DataType>> {
        if self.is_repeated() {
            self.to_struct().map(|opt| {
                opt.map(|dt| {
                    DataType::List(Box::new(Field::new(
                        self.schema.name(),
                        dt,
                        self.is_nullable(),
                    )))
                })
            })
        } else {
            match self.schema.get_basic_info().logical_type() {
                LogicalType::LIST => self.to_list(),
                _ => self.to_struct(),
            }
        }
    }

    /// Converts a parquet group type to arrow struct.
    fn to_struct(&self) -> Result<Option<DataType>> {
        match self.schema {
            Type::PrimitiveType { .. } => panic!(
                "{:?} is a struct type, and can't be processed as primitive.",
                self.schema
            ),
            Type::GroupType {
                basic_info: _,
                fields,
            } => fields
                .iter()
                .map(|field_ptr| self.clone_with_schema(field_ptr).to_field())
                .collect::<Result<Vec<Option<Field>>>>()
                .map(|result| {
                    result.into_iter().filter_map(|f| f).collect::<Vec<Field>>()
                })
                .map(|fields| {
                    if fields.is_empty() {
                        None
                    } else {
                        Some(DataType::Struct(fields))
                    }
                }),
        }
    }

    /// Converts a parquet list to arrow list.
    ///
    /// To fully understand this algorithm, please refer to
    /// [parquet doc](https://github.com/apache/parquet-format/blob/master/LogicalTypes.md).
    fn to_list(&self) -> Result<Option<DataType>> {
        match self.schema {
            Type::PrimitiveType { .. } => panic!(
                "{:?} is a list type and can't be processed as primitive.",
                self.schema
            ),
            Type::GroupType {
                basic_info: _,
                fields,
            } if fields.len() == 1 => {
                let list_item = fields.first().unwrap();
                let item_converter = self.clone_with_schema(list_item);

                let item_type = match list_item.as_ref() {
                    Type::PrimitiveType { .. } => {
                        if item_converter.is_repeated() {
                            item_converter.to_primitive_type_inner().map(Some)
                        } else {
                            Err(ArrowError(
                                "Primitive element type of list must be repeated."
                                    .to_string(),
                            ))
                        }
                    }
                    Type::GroupType {
                        basic_info: _,
                        fields,
                    } => {
                        if fields.len() > 1 {
                            item_converter.to_struct()
                        } else if fields.len() == 1
                            && list_item.name() != "array"
                            && list_item.name() != format!("{}_tuple", self.schema.name())
                        {
                            let nested_item = fields.first().unwrap();
                            let nested_item_converter =
                                self.clone_with_schema(nested_item);

                            nested_item_converter.to_data_type()
                        } else {
                            item_converter.to_struct()
                        }
                    }
                };

                item_type.map(|opt| {
                    opt.map(|dt| {
                        DataType::List(Box::new(Field::new(
                            list_item.name(),
                            dt,
                            list_item.is_optional(),
                        )))
                    })
                })
            }
            _ => Err(ArrowError(
                "Group element type of list can only contain one field.".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{collections::HashMap, convert::TryFrom, sync::Arc};

    use arrow::datatypes::{DataType, DateUnit, Field, IntervalUnit, TimeUnit};

    use crate::file::{metadata::KeyValue, reader::SerializedFileReader};
    use crate::{
        arrow::{ArrowReader, ArrowWriter, ParquetFileArrowReader},
        schema::{parser::parse_message_type, types::SchemaDescriptor},
        util::test_common::get_temp_file,
    };

    #[test]
    fn test_flat_primitives() {
        let message_type = "
        message test_schema {
            REQUIRED BOOLEAN boolean;
            REQUIRED INT32   int8  (INT_8);
            REQUIRED INT32   int16 (INT_16);
            REQUIRED INT32   int32;
            REQUIRED INT64   int64 ;
            OPTIONAL DOUBLE  double;
            OPTIONAL FLOAT   float;
            OPTIONAL BINARY  string (UTF8);
        }
        ";
        let parquet_group_type = parse_message_type(message_type).unwrap();

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema(&parquet_schema, &None).unwrap();

        let arrow_fields = vec![
            Field::new("boolean", DataType::Boolean, false),
            Field::new("int8", DataType::Int8, false),
            Field::new("int16", DataType::Int16, false),
            Field::new("int32", DataType::Int32, false),
            Field::new("int64", DataType::Int64, false),
            Field::new("double", DataType::Float64, true),
            Field::new("float", DataType::Float32, true),
            Field::new("string", DataType::Utf8, true),
        ];

        assert_eq!(&arrow_fields, converted_arrow_schema.fields());
    }

    #[test]
    fn test_byte_array_fields() {
        let message_type = "
        message test_schema {
            REQUIRED BYTE_ARRAY binary;
            REQUIRED FIXED_LEN_BYTE_ARRAY (20) fixed_binary;
        }
        ";

        let parquet_group_type = parse_message_type(message_type).unwrap();

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema(&parquet_schema, &None).unwrap();

        let arrow_fields = vec![
            Field::new("binary", DataType::Binary, false),
            Field::new("fixed_binary", DataType::FixedSizeBinary(20), false),
        ];
        assert_eq!(&arrow_fields, converted_arrow_schema.fields());
    }

    #[test]
    fn test_duplicate_fields() {
        let message_type = "
        message test_schema {
            REQUIRED BOOLEAN boolean;
            REQUIRED INT32 int8 (INT_8);
        }
        ";

        let parquet_group_type = parse_message_type(message_type).unwrap();

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema(&parquet_schema, &None).unwrap();

        let arrow_fields = vec![
            Field::new("boolean", DataType::Boolean, false),
            Field::new("int8", DataType::Int8, false),
        ];
        assert_eq!(&arrow_fields, converted_arrow_schema.fields());

        let converted_arrow_schema = parquet_to_arrow_schema_by_columns(
            &parquet_schema,
            vec![0usize, 1usize],
            &None,
        )
        .unwrap();
        assert_eq!(&arrow_fields, converted_arrow_schema.fields());
    }

    #[test]
    fn test_parquet_lists() {
        let mut arrow_fields = Vec::new();

        // LIST encoding example taken from parquet-format/LogicalTypes.md
        let message_type = "
        message test_schema {
          REQUIRED GROUP my_list (LIST) {
            REPEATED GROUP list {
              OPTIONAL BINARY element (UTF8);
            }
          }
          OPTIONAL GROUP my_list (LIST) {
            REPEATED GROUP list {
              REQUIRED BINARY element (UTF8);
            }
          }
          OPTIONAL GROUP array_of_arrays (LIST) {
            REPEATED GROUP list {
              REQUIRED GROUP element (LIST) {
                REPEATED GROUP list {
                  REQUIRED INT32 element;
                }
              }
            }
          }
          OPTIONAL GROUP my_list (LIST) {
            REPEATED GROUP element {
              REQUIRED BINARY str (UTF8);
            }
          }
          OPTIONAL GROUP my_list (LIST) {
            REPEATED INT32 element;
          }
          OPTIONAL GROUP my_list (LIST) {
            REPEATED GROUP element {
              REQUIRED BINARY str (UTF8);
              REQUIRED INT32 num;
            }
          }
          OPTIONAL GROUP my_list (LIST) {
            REPEATED GROUP array {
              REQUIRED BINARY str (UTF8);
            }

          }
          OPTIONAL GROUP my_list (LIST) {
            REPEATED GROUP my_list_tuple {
              REQUIRED BINARY str (UTF8);
            }
          }
          REPEATED INT32 name;
        }
        ";

        // // List<String> (list non-null, elements nullable)
        // required group my_list (LIST) {
        //   repeated group list {
        //     optional binary element (UTF8);
        //   }
        // }
        {
            arrow_fields.push(Field::new(
                "my_list",
                DataType::List(Box::new(Field::new("list", DataType::Utf8, true))),
                false,
            ));
        }

        // // List<String> (list nullable, elements non-null)
        // optional group my_list (LIST) {
        //   repeated group list {
        //     required binary element (UTF8);
        //   }
        // }
        {
            arrow_fields.push(Field::new(
                "my_list",
                DataType::List(Box::new(Field::new("list", DataType::Utf8, true))),
                true,
            ));
        }

        // Element types can be nested structures. For example, a list of lists:
        //
        // // List<List<Integer>>
        // optional group array_of_arrays (LIST) {
        //   repeated group list {
        //     required group element (LIST) {
        //       repeated group list {
        //         required int32 element;
        //       }
        //     }
        //   }
        // }
        {
            let arrow_inner_list =
                DataType::List(Box::new(Field::new("list", DataType::Int32, true)));
            arrow_fields.push(Field::new(
                "array_of_arrays",
                DataType::List(Box::new(Field::new("list", arrow_inner_list, true))),
                true,
            ));
        }

        // // List<String> (list nullable, elements non-null)
        // optional group my_list (LIST) {
        //   repeated group element {
        //     required binary str (UTF8);
        //   };
        // }
        {
            arrow_fields.push(Field::new(
                "my_list",
                DataType::List(Box::new(Field::new("element", DataType::Utf8, true))),
                true,
            ));
        }

        // // List<Integer> (nullable list, non-null elements)
        // optional group my_list (LIST) {
        //   repeated int32 element;
        // }
        {
            arrow_fields.push(Field::new(
                "my_list",
                DataType::List(Box::new(Field::new("element", DataType::Int32, true))),
                true,
            ));
        }

        // // List<Tuple<String, Integer>> (nullable list, non-null elements)
        // optional group my_list (LIST) {
        //   repeated group element {
        //     required binary str (UTF8);
        //     required int32 num;
        //   };
        // }
        {
            let arrow_struct = DataType::Struct(vec![
                Field::new("str", DataType::Utf8, false),
                Field::new("num", DataType::Int32, false),
            ]);
            arrow_fields.push(Field::new(
                "my_list",
                DataType::List(Box::new(Field::new("element", arrow_struct, true))),
                true,
            ));
        }

        // // List<OneTuple<String>> (nullable list, non-null elements)
        // optional group my_list (LIST) {
        //   repeated group array {
        //     required binary str (UTF8);
        //   };
        // }
        // Special case: group is named array
        {
            let arrow_struct =
                DataType::Struct(vec![Field::new("str", DataType::Utf8, false)]);
            arrow_fields.push(Field::new(
                "my_list",
                DataType::List(Box::new(Field::new("array", arrow_struct, true))),
                true,
            ));
        }

        // // List<OneTuple<String>> (nullable list, non-null elements)
        // optional group my_list (LIST) {
        //   repeated group my_list_tuple {
        //     required binary str (UTF8);
        //   };
        // }
        // Special case: group named ends in _tuple
        {
            let arrow_struct =
                DataType::Struct(vec![Field::new("str", DataType::Utf8, false)]);
            arrow_fields.push(Field::new(
                "my_list",
                DataType::List(Box::new(Field::new("my_list_tuple", arrow_struct, true))),
                true,
            ));
        }

        // One-level encoding: Only allows required lists with required cells
        //   repeated value_type name
        {
            arrow_fields.push(Field::new(
                "name",
                DataType::List(Box::new(Field::new("name", DataType::Int32, true))),
                true,
            ));
        }

        let parquet_group_type = parse_message_type(message_type).unwrap();

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema(&parquet_schema, &None).unwrap();
        let converted_fields = converted_arrow_schema.fields();

        assert_eq!(arrow_fields.len(), converted_fields.len());
        for i in 0..arrow_fields.len() {
            assert_eq!(arrow_fields[i], converted_fields[i]);
        }
    }

    #[test]
    fn test_nested_schema() {
        let mut arrow_fields = Vec::new();
        {
            let group1_fields = vec![
                Field::new("leaf1", DataType::Boolean, false),
                Field::new("leaf2", DataType::Int32, false),
            ];
            let group1_struct =
                Field::new("group1", DataType::Struct(group1_fields), false);
            arrow_fields.push(group1_struct);

            let leaf3_field = Field::new("leaf3", DataType::Int64, false);
            arrow_fields.push(leaf3_field);
        }

        let message_type = "
        message test_schema {
          REQUIRED GROUP group1 {
            REQUIRED BOOLEAN leaf1;
            REQUIRED INT32 leaf2;
          }
          REQUIRED INT64 leaf3;
        }
        ";
        let parquet_group_type = parse_message_type(message_type).unwrap();

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema(&parquet_schema, &None).unwrap();
        let converted_fields = converted_arrow_schema.fields();

        assert_eq!(arrow_fields.len(), converted_fields.len());
        for i in 0..arrow_fields.len() {
            assert_eq!(arrow_fields[i], converted_fields[i]);
        }
    }

    #[test]
    fn test_nested_schema_partial() {
        let mut arrow_fields = Vec::new();
        {
            let group1_fields = vec![Field::new("leaf1", DataType::Int64, false)];
            let group1 = Field::new("group1", DataType::Struct(group1_fields), false);
            arrow_fields.push(group1);

            let group2_fields = vec![Field::new("leaf4", DataType::Int64, false)];
            let group2 = Field::new("group2", DataType::Struct(group2_fields), false);
            arrow_fields.push(group2);

            arrow_fields.push(Field::new("leaf5", DataType::Int64, false));
        }

        let message_type = "
        message test_schema {
          REQUIRED GROUP group1 {
            REQUIRED INT64 leaf1;
            REQUIRED INT64 leaf2;
          }
          REQUIRED  GROUP group2 {
            REQUIRED INT64 leaf3;
            REQUIRED INT64 leaf4;
          }
          REQUIRED INT64 leaf5;
        }
        ";
        let parquet_group_type = parse_message_type(message_type).unwrap();

        // Expected partial arrow schema (columns 0, 3, 4):
        // required group group1 {
        //   required int64 leaf1;
        // }
        // required group group2 {
        //   required int64 leaf4;
        // }
        // required int64 leaf5;

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema_by_columns(&parquet_schema, vec![0, 3, 4], &None)
                .unwrap();
        let converted_fields = converted_arrow_schema.fields();

        assert_eq!(arrow_fields.len(), converted_fields.len());
        for i in 0..arrow_fields.len() {
            assert_eq!(arrow_fields[i], converted_fields[i]);
        }
    }

    #[test]
    fn test_nested_schema_partial_ordering() {
        let mut arrow_fields = Vec::new();
        {
            let group2_fields = vec![Field::new("leaf4", DataType::Int64, false)];
            let group2 = Field::new("group2", DataType::Struct(group2_fields), false);
            arrow_fields.push(group2);

            arrow_fields.push(Field::new("leaf5", DataType::Int64, false));

            let group1_fields = vec![Field::new("leaf1", DataType::Int64, false)];
            let group1 = Field::new("group1", DataType::Struct(group1_fields), false);
            arrow_fields.push(group1);
        }

        let message_type = "
        message test_schema {
          REQUIRED GROUP group1 {
            REQUIRED INT64 leaf1;
            REQUIRED INT64 leaf2;
          }
          REQUIRED  GROUP group2 {
            REQUIRED INT64 leaf3;
            REQUIRED INT64 leaf4;
          }
          REQUIRED INT64 leaf5;
        }
        ";
        let parquet_group_type = parse_message_type(message_type).unwrap();

        // Expected partial arrow schema (columns 3, 4, 0):
        // required group group1 {
        //   required int64 leaf1;
        // }
        // required group group2 {
        //   required int64 leaf4;
        // }
        // required int64 leaf5;

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema_by_columns(&parquet_schema, vec![3, 4, 0], &None)
                .unwrap();
        let converted_fields = converted_arrow_schema.fields();

        assert_eq!(arrow_fields.len(), converted_fields.len());
        for i in 0..arrow_fields.len() {
            assert_eq!(arrow_fields[i], converted_fields[i]);
        }
    }

    #[test]
    fn test_repeated_nested_schema() {
        let mut arrow_fields = Vec::new();
        {
            arrow_fields.push(Field::new("leaf1", DataType::Int32, true));

            let inner_group_list = Field::new(
                "innerGroup",
                DataType::List(Box::new(Field::new(
                    "innerGroup",
                    DataType::Struct(vec![Field::new("leaf3", DataType::Int32, true)]),
                    true,
                ))),
                true,
            );

            let outer_group_list = Field::new(
                "outerGroup",
                DataType::List(Box::new(Field::new(
                    "outerGroup",
                    DataType::Struct(vec![
                        Field::new("leaf2", DataType::Int32, true),
                        inner_group_list,
                    ]),
                    true,
                ))),
                true,
            );
            arrow_fields.push(outer_group_list);
        }

        let message_type = "
        message test_schema {
          OPTIONAL INT32 leaf1;
          REPEATED GROUP outerGroup {
            OPTIONAL INT32 leaf2;
            REPEATED GROUP innerGroup {
              OPTIONAL INT32 leaf3;
            }
          }
        }
        ";
        let parquet_group_type = parse_message_type(message_type).unwrap();

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema(&parquet_schema, &None).unwrap();
        let converted_fields = converted_arrow_schema.fields();

        assert_eq!(arrow_fields.len(), converted_fields.len());
        for i in 0..arrow_fields.len() {
            assert_eq!(arrow_fields[i], converted_fields[i]);
        }
    }

    #[test]
    fn test_column_desc_to_field() {
        let message_type = "
        message test_schema {
            REQUIRED BOOLEAN boolean;
            REQUIRED INT32   int8  (INT_8);
            REQUIRED INT32   int16 (INT_16);
            REQUIRED INT32   int32;
            REQUIRED INT64   int64 ;
            OPTIONAL DOUBLE  double;
            OPTIONAL FLOAT   float;
            OPTIONAL BINARY  string (UTF8);
            REPEATED BOOLEAN bools;
            OPTIONAL INT32   date       (DATE);
            OPTIONAL INT32   time_milli (TIME_MILLIS);
            OPTIONAL INT64   time_micro (TIME_MICROS);
            OPTIONAL INT64   ts_milli (TIMESTAMP_MILLIS);
            REQUIRED INT64   ts_micro (TIMESTAMP_MICROS);
        }
        ";
        let parquet_group_type = parse_message_type(message_type).unwrap();

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_fields = parquet_schema
            .columns()
            .iter()
            .map(|c| parquet_to_arrow_field(c).unwrap())
            .collect::<Vec<Field>>();

        let arrow_fields = vec![
            Field::new("boolean", DataType::Boolean, false),
            Field::new("int8", DataType::Int8, false),
            Field::new("int16", DataType::Int16, false),
            Field::new("int32", DataType::Int32, false),
            Field::new("int64", DataType::Int64, false),
            Field::new("double", DataType::Float64, true),
            Field::new("float", DataType::Float32, true),
            Field::new("string", DataType::Utf8, true),
            Field::new(
                "bools",
                DataType::List(Box::new(Field::new("bools", DataType::Boolean, true))),
                true,
            ),
            Field::new("date", DataType::Date32(DateUnit::Day), true),
            Field::new("time_milli", DataType::Time32(TimeUnit::Millisecond), true),
            Field::new("time_micro", DataType::Time64(TimeUnit::Microsecond), true),
            Field::new(
                "ts_milli",
                DataType::Timestamp(TimeUnit::Millisecond, None),
                true,
            ),
            Field::new(
                "ts_micro",
                DataType::Timestamp(TimeUnit::Microsecond, None),
                false,
            ),
        ];

        assert_eq!(arrow_fields, converted_arrow_fields);
    }

    #[test]
    fn test_field_to_column_desc() {
        let message_type = "
        message arrow_schema {
            REQUIRED BOOLEAN boolean;
            REQUIRED INT32   int8  (INT_8);
            REQUIRED INT32   int16 (INT_16);
            REQUIRED INT32   int32;
            REQUIRED INT64   int64;
            OPTIONAL DOUBLE  double;
            OPTIONAL FLOAT   float;
            OPTIONAL BINARY  string (UTF8);
            REQUIRED GROUP   bools (LIST) {
                REPEATED GROUP list {
                    OPTIONAL BOOLEAN element;
                }
            }
            OPTIONAL INT32   date       (DATE);
            OPTIONAL INT32   time_milli (TIME_MILLIS);
            OPTIONAL INT64   time_micro (TIME_MICROS);
            OPTIONAL INT64   ts_milli (TIMESTAMP_MILLIS);
            REQUIRED INT64   ts_micro (TIMESTAMP_MICROS);
            REQUIRED GROUP struct {
                REQUIRED BOOLEAN bools;
                REQUIRED INT32 uint32 (UINT_32);
                REQUIRED GROUP   int32 (LIST) {
                    REPEATED GROUP list {
                        OPTIONAL INT32 element;
                    }
                }
            }
            REQUIRED BINARY  dictionary_strings (UTF8);
        }
        ";
        let parquet_group_type = parse_message_type(message_type).unwrap();

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));

        let arrow_fields = vec![
            Field::new("boolean", DataType::Boolean, false),
            Field::new("int8", DataType::Int8, false),
            Field::new("int16", DataType::Int16, false),
            Field::new("int32", DataType::Int32, false),
            Field::new("int64", DataType::Int64, false),
            Field::new("double", DataType::Float64, true),
            Field::new("float", DataType::Float32, true),
            Field::new("string", DataType::Utf8, true),
            Field::new(
                "bools",
                DataType::List(Box::new(Field::new("element", DataType::Boolean, true))),
                true,
            ),
            Field::new("date", DataType::Date32(DateUnit::Day), true),
            Field::new("time_milli", DataType::Time32(TimeUnit::Millisecond), true),
            Field::new("time_micro", DataType::Time64(TimeUnit::Microsecond), true),
            Field::new(
                "ts_milli",
                DataType::Timestamp(TimeUnit::Millisecond, None),
                true,
            ),
            Field::new(
                "ts_micro",
                DataType::Timestamp(TimeUnit::Microsecond, None),
                false,
            ),
            Field::new(
                "struct",
                DataType::Struct(vec![
                    Field::new("bools", DataType::Boolean, false),
                    Field::new("uint32", DataType::UInt32, false),
                    Field::new(
                        "int32",
                        DataType::List(Box::new(Field::new(
                            "element",
                            DataType::Int32,
                            true,
                        ))),
                        true,
                    ),
                ]),
                false,
            ),
            Field::new(
                "dictionary_strings",
                DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
                false,
            ),
        ];
        let arrow_schema = Schema::new(arrow_fields);
        let converted_arrow_schema = arrow_to_parquet_schema(&arrow_schema).unwrap();

        assert_eq!(
            parquet_schema.columns().len(),
            converted_arrow_schema.columns().len()
        );
        parquet_schema
            .columns()
            .iter()
            .zip(converted_arrow_schema.columns())
            .for_each(|(a, b)| {
                assert_eq!(a, b);
            });
    }

    #[test]
    #[should_panic(expected = "Parquet does not support writing empty structs")]
    fn test_empty_struct_field() {
        let arrow_fields = vec![Field::new("struct", DataType::Struct(vec![]), false)];
        let arrow_schema = Schema::new(arrow_fields);
        let converted_arrow_schema = arrow_to_parquet_schema(&arrow_schema);

        assert!(converted_arrow_schema.is_err());
        converted_arrow_schema.unwrap();
    }

    #[test]
    fn test_metadata() {
        let message_type = "
        message test_schema {
            OPTIONAL BINARY  string (UTF8);
        }
        ";
        let parquet_group_type = parse_message_type(message_type).unwrap();

        let mut key_value_metadata: Vec<KeyValue> = Vec::new();
        key_value_metadata.push(KeyValue::new("foo".to_owned(), Some("bar".to_owned())));
        key_value_metadata.push(KeyValue::new("baz".to_owned(), None));

        let mut expected_metadata: HashMap<String, String> = HashMap::new();
        expected_metadata.insert("foo".to_owned(), "bar".to_owned());

        let parquet_schema = SchemaDescriptor::new(Rc::new(parquet_group_type));
        let converted_arrow_schema =
            parquet_to_arrow_schema(&parquet_schema, &Some(key_value_metadata)).unwrap();

        assert_eq!(converted_arrow_schema.metadata(), &expected_metadata);
    }

    #[test]
    fn test_arrow_schema_roundtrip() -> Result<()> {
        // This tests the roundtrip of an Arrow schema
        // Fields that are commented out fail roundtrip tests or are unsupported by the writer
        let metadata: HashMap<String, String> =
            [("Key".to_string(), "Value".to_string())]
                .iter()
                .cloned()
                .collect();

        let schema = Schema::new_with_metadata(
            vec![
                Field::new("c1", DataType::Utf8, false),
                Field::new("c2", DataType::Binary, false),
                Field::new("c3", DataType::FixedSizeBinary(3), false),
                Field::new("c4", DataType::Boolean, false),
                Field::new("c5", DataType::Date32(DateUnit::Day), false),
                Field::new("c6", DataType::Date64(DateUnit::Millisecond), false),
                Field::new("c7", DataType::Time32(TimeUnit::Second), false),
                Field::new("c8", DataType::Time32(TimeUnit::Millisecond), false),
                Field::new("c13", DataType::Time64(TimeUnit::Microsecond), false),
                Field::new("c14", DataType::Time64(TimeUnit::Nanosecond), false),
                Field::new("c15", DataType::Timestamp(TimeUnit::Second, None), false),
                Field::new(
                    "c16",
                    DataType::Timestamp(
                        TimeUnit::Millisecond,
                        Some(Arc::new("UTC".to_string())),
                    ),
                    false,
                ),
                Field::new(
                    "c17",
                    DataType::Timestamp(
                        TimeUnit::Microsecond,
                        Some(Arc::new("Africa/Johannesburg".to_string())),
                    ),
                    false,
                ),
                Field::new(
                    "c18",
                    DataType::Timestamp(TimeUnit::Nanosecond, None),
                    false,
                ),
                Field::new("c19", DataType::Interval(IntervalUnit::DayTime), false),
                Field::new("c20", DataType::Interval(IntervalUnit::YearMonth), false),
                Field::new(
                    "c21",
                    DataType::List(Box::new(Field::new("list", DataType::Boolean, true))),
                    false,
                ),
                // Field::new(
                //     "c22",
                //     DataType::FixedSizeList(Box::new(DataType::Boolean), 5),
                //     false,
                // ),
                // Field::new(
                //     "c23",
                //     DataType::List(Box::new(DataType::LargeList(Box::new(
                //         DataType::Struct(vec![
                //             Field::new("a", DataType::Int16, true),
                //             Field::new("b", DataType::Float64, false),
                //         ]),
                //     )))),
                //     true,
                // ),
                Field::new(
                    "c24",
                    DataType::Struct(vec![
                        Field::new("a", DataType::Utf8, false),
                        Field::new("b", DataType::UInt16, false),
                    ]),
                    false,
                ),
                Field::new("c25", DataType::Interval(IntervalUnit::YearMonth), true),
                Field::new("c26", DataType::Interval(IntervalUnit::DayTime), true),
                // Field::new("c27", DataType::Duration(TimeUnit::Second), false),
                // Field::new("c28", DataType::Duration(TimeUnit::Millisecond), false),
                // Field::new("c29", DataType::Duration(TimeUnit::Microsecond), false),
                // Field::new("c30", DataType::Duration(TimeUnit::Nanosecond), false),
                Field::new_dict(
                    "c31",
                    DataType::Dictionary(
                        Box::new(DataType::Int32),
                        Box::new(DataType::Utf8),
                    ),
                    true,
                    123,
                    true,
                ),
                Field::new("c32", DataType::LargeBinary, true),
                Field::new("c33", DataType::LargeUtf8, true),
                // Field::new(
                //     "c34",
                //     DataType::LargeList(Box::new(DataType::List(Box::new(
                //         DataType::Struct(vec![
                //             Field::new("a", DataType::Int16, true),
                //             Field::new("b", DataType::Float64, true),
                //         ]),
                //     )))),
                //     true,
                // ),
                Field::new("c35", DataType::Null, true),
            ],
            metadata,
        );

        // write to an empty parquet file so that schema is serialized
        let file = get_temp_file("test_arrow_schema_roundtrip.parquet", &[]);
        let mut writer = ArrowWriter::try_new(
            file.try_clone().unwrap(),
            Arc::new(schema.clone()),
            None,
        )?;
        writer.close()?;

        // read file back
        let parquet_reader = SerializedFileReader::try_from(file)?;
        let mut arrow_reader = ParquetFileArrowReader::new(Rc::new(parquet_reader));
        let read_schema = arrow_reader.get_schema()?;
        assert_eq!(schema, read_schema);

        // read all fields by columns
        let partial_read_schema =
            arrow_reader.get_schema_by_columns(0..(schema.fields().len()), false)?;
        assert_eq!(schema, partial_read_schema);

        Ok(())
    }

    #[test]
    #[ignore = "Roundtrip of lists currently fails because we don't check their types correctly in the Arrow schema"]
    fn test_arrow_schema_roundtrip_lists() -> Result<()> {
        let metadata: HashMap<String, String> =
            [("Key".to_string(), "Value".to_string())]
                .iter()
                .cloned()
                .collect();

        let schema = Schema::new_with_metadata(
            vec![
                Field::new(
                    "c21",
                    DataType::List(Box::new(Field::new(
                        "array",
                        DataType::Boolean,
                        true,
                    ))),
                    false,
                ),
                Field::new(
                    "c22",
                    DataType::FixedSizeList(
                        Box::new(Field::new("items", DataType::Boolean, false)),
                        5,
                    ),
                    false,
                ),
                Field::new(
                    "c23",
                    DataType::List(Box::new(Field::new(
                        "items",
                        DataType::LargeList(Box::new(Field::new(
                            "items",
                            DataType::Struct(vec![
                                Field::new("a", DataType::Int16, true),
                                Field::new("b", DataType::Float64, false),
                            ]),
                            true,
                        ))),
                        true,
                    ))),
                    true,
                ),
            ],
            metadata,
        );

        // write to an empty parquet file so that schema is serialized
        let file = get_temp_file("test_arrow_schema_roundtrip_lists.parquet", &[]);
        let mut writer = ArrowWriter::try_new(
            file.try_clone().unwrap(),
            Arc::new(schema.clone()),
            None,
        )?;
        writer.close()?;

        // read file back
        let parquet_reader = SerializedFileReader::try_from(file)?;
        let mut arrow_reader = ParquetFileArrowReader::new(Rc::new(parquet_reader));
        let read_schema = arrow_reader.get_schema()?;
        assert_eq!(schema, read_schema);

        // read all fields by columns
        let partial_read_schema =
            arrow_reader.get_schema_by_columns(0..(schema.fields().len()), false)?;
        assert_eq!(schema, partial_read_schema);

        Ok(())
    }
}
