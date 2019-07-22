// TODO
#![allow(dead_code)]

use failure::prelude::*;
use log::*;
use state_view::StateView;
use std::collections::HashMap;
use types::{
    access_path::AccessPath,
    account_address::AccountAddress,
    account_config,
    language_storage::ModuleId,
    write_set::{WriteOp, WriteSet, WriteSetMut},
};
use vm::{errors::VMInvariantViolation, CompiledModule};
use vm_runtime::{
    data_cache::RemoteCache,
    identifier::create_access_path,
    loaded_data::{struct_def::StructDef, types::Type},
    value::Value,
};

/// An in-memory implementation of [`StateView`] and [`RemoteCache`] for the VM.
#[derive(Debug, Default)]
pub struct DataStore {
    data: HashMap<AccessPath, Vec<u8>>,
}

impl DataStore {
    /// Creates a new `DataStore` with the provided initial data.
    pub fn new(data: HashMap<AccessPath, Vec<u8>>) -> Self {
        DataStore { data }
    }

    /// Applies a [`WriteSet`] to this data store.
    pub fn apply_write_set(&mut self, write_set: &WriteSet) {
        for (access_path, write_op) in write_set {
            match write_op {
                WriteOp::Value(value) => {
                    self.set(access_path.clone(), value.clone());
                }
                WriteOp::Deletion => {
                    self.remove(access_path);
                }
            }
        }
    }

    /// Returns a `WriteSet` for each account in the `DataStore`
    pub fn into_write_sets(mut self) -> HashMap<AccountAddress, WriteSet> {
        let mut write_set_muts: HashMap<AccountAddress, WriteSetMut> = HashMap::new();
        for (access_path, value) in self.data.drain() {
            match write_set_muts.get_mut(&access_path.address) {
                Some(write_set_mut) => write_set_mut.push((access_path, WriteOp::Value(value))),
                None => {
                    write_set_muts.insert(
                        access_path.address,
                        WriteSetMut::new(vec![(access_path, WriteOp::Value(value))]),
                    );
                }
            }
        }
        // Freeze each WriteSet
        let mut write_sets: HashMap<AccountAddress, WriteSet> = HashMap::new();
        for (address, write_set_mut) in write_set_muts.drain() {
            write_sets.insert(address, write_set_mut.freeze().unwrap());
        }
        write_sets
    }

    /// Read an account's resource
    pub fn read_account_resource(&self, addr: &AccountAddress) -> Option<Value> {
        let access_path = create_access_path(&addr, account_config::account_struct_tag());
        match self.data.get(&access_path) {
            None => None,
            Some(blob) => {
                let account_type = get_account_struct_def();
                match Value::simple_deserialize(blob, account_type) {
                    Ok(account) => Some(account),
                    Err(_) => None,
                }
            }
        }
    }

    /// Sets a (key, value) pair within this data store.
    ///
    /// Returns the previous data if the key was occupied.
    pub fn set(&mut self, access_path: AccessPath, data_blob: Vec<u8>) -> Option<Vec<u8>> {
        self.data.insert(access_path, data_blob)
    }

    /// Deletes a key from this data store.
    ///
    /// Returns the previous data if the key was occupied.
    pub fn remove(&mut self, access_path: &AccessPath) -> Option<Vec<u8>> {
        self.data.remove(access_path)
    }

    /// Adds a [`CompiledModule`] to this data store.
    ///
    /// Does not do any sort of verification on the module.
    pub fn add_module(&mut self, module_id: &ModuleId, module: &CompiledModule) {
        let access_path = AccessPath::from(module_id);
        let mut value = vec![];
        module
            .serialize(&mut value)
            .expect("serializing this module should work");
        self.set(access_path, value);
    }

    /// Dumps the data store to stdout
    pub fn dump(&self) {
        for (access_path, value) in &self.data {
            trace!("{:?}: \"{:?}\"", access_path, value.len());
        }
    }
}

impl StateView for DataStore {
    fn get(&self, access_path: &AccessPath) -> Result<Option<Vec<u8>>> {
        // Since the data is in-memory, it can't fail.
        match self.data.get(access_path) {
            None => Ok(None),
            Some(value) => Ok(Some(value.clone())),
        }
    }

    fn multi_get(&self, _access_paths: &[AccessPath]) -> Result<Vec<Option<Vec<u8>>>> {
        unimplemented!();
    }

    fn is_genesis(&self) -> bool {
        false
    }
}

impl RemoteCache for DataStore {
    fn get(
        &self,
        access_path: &AccessPath,
    ) -> ::std::result::Result<Option<Vec<u8>>, VMInvariantViolation> {
        Ok(StateView::get(self, access_path).expect("it should not error"))
    }
}

// TODO: internal Libra function and very likely to break soon, need something better
fn get_account_struct_def() -> StructDef {
    // STRUCT DEF StructDef(StructDefInner { field_definitions: [ByteArray,
    // Struct(StructDef(StructDefInner { field_definitions: [U64] })), U64, U64,
    // U64] }) let coin = StructDef(StructDefInner { field_definitions:
    // [Type::U64] })
    let int_type = Type::U64;
    let byte_array_type = Type::ByteArray;
    let coin = Type::Struct(StructDef::new(vec![int_type.clone()]));
    StructDef::new(vec![
        byte_array_type,
        coin,
        int_type.clone(),
        int_type.clone(),
        int_type.clone(),
    ])
}
