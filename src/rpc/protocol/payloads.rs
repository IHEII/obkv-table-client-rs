/*-
 * #%L
 * OBKV Table Client Framework
 * %%
 * Copyright (C) 2021 OceanBase
 * %%
 * OBKV Table Client Framework is licensed under Mulan PSL v2.
 * You can use this software according to the terms and conditions of the
 * Mulan PSL v2. You may obtain a copy of Mulan PSL v2 at:
 *          http://license.coscl.org.cn/MulanPSL2
 * THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
 * KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
 * NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
 * See the Mulan PSL v2 for more details.
 * #L%
 */

#![allow(dead_code)]

use std::{
    collections::{HashMap, HashSet},
    io, mem,
    net::SocketAddr,
    time::Duration,
};

use bytes::{Buf, BufMut, BytesMut};

use super::{
    BasePayLoad, ObPayload, ObTablePacketCode, ProtoDecoder, ProtoEncoder, Result, TraceId,
};
use crate::filter::FilterEncoder;
use crate::rpc::protocol::lsop::{ObTableSingleOpEntity, ObTableSingleOpQuery};
use crate::{
    location::OB_INVALID_ID,
    query::ObNewRange,
    rpc::protocol::{
        codes::ResultCodes,
        lsop::{ObTableSingleOp, ObTableTabletOp, ObTableTabletOpFlag},
    },
    serde_obkv::{util, value::Value},
    util::{
        decode_value, duration_to_millis, obversion::ob_vsn_major, security, string_from_bytes,
    },
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ObTableEntityType {
    Dynamic = 0,
    KV = 1,
    HKV = 2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ObTableConsistencyLevel {
    Strong = 0,
    Eventual = 1,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ObTableOperationType {
    Get = 0,
    Insert = 1,
    Del = 2,
    Update = 3,
    InsertOrUpdate = 4,
    Replace = 5,
    Increment = 6,
    Append = 7,
    Scan = 8,
    TTL = 9,
    CheckAndInsertUp = 10,
    Invalid = 11,
}

impl From<i8> for ObTableOperationType {
    fn from(value: i8) -> Self {
        match value {
            0 => ObTableOperationType::Get,
            1 => ObTableOperationType::Insert,
            2 => ObTableOperationType::Del,
            3 => ObTableOperationType::Update,
            4 => ObTableOperationType::InsertOrUpdate,
            5 => ObTableOperationType::Replace,
            6 => ObTableOperationType::Increment,
            7 => ObTableOperationType::Append,
            8 => ObTableOperationType::Scan,
            9 => ObTableOperationType::TTL,
            10 => ObTableOperationType::CheckAndInsertUp,
            _ => panic!("Invalid value for ObTableSingleOpType"),
        }
    }
}

impl ObTableOperationType {
    pub fn from_i8(i: i8) -> Result<ObTableOperationType> {
        match i {
            0 => Ok(ObTableOperationType::Get),
            1 => Ok(ObTableOperationType::Insert),
            2 => Ok(ObTableOperationType::Del),
            3 => Ok(ObTableOperationType::Update),
            4 => Ok(ObTableOperationType::InsertOrUpdate),
            5 => Ok(ObTableOperationType::Replace),
            6 => Ok(ObTableOperationType::Increment),
            7 => Ok(ObTableOperationType::Append),
            8 => Ok(ObTableOperationType::Scan),
            9 => Ok(ObTableOperationType::TTL),
            10 => Ok(ObTableOperationType::CheckAndInsertUp),
            _ => Err(io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid operation type: {i}"),
            )),
        }
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        match self {
            ObTableOperationType::Get => "get",
            ObTableOperationType::Insert => "insert",
            ObTableOperationType::Del => "delete",
            ObTableOperationType::Update => "update",
            ObTableOperationType::InsertOrUpdate => "insert_or_update",
            ObTableOperationType::Replace => "replace",
            ObTableOperationType::Increment => "increment",
            ObTableOperationType::Append => "append",
            ObTableOperationType::Scan => "scan",
            ObTableOperationType::TTL => "TTL",
            ObTableOperationType::CheckAndInsertUp => "check_and_upsert",
            ObTableOperationType::Invalid => "invalid_type",
        }
    }

    pub fn need_encode_query(&self) -> bool {
        match self {
            ObTableOperationType::Get => false,
            ObTableOperationType::Insert => false,
            ObTableOperationType::Del => false,
            ObTableOperationType::Update => false,
            ObTableOperationType::InsertOrUpdate => false,
            ObTableOperationType::Replace => false,
            ObTableOperationType::Increment => false,
            ObTableOperationType::Append => false,
            ObTableOperationType::Scan => false,
            ObTableOperationType::TTL => false,
            ObTableOperationType::CheckAndInsertUp => true,
            ObTableOperationType::Invalid => false,
        }
    }
}

/// OB Obj encode type
/// [`ObjType`] or [`TableObjType`]
#[derive(Default, Debug, Clone, PartialEq, Copy)]
pub enum ObjEncodeType {
    #[default]
    Obj = 0,
    TableObj = 1,
}

/// OB row key list.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct ObRowKey {
    column_names: Vec<String>,
    keys: Vec<Value>,
    obj_type: ObjEncodeType,
}

impl ObRowKey {
    pub fn new(keys: Vec<Value>) -> ObRowKey {
        ObRowKey {
            column_names: Vec::with_capacity(0),
            keys,
            obj_type: ObjEncodeType::Obj,
        }
    }

    pub fn new_with_obj_type(keys: Vec<Value>, obj_type: ObjEncodeType) -> ObRowKey {
        ObRowKey {
            column_names: Vec::with_capacity(0),
            keys,
            obj_type,
        }
    }

    pub fn set_column_names(&mut self, column_names: Vec<String>) {
        self.column_names = column_names
    }

    pub fn column_names(&self) -> &[String] {
        &self.column_names
    }

    pub fn take_column_names(self) -> Vec<String> {
        self.column_names
    }

    pub fn keys(&self) -> &[Value] {
        &self.keys
    }

    pub fn take_keys(self) -> Vec<Value> {
        self.keys
    }

    pub fn set_obj_type(&mut self, obj_type: ObjEncodeType) {
        self.obj_type = obj_type;
    }

    pub fn content_len(&self) -> Result<usize> {
        let mut len: usize = 0;
        len += util::encoded_length_vi64(self.keys.len() as i64);

        match &self.obj_type {
            ObjEncodeType::Obj => {
                for key in &self.keys {
                    len += key.len();
                }
            }
            ObjEncodeType::TableObj => {
                for key in &self.keys {
                    len += key.table_obj_len();
                }
            }
        }

        Ok(len)
    }
}

impl ProtoEncoder for ObRowKey {
    fn encode(&self, buf: &mut BytesMut) -> Result<()> {
        util::encode_vi64(self.keys.len() as i64, buf)?;

        match &self.obj_type {
            ObjEncodeType::Obj => {
                for key in &self.keys {
                    key.encode(buf)?;
                }
            }
            ObjEncodeType::TableObj => {
                for key in &self.keys {
                    key.table_obj_encode(buf)?;
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ObTableEntity {
    base: BasePayLoad,
    row_key: ObRowKey,
    properties: HashMap<String, Value>,
}

impl ObTableEntity {
    pub fn new(row_keys: Vec<Value>) -> ObTableEntity {
        ObTableEntity {
            base: BasePayLoad::dummy(),
            row_key: ObRowKey::new(row_keys),
            properties: HashMap::new(),
        }
    }

    pub fn properties(&self) -> &HashMap<String, Value> {
        &self.properties
    }

    pub fn take_properties(self) -> HashMap<String, Value> {
        self.properties
    }

    pub fn row_key(&self) -> &ObRowKey {
        &self.row_key
    }

    pub fn add_attr(&mut self, name: &str, v: Value) -> Option<Value> {
        self.properties.insert(name.to_owned(), v)
    }

    pub fn remove_attr(&mut self, name: &str) -> Option<Value> {
        self.properties.remove(name)
    }

    pub fn get_attr(&self, name: &str) -> Option<&Value> {
        self.properties.get(name)
    }

    pub fn set_row_key(&mut self, keys: Vec<Value>) {
        self.row_key.keys = keys;
    }

    pub fn set_row_key_names(&mut self, column_names: Vec<String>) {
        self.row_key.column_names = column_names
    }

    pub fn properties_names(&self) -> Vec<String> {
        self.properties.keys().cloned().collect()
    }

    pub fn properties_values(&self) -> Vec<Value> {
        self.properties.values().cloned().collect()
    }
}

impl ObPayload for ObTableEntity {
    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }

    //payload size, without header bytes
    fn content_len(&self) -> Result<usize> {
        let mut len: usize = self.row_key.content_len()?;

        len += util::encoded_length_vi64(self.properties.len() as i64);

        for (key, value) in &self.properties {
            len += util::encoded_length_vstring(key);
            len += value.len();
        }

        Ok(len)
    }
}

impl ProtoEncoder for ObTableEntity {
    fn encode(&self, buf: &mut BytesMut) -> Result<()> {
        self.encode_header(buf)?;

        self.row_key.encode(buf)?;

        util::encode_vi64(self.properties.len() as i64, buf)?;

        for (key, value) in &self.properties {
            util::encode_vstring(key, buf)?;
            value.encode(buf)?;
        }

        Ok(())
    }
}
impl ProtoDecoder for ObTableEntity {
    fn decode(&mut self, src: &mut BytesMut) -> Result<()> {
        self.decode_base(src)?;

        let row_keys_len = util::decode_vi64(src)?;

        if row_keys_len > 0 {
            let mut row_keys = vec![];
            for _ in 0..row_keys_len {
                row_keys.push(decode_value(src)?);
            }
            self.set_row_key(row_keys);
        }

        let properties_len = util::decode_vi64(src)?;

        if properties_len > 0 {
            for _ in 0..properties_len {
                let name = util::decode_vstring(src)?;

                self.add_attr(&name, decode_value(src)?);
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ObTableOperation {
    base: BasePayLoad,
    op_type: ObTableOperationType,
    entity: ObTableEntity,
}

impl ObTableOperation {
    pub fn new(
        operation_type: ObTableOperationType,
        row_keys: Vec<Value>,
        columns: Option<Vec<String>>,
        properties: Option<Vec<Value>>,
    ) -> ObTableOperation {
        let mut entity = ObTableEntity::new(row_keys);

        if let Some(cols) = columns {
            for i in 0..cols.len() {
                let name = &cols[i];

                let value = match properties {
                    Some(ref props) => props[i].to_owned(),
                    None => Value::default(),
                };

                entity.add_attr(name, value);
            }
        }

        ObTableOperation {
            base: BasePayLoad::dummy(),
            op_type: operation_type,
            entity,
        }
    }

    pub fn get_table_entity(&self) -> &ObTableEntity {
        &self.entity
    }

    pub fn take_table_entity(self) -> ObTableEntity {
        self.entity
    }

    pub fn get_type(&self) -> ObTableOperationType {
        self.op_type.to_owned()
    }

    pub fn get_row_key(&self) -> &ObRowKey {
        &self.entity.row_key
    }

    pub fn take_row_key(self) -> ObRowKey {
        self.entity.row_key
    }

    pub fn set_row_key_names(&mut self, row_key_names: Vec<String>) {
        self.entity.set_row_key_names(row_key_names)
    }
}

impl ObPayload for ObTableOperation {
    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }

    //payload size, without header bytes
    fn content_len(&self) -> Result<usize> {
        Ok(1 + self.entity.len()?)
    }
}

impl ProtoEncoder for ObTableOperation {
    fn encode(&self, buf: &mut BytesMut) -> Result<()> {
        self.encode_header(buf)?;

        buf.put_i8(self.op_type as i8);
        self.entity.encode(buf)?;

        Ok(())
    }
}

impl ProtoDecoder for ObTableOperation {
    fn decode(&mut self, _src: &mut BytesMut) -> Result<()> {
        unimplemented!();
    }
}

pub struct ObTableOperationRequest {
    base: BasePayLoad,
    credential: Vec<u8>,
    table_name: String,
    table_id: i64,
    partition_id: i64,
    entity_type: ObTableEntityType,
    table_operation: ObTableOperation,
    consistency_level: ObTableConsistencyLevel,
    return_row_key: bool,
    return_affected_entity: bool,
    return_affected_rows: bool,
}

impl ObTableOperationRequest {
    pub fn new(
        table_name: &str,
        operation_type: ObTableOperationType,
        row_keys: Vec<Value>,
        columns: Option<Vec<String>>,
        properties: Option<Vec<Value>>,
        timeout: Duration,
        flag: u16,
    ) -> ObTableOperationRequest {
        let operation = ObTableOperation::new(operation_type, row_keys, columns, properties);
        let mut base = BasePayLoad::new();
        base.timeout = duration_to_millis(&timeout);
        base.flag = flag;
        ObTableOperationRequest {
            base,
            credential: vec![],
            table_name: table_name.to_owned(),
            table_id: OB_INVALID_ID,
            partition_id: OB_INVALID_ID,
            entity_type: ObTableEntityType::Dynamic,
            table_operation: operation,
            consistency_level: ObTableConsistencyLevel::Strong,
            return_row_key: false,
            return_affected_entity: false,
            return_affected_rows: true,
        }
    }

    pub fn set_table_id(&mut self, table_id: i64) {
        self.table_id = table_id;
    }

    pub fn set_partition_id(&mut self, partition_id: i64) {
        self.partition_id = partition_id;
    }
}

impl ObPayload for ObTableOperationRequest {
    fn set_credential(&mut self, credential: &[u8]) {
        self.credential = credential.to_owned();
    }

    fn pcode(&self) -> ObTablePacketCode {
        ObTablePacketCode::Execute
    }

    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }

    //payload size, without header bytes
    fn content_len(&self) -> Result<usize> {
        Ok(util::encoded_length_bytes_string(&self.credential)
            + util::encoded_length_vstring(&self.table_name)
            + util::encoded_length_vi64(self.table_id)
            + if ob_vsn_major() >= 4 {
                8
            } else {
                util::encoded_length_vi64(self.partition_id)
            }
            + util::encoded_length_i8(self.entity_type as i8)
            + util::encoded_length_i8(self.consistency_level as i8)
            + util::encoded_length_i8(self.return_row_key as i8)
            + util::encoded_length_i8(self.return_affected_entity as i8)
            + util::encoded_length_i8(self.return_affected_rows as i8)
            + self.table_operation.len()?)
    }
}

impl ProtoEncoder for ObTableOperationRequest {
    fn encode(&self, buf: &mut BytesMut) -> Result<()> {
        self.encode_header(buf)?;

        util::encode_bytes_string(&self.credential, buf)?;
        util::encode_vstring(&self.table_name, buf)?;
        util::encode_vi64(self.table_id, buf)?;

        if ob_vsn_major() >= 4 {
            buf.put_i64(self.partition_id);
        } else {
            util::encode_vi64(self.partition_id, buf)?;
        }

        buf.put_i8(self.entity_type as i8);
        self.table_operation.encode(buf)?;
        buf.put_i8(self.consistency_level as i8);
        buf.put_i8(self.return_row_key as i8);
        buf.put_i8(self.return_affected_entity as i8);
        buf.put_i8(self.return_affected_rows as i8);

        Ok(())
    }
}

impl ProtoDecoder for ObTableOperationRequest {
    fn decode(&mut self, _src: &mut BytesMut) -> Result<()> {
        unimplemented!()
    }
}

/// Option for [`RawObTableOperation`]
#[derive(Debug, Clone, PartialEq)]
pub struct RawObTableOperationFlag {
    /// whether a check_and_execute option
    pub check_and_execute: bool,
    /// check whether any data meet the filter in check_and_execute
    pub check_exists: bool,
}

impl Default for RawObTableOperationFlag {
    fn default() -> Self {
        RawObTableOperationFlag::new()
    }
}

impl RawObTableOperationFlag {
    pub fn new() -> Self {
        RawObTableOperationFlag {
            check_and_execute: false,
            check_exists: true,
        }
    }
}

pub type RawObTableOperation = (
    ObTableOperationType,
    Option<Vec<String>>,             // row keys column names
    Vec<Value>,                      // row keys
    Option<Vec<String>>,             // properties column names
    Option<Vec<Value>>,              // properties
    Option<String>,                  // Filter String
    Option<RawObTableOperationFlag>, // option for RawObTableOperation
);

#[derive(Debug, Clone)]
pub struct ObTableBatchOperation {
    raw: bool,
    raw_ops: Vec<RawObTableOperation>,
    table_name: String,
    table_id: i64,
    partition_id: i64,
    base: BasePayLoad,
    ops: Vec<ObTableOperation>,
    read_only: bool,
    same_type: bool,
    same_properties_names: bool,
    atomic_op: bool,
    filters: Vec<String>,
    options: Vec<RawObTableOperationFlag>,
}

impl Default for ObTableBatchOperation {
    fn default() -> ObTableBatchOperation {
        ObTableBatchOperation::new()
    }
}

impl ObTableBatchOperation {
    fn internal_new(raw: bool, ops_num: usize) -> Self {
        let (raw_ops, ops) = if raw {
            (Vec::with_capacity(ops_num), Vec::new())
        } else {
            (Vec::new(), Vec::with_capacity(ops_num))
        };
        Self {
            raw,
            raw_ops,
            table_name: "".to_owned(),
            table_id: OB_INVALID_ID,
            partition_id: OB_INVALID_ID,
            base: BasePayLoad::dummy(),
            ops,
            read_only: true,
            same_type: true,
            same_properties_names: true,
            atomic_op: false,
            filters: Vec::new(),
            options: Vec::new(),
        }
    }

    pub fn new() -> Self {
        Self::internal_new(false, 0)
    }

    pub fn raw() -> Self {
        Self::internal_new(true, 0)
    }

    pub fn with_ops_num(num: usize) -> Self {
        Self::internal_new(false, num)
    }

    pub fn with_ops_num_raw(num: usize) -> Self {
        Self::internal_new(true, num)
    }

    pub fn is_raw(&self) -> bool {
        self.raw
    }

    pub fn ops_len(&self) -> usize {
        if self.raw_ops.is_empty() {
            self.ops.len()
        } else {
            self.raw_ops.len()
        }
    }

    pub fn set_table_name(&mut self, table_name: String) {
        self.table_name = table_name;
    }

    pub fn partition_id(&self) -> i64 {
        self.partition_id
    }

    pub fn set_partition_id(&mut self, part_id: i64) {
        self.partition_id = part_id
    }

    pub fn table_id(&self) -> i64 {
        self.table_id
    }

    pub fn set_table_id(&mut self, table_id: i64) {
        self.table_id = table_id
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_same_type(&self) -> bool {
        self.same_type
    }

    pub fn is_same_properties_names(&self) -> bool {
        self.same_properties_names
    }

    pub fn set_atomic_op(&mut self, atomic_op: bool) {
        self.atomic_op = atomic_op;
    }

    pub fn is_atomic_op(&self) -> bool {
        self.atomic_op
    }

    pub fn add_table_op(&mut self, op: ObTableOperation) {
        self.ops.push(op)
    }

    pub fn add_op(&mut self, raw_op: RawObTableOperation) {
        if self.raw {
            self.raw_ops.push(raw_op);
        } else {
            let (
                op_type,
                row_keys_names,
                row_keys,
                columns,
                properties,
                filter_string,
                option_flag,
            ) = raw_op;
            // update read_only
            if self.read_only && op_type != ObTableOperationType::Get {
                self.read_only = false;
            }

            // update same_type
            if self.same_type && !self.ops.is_empty() {
                let first_op = self.ops.first().unwrap();
                if first_op.get_type() != op_type {
                    self.same_type = false
                }
            }

            // update same_properties_names
            if self.same_properties_names && !self.ops.is_empty() {
                let first_op = self.ops.first().unwrap();
                let entity = first_op.get_table_entity();
                let properties = &entity.properties;

                if properties.is_empty() && columns.is_none() {
                    // no properties found
                    // so keep the same_properties_names as true
                } else if !properties.is_empty() && columns.is_some() {
                    let names = columns.as_ref().unwrap();
                    if properties.len() != names.len() {
                        self.same_properties_names = false;
                    } else {
                        let mut set = HashSet::new();
                        for name in names {
                            set.insert(name.to_owned());
                            if !properties.contains_key(name) {
                                break;
                            }
                        }
                        self.same_properties_names = set.len() == names.len()
                    }
                } else {
                    self.same_properties_names = false;
                }
            }

            // set filters
            if let Some(filter) = filter_string {
                self.filters.push(filter);
            }

            // set option flags
            if let Some(option) = option_flag {
                self.options.push(option);
            }
            let mut temp_op = ObTableOperation::new(op_type, row_keys, columns, properties);
            if let Some(rk_names) = row_keys_names {
                temp_op.set_row_key_names(rk_names);
            }
            self.ops.push(temp_op)
        }
    }

    pub fn get(&mut self, row_keys: Vec<Value>, columns: Vec<String>) {
        self.add_op((
            ObTableOperationType::Get,
            None,
            row_keys,
            Some(columns),
            None,
            None,
            None,
        ));
    }

    pub fn insert(&mut self, row_keys: Vec<Value>, columns: Vec<String>, properties: Vec<Value>) {
        self.add_op((
            ObTableOperationType::Insert,
            None,
            row_keys,
            Some(columns),
            Some(properties),
            None,
            None,
        ));
    }

    pub fn delete(&mut self, row_keys: Vec<Value>) {
        self.add_op((
            ObTableOperationType::Del,
            None,
            row_keys,
            None,
            None,
            None,
            None,
        ));
    }

    pub fn update(&mut self, row_keys: Vec<Value>, columns: Vec<String>, properties: Vec<Value>) {
        self.add_op((
            ObTableOperationType::Update,
            None,
            row_keys,
            Some(columns),
            Some(properties),
            None,
            None,
        ));
    }

    pub fn insert_or_update(
        &mut self,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
    ) {
        self.add_op((
            ObTableOperationType::InsertOrUpdate,
            None,
            row_keys,
            Some(columns),
            Some(properties),
            None,
            None,
        ));
    }

    /// check the data with corresponding row_keys
    /// whether meet the filter and execute the insertUp
    /// if check_exist is true: check if any data meet the filter
    /// if check_exist is false: check if all data do not meet the filter
    pub fn check_and_upsert(
        &mut self,
        row_keys_names: Vec<String>,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
        filter: impl FilterEncoder,
        check_exists: bool,
    ) {
        let mut option = RawObTableOperationFlag::new();
        option.check_and_execute = true;
        option.check_exists = check_exists;
        self.add_op((
            ObTableOperationType::CheckAndInsertUp,
            Some(row_keys_names),
            row_keys,
            Some(columns),
            Some(properties),
            Some(filter.encode()),
            Some(option),
        ))
    }

    /// check the data with corresponding row_keys
    /// if meet the filter: execute the InsertOrUpdate
    /// if do not meet the filter: do nothing
    pub fn check_and_upsert_if_exists(
        &mut self,
        row_keys_names: Vec<String>,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
        filter: impl FilterEncoder,
    ) {
        self.check_and_upsert(row_keys_names, row_keys, columns, properties, filter, true)
    }

    /// check the data with corresponding row_keys
    /// if row doesn't exist or do not meet the filter: execute the InsertOrUpdate
    /// if row meet the filter: do nothing
    pub fn check_and_upsert_if_not_exists(
        &mut self,
        row_keys_names: Vec<String>,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
        filter: impl FilterEncoder,
    ) {
        self.check_and_upsert(row_keys_names, row_keys, columns, properties, filter, false)
    }

    pub fn replace(&mut self, row_keys: Vec<Value>, columns: Vec<String>, properties: Vec<Value>) {
        self.add_op((
            ObTableOperationType::Replace,
            None,
            row_keys,
            Some(columns),
            Some(properties),
            None,
            None,
        ));
    }

    pub fn increment(
        &mut self,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
    ) {
        self.add_op((
            ObTableOperationType::Increment,
            None,
            row_keys,
            Some(columns),
            Some(properties),
            None,
            None,
        ));
    }

    pub fn append(&mut self, row_keys: Vec<Value>, columns: Vec<String>, properties: Vec<Value>) {
        self.add_op((
            ObTableOperationType::Append,
            None,
            row_keys,
            Some(columns),
            Some(properties),
            None,
            None,
        ));
    }

    pub fn get_ops(&self) -> &[ObTableOperation] {
        &self.ops
    }

    pub fn get_raw_ops(&self) -> &[RawObTableOperation] {
        &self.raw_ops
    }

    pub fn take_raw_ops(&mut self) -> Vec<RawObTableOperation> {
        mem::take(&mut self.raw_ops)
    }

    pub fn take_ops(&mut self) -> Vec<ObTableOperation> {
        mem::take(&mut self.ops)
    }

    pub fn get_filters(&self) -> &[String] {
        &self.filters
    }

    pub fn take_filters(&mut self) -> Vec<String> {
        mem::take(&mut self.filters)
    }

    pub fn get_options(&self) -> &[RawObTableOperationFlag] {
        &self.options
    }

    pub fn take_options(&mut self) -> Vec<RawObTableOperationFlag> {
        mem::take(&mut self.options)
    }

    pub fn generate_tablet_ops(&mut self) -> ObTableTabletOp {
        // only use this method when all operation is insertUp
        let mut ops = Vec::with_capacity(self.ops_len());
        for ((op, filter_string), option) in self
            .take_ops()
            .into_iter()
            .zip(self.take_filters().into_iter())
            .zip(self.take_options().into_iter())
        {
            // generate single op entity
            let orig_entity = op.take_table_entity();
            let row_key = orig_entity.row_key();
            let entity = ObTableSingleOpEntity::new(
                row_key.column_names.clone(),
                row_key.keys.clone(),
                orig_entity.properties_names(),
                orig_entity.properties_values(),
            );

            // generate query
            let range = ObNewRange::from_keys(row_key.keys.clone(), row_key.keys.clone());
            let mut query = ObTableSingleOpQuery::new(row_key.column_names.clone(), vec![range]);
            query.set_filter_string(filter_string);
            query.set_obj_type(ObjEncodeType::TableObj);

            // generate single op
            let mut single_op = ObTableSingleOp::new(ObTableOperationType::CheckAndInsertUp);
            single_op.set_check_not_exists(!option.check_exists);
            single_op.set_query(query);
            single_op.add_entity(entity);

            ops.push(single_op);
        }

        let mut tablet_option = ObTableTabletOpFlag::default();
        tablet_option.set_flag_is_same_type(true);

        ObTableTabletOp::internal_new(OB_INVALID_ID, tablet_option, ops)
    }
}

impl ObPayload for ObTableBatchOperation {
    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }

    fn content_len(&self) -> Result<usize> {
        let mut sz = 0usize;
        sz += util::encoded_length_vi64(self.ops.len() as i64);
        for op in self.ops.iter() {
            sz += op.len()?;
        }
        Ok(3 + sz)
    }
}

impl ProtoEncoder for ObTableBatchOperation {
    fn encode(&self, buf: &mut BytesMut) -> Result<()> {
        self.encode_header(buf)?;
        util::encode_vi64(self.ops.len() as i64, buf)?;
        for op in self.ops.iter() {
            op.encode(buf)?;
        }

        buf.put_i8(self.read_only as i8);
        buf.put_i8(self.same_type as i8);
        buf.put_i8(self.same_properties_names as i8);
        Ok(())
    }
}

impl ProtoDecoder for ObTableBatchOperation {
    fn decode(&mut self, _src: &mut BytesMut) -> Result<()> {
        unimplemented!();
    }
}

pub struct ObTableBatchOperationRequest {
    base: BasePayLoad,
    credential: Vec<u8>,
    table_name: String,
    table_id: i64,
    partition_id: i64,
    entity_type: ObTableEntityType,
    batch_operation: ObTableBatchOperation,
    consistency_level: ObTableConsistencyLevel,
    return_row_key: bool,
    return_affected_entity: bool,
    return_affected_rows: bool,
    atomic_op: bool,
}

impl ObTableBatchOperationRequest {
    pub fn new(batch_operation: ObTableBatchOperation, timeout: Duration, flag: u16) -> Self {
        let mut base = BasePayLoad::new();
        base.timeout = duration_to_millis(&timeout);
        base.flag = flag;
        Self {
            base,
            credential: vec![],
            table_name: batch_operation.table_name.to_owned(),
            table_id: batch_operation.table_id,
            partition_id: batch_operation.partition_id,
            entity_type: ObTableEntityType::Dynamic,
            atomic_op: batch_operation.is_atomic_op(),
            batch_operation,
            consistency_level: ObTableConsistencyLevel::Strong,
            return_row_key: false,
            return_affected_entity: false,
            return_affected_rows: true,
        }
    }
}

impl ObPayload for ObTableBatchOperationRequest {
    fn set_credential(&mut self, credential: &[u8]) {
        self.credential = credential.to_owned();
    }

    fn pcode(&self) -> ObTablePacketCode {
        ObTablePacketCode::BatchExecute
    }

    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }

    //payload size, without header bytes
    fn content_len(&self) -> Result<usize> {
        Ok(util::encoded_length_bytes_string(&self.credential)
            + util::encoded_length_vstring(&self.table_name)
            + util::encoded_length_vi64(self.table_id)
            + if ob_vsn_major() >= 4 {
                8
            } else {
                util::encoded_length_vi64(self.partition_id)
            }
            + self.batch_operation.len()?
            + util::encoded_length_i8(self.entity_type as i8)
            + util::encoded_length_i8(self.consistency_level as i8)
            + util::encoded_length_i8(self.return_row_key as i8)
            + util::encoded_length_i8(self.return_affected_entity as i8)
            + util::encoded_length_i8(self.return_affected_rows as i8)
            + util::encoded_length_i8(self.atomic_op as i8))
    }
}

impl ProtoEncoder for ObTableBatchOperationRequest {
    fn encode(&self, buf: &mut BytesMut) -> Result<()> {
        self.encode_header(buf)?;

        util::encode_bytes_string(&self.credential, buf)?;
        util::encode_vstring(&self.table_name, buf)?;
        util::encode_vi64(self.table_id, buf)?;

        buf.put_i8(self.entity_type as i8);
        self.batch_operation.encode(buf)?;
        buf.put_i8(self.consistency_level as i8);
        buf.put_i8(self.return_row_key as i8);
        buf.put_i8(self.return_affected_entity as i8);
        buf.put_i8(self.return_affected_rows as i8);
        if ob_vsn_major() >= 4 {
            buf.put_i64(self.partition_id);
        } else {
            util::encode_vi64(self.partition_id, buf)?;
        }
        buf.put_i8(self.atomic_op as i8);
        Ok(())
    }
}

impl ProtoDecoder for ObTableBatchOperationRequest {
    fn decode(&mut self, _src: &mut BytesMut) -> Result<()> {
        unimplemented!()
    }
}

/// Warning message returned from observer.
#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub struct ObRpcResultWarningMsg {
    base: BasePayLoad,
    timestamp: i64,
    log_level: i32,
    line_no: i32,
    code: i32,
    msg: Vec<u8>,
}

impl ObRpcResultWarningMsg {
    pub fn new() -> Self {
        ObRpcResultWarningMsg {
            base: BasePayLoad::dummy(),
            timestamp: 0,
            log_level: 0,
            line_no: 0,
            code: 0,
            msg: vec![],
        }
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    pub fn log_level(&self) -> i32 {
        self.log_level
    }

    pub fn line_number(&self) -> i32 {
        self.line_no
    }

    pub fn code(&self) -> i32 {
        self.code
    }

    pub fn message(&self) -> String {
        string_from_bytes(&self.msg)
    }
}

impl ObPayload for ObRpcResultWarningMsg {
    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }
}

impl ProtoDecoder for ObRpcResultWarningMsg {
    fn decode(&mut self, src: &mut BytesMut) -> Result<()> {
        self.decode_base(src)?;
        let len = util::decode_vi32(src)?;
        self.msg = util::split_buf_to(src, len as usize)?.to_vec();
        self.timestamp = util::decode_vi64(src)?;
        self.log_level = util::decode_vi32(src)?;
        self.line_no = util::decode_vi32(src)?;
        self.code = util::decode_vi32(src)?;

        Ok(())
    }
}

impl ProtoEncoder for ObRpcResultWarningMsg {
    fn encode(&self, _buf: &mut BytesMut) -> Result<()> {
        unimplemented!();
    }
}

#[derive(Debug)]
pub struct ObRpcResultCode {
    base: BasePayLoad,
    rcode: ResultCodes,
    msg: Vec<u8>,
    warning_msgs: Vec<ObRpcResultWarningMsg>,
}

impl Default for ObRpcResultCode {
    fn default() -> ObRpcResultCode {
        ObRpcResultCode::new()
    }
}

impl ObRpcResultCode {
    pub fn new() -> Self {
        ObRpcResultCode {
            base: BasePayLoad::dummy(),
            rcode: ResultCodes::OB_SUCCESS,
            msg: vec![],
            warning_msgs: vec![],
        }
    }

    pub fn is_success(&self) -> bool {
        self.rcode == ResultCodes::OB_SUCCESS
    }

    pub fn rcode(&self) -> ResultCodes {
        self.rcode
    }

    pub fn message(&self) -> String {
        string_from_bytes(&self.msg)
    }

    pub fn warning_msgs(&self) -> Vec<ObRpcResultWarningMsg> {
        self.warning_msgs.clone()
    }
}

impl ObPayload for ObRpcResultCode {
    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }
}

impl ProtoDecoder for ObRpcResultCode {
    fn decode(&mut self, src: &mut BytesMut) -> Result<()> {
        self.decode_base(src)?;

        self.rcode = ResultCodes::from_i32(util::decode_vi32(src)?);

        let len = util::decode_vi32(src)?;
        self.msg = util::split_buf_to(src, len as usize)?.to_vec();

        let mut len = util::decode_vi32(src)?;
        if len > 0 {
            self.warning_msgs = Vec::with_capacity(len as usize);
        }
        while len > 0 {
            let mut warn_msg = ObRpcResultWarningMsg::new();
            warn_msg.decode(src)?;
            self.warning_msgs.push(warn_msg);
            len -= 1;
        }
        Ok(())
    }
}

impl ProtoEncoder for ObRpcResultCode {
    fn encode(&self, _buf: &mut BytesMut) -> Result<()> {
        unimplemented!();
    }
}

/// Login request
pub struct ObTableLoginRequest {
    base: BasePayLoad,

    auth_method: u8,
    client_type: u8,
    client_version: u8,
    reserved1: u8,

    client_capabilities: i32,
    max_packet_size: i32,
    reserved2: i32,
    reserved3: i64,

    tenant_name: String,
    user_name: String,
    pass_secret: Vec<u8>,
    //password after hash
    pass_scramble: String,
    // 20-bytes random string
    database_name: String,
    ttl_us: i64,
}

impl ObPayload for ObTableLoginRequest {
    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }

    fn content_len(&self) -> Result<usize> {
        Ok(4 + util::encoded_length_vi32(self.client_capabilities)
            + util::encoded_length_vi32(self.max_packet_size)
            + util::encoded_length_vi32(self.reserved2)
            + util::encoded_length_vi64(self.reserved3)
            + util::encoded_length_vstring(&self.tenant_name)
            + util::encoded_length_vstring(&self.user_name)
            + util::encoded_length_bytes_string(&self.pass_secret)
            + util::encoded_length_vstring(&self.pass_scramble)
            + util::encoded_length_vstring(&self.database_name)
            + util::encoded_length_vi64(self.ttl_us))
    }

    fn pcode(&self) -> ObTablePacketCode {
        ObTablePacketCode::Login
    }
}

impl ProtoEncoder for ObTableLoginRequest {
    fn encode(&self, buf: &mut BytesMut) -> Result<()> {
        self.encode_header(buf)?;

        buf.put_u8(self.auth_method);
        buf.put_u8(self.client_type);
        buf.put_u8(self.client_version);
        buf.put_u8(self.reserved1);

        util::encode_vi32(self.client_capabilities, buf)?;
        util::encode_vi32(self.max_packet_size, buf)?;
        util::encode_vi32(self.reserved2, buf)?;
        util::encode_vi64(self.reserved3, buf)?;

        util::encode_vstring(&self.tenant_name, buf)?;
        util::encode_vstring(&self.user_name, buf)?;
        util::encode_bytes_string(&self.pass_secret, buf)?;
        util::encode_vstring(&self.pass_scramble, buf)?;
        util::encode_vstring(&self.database_name, buf)?;

        util::encode_vi64(self.ttl_us, buf)?;

        Ok(())
    }
}

const PASS_SCRAMBLE_LEN: usize = 20;

impl ObTableLoginRequest {
    pub fn new(
        tenant_name: &str,
        user_name: &str,
        database_name: &str,
        password: &str,
    ) -> ObTableLoginRequest {
        let pass_scramble = security::get_password_scramble(PASS_SCRAMBLE_LEN);
        let pass_secret = security::scramble_password(password, &pass_scramble);

        ObTableLoginRequest {
            base: BasePayLoad::new(),

            auth_method: 0x01,
            client_type: 0x02,
            client_version: 0x01,
            reserved1: 0,

            client_capabilities: 0,
            max_packet_size: 0,
            reserved2: 0,
            reserved3: 0,

            tenant_name: tenant_name.to_owned(),
            user_name: user_name.to_owned(),
            pass_secret,
            pass_scramble,
            database_name: database_name.to_owned(),
            ttl_us: 0,
        }
    }
}

impl ProtoDecoder for ObTableLoginRequest {
    fn decode(&mut self, _src: &mut BytesMut) -> Result<()> {
        unimplemented!()
    }
}

#[derive(Debug, Default)]
pub struct ObTableLoginResult {
    base: BasePayLoad,
    server_capabilities: i32,
    reserved1: i32,
    reserved2: i64,

    server_version: String,
    credential: Vec<u8>,
    tenant_id: u64,
    user_id: i64,
    database_id: i64,
}

impl ObTableLoginResult {
    pub fn new() -> ObTableLoginResult {
        ObTableLoginResult {
            base: BasePayLoad::dummy(),
            server_capabilities: 0,
            reserved1: 0,
            reserved2: 0,
            server_version: "".to_owned(),
            credential: vec![],
            tenant_id: 0,
            user_id: 0,
            database_id: 0,
        }
    }

    pub fn take_credential(&mut self) -> Vec<u8> {
        mem::take(&mut self.credential)
    }

    pub fn tenant_id(&self) -> u64 {
        self.tenant_id
    }

    pub fn serer_version(&self) -> &str {
        &self.server_version
    }
}

impl ObPayload for ObTableLoginResult {
    fn pcode(&self) -> ObTablePacketCode {
        ObTablePacketCode::Login
    }

    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }
}

impl ProtoDecoder for ObTableLoginResult {
    fn decode(&mut self, src: &mut BytesMut) -> Result<()> {
        self.decode_base(src)?;

        self.server_capabilities = util::decode_vi32(src)?;
        self.reserved1 = util::decode_vi32(src)?;
        self.reserved2 = util::decode_vi64(src)?;

        self.server_version = util::decode_vstring(src)?;
        self.credential = util::decode_bytes_string(src)?;

        self.tenant_id = util::decode_vi64(src)? as u64;
        self.user_id = util::decode_vi64(src)?;
        self.database_id = util::decode_vi64(src)?;

        Ok(())
    }
}

impl ProtoEncoder for ObTableLoginResult {
    fn encode(&self, _buf: &mut BytesMut) -> Result<()> {
        unimplemented!();
    }
}

#[derive(Debug, Default)]
pub struct ObTableResult {
    base: BasePayLoad,
    // -5024: duplicate key
    errorno: i32,
    sql_state: Vec<u8>,
    msg: Vec<u8>,
}

impl ObPayload for ObTableResult {
    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }
}

impl ProtoEncoder for ObTableResult {
    fn encode(&self, _buf: &mut BytesMut) -> Result<()> {
        unimplemented!();
    }
}

impl ProtoDecoder for ObTableResult {
    fn decode(&mut self, src: &mut BytesMut) -> Result<()> {
        self.decode_base(src)?;

        self.errorno = util::decode_vi32(src)?;
        self.sql_state = util::decode_bytes(src)?;
        self.msg = util::decode_bytes(src)?;
        Ok(())
    }
}

impl ObTableResult {
    pub fn new() -> ObTableResult {
        ObTableResult {
            base: BasePayLoad::dummy(),
            errorno: 0,
            sql_state: vec![],
            msg: vec![],
        }
    }

    pub fn message(&self) -> String {
        string_from_bytes(&self.msg)
    }

    pub fn errorno(&self) -> i32 {
        self.errorno
    }
}

#[derive(Debug)]
pub struct ObTableOperationResult {
    base: BasePayLoad,
    header: ObTableResult,
    operation_type: ObTableOperationType,
    entity: ObTableEntity,
    affected_rows: i64,
    // debug info
    trace_id: TraceId,
    peer_addr: Option<SocketAddr>,
}

impl Default for ObTableOperationResult {
    fn default() -> ObTableOperationResult {
        ObTableOperationResult::new()
    }
}

impl ObTableOperationResult {
    pub fn new() -> ObTableOperationResult {
        ObTableOperationResult {
            base: BasePayLoad::dummy(),
            operation_type: ObTableOperationType::Get,
            header: ObTableResult::new(),
            entity: ObTableEntity::new(vec![]),
            affected_rows: 0,
            trace_id: TraceId(0, 0),
            peer_addr: None,
        }
    }

    pub fn header(&self) -> &ObTableResult {
        &self.header
    }

    pub fn operation_type(&self) -> ObTableOperationType {
        self.operation_type
    }

    pub fn affected_rows(&self) -> i64 {
        self.affected_rows
    }

    pub fn take_entity(self) -> ObTableEntity {
        self.entity
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }
}

impl ObPayload for ObTableOperationResult {
    fn pcode(&self) -> ObTablePacketCode {
        ObTablePacketCode::Execute
    }

    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }

    fn set_trace_id(&mut self, trace_id: TraceId) {
        self.trace_id = trace_id;
    }

    fn set_peer_addr(&mut self, addr: SocketAddr) {
        self.peer_addr = Some(addr);
    }
}

impl ProtoEncoder for ObTableOperationResult {
    fn encode(&self, _buf: &mut BytesMut) -> Result<()> {
        unimplemented!();
    }
}

impl ProtoDecoder for ObTableOperationResult {
    fn decode(&mut self, src: &mut BytesMut) -> Result<()> {
        self.decode_base(src)?;

        self.header.decode(src)?;
        self.operation_type = ObTableOperationType::from_i8(util::split_buf_to(src, 1)?.get_i8())?;
        self.entity.decode(src)?;
        self.affected_rows = util::decode_vi64(src)?;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ObTableBatchOperationResult {
    base: BasePayLoad,
    op_results: Vec<ObTableOperationResult>,
}

impl ObTableBatchOperationResult {
    pub fn new() -> Self {
        Self {
            base: BasePayLoad::dummy(),
            op_results: Vec::new(),
        }
    }

    pub fn get_op_results(&self) -> &[ObTableOperationResult] {
        &self.op_results
    }

    pub fn take_op_results(self) -> Vec<ObTableOperationResult> {
        self.op_results
    }
}

impl ObPayload for ObTableBatchOperationResult {
    fn pcode(&self) -> ObTablePacketCode {
        ObTablePacketCode::BatchExecute
    }

    fn base(&self) -> &BasePayLoad {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BasePayLoad {
        &mut self.base
    }
}

impl ProtoEncoder for ObTableBatchOperationResult {
    fn encode(&self, _buf: &mut BytesMut) -> Result<()> {
        unimplemented!();
    }
}

impl ProtoDecoder for ObTableBatchOperationResult {
    fn decode(&mut self, src: &mut BytesMut) -> Result<()> {
        self.decode_base(src)?;

        let op_res_num = util::decode_vi64(src)?;
        if op_res_num < 0 {
            return Err(io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid operation results num:{op_res_num}"),
            ));
        }
        assert_eq!(0, self.op_results.len());
        self.op_results.reserve(op_res_num as usize);

        for _ in 0..op_res_num {
            let mut op_res = ObTableOperationResult::new();
            op_res.decode(src)?;
            self.op_results.push(op_res);
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::time;

    use bytes::BytesMut;

    use super::{super::OP_TIMEOUT, *};
    use crate::rpc::protocol::DEFAULT_FLAG;

    #[test]
    fn test_obtable_operation_request_encode() {
        let base = BasePayLoad {
            version: 1,
            channel_id: 99,
            timeout: OP_TIMEOUT,
            flag: DEFAULT_FLAG,
        };

        let entity = ObTableEntity {
            base: base.clone(),
            row_key: ObRowKey {
                column_names: vec!["rowKey".to_string()],
                keys: vec![Value::from("test")],
                obj_type: ObjEncodeType::Obj,
            },
            properties: HashMap::new(),
        };
        let req = ObTableOperationRequest {
            base: base.clone(),
            credential: "test".as_bytes().to_vec(),
            table_name: "test".to_owned(),
            table_id: 1,
            partition_id: 1,
            entity_type: ObTableEntityType::KV,
            table_operation: ObTableOperation {
                base,
                op_type: ObTableOperationType::Insert,
                entity,
            },
            consistency_level: ObTableConsistencyLevel::Strong,
            return_row_key: true,
            return_affected_entity: true,
            return_affected_rows: false,
        };

        let mut buf = BytesMut::new();
        let ret = req.encode(&mut buf);
        assert!(ret.is_ok());
        assert_eq!(req.len().unwrap(), buf.len());
    }

    #[test]
    fn test_obtable_batch_operation_request_encode() {
        let base = BasePayLoad {
            version: 1,
            channel_id: 99,
            timeout: OP_TIMEOUT,
            flag: DEFAULT_FLAG,
        };

        let mut batch_op = ObTableBatchOperation::new();
        batch_op.set_table_name("test".to_owned());
        let row_keys = vec![Value::from("test")];
        let columns = vec![String::from("column-0"), String::from("column-1")];
        let properties = vec![Value::from("column-v1"), Value::from("column-v2")];
        batch_op.insert(row_keys.clone(), columns, properties);
        batch_op.delete(row_keys);

        let req = ObTableBatchOperationRequest::new(
            batch_op.clone(),
            time::Duration::new(base.timeout as u64, 0),
            DEFAULT_FLAG,
        );

        let mut buf = BytesMut::new();
        let ret = req.encode(&mut buf);
        assert!(ret.is_ok());
        assert_eq!(req.len().unwrap(), buf.len());
    }

    #[test]
    fn test_obtable_batch_operation_properties() {
        let mut batch_op = ObTableBatchOperation::new();
        assert!(batch_op.is_read_only());
        assert!(batch_op.is_same_type());
        assert!(batch_op.is_same_properties_names());

        let row_keys = vec![Value::from("test")];
        let columns = vec![String::from("column-0"), String::from("column-1")];
        let properties = vec![Value::from("column-v1"), Value::from("column-v2")];

        batch_op.get(row_keys.clone(), columns.clone());
        assert!(batch_op.is_read_only());
        assert!(batch_op.is_same_type());
        assert!(batch_op.is_same_properties_names());

        batch_op.insert(row_keys.clone(), columns.clone(), properties.clone());
        assert!(!batch_op.is_read_only());
        assert!(!batch_op.is_same_type());
        assert!(batch_op.is_same_properties_names());

        batch_op.update(row_keys.clone(), columns, properties);
        assert!(!batch_op.is_read_only());
        assert!(!batch_op.is_same_type());
        assert!(batch_op.is_same_properties_names());

        let columns = vec![String::from("column-3"), String::from("column-4")];
        let properties = vec![Value::from("column-v3"), Value::from("column-v4")];
        batch_op.update(row_keys, columns, properties);
        assert!(!batch_op.is_read_only());
        assert!(!batch_op.is_same_type());
        assert!(!batch_op.is_same_properties_names());
    }
}
