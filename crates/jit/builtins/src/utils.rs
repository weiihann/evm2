use alloy_primitives::{Address, U256};
use core::{hint::cold_path, num::NonZero};
use evm2::{
    HostError,
    evm::AccountLoad,
    interpreter::Host,
    utils::{word_to_usize, word_to_usize_saturated},
};
use evm2_jit_context::{EvmContext, EvmWord, InstrStop};

pub type BuiltinResult = Result<(), BuiltinError>;

/// Represents an error that occurred during a builtin execution.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct BuiltinError(NonZero<u8>);

impl From<BuiltinError> for InstrStop {
    #[inline]
    fn from(value: BuiltinError) -> Self {
        // SAFETY: BuiltinError is always created from a valid InstrStop.
        unsafe { core::mem::transmute::<_, _>(value.0.get()) }
    }
}

impl From<InstrStop> for BuiltinError {
    #[inline]
    fn from(value: InstrStop) -> Self {
        cold_path();
        Self(unsafe { NonZero::new_unchecked(value as u8) })
    }
}

#[inline]
pub(crate) fn require_non_staticcall(ecx: &EvmContext<'_, '_, '_>) -> BuiltinResult {
    if ecx.is_static() {
        return Err(InstrStop::StateChangeDuringStaticCall.into());
    }
    Ok(())
}

#[inline]
pub(crate) fn word_to_u64_saturated(value: U256) -> u64 {
    value.try_into().unwrap_or(u64::MAX)
}

/// Runs a host account access, charging the cold-account surcharge when the access was cold.
///
/// Pre-Berlin, `cold_account_additional_cost` is 0, so the cold load logic is a no-op.
#[inline]
fn with_cold_load_gas<R>(
    ecx: &mut EvmContext<'_, '_, '_>,
    load: impl FnOnce(&mut EvmContext<'_, '_, '_>, bool) -> Result<R, HostError>,
    is_cold: impl FnOnce(&R) -> bool,
) -> Result<R, BuiltinError> {
    let cold_load_gas = ecx.gas_params().cold_account_additional_cost();
    let skip_cold_load = ecx.gas.remaining() < cold_load_gas;
    let loaded = load(ecx, skip_cold_load).map_err(|stop| ecx.interpreter_mut().fail(stop))?;
    if is_cold(&loaded) {
        ecx.gas.spend(cold_load_gas)?;
    }
    Ok(loaded)
}

/// Loads an account, handling cold load gas accounting.
pub(crate) fn load_account(
    ecx: &mut EvmContext<'_, '_, '_>,
    address: &Address,
    load_code: bool,
) -> Result<AccountLoad, BuiltinError> {
    with_cold_load_gas(
        ecx,
        |ecx, skip_cold_load| ecx.host().load_account(address, load_code, skip_cold_load),
        |account| account.is_cold,
    )
}

/// Loads an account's code length, handling cold load gas accounting.
///
/// Reads bytecode only when the host does not know the length.
pub(crate) fn load_code_size(
    ecx: &mut EvmContext<'_, '_, '_>,
    address: &Address,
) -> Result<u64, BuiltinError> {
    let loaded = with_cold_load_gas(
        ecx,
        |ecx, skip_cold_load| ecx.host().load_account_code_size(address, skip_cold_load),
        |loaded| loaded.is_cold,
    )?;
    Ok(loaded.size)
}

/// Splits the stack pointer into `N` elements by casting it to an array.
///
/// NOTE: this returns the arguments in **reverse order**. Use `rev!` to get them in order.
///
/// The returned lifetime is valid for the entire duration of the builtin.
///
/// # Safety
///
/// Caller must ensure that `N` matches the number of elements popped in JIT code.
#[inline(always)]
pub(crate) unsafe fn read_words_rev<'a, const N: usize>(sp: *mut EvmWord) -> &'a mut [EvmWord; N] {
    unsafe { &mut *sp.cast::<[EvmWord; N]>() }
}

#[inline]
pub(crate) fn ensure_memory(
    ecx: &mut EvmContext<'_, '_, '_>,
    offset: usize,
    len: usize,
) -> BuiltinResult {
    ecx.resize_memory(offset, len)?;
    Ok(())
}

pub(crate) unsafe fn copy_operation(
    ecx: &mut EvmContext<'_, '_, '_>,
    rev![memory_offset, data_offset, len]: &mut [EvmWord; 3],
    data: &[u8],
) -> BuiltinResult {
    let len = word_to_usize(len.to_u256())?;
    if len != 0 {
        ecx.gas.spend(ecx.gas_params().copy_cost(len))?;
        let memory_offset = word_to_usize(memory_offset.to_u256())?;
        ensure_memory(ecx, memory_offset, len)?;
        let data_offset = word_to_usize_saturated(data_offset.to_u256());
        ecx.memory_mut().set_data(memory_offset, data_offset, len, data);
    }
    Ok(())
}
