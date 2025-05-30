mod column_operation;
mod column_writers;
mod serializer;
mod value_index;

use std::io;

use crate::common::CountingWriter;
use crate::stacker::{Addr, ArenaHashMap, MemoryArena};
use column_operation::ColumnOperation;
pub(crate) use column_writers::CompatibleNumericalTypes;
pub(crate) use serializer::ColumnarSerializer;

use crate::columnar::column_index::SerializableColumnIndex;
use crate::columnar::column_values::MonotonicallyMappableToU64;
use crate::columnar::columnar::column_type::ColumnType;
use crate::columnar::columnar::writer::column_writers::{
    ColumnWriter, NumericalColumnWriter, StrOrBytesColumnWriter,
};
use crate::columnar::columnar::writer::value_index::{IndexBuilder, PreallocatedIndexBuilders};
use crate::columnar::dictionary::{DictionaryBuilder, TermIdMapping, UnorderedId};
use crate::columnar::value::{Coerce, NumericalType, NumericalValue};
use crate::columnar::{Cardinality, MonotonicallyMappableToU128, RowId};

/// This is a set of buffers that are used to temporarily write the values into before passing them
/// to the columnar field codecs.
#[derive(Default)]
struct SpareBuffers {
    value_index_builders: PreallocatedIndexBuilders,
    u64_values: Vec<u64>,
    u128_values: Vec<u128>,
}

/// Makes it possible to create a new columnar.
///
/// ```rust
/// use tantivy::columnar::ColumnarWriter;
///
/// let mut columnar_writer = ColumnarWriter::default();
/// columnar_writer.record_numerical(0u32 /* doc id */, "price", 10u64);
/// columnar_writer.record_numerical(0u32 /* doc id */, "price", 10.5f64); //< uh oh we ended up mixing integer and floats.
/// let mut wrt: Vec<u8> =  Vec::new();
/// columnar_writer.serialize(2u32, None, &mut wrt).unwrap();
/// ```
#[derive(Default)]
pub struct ColumnarWriter {
    numerical_field_hash_map: ArenaHashMap,
    datetime_field_hash_map: ArenaHashMap,
    bool_field_hash_map: ArenaHashMap,
    ip_addr_field_hash_map: ArenaHashMap,
    bytes_field_hash_map: ArenaHashMap,
    str_field_hash_map: ArenaHashMap,
    arena: MemoryArena,
    // Dictionaries used to store dictionary-encoded values.
    dictionaries: Vec<DictionaryBuilder>,
    buffers: SpareBuffers,
}

impl ColumnarWriter {
    pub fn mem_usage(&self) -> usize {
        self.arena.mem_usage()
            + self.numerical_field_hash_map.mem_usage()
            + self.bool_field_hash_map.mem_usage()
            + self.bytes_field_hash_map.mem_usage()
            + self.str_field_hash_map.mem_usage()
            + self.ip_addr_field_hash_map.mem_usage()
            + self.datetime_field_hash_map.mem_usage()
            + self
                .dictionaries
                .iter()
                .map(|dict| dict.mem_usage())
                .sum::<usize>()
    }

    /// Returns the list of doc ids from 0..num_docs sorted by the `sort_field`
    /// column.
    ///
    /// If the column is multivalued, use the first value for scoring.
    /// If no value is associated to a specific row, the document is assigned
    /// the lowest possible score.
    ///
    /// The sort applied is stable.
    pub fn sort_order(&self, sort_field: &str, num_docs: RowId, reversed: bool) -> Vec<u32> {
        let Some(numerical_col_writer) = self
            .numerical_field_hash_map
            .get::<NumericalColumnWriter>(sort_field.as_bytes())
            .or_else(|| {
                self.datetime_field_hash_map
                    .get::<NumericalColumnWriter>(sort_field.as_bytes())
            })
        else {
            return Vec::new();
        };
        let mut symbols_buffer = Vec::new();
        let mut values = Vec::new();
        let mut start_doc_check_fill = 0;
        let mut current_doc_opt: Option<RowId> = None;
        // Assumption: NewDoc will never call the same doc twice and is strictly increasing between
        // calls
        for op in numerical_col_writer.operation_iterator(&self.arena, None, &mut symbols_buffer) {
            match op {
                ColumnOperation::NewDoc(doc) => {
                    current_doc_opt = Some(doc);
                }
                ColumnOperation::Value(numerical_value) => {
                    if let Some(current_doc) = current_doc_opt {
                        // Fill up with 0.0 since last doc
                        values.extend((start_doc_check_fill..current_doc).map(|doc| (0.0, doc)));
                        start_doc_check_fill = current_doc + 1;
                        // handle multi values
                        current_doc_opt = None;

                        let score: f32 = f64::coerce(numerical_value) as f32;
                        values.push((score, current_doc));
                    }
                }
            }
        }
        for doc in values.len() as u32..num_docs {
            values.push((0.0f32, doc));
        }
        values.sort_by(|(left_score, _), (right_score, _)| {
            if reversed {
                right_score.total_cmp(left_score)
            } else {
                left_score.total_cmp(right_score)
            }
        });
        values.into_iter().map(|(_score, doc)| doc).collect()
    }

    /// Records a column type. This is useful to bypass the coercion process,
    /// makes sure the empty is present in the resulting columnar, or set
    /// the `sort_values_within_row`.
    ///
    /// `sort_values_within_row` is only allowed for `Bytes` or `Str` columns.
    pub fn record_column_type(
        &mut self,
        column_name: &str,
        column_type: ColumnType,
        sort_values_within_row: bool,
    ) {
        if sort_values_within_row {
            assert!(
                column_type == ColumnType::Bytes,
                "sort_values_within_row is only allowed for Bytes columns",
            );
        }
        match column_type {
            ColumnType::Bytes => {
                let (hash_map, dictionaries) =
                    (&mut self.bytes_field_hash_map, &mut self.dictionaries);
                hash_map.mutate_or_create(
                    column_name.as_bytes(),
                    |column_opt: Option<StrOrBytesColumnWriter>| {
                        let mut column_writer = if let Some(column_writer) = column_opt {
                            column_writer
                        } else {
                            let dictionary_id = dictionaries.len() as u32;
                            dictionaries.push(DictionaryBuilder::default());
                            StrOrBytesColumnWriter::with_dictionary_id(dictionary_id)
                        };
                        column_writer.sort_values_within_row = sort_values_within_row;
                        column_writer
                    },
                );
            }
            ColumnType::Bool => {
                self.bool_field_hash_map.mutate_or_create(
                    column_name.as_bytes(),
                    |column_opt: Option<ColumnWriter>| column_opt.unwrap_or_default(),
                );
            }
            ColumnType::DateTime => {
                self.datetime_field_hash_map.mutate_or_create(
                    column_name.as_bytes(),
                    |column_opt: Option<ColumnWriter>| column_opt.unwrap_or_default(),
                );
            }
            ColumnType::I64 | ColumnType::F64 | ColumnType::U64 | ColumnType::U128 => {
                let numerical_type = column_type.numerical_type().unwrap();
                self.numerical_field_hash_map.mutate_or_create(
                    column_name.as_bytes(),
                    |column_opt: Option<NumericalColumnWriter>| {
                        let mut column: NumericalColumnWriter = column_opt.unwrap_or_default();
                        column.force_numerical_type(numerical_type);
                        column
                    },
                );
            }
        }
    }

    pub fn record_numerical<T: Into<NumericalValue> + Copy>(
        &mut self,
        doc: RowId,
        column_name: &str,
        numerical_value: T,
    ) {
        let (hash_map, arena) = (&mut self.numerical_field_hash_map, &mut self.arena);
        hash_map.mutate_or_create(
            column_name.as_bytes(),
            |column_opt: Option<NumericalColumnWriter>| {
                let mut column: NumericalColumnWriter = column_opt.unwrap_or_default();
                column.record_numerical_value(doc, numerical_value.into(), arena);
                column
            },
        );
    }

    pub fn record_bool(&mut self, doc: RowId, column_name: &str, val: bool) {
        let (hash_map, arena) = (&mut self.bool_field_hash_map, &mut self.arena);
        hash_map.mutate_or_create(
            column_name.as_bytes(),
            |column_opt: Option<ColumnWriter>| {
                let mut column: ColumnWriter = column_opt.unwrap_or_default();
                column.record(doc, val, arena);
                column
            },
        );
    }

    pub fn record_datetime(
        &mut self,
        doc: RowId,
        column_name: &str,
        datetime: crate::common::DateTime,
    ) {
        let (hash_map, arena) = (&mut self.datetime_field_hash_map, &mut self.arena);
        hash_map.mutate_or_create(
            column_name.as_bytes(),
            |column_opt: Option<ColumnWriter>| {
                let mut column: ColumnWriter = column_opt.unwrap_or_default();
                column.record(
                    doc,
                    NumericalValue::I64(datetime.into_timestamp_nanos()),
                    arena,
                );
                column
            },
        );
    }

    pub fn record_bytes(&mut self, doc: RowId, column_name: &str, value: &[u8]) {
        let (hash_map, arena, dictionaries) = (
            &mut self.bytes_field_hash_map,
            &mut self.arena,
            &mut self.dictionaries,
        );
        hash_map.mutate_or_create(
            column_name.as_bytes(),
            |column_opt: Option<StrOrBytesColumnWriter>| {
                let mut column: StrOrBytesColumnWriter = column_opt.unwrap_or_else(|| {
                    // Each column has its own dictionary
                    let dictionary_id = dictionaries.len() as u32;
                    dictionaries.push(DictionaryBuilder::default());
                    StrOrBytesColumnWriter::with_dictionary_id(dictionary_id)
                });
                column.record_bytes(doc, value, dictionaries, arena);
                column
            },
        );
    }
    pub fn serialize(
        &mut self,
        num_docs: RowId,
        old_to_new_row_ids: Option<&[RowId]>,
        wrt: &mut dyn io::Write,
    ) -> io::Result<()> {
        let mut serializer = ColumnarSerializer::new(wrt);
        let mut columns: Vec<(&[u8], ColumnType, Addr)> = self
            .numerical_field_hash_map
            .iter()
            .map(|(column_name, addr)| {
                let numerical_column_writer: NumericalColumnWriter =
                    self.numerical_field_hash_map.read(addr);
                let column_type = numerical_column_writer.numerical_type().into();
                (column_name, column_type, addr)
            })
            .collect();
        columns.extend(
            self.bytes_field_hash_map
                .iter()
                .map(|(term, addr)| (term, ColumnType::Bytes, addr)),
        );
        columns.extend(
            self.bool_field_hash_map
                .iter()
                .map(|(column_name, addr)| (column_name, ColumnType::Bool, addr)),
        );
        columns.extend(
            self.datetime_field_hash_map
                .iter()
                .map(|(column_name, addr)| (column_name, ColumnType::DateTime, addr)),
        );
        columns.sort_unstable_by_key(|(column_name, col_type, _)| (*column_name, *col_type));

        let (arena, buffers, dictionaries) = (&self.arena, &mut self.buffers, &self.dictionaries);
        let mut symbol_byte_buffer: Vec<u8> = Vec::new();
        for (column_name, column_type, addr) in columns {
            match column_type {
                ColumnType::Bool => {
                    let column_writer: ColumnWriter = self.bool_field_hash_map.read(addr);
                    let cardinality = column_writer.get_cardinality();
                    let mut column_serializer =
                        serializer.start_serialize_column(column_name, column_type);
                    serialize_bool_column(
                        cardinality,
                        column_writer.operation_iterator(
                            arena,
                            old_to_new_row_ids,
                            &mut symbol_byte_buffer,
                        ),
                        buffers,
                        &mut column_serializer,
                    )?;
                    column_serializer.finalize()?;
                }
                ColumnType::Bytes => {
                    let str_or_bytes_column_writer: StrOrBytesColumnWriter =
                        self.bytes_field_hash_map.read(addr);
                    let dictionary_builder =
                        &dictionaries[str_or_bytes_column_writer.dictionary_id as usize];
                    let cardinality = str_or_bytes_column_writer.column_writer.get_cardinality();
                    let mut column_serializer =
                        serializer.start_serialize_column(column_name, column_type);
                    serialize_bytes_or_str_column(
                        cardinality,
                        dictionary_builder,
                        str_or_bytes_column_writer.operation_iterator(
                            arena,
                            old_to_new_row_ids,
                            &mut symbol_byte_buffer,
                        ),
                        buffers,
                        &self.arena,
                        &mut column_serializer,
                    )?;
                    column_serializer.finalize()?;
                }
                ColumnType::F64 | ColumnType::I64 | ColumnType::U64 | ColumnType::U128 => {
                    let numerical_column_writer: NumericalColumnWriter =
                        self.numerical_field_hash_map.read(addr);
                    let cardinality = numerical_column_writer.cardinality();
                    let mut column_serializer =
                        serializer.start_serialize_column(column_name, column_type);
                    let numerical_type = column_type.numerical_type().unwrap();
                    serialize_numerical_column(
                        cardinality,
                        numerical_type,
                        numerical_column_writer.operation_iterator(
                            arena,
                            old_to_new_row_ids,
                            &mut symbol_byte_buffer,
                        ),
                        buffers,
                        &mut column_serializer,
                    )?;
                    column_serializer.finalize()?;
                }
                ColumnType::DateTime => {
                    let column_writer: ColumnWriter = self.datetime_field_hash_map.read(addr);
                    let cardinality = column_writer.get_cardinality();
                    let mut column_serializer =
                        serializer.start_serialize_column(column_name, ColumnType::DateTime);
                    serialize_numerical_column(
                        cardinality,
                        NumericalType::I64,
                        column_writer.operation_iterator(
                            arena,
                            old_to_new_row_ids,
                            &mut symbol_byte_buffer,
                        ),
                        buffers,
                        &mut column_serializer,
                    )?;
                    column_serializer.finalize()?;
                }
            };
        }
        serializer.finalize(num_docs)?;
        Ok(())
    }
}

// Serialize [Dictionary, Column, dictionary num bytes U64::LE]
// Column: [Column Index, Column Values, column index num bytes U32::LE]
#[allow(clippy::too_many_arguments)]
fn serialize_bytes_or_str_column(
    cardinality: Cardinality,
    dictionary_builder: &DictionaryBuilder,
    operation_it: impl Iterator<Item = ColumnOperation<UnorderedId>>,
    buffers: &mut SpareBuffers,
    arena: &MemoryArena,
    wrt: impl io::Write,
) -> io::Result<()> {
    let SpareBuffers {
        value_index_builders,
        u64_values,
        u128_values: _,
    } = buffers;
    let mut counting_writer = CountingWriter::wrap(wrt);
    let term_id_mapping: TermIdMapping =
        dictionary_builder.serialize(arena, &mut counting_writer)?;
    let dictionary_num_bytes: u64 = counting_writer.written_bytes();
    let mut wrt = counting_writer.finish();
    let operation_iterator = operation_it.map(|symbol: ColumnOperation<UnorderedId>| {
        // We map unordered ids to ordered ids.
        match symbol {
            ColumnOperation::Value(unordered_id) => {
                let ordered_id = term_id_mapping.to_ord(unordered_id);
                ColumnOperation::Value(ordered_id.0 as u64)
            }
            ColumnOperation::NewDoc(doc) => ColumnOperation::NewDoc(doc),
        }
    });
    send_to_serialize_column_mappable_to_u64(
        operation_iterator,
        cardinality,
        value_index_builders,
        u64_values,
        &mut wrt,
    )?;
    wrt.write_all(&dictionary_num_bytes.to_le_bytes()[..])?;
    Ok(())
}

fn serialize_numerical_column(
    cardinality: Cardinality,
    numerical_type: NumericalType,
    op_iterator: impl Iterator<Item = ColumnOperation<NumericalValue>>,
    buffers: &mut SpareBuffers,
    wrt: &mut impl io::Write,
) -> io::Result<()> {
    let SpareBuffers {
        value_index_builders,
        u64_values,
        u128_values,
    } = buffers;
    match numerical_type {
        NumericalType::I64 => {
            send_to_serialize_column_mappable_to_u64(
                coerce_numerical_symbol::<i64>(op_iterator),
                cardinality,
                value_index_builders,
                u64_values,
                wrt,
            )?;
        }
        NumericalType::U64 => {
            send_to_serialize_column_mappable_to_u64(
                coerce_numerical_symbol::<u64>(op_iterator),
                cardinality,
                value_index_builders,
                u64_values,
                wrt,
            )?;
        }
        NumericalType::F64 => {
            send_to_serialize_column_mappable_to_u64(
                coerce_numerical_symbol::<f64>(op_iterator),
                cardinality,
                value_index_builders,
                u64_values,
                wrt,
            )?;
        }
        NumericalType::U128 => {
            send_to_serialize_column_mappable_to_u128(
                coerce_numerical_symbol_to_u128::<u128>(op_iterator),
                cardinality,
                value_index_builders,
                u128_values,
                wrt,
            )?;
        }
    };
    Ok(())
}

fn serialize_bool_column(
    cardinality: Cardinality,
    column_operations_it: impl Iterator<Item = ColumnOperation<bool>>,
    buffers: &mut SpareBuffers,
    wrt: &mut impl io::Write,
) -> io::Result<()> {
    let SpareBuffers {
        value_index_builders,
        u64_values,
        u128_values: _,
    } = buffers;
    send_to_serialize_column_mappable_to_u64(
        column_operations_it.map(|bool_column_operation| match bool_column_operation {
            ColumnOperation::NewDoc(doc) => ColumnOperation::NewDoc(doc),
            ColumnOperation::Value(bool_val) => ColumnOperation::Value(bool_val.to_u64()),
        }),
        cardinality,
        value_index_builders,
        u64_values,
        wrt,
    )?;
    Ok(())
}

fn send_to_serialize_column_mappable_to_u64(
    op_iterator: impl Iterator<Item = ColumnOperation<u64>>,
    cardinality: Cardinality,
    value_index_builders: &mut PreallocatedIndexBuilders,
    values: &mut Vec<u64>,
    mut wrt: impl io::Write,
) -> io::Result<()> {
    values.clear();
    let serializable_column_index = match cardinality {
        Cardinality::Full => {
            consume_operation_iterator(
                op_iterator,
                value_index_builders.borrow_required_index_builder(),
                values,
            );
            SerializableColumnIndex::Full
        }
    };
    crate::columnar::column::serialize_column_mappable_to_u64(
        serializable_column_index,
        &&values[..],
        &mut wrt,
    )?;
    Ok(())
}

fn send_to_serialize_column_mappable_to_u128(
    op_iterator: impl Iterator<Item = ColumnOperation<u128>>,
    cardinality: Cardinality,
    value_index_builders: &mut PreallocatedIndexBuilders,
    values: &mut Vec<u128>,
    mut wrt: impl io::Write,
) -> io::Result<()> {
    values.clear();
    let serializable_column_index = match cardinality {
        Cardinality::Full => {
            consume_operation_iterator(
                op_iterator,
                value_index_builders.borrow_required_index_builder(),
                values,
            );
            SerializableColumnIndex::Full
        }
    };
    crate::columnar::column::serialize_column_mappable_to_u128(
        serializable_column_index,
        &&values[..],
        &mut wrt,
    )?;
    Ok(())
}

fn coerce_numerical_symbol<T>(
    operation_iterator: impl Iterator<Item = ColumnOperation<NumericalValue>>,
) -> impl Iterator<Item = ColumnOperation<u64>>
where
    T: Coerce + MonotonicallyMappableToU64,
{
    operation_iterator.map(|symbol| match symbol {
        ColumnOperation::NewDoc(doc) => ColumnOperation::NewDoc(doc),
        ColumnOperation::Value(numerical_value) => {
            ColumnOperation::Value(T::coerce(numerical_value).to_u64())
        }
    })
}

fn coerce_numerical_symbol_to_u128<T>(
    operation_iterator: impl Iterator<Item = ColumnOperation<NumericalValue>>,
) -> impl Iterator<Item = ColumnOperation<u128>>
where
    T: Coerce + MonotonicallyMappableToU128,
{
    operation_iterator.map(|symbol| match symbol {
        ColumnOperation::NewDoc(doc) => ColumnOperation::NewDoc(doc),
        ColumnOperation::Value(numerical_value) => {
            ColumnOperation::Value(T::coerce(numerical_value).to_u128())
        }
    })
}

fn consume_operation_iterator<T: Ord, TIndexBuilder: IndexBuilder>(
    operation_iterator: impl Iterator<Item = ColumnOperation<T>>,
    index_builder: &mut TIndexBuilder,
    values: &mut Vec<T>,
) {
    for symbol in operation_iterator {
        match symbol {
            ColumnOperation::NewDoc(doc) => {
                index_builder.record_row(doc);
            }
            ColumnOperation::Value(value) => {
                index_builder.record_value();
                values.push(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::stacker::MemoryArena;

    use super::{Cardinality, NumericalValue};
    use crate::columnar::columnar::writer::column_operation::ColumnOperation;

    #[test]
    fn test_column_writer_required_simple() {
        let mut arena = MemoryArena::default();
        let mut column_writer = super::ColumnWriter::default();
        column_writer.record(0u32, NumericalValue::from(14i64), &mut arena);
        column_writer.record(1u32, NumericalValue::from(15i64), &mut arena);
        column_writer.record(2u32, NumericalValue::from(-16i64), &mut arena);
        assert_eq!(column_writer.get_cardinality(), Cardinality::Full);
        let mut buffer = Vec::new();
        let symbols: Vec<ColumnOperation<NumericalValue>> = column_writer
            .operation_iterator(&arena, None, &mut buffer)
            .collect();
        assert_eq!(symbols.len(), 6);
        assert!(matches!(symbols[0], ColumnOperation::NewDoc(0u32)));
        assert!(matches!(
            symbols[1],
            ColumnOperation::Value(NumericalValue::I64(14i64))
        ));
        assert!(matches!(symbols[2], ColumnOperation::NewDoc(1u32)));
        assert!(matches!(
            symbols[3],
            ColumnOperation::Value(NumericalValue::I64(15i64))
        ));
        assert!(matches!(symbols[4], ColumnOperation::NewDoc(2u32)));
        assert!(matches!(
            symbols[5],
            ColumnOperation::Value(NumericalValue::I64(-16i64))
        ));
    }
}
