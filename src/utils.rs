use crate::{
    business_logic::{
        execution::{
            gas_usage::calculate_tx_gas_usage, objects::CallInfo,
            os_usage::get_additional_os_resources,
        },
        fact_state::state::ExecutionResourcesManager,
        state::{
            cached_state::UNINITIALIZED_CLASS_HASH, state_api::StateReader,
            state_cache::StorageEntry,
        },
        transaction::error::TransactionError,
    },
    core::errors::syscall_handler_errors::SyscallHandlerError,
    definitions::transaction_type::TransactionType,
    services::api::contract_class::EntryPointType,
};
use cairo_rs::{types::relocatable::Relocatable, vm::vm_core::VirtualMachine};
use felt::{Felt252, ParseFeltError};
use num_traits::{Num, ToPrimitive};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use starknet_crypto::FieldElement;
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

pub type ClassHash = [u8; 32];

//* -------------------
//*      Address
//* -------------------

#[derive(Debug, Clone, PartialEq, Hash, Eq, Default, Serialize, Deserialize)]
pub struct Address(pub Felt252);

//* -------------------
//*  Helper Functions
//* -------------------

pub fn get_integer(
    vm: &VirtualMachine,
    syscall_ptr: Relocatable,
) -> Result<usize, SyscallHandlerError> {
    vm.get_integer(syscall_ptr)?
        .as_ref()
        .to_usize()
        .ok_or(SyscallHandlerError::FeltToUsizeFail)
}

pub fn get_big_int(
    vm: &VirtualMachine,
    syscall_ptr: Relocatable,
) -> Result<Felt252, SyscallHandlerError> {
    Ok(vm.get_integer(syscall_ptr)?.into_owned())
}

pub fn get_relocatable(
    vm: &VirtualMachine,
    syscall_ptr: Relocatable,
) -> Result<Relocatable, SyscallHandlerError> {
    Ok(vm.get_relocatable(syscall_ptr)?)
}

pub fn get_integer_range(
    vm: &VirtualMachine,
    addr: Relocatable,
    size: usize,
) -> Result<Vec<Felt252>, SyscallHandlerError> {
    Ok(vm
        .get_integer_range(addr, size)?
        .into_iter()
        .map(|c| c.into_owned())
        .collect::<Vec<Felt252>>())
}

pub fn felt_to_field_element(value: &Felt252) -> Result<FieldElement, SyscallHandlerError> {
    FieldElement::from_dec_str(&value.to_str_radix(10))
        .map_err(|_| SyscallHandlerError::FailToComputeHash)
}

pub fn field_element_to_felt(felt: &FieldElement) -> Felt252 {
    Felt252::from_bytes_be(&felt.to_bytes_be())
}

pub fn string_to_hash(class_string: &String) -> Result<ClassHash, ParseFeltError> {
    let parsed_felt =
        Felt252::from_str_radix(class_string.strip_prefix("0x").unwrap_or(class_string), 16)?
            .to_be_bytes();

    Ok(parsed_felt)
}

// -------------------
//    STATE UTILS
// -------------------

/// Converts CachedState storage mapping to StateDiff storage mapping.
/// See to_cached_state_storage_mapping documentation.

pub fn to_state_diff_storage_mapping(
    storage_writes: HashMap<StorageEntry, Felt252>,
) -> HashMap<Felt252, HashMap<ClassHash, Address>> {
    let mut storage_updates: HashMap<Felt252, HashMap<ClassHash, Address>> = HashMap::new();
    for ((address, key), value) in storage_writes {
        let mut map = storage_updates.get(&address.0).cloned().unwrap_or_default();
        map.insert(key, Address(value));
        storage_updates.insert(address.0, map);
    }
    storage_updates
}

/// Returns the total resources needed to include the most recent transaction in a StarkNet batch
/// (recent w.r.t. application on the given state) i.e., L1 gas usage and Cairo execution resources.
/// Used for transaction fee; calculation is made as if the transaction is the first in batch, for
/// consistency.

pub fn get_call_n_deployments(call_info: &CallInfo) -> usize {
    call_info
        .gen_call_topology()
        .iter()
        .fold(0, |acc, c| match c.entry_point_type {
            Some(EntryPointType::Constructor) => acc + 1,
            _ => acc,
        })
}

pub fn calculate_tx_resources(
    resources_manager: ExecutionResourcesManager,
    call_info: &[Option<CallInfo>],
    tx_type: TransactionType,
    storage_changes: (usize, usize),
    l1_handler_payload_size: Option<usize>,
) -> Result<HashMap<String, usize>, TransactionError> {
    let (n_modified_contracts, n_storage_changes) = storage_changes;

    let non_optional_calls: Vec<CallInfo> = call_info.iter().flatten().cloned().collect();
    let n_deployments = non_optional_calls.iter().map(get_call_n_deployments).sum();

    let mut l2_to_l1_messages = Vec::new();

    for call_info in non_optional_calls {
        l2_to_l1_messages.extend(call_info.get_sorted_l2_to_l1_messages()?)
    }

    let l1_gas_usage = calculate_tx_gas_usage(
        l2_to_l1_messages,
        n_modified_contracts,
        n_storage_changes,
        l1_handler_payload_size,
        n_deployments,
    );

    let cairo_usage = resources_manager.cairo_usage.clone();
    let tx_syscall_counter = resources_manager.syscall_counter;

    // Add additional Cairo resources needed for the OS to run the transaction.
    let additional_resources = get_additional_os_resources(tx_syscall_counter, &tx_type)?;
    let new_resources = &cairo_usage + &additional_resources;
    let filtered_builtins = new_resources.filter_unused_builtins();

    let mut resources: HashMap<String, usize> = HashMap::new();
    resources.insert("l1_gas_usage".to_string(), l1_gas_usage);
    for (builtin, value) in filtered_builtins.builtin_instance_counter {
        resources.insert(builtin, value);
    }

    Ok(resources)
}

/// Returns a mapping containing key-value pairs from a that are not included in b (if
/// a key appears in b with a different value, it will be part of the output).
/// Uses to take only updated cells from a mapping.

fn contained_and_not_updated<K, V>(key: &K, value: &V, map: &HashMap<K, V>) -> bool
where
    K: Hash + Eq,
    V: PartialEq + Clone,
{
    let val = map.get(key);
    !(map.contains_key(key) && (Some(value) == val))
}

pub fn subtract_mappings<K, V>(map_a: HashMap<K, V>, map_b: HashMap<K, V>) -> HashMap<K, V>
where
    K: Hash + Eq + Clone,
    V: PartialEq + Clone,
{
    map_a
        .into_iter()
        .filter(|(k, v)| contained_and_not_updated(k, v, &map_b))
        .collect()
}

/// Converts StateDiff storage mapping (addresses map to a key-value mapping) to CachedState
/// storage mapping (Tuple of address and key map to the associated value).

pub fn to_cache_state_storage_mapping(
    map: HashMap<Felt252, HashMap<ClassHash, Address>>,
) -> HashMap<StorageEntry, Felt252> {
    let mut storage_writes = HashMap::new();
    for (address, contract_storage) in map {
        for (key, value) in contract_storage {
            storage_writes.insert((Address(address.clone()), key), value.0);
        }
    }
    storage_writes
}

// get a vector of keys from two hashmaps

pub fn get_keys<K, V>(map_a: HashMap<K, V>, map_b: HashMap<K, V>) -> Vec<K>
where
    K: Hash + Eq,
{
    let mut keys1: HashSet<K> = map_a.into_keys().collect();
    let keys2: HashSet<K> = map_b.into_keys().collect();

    keys1.extend(keys2);

    keys1.into_iter().collect()
}

//* ----------------------------
//* Execution entry point utils
//* ----------------------------

pub fn get_deployed_address_class_hash_at_address<S: StateReader>(
    state: &mut S,
    contract_address: &Address,
) -> Result<ClassHash, TransactionError> {
    let class_hash: ClassHash = state
        .get_class_hash_at(contract_address)
        .map_err(|_| TransactionError::FailToReadClassHash)?
        .to_owned();

    if class_hash == *UNINITIALIZED_CLASS_HASH {
        return Err(TransactionError::NotDeployedContract(class_hash));
    }
    Ok(class_hash)
}

pub fn validate_contract_deployed<S: StateReader>(
    state: &mut S,
    contract_address: &Address,
) -> Result<ClassHash, TransactionError> {
    get_deployed_address_class_hash_at_address(state, contract_address)
}

//* ----------------------------
//* Internal objects utils
//* ----------------------------

pub(crate) fn verify_no_calls_to_other_contracts(
    call_info: &CallInfo,
) -> Result<(), TransactionError> {
    let invoked_contract_address = call_info.contract_address.clone();
    for internal_call in call_info.gen_call_topology() {
        if internal_call.contract_address != invoked_contract_address {
            return Err(TransactionError::UnauthorizedActionOnValidate);
        }
    }
    Ok(())
}
pub fn calculate_sn_keccak(data: &[u8]) -> ClassHash {
    let mut hasher = Keccak256::default();
    hasher.update(data);
    let mut result: ClassHash = hasher.finalize().into();
    // Only the first 250 bits from the hash are used.
    result[0] &= 0b0000_0011;
    result
}

//* -------------------
//*      Macros
//* -------------------

#[cfg(test)]
#[macro_use]
pub mod test_utils {
    #![allow(unused_imports)]

    #[macro_export]
    macro_rules! any_box {
        ($val : expr) => {
            Box::new($val) as Box<dyn Any>
        };
    }
    pub(crate) use any_box;

    macro_rules! references {
        ($num: expr) => {{
            let mut references = HashMap::<
                usize,
                cairo_rs::hint_processor::hint_processor_definition::HintReference,
            >::new();
            for i in 0..$num {
                references.insert(
                    i as usize,
                    cairo_rs::hint_processor::hint_processor_definition::HintReference::new_simple(
                        (i as i32),
                    ),
                );
            }
            references
        }};
    }
    pub(crate) use references;

    macro_rules! ids_data {
        ( $( $name: expr ),* ) => {
            {
                let ids_names = vec![$( $name ),*];
                let references = $crate::utils::test_utils::references!(ids_names.len() as i32);
                let mut ids_data = HashMap::<String, cairo_rs::hint_processor::hint_processor_definition::HintReference>::new();
                for (i, name) in ids_names.iter().enumerate() {
                    ids_data.insert(name.to_string(), references.get(&i).unwrap().clone());
                }
                ids_data
            }
        };
    }
    pub(crate) use ids_data;

    macro_rules! vm {
        () => {
            VirtualMachine::new(false)
        };

        ($use_trace:expr) => {
            VirtualMachine::new($use_trace, Vec::new())
        };
    }
    pub(crate) use vm;

    #[macro_export]
    macro_rules! add_segments {
        ($vm:expr, $n:expr) => {
            for _ in 0..$n {
                $vm.add_memory_segment();
            }
        };
    }
    pub(crate) use add_segments;

    #[macro_export]
    macro_rules! memory_insert {
        ($vm:expr, [ $( (($si:expr, $off:expr), $val:tt) ),* ] ) => {
            $( $crate::allocate_values!($vm, $si, $off, $val); )*
        };
    }
    pub(crate) use memory_insert;

    #[macro_export]
    macro_rules! allocate_values {
        ($vm: expr, $si:expr, $off:expr, ($sival:expr, $offval:expr)) => {
            let k = $crate::relocatable_value!($si, $off);
            let v = $crate::relocatable_value!($sival, $offval);
            $vm.insert_value(k, &v).unwrap();
        };
        ($vm: expr, $si:expr, $off:expr, $val:expr) => {
            let v: felt::Felt252 = $val.into();
            let k = $crate::relocatable_value!($si, $off);
            $vm.insert_value(k, v).unwrap();
        };
    }
    pub(crate) use allocate_values;

    #[macro_export]
    macro_rules! allocate_selector {
        ($vm: expr, (($si:expr, $off:expr), $val:expr)) => {
            let v = felt::Felt252::from_bytes_be($val);
            let k = $crate::relocatable_value!($si, $off);
            $vm.insert_value(k, v).unwrap();
        };
    }
    pub(crate) use allocate_selector;

    #[macro_export]
    macro_rules! relocatable_value {
        ($val1 : expr, $val2 : expr) => {
            Relocatable {
                segment_index: ($val1),
                offset: ($val2),
            }
        };
    }
    pub(crate) use relocatable_value;

    #[macro_export]
    macro_rules! exec_scopes_ref {
        () => {
            &mut ExecutionScopes::new()
        };
    }
    pub(crate) use exec_scopes_ref;

    #[macro_export]
    macro_rules! run_hint {
        ($vm:expr, $ids_data:expr, $hint_code:expr, $exec_scopes:expr, $constants:expr) => {{
            let hint_data = HintProcessorData::new_default($hint_code.to_string(), $ids_data);
            let hint_processor = BuiltinHintProcessor::new_empty();
            hint_processor.execute_hint(&mut $vm, $exec_scopes, &any_box!(hint_data), $constants)
        }};
        ($vm:expr, $ids_data:expr, $hint_code:expr, $exec_scopes:expr) => {{
            let hint_data = HintProcessorData::new_default($hint_code.to_string(), $ids_data);
            let hint_processor = BuiltinHintProcessor::new_empty();
            hint_processor.execute_hint(
                &mut $vm,
                $exec_scopes,
                &any_box!(hint_data),
                &HashMap::new(),
            )
        }};
        ($vm:expr, $ids_data:expr, $hint_code:expr) => {{
            let hint_data = HintProcessorData::new_default($hint_code.to_string(), $ids_data);
            let mut hint_processor = BuiltinHintProcessor::new_empty();
            hint_processor.execute_hint(
                &mut $vm,
                exec_scopes_ref!(),
                &any_box!(hint_data),
                &HashMap::new(),
            )
        }};
    }
    pub(crate) use run_hint;

    #[macro_export]
    macro_rules! run_syscall_hint {
        ($vm:expr, $ids_data:expr, $hint_code:expr, $exec_scopes:expr, $constants:expr) => {{
            let hint_data = HintProcessorData::new_default($hint_code.to_string(), $ids_data);
            let hint_processor = BuiltinHintProcessor::new_empty();
            hint_processor.execute_hint(&mut $vm, $exec_scopes, &any_box!(hint_data), $constants)
        }};
        ($vm:expr, $ids_data:expr, $hint_code:expr, $exec_scopes:expr) => {{
            let hint_data = HintProcessorData::new_default($hint_code.to_string(), $ids_data);
            let hint_processor = BuiltinHintProcessor::new_empty();
            hint_processor.execute_hint(
                &mut $vm,
                $exec_scopes,
                &any_box!(hint_data),
                &HashMap::new(),
            )
        }};
        ($vm:expr, $ids_data:expr, $hint_code:expr) => {{
            let hint_data = HintProcessorData::new_default($hint_code.to_string(), $ids_data);
            let mut state = CachedState::<InMemoryStateReader>::default();
            let mut hint_processor = $crate::core::syscalls::syscall_handler::SyscallHintProcessor::<
                $crate::core::syscalls::business_logic_syscall_handler::BusinessLogicSyscallHandler::<
                    $crate::business_logic::state::cached_state::CachedState<
                        $crate::business_logic::fact_state::in_memory_state_reader::InMemoryStateReader,
                    >,
                >,
            >::new(BusinessLogicSyscallHandler::default_with(&mut state));
            hint_processor.execute_hint(
                &mut $vm,
                exec_scopes_ref!(),
                &any_box!(hint_data),
                &HashMap::new(),
            )
        }};
    }
    pub(crate) use run_syscall_hint;
}

#[cfg(test)]
mod test {
    use super::*;
    use coverage_helper::test;
    use felt::{felt_str, Felt252};
    use num_traits::{Bounded, One, Zero};
    use std::collections::HashMap;

    #[test]
    fn to_state_diff_storage_mapping_test() {
        let mut storage: HashMap<(Address, ClassHash), Felt252> = HashMap::new();
        let address1: Address = Address(1.into());
        let key1 = [0; 32];
        let value1: Felt252 = 2.into();

        let address2: Address = Address(3.into());
        let key2 = [1; 32];

        let value2: Felt252 = 4.into();

        storage.insert((address1.clone(), key1), value1.clone());
        storage.insert((address2.clone(), key2), value2.clone());

        let map = to_state_diff_storage_mapping(storage);

        assert_eq!(
            *map.get(&address1.0).unwrap().get(&key1).unwrap(),
            Address(value1)
        );
        assert_eq!(
            *map.get(&address2.0).unwrap().get(&key2).unwrap(),
            Address(value2)
        );
    }

    #[test]

    fn subtract_mappings_test() {
        let mut a = HashMap::new();
        let mut b = HashMap::new();

        a.insert("a", 2);
        a.insert("b", 3);

        b.insert("c", 2);
        b.insert("d", 4);
        b.insert("a", 3);

        let res = [("a", 2), ("b", 3)]
            .into_iter()
            .collect::<HashMap<&str, i32>>();

        assert_eq!(subtract_mappings(a, b), res);

        let mut c = HashMap::new();
        let mut d = HashMap::new();

        c.insert(1, 2);
        c.insert(3, 4);
        c.insert(6, 7);

        d.insert(1, 3);
        d.insert(3, 5);
        d.insert(6, 8);

        let res = [(1, 2), (3, 4), (6, 7)]
            .into_iter()
            .collect::<HashMap<i32, i32>>();

        assert_eq!(subtract_mappings(c, d), res);

        let mut e = HashMap::new();
        let mut f = HashMap::new();
        e.insert(1, 2);
        e.insert(3, 4);
        e.insert(6, 7);

        f.insert(1, 2);
        f.insert(3, 4);
        f.insert(6, 7);

        assert_eq!(subtract_mappings(e, f), HashMap::new())
    }

    #[test]

    fn to_cache_state_storage_mapping_test() {
        let mut storage: HashMap<(Address, ClassHash), Felt252> = HashMap::new();
        let address1: Address = Address(1.into());
        let key1 = [0; 32];
        let value1: Felt252 = 2.into();

        let address2: Address = Address(3.into());
        let key2 = [1; 32];

        let value2: Felt252 = 4.into();

        storage.insert((address1.clone(), key1), value1.clone());
        storage.insert((address2.clone(), key2), value2.clone());

        let state_dff = to_state_diff_storage_mapping(storage);
        let cache_storage = to_cache_state_storage_mapping(state_dff);

        let mut expected_res = HashMap::new();

        expected_res.insert((Address(address1.0), key1), value1);
        expected_res.insert((Address(address2.0), key2), value2);

        assert_eq!(cache_storage, expected_res)
    }

    #[test]
    fn test_to_be_bytes() {
        assert_eq!(&Felt252::zero().to_be_bytes(), &[0u8; 32]);
        assert_eq!(
            Felt252::one().to_be_bytes(),
            [
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 1
            ],
        );
        assert_eq!(
            Felt252::new(257).to_be_bytes(),
            [
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 1, 1
            ],
        );

        assert_eq!(
            felt_str!(
                "2151680050850558576753658069693146429350618838199373217695410689374331200218"
            )
            .to_be_bytes(),
            [
                4, 193, 206, 200, 202, 13, 38, 110, 16, 37, 89, 67, 39, 3, 185, 128, 123, 117, 218,
                224, 80, 72, 144, 143, 109, 237, 203, 41, 241, 37, 226, 218
            ],
        );
    }

    #[test]
    fn test_felt_to_field_element() {
        // test with a negative number
        let w = Felt252::new(-10);
        let w_bytes = w.to_be_bytes();
        let v = FieldElement::from_bytes_be(&w_bytes).unwrap();

        assert_eq!(felt_to_field_element(&w).unwrap(), v);

        // test with zero
        let x = Felt252::zero();
        assert_eq!(felt_to_field_element(&x).unwrap(), FieldElement::ZERO);

        // test with 1
        let y = Felt252::new(1);
        assert_eq!(felt_to_field_element(&y).unwrap(), FieldElement::ONE);

        // test with the largest possible number
        let z = Felt252::max_value();
        assert_eq!(felt_to_field_element(&z).unwrap(), FieldElement::MAX);
    }

    #[test]
    fn test_field_element_to_felt() {
        // test with a negative number
        let w = Felt252::new(-10);
        let w_bytes = w.to_be_bytes();
        let v = FieldElement::from_bytes_be(&w_bytes).unwrap();

        assert_eq!(field_element_to_felt(&v), w);

        // test with zero
        assert_eq!(field_element_to_felt(&FieldElement::ZERO), Felt252::zero());

        // test with 1
        assert_eq!(field_element_to_felt(&FieldElement::ONE), Felt252::new(1));

        // test with the largest possible number
        assert_eq!(
            field_element_to_felt(&FieldElement::MAX),
            Felt252::max_value()
        );
    }

    pub fn field_element_to_felt(felt: &FieldElement) -> Felt252 {
        Felt252::from_bytes_be(&felt.to_bytes_be())
    }

    #[test]
    fn test_get_keys() {
        let hashmap_one = HashMap::from([("one", 1), ("two", 2)]);
        let hashmap_two = HashMap::from([("three", 3), ("four", 4)]);

        assert_eq!(
            get_keys(hashmap_one, hashmap_two)
                .iter()
                .collect::<HashSet<_>>(),
            Vec::from(["one", "two", "three", "four"])
                .iter()
                .collect::<HashSet<_>>()
        );
    }

    #[test]
    fn test_string_to_hash() {
        assert_eq!(
            string_to_hash(&String::from("0x0")).unwrap(),
            [
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0
            ]
        );

        // testing with max value: 3618502788666131213697322783095070105623107215331596699973092056135872020480
        assert_eq!(
            string_to_hash(&String::from(
                "0x800000000000011000000000000000000000000000000000000000000000000"
            ))
            .unwrap(),
            [
                8, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0
            ]
        );

        // testing with number above max value: 4000000000000000000000000000000000000000000000000000000000000000000000000000
        assert_eq!(
            string_to_hash(&String::from(
                "0x8D7EB76070A08AECFC1E1DE5CF543CA2FC8A8C6FA3BA0000000000000000000"
            ))
            .unwrap(),
            [
                0, 215, 235, 118, 7, 10, 8, 157, 207, 193, 225, 222, 92, 245, 67, 202, 47, 200,
                168, 198, 250, 59, 159, 255, 255, 255, 255, 255, 255, 255, 255, 255
            ]
        );
    }
}
