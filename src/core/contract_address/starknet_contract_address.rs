use crate::{
    core::errors::contract_address_errors::ContractAddressError,
    services::api::contract_class::{ContractClass, ContractEntryPoint, EntryPointType},
};
use cairo_rs::{
    hint_processor::builtin_hint_processor::builtin_hint_processor_definition::BuiltinHintProcessor,
    serde::deserialize_program::Identifier,
    types::{program::Program, relocatable::MaybeRelocatable},
    vm::{
        runners::cairo_runner::{CairoArg, CairoRunner},
        vm_core::VirtualMachine,
    },
};
use felt::Felt252;
use lazy_static::lazy_static;
use sha3::{Digest, Keccak256};
use std::{collections::HashMap, path::Path};

/// Instead of doing a Mask with 250 bits, we are only masking the most significant byte.
pub const MASK_3: u8 = 3;
pub const CONTRACT_STR: &str = include_str!("../../../cairo_programs/contracts.json");

lazy_static! {
    // static ref PATH_BUF_CONTRACTS = BufReader::from(CONTRACT_STR);
    static ref HASH_CALCULATION_PROGRAM: Program =
        Program::from_bytes(CONTRACT_STR.as_bytes(), None).unwrap();
}

#[allow(dead_code)]
fn load_program() -> Result<Program, ContractAddressError> {
    Ok(Program::from_file(
        Path::new("cairo_programs/contracts.json"),
        None,
    )?)
}

fn get_contract_entry_points(
    contract_class: &ContractClass,
    entry_point_type: &EntryPointType,
) -> Result<Vec<ContractEntryPoint>, ContractAddressError> {
    let program_length = contract_class.program().data.len();

    let entry_points = contract_class
        .entry_points_by_type()
        .get(entry_point_type)
        .ok_or(ContractAddressError::NoneExistingEntryPointType)?;

    let program_len = program_length;
    for entry_point in entry_points {
        if entry_point.offset > program_len {
            return Err(ContractAddressError::InvalidOffset(entry_point.offset));
        }
    }
    Ok(entry_points
        .iter()
        .map(|entry_point| ContractEntryPoint {
            offset: entry_point.offset,
            selector: entry_point.selector.clone(),
        })
        .collect())
}

/// A variant of eth-keccak that computes a value that fits in a StarkNet field element.
fn starknet_keccak(data: &[u8]) -> Felt252 {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    let mut finalized_hash = hasher.finalize();
    let hashed_slice: &[u8] = finalized_hash.as_slice();

    // This is the same than doing a mask 3 only with the most significant byte.
    // and then copying the other values.
    let res = hashed_slice[0] & MASK_3;
    finalized_hash[0] = res;
    Felt252::from_bytes_be(finalized_hash.as_slice())
}

/// Computes the hash of the contract class, including hints.
/// We are not supporting backward compatibility now.
fn compute_hinted_class_hash(_contract_class: &ContractClass) -> Felt252 {
    let keccak_input =
        r#"{"abi": contract_class.abi, "program": contract_class.program}"#.as_bytes();
    starknet_keccak(keccak_input)
}

/// Returns the serialization of a contract as a list of field elements.
fn get_contract_class_struct(
    identifiers: &HashMap<String, Identifier>,
    contract_class: &ContractClass,
) -> Result<StructContractClass, ContractAddressError> {
    let api_version = identifiers.get("__main__.API_VERSION").ok_or_else(|| {
        ContractAddressError::MissingIdentifier("__main__.API_VERSION".to_string())
    })?;
    let external_functions = get_contract_entry_points(contract_class, &EntryPointType::External)?;
    let l1_handlers = get_contract_entry_points(contract_class, &EntryPointType::L1Handler)?;
    let constructors = get_contract_entry_points(contract_class, &EntryPointType::Constructor)?;
    let builtin_list = &contract_class.program().builtins;

    Ok(StructContractClass {
        api_version: api_version
            .value
            .as_ref()
            .ok_or(ContractAddressError::NoneApiVersion)?
            .to_owned()
            .into(),
        n_external_functions: Felt252::from(external_functions.len()).into(),
        external_functions,
        n_l1_handlers: Felt252::from(l1_handlers.len()).into(),
        l1_handlers,
        n_constructors: Felt252::from(constructors.len()).into(),
        constructors,
        n_builtins: Felt252::from(builtin_list.len()).into(),
        builtin_list: builtin_list
            .iter()
            .map(|builtin| {
                Felt252::from_bytes_be(builtin.name().to_ascii_lowercase().as_bytes()).into()
            })
            .collect::<Vec<MaybeRelocatable>>(),
        hinted_class_hash: compute_hinted_class_hash(contract_class).into(),
        bytecode_length: Felt252::from(contract_class.program().data.len()).into(),
        bytecode_ptr: contract_class.program().data.clone(),
    })
}

// TODO: think about a new name for this struct (ContractClass already exists)
#[derive(Debug)]
struct StructContractClass {
    api_version: MaybeRelocatable,
    n_external_functions: MaybeRelocatable,
    external_functions: Vec<ContractEntryPoint>,
    n_l1_handlers: MaybeRelocatable,
    l1_handlers: Vec<ContractEntryPoint>,
    n_constructors: MaybeRelocatable,
    constructors: Vec<ContractEntryPoint>,
    n_builtins: MaybeRelocatable,
    builtin_list: Vec<MaybeRelocatable>,
    hinted_class_hash: MaybeRelocatable,
    bytecode_length: MaybeRelocatable,
    bytecode_ptr: Vec<MaybeRelocatable>,
}

fn flat_into_maybe_relocs(contract_entrypoints: Vec<ContractEntryPoint>) -> Vec<MaybeRelocatable> {
    contract_entrypoints
        .iter()
        .flat_map::<Vec<MaybeRelocatable>, _>(|contract_entrypoint| contract_entrypoint.into())
        .collect::<Vec<MaybeRelocatable>>()
}

impl From<StructContractClass> for CairoArg {
    fn from(contract_class: StructContractClass) -> Self {
        let external_functions_flatted = flat_into_maybe_relocs(contract_class.external_functions);
        let l1_handlers_flatted = flat_into_maybe_relocs(contract_class.l1_handlers);
        let constructors_flatted = flat_into_maybe_relocs(contract_class.constructors);

        let result = vec![
            CairoArg::Single(contract_class.api_version),
            CairoArg::Single(contract_class.n_external_functions),
            CairoArg::Array(external_functions_flatted),
            CairoArg::Single(contract_class.n_l1_handlers),
            CairoArg::Array(l1_handlers_flatted),
            CairoArg::Single(contract_class.n_constructors),
            CairoArg::Array(constructors_flatted),
            CairoArg::Single(contract_class.n_builtins),
            CairoArg::Array(contract_class.builtin_list),
            CairoArg::Single(contract_class.hinted_class_hash),
            CairoArg::Single(contract_class.bytecode_length),
            CairoArg::Array(contract_class.bytecode_ptr),
        ];
        CairoArg::Composed(result)
    }
}

// TODO: Maybe this could be hard-coded (to avoid returning a result)?
pub fn compute_class_hash(contract_class: &ContractClass) -> Result<Felt252, ContractAddressError> {
    // Since we are not using a cache, this function replace compute_class_hash_inner.
    let hash_calculation_program = HASH_CALCULATION_PROGRAM.clone();
    let contract_class_struct =
        &get_contract_class_struct(&hash_calculation_program.identifiers, contract_class)?.into();

    let mut vm = VirtualMachine::new(false);
    let mut runner = CairoRunner::new(&hash_calculation_program, "all", false)?;
    runner.initialize_function_runner(&mut vm)?;
    let mut hint_processor = BuiltinHintProcessor::new_empty();

    // 188 is the entrypoint since is the __main__.class_hash function in our compiled program identifier.
    // TODO: Looks like we can get this value from the identifier, but the value is a Felt252.
    // We need to cast that into a usize.
    // let entrypoint = program.identifiers.get("__main__.class_hash").unwrap().pc.unwrap();
    let hash_base: MaybeRelocatable = runner.add_additional_hash_builtin(&mut vm).into();

    runner.run_from_entrypoint(
        188,
        &[&hash_base.into(), contract_class_struct],
        true,
        &mut vm,
        &mut hint_processor,
    )?;

    match vm.get_return_values(2)?.get(1) {
        Some(MaybeRelocatable::Int(felt)) => Ok(felt.clone()),
        _ => Err(ContractAddressError::IndexOutOfRange),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coverage_helper::test;
    use felt::Felt252;
    use num_traits::Num;

    #[test]
    fn test_starknet_keccak() {
        let data: &[u8] = "hello".as_bytes();

        // This expected output is the result of calling the python version in cairo-lang of the function.
        // starknet_keccak("hello".encode())
        let expected_result = Felt252::from_str_radix(
            "245588857976802048747271734601661359235654411526357843137188188665016085192",
            10,
        )
        .unwrap();
        let result = starknet_keccak(data);

        assert_eq!(expected_result, result);
    }

    #[test]
    fn test_get_contract_entrypoints() {
        let mut entry_points_by_type = HashMap::new();
        entry_points_by_type.insert(
            EntryPointType::Constructor,
            vec![ContractEntryPoint {
                selector: 1.into(),
                offset: 2,
            }],
        );
        let contract_class = ContractClass {
            program: load_program().unwrap(),
            entry_points_by_type,
            abi: None,
        };

        assert_eq!(
            get_contract_entry_points(&contract_class, &EntryPointType::Constructor).unwrap(),
            vec![ContractEntryPoint {
                selector: 1.into(),
                offset: 2
            }]
        );
        assert_matches!(
            get_contract_entry_points(&contract_class, &EntryPointType::External),
            Err(ContractAddressError::NoneExistingEntryPointType)
        );
    }

    #[test]
    fn test_compute_class_hash() {
        let mut entry_points_by_type = HashMap::new();
        entry_points_by_type.insert(
            EntryPointType::Constructor,
            vec![ContractEntryPoint {
                selector: 3.into(),
                offset: 2,
            }],
        );
        entry_points_by_type.insert(
            EntryPointType::L1Handler,
            vec![ContractEntryPoint {
                selector: 4.into(),
                offset: 2,
            }],
        );
        entry_points_by_type.insert(
            EntryPointType::External,
            vec![ContractEntryPoint {
                selector: 5.into(),
                offset: 2,
            }],
        );
        let contract_class = ContractClass {
            program: load_program().unwrap(),
            entry_points_by_type,
            abi: None,
        };
        assert_eq!(
            compute_class_hash(&contract_class).unwrap(),
            Felt252::from_str_radix(
                "1809635095607326950459993008040437939724930328662161791121345395618950656878",
                10
            )
            .unwrap()
        );
    }

    #[test]
    fn test_compute_hinted_class_hash() {
        let mut entry_points_by_type = HashMap::new();
        entry_points_by_type.insert(
            EntryPointType::Constructor,
            vec![ContractEntryPoint {
                selector: 1.into(),
                offset: 12,
            }],
        );
        entry_points_by_type.insert(
            EntryPointType::L1Handler,
            vec![ContractEntryPoint {
                selector: 2.into(),
                offset: 12,
            }],
        );
        entry_points_by_type.insert(
            EntryPointType::External,
            vec![ContractEntryPoint {
                selector: 3.into(),
                offset: 12,
            }],
        );
        let contract_class = ContractClass {
            program: load_program().unwrap(),
            entry_points_by_type,
            abi: None,
        };

        assert_eq!(
            compute_hinted_class_hash(&contract_class),
            Felt252::from_str_radix(
                "1703103364832599665802491695999915073351807236114175062140703903952998591438",
                10
            )
            .unwrap()
        );
    }
}
