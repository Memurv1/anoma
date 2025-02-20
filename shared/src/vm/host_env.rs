//! Virtual machine's host environment exposes functions that may be called from
//! within a virtual machine.
use std::collections::HashSet;
use std::convert::TryInto;
use std::num::TryFromIntError;
use std::sync::{Arc, Mutex};

use borsh::{BorshDeserialize, BorshSerialize};
use thiserror::Error;

use crate::gossip::mm::MmHost;
use crate::ledger::gas::{self, BlockGasMeter, VpGasMeter};
use crate::ledger::storage::write_log::{self, WriteLog};
use crate::ledger::storage::{self, Storage, StorageHasher};
use crate::ledger::vp_env;
use crate::proto::Tx;
use crate::types::address::{self, Address};
use crate::types::internal::HostEnvResult;
use crate::types::key::ed25519::{verify_tx_sig, PublicKey, Signature};
use crate::types::storage::Key;
use crate::vm::memory::VmMemory;
use crate::vm::prefix_iter::{PrefixIteratorId, PrefixIterators};
use crate::vm::types::KeyVal;
use crate::vm::{
    validate_untrusted_wasm, HostRef, MutHostRef, WasmValidationError,
};

const VERIFY_TX_SIG_GAS_COST: u64 = 1000;
const WASM_VALIDATION_GAS_PER_BYTE: u64 = 1;

/// These runtime errors will abort tx WASM execution immediately
#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum TxRuntimeError {
    #[error("Out of gas: {0}")]
    OutOfGas(gas::Error),
    #[error("Trying to modify storage for an address that doesn't exit {0}")]
    UnknownAddressStorageModification(Address),
    #[error("Trying to update a validity predicate with an invalid WASM {0}")]
    UpdateVpInvalid(WasmValidationError),
    #[error(
        "Trying to initialize an account with an invalid validity predicate \
         WASM {0}"
    )]
    InitAccountInvalidVpWasm(WasmValidationError),
    #[error("Storage modification error: {0}")]
    StorageModificationError(write_log::Error),
    #[error("Storage error: {0}")]
    StorageError(storage::Error),
    #[error("Storage data error: {0}")]
    StorageDataError(crate::types::storage::Error),
    #[error("Encoding error: {0}")]
    EncodingError(std::io::Error),
    #[error("Address error: {0}")]
    AddressError(address::Error),
    #[error("Numeric conversion error: {0}")]
    NumConversionError(TryFromIntError),
    #[error("Memory error: {0}")]
    MemoryError(Box<dyn std::error::Error + Sync + Send + 'static>),
}

type TxResult<T> = std::result::Result<T, TxRuntimeError>;

/// A transaction's host environment
pub struct TxEnv<'a, MEM, DB, H>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    /// The VM memory for bi-directional data passing
    pub memory: MEM,
    /// The tx context contains references to host structures.
    pub ctx: TxCtx<'a, DB, H>,
}

/// A transaction's host context
pub struct TxCtx<'a, DB, H>
where
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    /// Read-only access to the storage.
    pub storage: HostRef<'a, &'a Storage<DB, H>>,
    /// Read/write access to the write log.
    pub write_log: MutHostRef<'a, &'a WriteLog>,
    /// Storage prefix iterators.
    pub iterators: MutHostRef<'a, &'a PrefixIterators<'a, DB>>,
    /// Transaction gas meter.
    pub gas_meter: MutHostRef<'a, &'a BlockGasMeter>,
    /// The verifiers whose validity predicates should be triggered.
    pub verifiers: MutHostRef<'a, &'a HashSet<Address>>,
    /// Cache for 2-step reads from host environment.
    pub result_buffer: MutHostRef<'a, &'a Option<Vec<u8>>>,
}

impl<'a, MEM, DB, H> TxEnv<'a, MEM, DB, H>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    /// Create a new environment for transaction execution.
    ///
    /// # Safety
    ///
    /// The way the arguments to this function are used is not thread-safe,
    /// we're assuming single-threaded tx execution with exclusive access to the
    /// mutable references.
    pub fn new(
        memory: MEM,
        storage: &Storage<DB, H>,
        write_log: &mut WriteLog,
        iterators: &mut PrefixIterators<'a, DB>,
        gas_meter: &mut BlockGasMeter,
        verifiers: &mut HashSet<Address>,
        result_buffer: &mut Option<Vec<u8>>,
    ) -> Self {
        let storage = unsafe { HostRef::new(storage) };
        let write_log = unsafe { MutHostRef::new(write_log) };
        let iterators = unsafe { MutHostRef::new(iterators) };
        let gas_meter = unsafe { MutHostRef::new(gas_meter) };
        let verifiers = unsafe { MutHostRef::new(verifiers) };
        let result_buffer = unsafe { MutHostRef::new(result_buffer) };
        let ctx = TxCtx {
            storage,
            write_log,
            iterators,
            gas_meter,
            verifiers,
            result_buffer,
        };

        Self { memory, ctx }
    }
}

impl<MEM, DB, H> Clone for TxEnv<'_, MEM, DB, H>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    fn clone(&self) -> Self {
        Self {
            memory: self.memory.clone(),
            ctx: self.ctx.clone(),
        }
    }
}

impl<'a, DB, H> Clone for TxCtx<'a, DB, H>
where
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            write_log: self.write_log.clone(),
            iterators: self.iterators.clone(),
            gas_meter: self.gas_meter.clone(),
            verifiers: self.verifiers.clone(),
            result_buffer: self.result_buffer.clone(),
        }
    }
}

/// A validity predicate's host environment
pub struct VpEnv<'a, MEM, DB, H, EVAL>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    /// The VM memory for bi-directional data passing
    pub memory: MEM,
    /// The VP context contains references to host structures.
    pub ctx: VpCtx<'a, DB, H, EVAL>,
}

/// A validity predicate's host context
pub struct VpCtx<'a, DB, H, EVAL>
where
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    /// The address of the account that owns the VP
    pub address: HostRef<'a, &'a Address>,
    /// Read-only access to the storage.
    pub storage: HostRef<'a, &'a Storage<DB, H>>,
    /// Read-only access to the write log.
    pub write_log: HostRef<'a, &'a WriteLog>,
    /// Storage prefix iterators.
    pub iterators: MutHostRef<'a, &'a PrefixIterators<'a, DB>>,
    /// VP gas meter.
    pub gas_meter: MutHostRef<'a, &'a VpGasMeter>,
    /// The transaction code is used for signature verification
    pub tx: HostRef<'a, &'a Tx>,
    /// The runner of the [`vp_eval`] function
    pub eval_runner: HostRef<'a, &'a EVAL>,
    /// Cache for 2-step reads from host environment.
    pub result_buffer: MutHostRef<'a, &'a Option<Vec<u8>>>,
    /// The storage keys that have been changed. Used for calls to `eval`.
    pub keys_changed: HostRef<'a, &'a HashSet<Key>>,
    /// The verifiers whose validity predicates should be triggered. Used for
    /// calls to `eval`.
    pub verifiers: HostRef<'a, &'a HashSet<Address>>,
}

/// A Validity predicate runner for calls from the [`vp_eval`] function.
pub trait VpEvaluator {
    /// Storage DB type
    type Db: storage::DB + for<'iter> storage::DBIter<'iter>;
    /// Storage hasher type
    type H: StorageHasher;
    /// Recursive VP evaluator type
    type Eval: VpEvaluator;

    /// Evaluate a given validity predicate code with the given input data.
    /// Currently, we can only evaluate VPs using WASM runner with WASM memory.
    ///
    /// Invariant: Calling `VpEvalRunner::eval` from the VP is synchronous as it
    /// shares mutable access to the host context with the VP.
    fn eval(
        &self,
        ctx: VpCtx<'static, Self::Db, Self::H, Self::Eval>,
        vp_code: Vec<u8>,
        input_data: Vec<u8>,
    ) -> HostEnvResult;
}

impl<'a, MEM, DB, H, EVAL> VpEnv<'a, MEM, DB, H, EVAL>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    /// Create a new environment for validity predicate execution.
    ///
    /// # Safety
    ///
    /// The way the arguments to this function are used is not thread-safe,
    /// we're assuming multi-threaded VP execution, but with with exclusive
    /// access to the mutable references (no shared access).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory: MEM,
        address: &Address,
        storage: &Storage<DB, H>,
        write_log: &WriteLog,
        gas_meter: &mut VpGasMeter,
        tx: &Tx,
        iterators: &mut PrefixIterators<'a, DB>,
        verifiers: &HashSet<Address>,
        result_buffer: &mut Option<Vec<u8>>,
        keys_changed: &HashSet<Key>,
        eval_runner: &EVAL,
    ) -> Self {
        let ctx = VpCtx::new(
            address,
            storage,
            write_log,
            gas_meter,
            tx,
            iterators,
            verifiers,
            result_buffer,
            keys_changed,
            eval_runner,
        );

        Self { memory, ctx }
    }
}

impl<MEM, DB, H, EVAL> Clone for VpEnv<'_, MEM, DB, H, EVAL>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    fn clone(&self) -> Self {
        Self {
            memory: self.memory.clone(),
            ctx: self.ctx.clone(),
        }
    }
}

impl<'a, DB, H, EVAL> VpCtx<'a, DB, H, EVAL>
where
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    /// Create a new context for validity predicate execution.
    ///
    /// # Safety
    ///
    /// The way the arguments to this function are used is not thread-safe,
    /// we're assuming multi-threaded VP execution, but with with exclusive
    /// access to the mutable references (no shared access).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: &Address,
        storage: &Storage<DB, H>,
        write_log: &WriteLog,
        gas_meter: &mut VpGasMeter,
        tx: &Tx,
        iterators: &mut PrefixIterators<'a, DB>,
        verifiers: &HashSet<Address>,
        result_buffer: &mut Option<Vec<u8>>,
        keys_changed: &HashSet<Key>,
        eval_runner: &EVAL,
    ) -> Self {
        let address = unsafe { HostRef::new(address) };
        let storage = unsafe { HostRef::new(storage) };
        let write_log = unsafe { HostRef::new(write_log) };
        let tx = unsafe { HostRef::new(tx) };
        let iterators = unsafe { MutHostRef::new(iterators) };
        let gas_meter = unsafe { MutHostRef::new(gas_meter) };
        let verifiers = unsafe { HostRef::new(verifiers) };
        let result_buffer = unsafe { MutHostRef::new(result_buffer) };
        let keys_changed = unsafe { HostRef::new(keys_changed) };
        let eval_runner = unsafe { HostRef::new(eval_runner) };
        Self {
            address,
            storage,
            write_log,
            iterators,
            gas_meter,
            tx,
            eval_runner,
            result_buffer,
            keys_changed,
            verifiers,
        }
    }
}

impl<'a, DB, H, EVAL> Clone for VpCtx<'a, DB, H, EVAL>
where
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    fn clone(&self) -> Self {
        Self {
            address: self.address.clone(),
            storage: self.storage.clone(),
            write_log: self.write_log.clone(),
            iterators: self.iterators.clone(),
            gas_meter: self.gas_meter.clone(),
            tx: self.tx.clone(),
            eval_runner: self.eval_runner.clone(),
            result_buffer: self.result_buffer.clone(),
            keys_changed: self.keys_changed.clone(),
            verifiers: self.verifiers.clone(),
        }
    }
}

/// A matchmakers's host environment
pub struct MatchmakerEnv<MEM, MM>
where
    MEM: VmMemory,
    MM: MmHost,
{
    /// The VM memory for bi-directional data passing
    pub memory: MEM,
    /// The matchmaker's host
    pub mm: Arc<Mutex<MM>>,
}

impl<MEM, MM> Clone for MatchmakerEnv<MEM, MM>
where
    MEM: VmMemory,
    MM: MmHost,
{
    fn clone(&self) -> Self {
        Self {
            memory: self.memory.clone(),
            mm: self.mm.clone(),
        }
    }
}

unsafe impl<MEM, MM> Send for MatchmakerEnv<MEM, MM>
where
    MEM: VmMemory,
    MM: MmHost,
{
}

unsafe impl<MEM, MM> Sync for MatchmakerEnv<MEM, MM>
where
    MEM: VmMemory,
    MM: MmHost,
{
}

#[derive(Clone)]
/// A matchmakers filter's host environment
pub struct FilterEnv<MEM>
where
    MEM: VmMemory,
{
    /// The VM memory for bi-directional data passing
    pub memory: MEM,
}

/// Called from tx wasm to request to use the given gas amount
pub fn tx_charge_gas<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    used_gas: i32,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    tx_add_gas(
        env,
        used_gas
            .try_into()
            .map_err(TxRuntimeError::NumConversionError)?,
    )
}

/// Add a gas cost incured in a transaction
pub fn tx_add_gas<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    used_gas: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    // if we run out of gas, we need to stop the execution
    let result = gas_meter.add(used_gas).map_err(TxRuntimeError::OutOfGas);
    if let Err(err) = &result {
        tracing::info!(
            "Stopping transaction execution because of gas error: {}",
            err
        );
    }
    result
}

/// Called from VP wasm to request to use the given gas amount
pub fn vp_charge_gas<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    used_gas: i32,
) -> vp_env::Result<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(
        gas_meter,
        used_gas
            .try_into()
            .map_err(vp_env::RuntimeError::NumConversionError)?,
    )
}

/// Storage `has_key` function exposed to the wasm VM Tx environment. It will
/// try to check the write log first and if no entry found then the storage.
pub fn tx_has_key<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    key_ptr: u64,
    key_len: u64,
) -> TxResult<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (key, gas) = env
        .memory
        .read_string(key_ptr, key_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    tracing::debug!("tx_has_key {}, key {}", key, key_ptr,);

    let key = Key::parse(key).map_err(TxRuntimeError::StorageDataError)?;

    // try to read from the write log first
    let write_log = unsafe { env.ctx.write_log.get() };
    let (log_val, gas) = write_log.read(&key);
    tx_add_gas(env, gas)?;
    Ok(match log_val {
        Some(&write_log::StorageModification::Write { .. }) => {
            HostEnvResult::Success.to_i64()
        }
        Some(&write_log::StorageModification::Delete) => {
            // the given key has been deleted
            HostEnvResult::Fail.to_i64()
        }
        Some(&write_log::StorageModification::InitAccount { .. }) => {
            HostEnvResult::Success.to_i64()
        }
        None => {
            // when not found in write log, try to check the storage
            let storage = unsafe { env.ctx.storage.get() };
            let (present, gas) = storage
                .has_key(&key)
                .map_err(TxRuntimeError::StorageError)?;
            tx_add_gas(env, gas)?;
            HostEnvResult::from(present).to_i64()
        }
    })
}

/// Storage read function exposed to the wasm VM Tx environment. It will try to
/// read from the write log first and if no entry found then from the storage.
///
/// Returns `-1` when the key is not present, or the length of the data when
/// the key is present (the length may be `0`).
pub fn tx_read<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    key_ptr: u64,
    key_len: u64,
) -> TxResult<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (key, gas) = env
        .memory
        .read_string(key_ptr, key_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    tracing::debug!("tx_read {}, key {}", key, key_ptr,);

    let key = Key::parse(key).map_err(TxRuntimeError::StorageDataError)?;

    // try to read from the write log first
    let write_log = unsafe { env.ctx.write_log.get() };
    let (log_val, gas) = write_log.read(&key);
    tx_add_gas(env, gas)?;
    Ok(match log_val {
        Some(&write_log::StorageModification::Write { ref value }) => {
            let len: i64 = value
                .len()
                .try_into()
                .map_err(TxRuntimeError::NumConversionError)?;
            let result_buffer = unsafe { env.ctx.result_buffer.get() };
            result_buffer.replace(value.clone());
            len
        }
        Some(&write_log::StorageModification::Delete) => {
            // fail, given key has been deleted
            HostEnvResult::Fail.to_i64()
        }
        Some(&write_log::StorageModification::InitAccount {
            ref vp, ..
        }) => {
            // read the VP of a new account
            let len: i64 = vp
                .len()
                .try_into()
                .map_err(TxRuntimeError::NumConversionError)?;
            let result_buffer = unsafe { env.ctx.result_buffer.get() };
            result_buffer.replace(vp.clone());
            len
        }
        None => {
            // when not found in write log, try to read from the storage
            let storage = unsafe { env.ctx.storage.get() };
            let (value, gas) =
                storage.read(&key).map_err(TxRuntimeError::StorageError)?;
            tx_add_gas(env, gas)?;
            match value {
                Some(value) => {
                    let len: i64 = value
                        .len()
                        .try_into()
                        .map_err(TxRuntimeError::NumConversionError)?;
                    let result_buffer = unsafe { env.ctx.result_buffer.get() };
                    result_buffer.replace(value);
                    len
                }
                None => HostEnvResult::Fail.to_i64(),
            }
        }
    })
}

/// This function is a helper to handle the first step of reading var-len
/// values from the host.
///
/// In cases where we're reading a value from the host in the guest and
/// we don't know the byte size up-front, we have to read it in 2-steps. The
/// first step reads the value into a result buffer and returns the size (if
/// any) back to the guest, the second step reads the value from cache into a
/// pre-allocated buffer with the obtained size.
pub fn tx_result_buffer<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    result_ptr: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let result_buffer = unsafe { env.ctx.result_buffer.get() };
    let value = result_buffer.take().unwrap();
    let gas = env
        .memory
        .write_bytes(result_ptr, value)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)
}

/// Storage prefix iterator function exposed to the wasm VM Tx environment.
/// It will try to get an iterator from the storage and return the corresponding
/// ID of the iterator.
pub fn tx_iter_prefix<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    prefix_ptr: u64,
    prefix_len: u64,
) -> TxResult<u64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (prefix, gas) = env
        .memory
        .read_string(prefix_ptr, prefix_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    tracing::debug!("tx_iter_prefix {}, prefix {}", prefix, prefix_ptr);

    let prefix =
        Key::parse(prefix).map_err(TxRuntimeError::StorageDataError)?;

    let storage = unsafe { env.ctx.storage.get() };
    let iterators = unsafe { env.ctx.iterators.get() };
    let (iter, gas) = storage.iter_prefix(&prefix);
    tx_add_gas(env, gas)?;
    Ok(iterators.insert(iter).id())
}

/// Storage prefix iterator next function exposed to the wasm VM Tx environment.
/// It will try to read from the write log first and if no entry found then from
/// the storage.
///
/// Returns `-1` when the key is not present, or the length of the data when
/// the key is present (the length may be `0`).
pub fn tx_iter_next<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    iter_id: u64,
) -> TxResult<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    tracing::debug!("tx_iter_next iter_id {}", iter_id,);

    let write_log = unsafe { env.ctx.write_log.get() };
    let iterators = unsafe { env.ctx.iterators.get() };
    let iter_id = PrefixIteratorId::new(iter_id);
    while let Some((key, val, iter_gas)) = iterators.next(iter_id) {
        let (log_val, log_gas) = write_log.read(
            &Key::parse(key.clone())
                .map_err(TxRuntimeError::StorageDataError)?,
        );
        tx_add_gas(env, iter_gas + log_gas)?;
        match log_val {
            Some(&write_log::StorageModification::Write { ref value }) => {
                let key_val = KeyVal {
                    key,
                    val: value.clone(),
                }
                .try_to_vec()
                .map_err(TxRuntimeError::EncodingError)?;
                let len: i64 = key_val
                    .len()
                    .try_into()
                    .map_err(TxRuntimeError::NumConversionError)?;
                let result_buffer = unsafe { env.ctx.result_buffer.get() };
                result_buffer.replace(key_val);
                return Ok(len);
            }
            Some(&write_log::StorageModification::Delete) => {
                // check the next because the key has already deleted
                continue;
            }
            Some(&write_log::StorageModification::InitAccount { .. }) => {
                // a VP of a new account doesn't need to be iterated
                continue;
            }
            None => {
                let key_val = KeyVal { key, val }
                    .try_to_vec()
                    .map_err(TxRuntimeError::EncodingError)?;
                let len: i64 = key_val
                    .len()
                    .try_into()
                    .map_err(TxRuntimeError::NumConversionError)?;
                let result_buffer = unsafe { env.ctx.result_buffer.get() };
                result_buffer.replace(key_val);
                return Ok(len);
            }
        }
    }
    Ok(HostEnvResult::Fail.to_i64())
}

/// Storage write function exposed to the wasm VM Tx environment. The given
/// key/value will be written to the write log.
pub fn tx_write<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    key_ptr: u64,
    key_len: u64,
    val_ptr: u64,
    val_len: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (key, gas) = env
        .memory
        .read_string(key_ptr, key_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;
    let (value, gas) = env
        .memory
        .read_bytes(val_ptr, val_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    tracing::debug!("tx_update {}, {:?}", key, value);

    let key = Key::parse(key).map_err(TxRuntimeError::StorageDataError)?;

    // check address existence
    let write_log = unsafe { env.ctx.write_log.get() };
    let storage = unsafe { env.ctx.storage.get() };
    for addr in key.find_addresses() {
        // skip the check for implicit and internal addresses
        if let Address::Implicit(_) | Address::Internal(_) = &addr {
            continue;
        }
        let vp_key = Key::validity_predicate(&addr);
        let (vp, gas) = write_log.read(&vp_key);
        tx_add_gas(env, gas)?;
        // just check the existence because the write log should not have the
        // delete log of the VP
        if vp.is_none() {
            let (is_present, gas) = storage
                .has_key(&vp_key)
                .map_err(TxRuntimeError::StorageError)?;
            tx_add_gas(env, gas)?;
            if !is_present {
                tracing::info!(
                    "Trying to write into storage with a key containing an \
                     address that doesn't exist: {}",
                    addr
                );
                return Err(TxRuntimeError::UnknownAddressStorageModification(
                    addr,
                ));
            }
        }
    }

    let (gas, _size_diff) = write_log
        .write(&key, value)
        .map_err(TxRuntimeError::StorageModificationError)?;
    tx_add_gas(env, gas)
    // TODO: charge the size diff
}

/// Storage delete function exposed to the wasm VM Tx environment. The given
/// key/value will be written as deleted to the write log.
pub fn tx_delete<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    key_ptr: u64,
    key_len: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (key, gas) = env
        .memory
        .read_string(key_ptr, key_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    tracing::debug!("tx_delete {}", key);

    let key = Key::parse(key).map_err(TxRuntimeError::StorageDataError)?;

    let write_log = unsafe { env.ctx.write_log.get() };
    let (gas, _size_diff) = write_log
        .delete(&key)
        .map_err(TxRuntimeError::StorageModificationError)?;
    tx_add_gas(env, gas)
    // TODO: charge the size diff
}

/// Storage read prior state (before tx execution) function exposed to the wasm
/// VM VP environment. It will try to read from the storage.
///
/// Returns `-1` when the key is not present, or the length of the data when
/// the key is present (the length may be `0`).
pub fn vp_read_pre<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    key_ptr: u64,
    key_len: u64,
) -> vp_env::Result<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let (key, gas) = env
        .memory
        .read_string(key_ptr, key_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(gas_meter, gas)?;

    // try to read from the storage
    let key =
        Key::parse(key).map_err(vp_env::RuntimeError::StorageDataError)?;
    let storage = unsafe { env.ctx.storage.get() };
    let value = vp_env::read_pre(gas_meter, storage, &key)?;
    tracing::debug!(
        "vp_read_pre addr {}, key {}, value {:?}",
        unsafe { env.ctx.address.get() },
        key,
        value,
    );
    Ok(match value {
        Some(value) => {
            let len: i64 = value
                .len()
                .try_into()
                .map_err(vp_env::RuntimeError::NumConversionError)?;
            let result_buffer = unsafe { env.ctx.result_buffer.get() };
            result_buffer.replace(value);
            len
        }
        None => HostEnvResult::Fail.to_i64(),
    })
}

/// Storage read posterior state (after tx execution) function exposed to the
/// wasm VM VP environment. It will try to read from the write log first and if
/// no entry found then from the storage.
///
/// Returns `-1` when the key is not present, or the length of the data when
/// the key is present (the length may be `0`).
pub fn vp_read_post<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    key_ptr: u64,
    key_len: u64,
) -> vp_env::Result<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let (key, gas) = env
        .memory
        .read_string(key_ptr, key_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(gas_meter, gas)?;

    tracing::debug!("vp_read_post {}, key {}", key, key_ptr,);

    // try to read from the write log first
    let key =
        Key::parse(key).map_err(vp_env::RuntimeError::StorageDataError)?;
    let storage = unsafe { env.ctx.storage.get() };
    let write_log = unsafe { env.ctx.write_log.get() };
    let value = vp_env::read_post(gas_meter, storage, write_log, &key)?;
    Ok(match value {
        Some(value) => {
            let len: i64 = value
                .len()
                .try_into()
                .map_err(vp_env::RuntimeError::NumConversionError)?;
            let result_buffer = unsafe { env.ctx.result_buffer.get() };
            result_buffer.replace(value);
            len
        }
        None => HostEnvResult::Fail.to_i64(),
    })
}

/// This function is a helper to handle the first step of reading var-len
/// values from the host.
///
/// In cases where we're reading a value from the host in the guest and
/// we don't know the byte size up-front, we have to read it in 2-steps. The
/// first step reads the value into a result buffer and returns the size (if
/// any) back to the guest, the second step reads the value from cache into a
/// pre-allocated buffer with the obtained size.
pub fn vp_result_buffer<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    result_ptr: u64,
) -> vp_env::Result<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let result_buffer = unsafe { env.ctx.result_buffer.get() };
    let value = result_buffer.take().unwrap();
    let gas = env
        .memory
        .write_bytes(result_ptr, value)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(gas_meter, gas)
}

/// Storage `has_key` in prior state (before tx execution) function exposed to
/// the wasm VM VP environment. It will try to read from the storage.
pub fn vp_has_key_pre<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    key_ptr: u64,
    key_len: u64,
) -> vp_env::Result<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let (key, gas) = env
        .memory
        .read_string(key_ptr, key_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(gas_meter, gas)?;

    tracing::debug!("vp_has_key_pre {}, key {}", key, key_ptr,);

    let key =
        Key::parse(key).map_err(vp_env::RuntimeError::StorageDataError)?;
    let storage = unsafe { env.ctx.storage.get() };
    let present = vp_env::has_key_pre(gas_meter, storage, &key)?;
    Ok(HostEnvResult::from(present).to_i64())
}

/// Storage `has_key` in posterior state (after tx execution) function exposed
/// to the wasm VM VP environment. It will try to check the write log first and
/// if no entry found then the storage.
pub fn vp_has_key_post<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    key_ptr: u64,
    key_len: u64,
) -> vp_env::Result<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let (key, gas) = env
        .memory
        .read_string(key_ptr, key_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(gas_meter, gas)?;

    tracing::debug!("vp_has_key_post {}, key {}", key, key_ptr,);

    let key =
        Key::parse(key).map_err(vp_env::RuntimeError::StorageDataError)?;
    let storage = unsafe { env.ctx.storage.get() };
    let write_log = unsafe { env.ctx.write_log.get() };
    let present = vp_env::has_key_post(gas_meter, storage, write_log, &key)?;
    Ok(HostEnvResult::from(present).to_i64())
}

/// Storage prefix iterator function exposed to the wasm VM VP environment.
/// It will try to get an iterator from the storage and return the corresponding
/// ID of the iterator.
pub fn vp_iter_prefix<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    prefix_ptr: u64,
    prefix_len: u64,
) -> vp_env::Result<u64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let (prefix, gas) = env
        .memory
        .read_string(prefix_ptr, prefix_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(gas_meter, gas)?;

    let prefix =
        Key::parse(prefix).map_err(vp_env::RuntimeError::StorageDataError)?;
    tracing::debug!("vp_iter_prefix {}", prefix);

    let storage = unsafe { env.ctx.storage.get() };
    let iter = vp_env::iter_prefix(gas_meter, storage, &prefix)?;
    let iterators = unsafe { env.ctx.iterators.get() };
    Ok(iterators.insert(iter).id())
}

/// Storage prefix iterator for prior state (before tx execution) function
/// exposed to the wasm VM VP environment. It will try to read from the storage.
///
/// Returns `-1` when the key is not present, or the length of the data when
/// the key is present (the length may be `0`).
pub fn vp_iter_pre_next<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    iter_id: u64,
) -> vp_env::Result<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    tracing::debug!("vp_iter_pre_next iter_id {}", iter_id);

    let iterators = unsafe { env.ctx.iterators.get() };
    let iter_id = PrefixIteratorId::new(iter_id);
    if let Some(iter) = iterators.get_mut(iter_id) {
        let gas_meter = unsafe { env.ctx.gas_meter.get() };
        if let Some((key, val)) = vp_env::iter_pre_next::<DB>(gas_meter, iter)?
        {
            let key_val = KeyVal { key, val }
                .try_to_vec()
                .map_err(vp_env::RuntimeError::EncodingError)?;
            let len: i64 = key_val
                .len()
                .try_into()
                .map_err(vp_env::RuntimeError::NumConversionError)?;
            let result_buffer = unsafe { env.ctx.result_buffer.get() };
            result_buffer.replace(key_val);
            return Ok(len);
        }
    }
    Ok(HostEnvResult::Fail.to_i64())
}

/// Storage prefix iterator next for posterior state (after tx execution)
/// function exposed to the wasm VM VP environment. It will try to read from the
/// write log first and if no entry found then from the storage.
///
/// Returns `-1` when the key is not present, or the length of the data when
/// the key is present (the length may be `0`).
pub fn vp_iter_post_next<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    iter_id: u64,
) -> vp_env::Result<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    tracing::debug!("vp_iter_post_next iter_id {}", iter_id);

    let iterators = unsafe { env.ctx.iterators.get() };
    let iter_id = PrefixIteratorId::new(iter_id);
    if let Some(iter) = iterators.get_mut(iter_id) {
        let gas_meter = unsafe { env.ctx.gas_meter.get() };
        let write_log = unsafe { env.ctx.write_log.get() };
        if let Some((key, val)) =
            vp_env::iter_post_next::<DB>(gas_meter, write_log, iter)?
        {
            let key_val = KeyVal { key, val }
                .try_to_vec()
                .map_err(vp_env::RuntimeError::EncodingError)?;
            let len: i64 = key_val
                .len()
                .try_into()
                .map_err(vp_env::RuntimeError::NumConversionError)?;
            let result_buffer = unsafe { env.ctx.result_buffer.get() };
            result_buffer.replace(key_val);
            return Ok(len);
        }
    }
    Ok(HostEnvResult::Fail.to_i64())
}

/// Verifier insertion function exposed to the wasm VM Tx environment.
pub fn tx_insert_verifier<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    addr_ptr: u64,
    addr_len: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (addr, gas) = env
        .memory
        .read_string(addr_ptr, addr_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    tracing::debug!("tx_insert_verifier {}, addr_ptr {}", addr, addr_ptr,);

    let addr = Address::decode(&addr).map_err(TxRuntimeError::AddressError)?;

    let verifiers = unsafe { env.ctx.verifiers.get() };
    verifiers.insert(addr);
    tx_add_gas(env, addr_len)
}

/// Update a validity predicate function exposed to the wasm VM Tx environment
pub fn tx_update_validity_predicate<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    addr_ptr: u64,
    addr_len: u64,
    code_ptr: u64,
    code_len: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (addr, gas) = env
        .memory
        .read_string(addr_ptr, addr_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    let addr = Address::decode(addr).map_err(TxRuntimeError::AddressError)?;
    tracing::debug!("tx_update_validity_predicate for addr {}", addr);

    let key = Key::validity_predicate(&addr);
    let (code, gas) = env
        .memory
        .read_bytes(code_ptr, code_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    tx_add_gas(env, code.len() as u64 * WASM_VALIDATION_GAS_PER_BYTE)?;
    validate_untrusted_wasm(&code).map_err(TxRuntimeError::UpdateVpInvalid)?;

    let write_log = unsafe { env.ctx.write_log.get() };
    let (gas, _size_diff) = write_log
        .write(&key, code)
        .map_err(TxRuntimeError::StorageModificationError)?;
    tx_add_gas(env, gas)
    // TODO: charge the size diff
}

/// Initialize a new account established address.
pub fn tx_init_account<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    code_ptr: u64,
    code_len: u64,
    result_ptr: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (code, gas) = env
        .memory
        .read_bytes(code_ptr, code_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)?;

    tx_add_gas(env, code.len() as u64 * WASM_VALIDATION_GAS_PER_BYTE)?;
    validate_untrusted_wasm(&code)
        .map_err(TxRuntimeError::InitAccountInvalidVpWasm)?;

    tracing::debug!("tx_init_account");

    let storage = unsafe { env.ctx.storage.get() };
    let write_log = unsafe { env.ctx.write_log.get() };
    let (addr, gas) = write_log.init_account(&storage.address_gen, code);
    let addr_bytes =
        addr.try_to_vec().map_err(TxRuntimeError::EncodingError)?;
    tx_add_gas(env, gas)?;
    let gas = env
        .memory
        .write_bytes(result_ptr, addr_bytes)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)
}

/// Getting the chain ID function exposed to the wasm VM Tx environment.
pub fn tx_get_chain_id<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    result_ptr: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let storage = unsafe { env.ctx.storage.get() };
    let (chain_id, gas) = storage.get_chain_id();
    tx_add_gas(env, gas)?;
    let gas = env
        .memory
        .write_string(result_ptr, chain_id)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)
}

/// Getting the block height function exposed to the wasm VM Tx
/// environment. The height is that of the block to which the current
/// transaction is being applied.
pub fn tx_get_block_height<MEM, DB, H>(env: &TxEnv<MEM, DB, H>) -> TxResult<u64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let storage = unsafe { env.ctx.storage.get() };
    let (height, gas) = storage.get_block_height();
    tx_add_gas(env, gas)?;
    Ok(height.0)
}

/// Getting the block hash function exposed to the wasm VM Tx environment. The
/// hash is that of the block to which the current transaction is being applied.
pub fn tx_get_block_hash<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    result_ptr: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let storage = unsafe { env.ctx.storage.get() };
    let (hash, gas) = storage.get_block_hash();
    tx_add_gas(env, gas)?;
    let gas = env
        .memory
        .write_bytes(result_ptr, hash.0)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tx_add_gas(env, gas)
}

/// Getting the block epoch function exposed to the wasm VM Tx
/// environment. The epoch is that of the block to which the current
/// transaction is being applied.
pub fn tx_get_block_epoch<MEM, DB, H>(env: &TxEnv<MEM, DB, H>) -> TxResult<u64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let storage = unsafe { env.ctx.storage.get() };
    let (epoch, gas) = storage.get_block_epoch();
    tx_add_gas(env, gas)?;
    Ok(epoch.0)
}

/// Getting the chain ID function exposed to the wasm VM VP environment.
pub fn vp_get_chain_id<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    result_ptr: u64,
) -> vp_env::Result<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    let storage = unsafe { env.ctx.storage.get() };
    let chain_id = vp_env::get_chain_id(gas_meter, storage)?;
    let gas = env
        .memory
        .write_string(result_ptr, chain_id)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    vp_env::add_gas(gas_meter, gas)
}

/// Getting the block height function exposed to the wasm VM VP
/// environment. The height is that of the block to which the current
/// transaction is being applied.
pub fn vp_get_block_height<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
) -> vp_env::Result<u64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    let storage = unsafe { env.ctx.storage.get() };
    let height = vp_env::get_block_height(gas_meter, storage)?;
    Ok(height.0)
}

/// Getting the block hash function exposed to the wasm VM VP environment. The
/// hash is that of the block to which the current transaction is being applied.
pub fn vp_get_block_hash<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    result_ptr: u64,
) -> vp_env::Result<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    let storage = unsafe { env.ctx.storage.get() };
    let hash = vp_env::get_block_hash(gas_meter, storage)?;
    let gas = env
        .memory
        .write_bytes(result_ptr, hash.0)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    vp_env::add_gas(gas_meter, gas)
}

/// Getting the block epoch function exposed to the wasm VM VP
/// environment. The epoch is that of the block to which the current
/// transaction is being applied.
pub fn vp_get_block_epoch<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
) -> vp_env::Result<u64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    let storage = unsafe { env.ctx.storage.get() };
    let epoch = vp_env::get_block_epoch(gas_meter, storage)?;
    Ok(epoch.0)
}

/// Verify a transaction signature.
pub fn vp_verify_tx_signature<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    pk_ptr: u64,
    pk_len: u64,
    sig_ptr: u64,
    sig_len: u64,
) -> vp_env::Result<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let (pk, gas) = env
        .memory
        .read_bytes(pk_ptr, pk_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(gas_meter, gas)?;
    let pk: PublicKey = BorshDeserialize::try_from_slice(&pk)
        .map_err(vp_env::RuntimeError::EncodingError)?;

    let (sig, gas) = env
        .memory
        .read_bytes(sig_ptr, sig_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    vp_env::add_gas(gas_meter, gas)?;
    let sig: Signature = BorshDeserialize::try_from_slice(&sig)
        .map_err(vp_env::RuntimeError::EncodingError)?;

    vp_env::add_gas(gas_meter, VERIFY_TX_SIG_GAS_COST)?;
    let tx = unsafe { env.ctx.tx.get() };
    Ok(HostEnvResult::from(verify_tx_sig(&pk, tx, &sig).is_ok()).to_i64())
}

/// Log a string from exposed to the wasm VM Tx environment. The message will be
/// printed at the [`tracing::Level::INFO`]. This function is for development
/// only.
pub fn tx_log_string<MEM, DB, H>(
    env: &TxEnv<MEM, DB, H>,
    str_ptr: u64,
    str_len: u64,
) -> TxResult<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
{
    let (str, _gas) = env
        .memory
        .read_string(str_ptr, str_len as _)
        .map_err(|e| TxRuntimeError::MemoryError(Box::new(e)))?;
    tracing::info!("WASM Transaction log: {}", str);
    Ok(())
}

/// Evaluate a validity predicate with the given input data.
pub fn vp_eval<MEM, DB, H, EVAL>(
    env: &VpEnv<'static, MEM, DB, H, EVAL>,
    vp_code_ptr: u64,
    vp_code_len: u64,
    input_data_ptr: u64,
    input_data_len: u64,
) -> vp_env::Result<i64>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator<Db = DB, H = H, Eval = EVAL>,
{
    let (vp_code, gas) =
        env.memory
            .read_bytes(vp_code_ptr, vp_code_len as _)
            .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    let gas_meter = unsafe { env.ctx.gas_meter.get() };
    vp_env::add_gas(gas_meter, gas)?;

    let (input_data, gas) = env
        .memory
        .read_bytes(input_data_ptr, input_data_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    vp_env::add_gas(gas_meter, gas)?;

    let eval_runner = unsafe { env.ctx.eval_runner.get() };
    Ok(eval_runner
        .eval(env.ctx.clone(), vp_code, input_data)
        .to_i64())
}

/// Log a string from exposed to the wasm VM VP environment. The message will be
/// printed at the [`tracing::Level::INFO`]. This function is for development
/// only.
pub fn vp_log_string<MEM, DB, H, EVAL>(
    env: &VpEnv<MEM, DB, H, EVAL>,
    str_ptr: u64,
    str_len: u64,
) -> vp_env::Result<()>
where
    MEM: VmMemory,
    DB: storage::DB + for<'iter> storage::DBIter<'iter>,
    H: StorageHasher,
    EVAL: VpEvaluator,
{
    let (str, _gas) = env
        .memory
        .read_string(str_ptr, str_len as _)
        .map_err(|e| vp_env::RuntimeError::MemoryError(Box::new(e)))?;
    tracing::info!("WASM Validity predicate log: {}", str);
    Ok(())
}

/// Remove given intents from the matchmaker's mempool
pub fn mm_remove_intents<MEM, MM>(
    env: &MatchmakerEnv<MEM, MM>,
    intents_id_ptr: u64,
    intents_id_len: u64,
) where
    MEM: VmMemory,
    MM: MmHost,
{
    let (intents_id_bytes, _gas) = env
        .memory
        .read_bytes(intents_id_ptr, intents_id_len as _)
        .expect("TODO: handle runtime errors");

    let intents_id =
        HashSet::<Vec<u8>>::try_from_slice(&intents_id_bytes).unwrap();

    let mm = env.mm.lock().unwrap();
    mm.remove_intents(intents_id);
}

/// Injupdate_stateaction from matchmaker's matched intents to the ledger
pub fn mm_send_match<MEM, MM>(
    env: &MatchmakerEnv<MEM, MM>,
    data_ptr: u64,
    data_len: u64,
) where
    MEM: VmMemory,
    MM: MmHost,
{
    let (tx_data, _gas) = env
        .memory
        .read_bytes(data_ptr, data_len as _)
        .expect("TODO: handle runtime errors");

    let mm = env.mm.lock().unwrap();
    mm.inject_tx(tx_data);
}

/// Update matchmaker's state data
pub fn mm_update_state<MEM, MM>(
    env: &MatchmakerEnv<MEM, MM>,
    state_ptr: u64,
    state_len: u64,
) where
    MEM: VmMemory,
    MM: MmHost,
{
    let (data, _gas) = env
        .memory
        .read_bytes(state_ptr, state_len as _)
        .expect("TODO: handle runtime errors");

    let mm = env.mm.lock().unwrap();
    mm.update_state(data);
}

/// Log a string from exposed to the wasm VM matchmaker environment. The message
/// will be printed at the [`tracing::Level::INFO`]. This function is for
/// development only.
pub fn mm_log_string<MEM, MM>(
    env: &MatchmakerEnv<MEM, MM>,
    str_ptr: u64,
    str_len: u64,
) where
    MEM: VmMemory,
    MM: MmHost,
{
    let (str, _gas) = env
        .memory
        .read_string(str_ptr, str_len as _)
        .expect("TODO: handle runtime errors");

    tracing::info!("WASM Matchmaker log: {}", str);
}

/// Log a string from exposed to the wasm VM filter environment. The message
/// will be printed at the [`tracing::Level::INFO`].
pub fn mm_filter_log_string<MEM>(
    env: &FilterEnv<MEM>,
    str_ptr: u64,
    str_len: u64,
) where
    MEM: VmMemory,
{
    let (str, _gas) = env
        .memory
        .read_string(str_ptr, str_len as _)
        .expect("TODO: handle runtime errors");
    tracing::info!("WASM Filter log: {}", str);
}

/// A helper module for testing
#[cfg(feature = "testing")]
pub mod testing {
    use super::*;
    use crate::ledger::storage::{self, StorageHasher};
    use crate::vm::memory::testing::NativeMemory;

    /// Setup a transaction environment
    pub fn tx_env<DB, H>(
        storage: &Storage<DB, H>,
        write_log: &mut WriteLog,
        iterators: &mut PrefixIterators<'static, DB>,
        verifiers: &mut HashSet<Address>,
        gas_meter: &mut BlockGasMeter,
        result_buffer: &mut Option<Vec<u8>>,
    ) -> TxEnv<'static, NativeMemory, DB, H>
    where
        DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
        H: StorageHasher,
    {
        TxEnv::new(
            NativeMemory::default(),
            storage,
            write_log,
            iterators,
            gas_meter,
            verifiers,
            result_buffer,
        )
    }

    /// Setup a validity predicate environment
    #[allow(clippy::too_many_arguments)]
    pub fn vp_env<DB, H, EVAL>(
        address: &Address,
        storage: &Storage<DB, H>,
        write_log: &WriteLog,
        iterators: &mut PrefixIterators<'static, DB>,
        gas_meter: &mut VpGasMeter,
        tx: &Tx,
        verifiers: &HashSet<Address>,
        result_buffer: &mut Option<Vec<u8>>,
        keys_changed: &HashSet<Key>,
        eval_runner: &EVAL,
    ) -> VpEnv<'static, NativeMemory, DB, H, EVAL>
    where
        DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
        H: StorageHasher,
        EVAL: VpEvaluator,
    {
        VpEnv::new(
            NativeMemory::default(),
            address,
            storage,
            write_log,
            gas_meter,
            tx,
            iterators,
            verifiers,
            result_buffer,
            keys_changed,
            eval_runner,
        )
    }
}
