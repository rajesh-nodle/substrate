// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Benchmarks for the contracts pallet

#![cfg(feature = "runtime-benchmarks")]

use crate::*;
use crate::Module as Contracts;
use crate::exec::StorageKey;
use crate::schedule::API_BENCHMARK_BATCH_SIZE;

use frame_benchmarking::{benchmarks, account};
use frame_system::{Module as System, RawOrigin};
use parity_wasm::elements::{Instruction, Instructions, FuncBody, ValueType, BlockType};
use sp_runtime::traits::{Hash, Bounded, SaturatedConversion, CheckedDiv};
use sp_std::{default::Default, convert::{TryFrom, TryInto}};

/// How many batches we do per API benchmark. 
const API_BENCHMARK_BATCHES: u32 = 20;

#[derive(Clone)]
struct WasmModule<T:Trait> {
	code: Vec<u8>,
	hash: <T::Hashing as Hash>::Output,
}

struct Contract<T: Trait> {
	caller: T::AccountId,
	account_id: T::AccountId,
	addr: <T::Lookup as StaticLookup>::Source,
	endowment: BalanceOf<T>,
	code_hash: <T::Hashing as Hash>::Output,
}

struct Tombstone<T: Trait> {
	contract: Contract<T>,
	storage: Vec<(StorageKey, Vec<u8>)>,
}

struct ModuleDefinition {
	data_segments: Vec<DataSegment>,
	memory: Option<ImportedMemory>,
	imported_functions: Vec<ImportedFunction>,
	deploy_body: Option<FuncBody>,
	call_body: Option<FuncBody>,
}

impl Default for ModuleDefinition {
	fn default() -> Self {
		Self {
			data_segments: vec![],
			memory: None,
			imported_functions: vec![],
			deploy_body: None,
			call_body: None,
		}
	}
}

struct ImportedFunction {
	name: &'static str,
	params: Vec<ValueType>,
	return_type: Option<ValueType>,
}

struct ImportedMemory {
	min_pages: u32,
	max_pages: u32,
}

impl ImportedMemory {
	fn max<T: Trait>() -> Self {
		let pages = max_pages::<T>();
		Self { min_pages: pages, max_pages: pages }
	}
}

struct DataSegment {
	offset: u32,
	value: Vec<u8>,
}

enum CountedInstruction {
	// (offset, increment_by)
	Counter(u32, u32),
	Regular(Instruction),
}

fn create_code<T: Trait>(def: ModuleDefinition) -> WasmModule<T> {
	// internal functions start at that offset.
	let func_offset = u32::try_from(def.imported_functions.len()).unwrap();

	// Every contract must export "deploy" and "call" functions
	let mut contract = parity_wasm::builder::module()
		// deploy function (first internal function)
		.function()
			.signature().with_params(vec![]).with_return_type(None).build()
			.with_body(def.deploy_body.unwrap_or_else(||
				FuncBody::new(Vec::new(), Instructions::empty())
			))
			.build()
		// call function (second internal function)
		.function()
			.signature().with_params(vec![]).with_return_type(None).build()
			.with_body(def.call_body.unwrap_or_else(||
				FuncBody::new(Vec::new(), Instructions::empty())
			))
			.build()
		.export().field("deploy").internal().func(func_offset).build()
		.export().field("call").internal().func(func_offset + 1).build();

	// Grant access to linear memory.
	if let Some(memory) = def.memory {
		contract = contract.import()
			.module("env").field("memory")
			.external().memory(memory.min_pages, Some(memory.max_pages))
			.build();
	}

	// Import supervisor functions. They start with idx 0.
	for func in def.imported_functions {
		let sig = parity_wasm::builder::signature()
			.with_params(func.params)
			.with_return_type(func.return_type)
			.build_sig();
		let sig = contract.push_signature(sig);
		contract = contract.import()
			.module("seal0")
			.field(func.name)
			.with_external(parity_wasm::elements::External::Function(sig))
			.build();
	}

	// Initialize memory
	for data in def.data_segments {
		contract = contract.data()
			.offset(Instruction::I32Const(data.offset as i32))
			.value(data.value)
			.build()
	}

	let code = contract.build().to_bytes().unwrap();
	let hash = T::Hashing::hash(&code);
	WasmModule {
		code,
		hash
	}
}

fn body(instructions: Vec<Instruction>) -> FuncBody {
	FuncBody::new(Vec::new(), Instructions::new(instructions))
}

fn body_repeated(repetitions: u32, instructions: &[Instruction]) -> FuncBody {
	let instructions = Instructions::new(
		instructions
			.iter()
			.cycle()
			.take(instructions.len() * usize::try_from(repetitions).unwrap())
			.cloned()
			.chain(sp_std::iter::once(Instruction::End))
			.collect()
	);
	FuncBody::new(Vec::new(), instructions)
}

fn body_counted(repetitions: u32, mut instructions: Vec<CountedInstruction>) -> FuncBody {
	// We need to iterate over indices because we cannot cycle over mutable references
	let body = (0..instructions.len())
		.cycle()
		.take(instructions.len() * usize::try_from(repetitions).unwrap())
		.map(|idx| {
			match &mut instructions[idx] {
				CountedInstruction::Counter(offset, increment_by) => {
					let current = *offset;
					*offset += *increment_by;
					Instruction::I32Const(current as i32)
				},
				CountedInstruction::Regular(instruction) => instruction.clone(),
			}
		})
		.chain(sp_std::iter::once(Instruction::End))
		.collect();
	FuncBody::new(Vec::new(), Instructions::new(body))
}

fn dummy_code<T: Trait>() -> WasmModule<T> {
	create_code::<T>(Default::default())
}

fn sized_code<T: Trait>(target_bytes: u32) -> WasmModule<T> {
	use parity_wasm::elements::Instruction::{If, I32Const, Return, End};
	// Base size of a contract is 47 bytes and each expansion adds 6 bytes.
	// We do one expansion less to account for the code section and function body
	// size fields inside the binary wasm module representation which are leb128 encoded
	// and therefore grow in size when the contract grows. We are not allowed to overshoot
	// because of the maximum code size that is enforced by `put_code`.
	let expansions = (target_bytes.saturating_sub(47) / 6).saturating_sub(1);
	const EXPANSION: [Instruction; 4] = [
		I32Const(0),
		If(BlockType::NoResult),
		Return,
		End,
	];
	create_code::<T>(ModuleDefinition {
		call_body: Some(body_repeated(expansions, &EXPANSION)),
		.. Default::default()
	})
}

fn getter_code<T: Trait>(getter_name: &'static str, repeat: u32) -> WasmModule<T> {
	let pages = max_pages::<T>();
	create_code::<T>(ModuleDefinition {
		memory: Some(ImportedMemory::max::<T>()),
		imported_functions: vec![ImportedFunction {
			name: getter_name,
			params: vec![ValueType::I32, ValueType::I32],
			return_type: None,
		}],
		// Write the output buffer size. The output size will be overwritten by the
		// supervisor with the real size when calling the getter. Since this size does not
		// change between calls it suffices to start with an initial value and then just
		// leave as whatever value was written there.
		data_segments: vec![DataSegment {
			offset: 0,
			value: (pages * 64 * 1024 - 4).to_le_bytes().to_vec(),
		}],
		call_body: Some(body_repeated(repeat, &[
			Instruction::I32Const(4), // ptr where to store output
			Instruction::I32Const(0), // ptr to length
			Instruction::Call(0), // call the imported function
		])),
		.. Default::default()
	})
}

fn hasher_code<T: Trait>(name: &'static str, repeat: u32, data_size: u32) -> WasmModule<T> {
	create_code::<T>(ModuleDefinition {
		memory: Some(ImportedMemory::max::<T>()),
		imported_functions: vec![ImportedFunction {
			name: name,
			params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
			return_type: None,
		}],
		call_body: Some(body_repeated(repeat, &[
			Instruction::I32Const(0), // input_ptr
			Instruction::I32Const(data_size as i32), // input_len
			Instruction::I32Const(0), // output_ptr
			Instruction::Call(0),
		])),
		.. Default::default()
	})
}

enum Endow {
	Max,
	CollectRent,
}

fn instantiate_contract_from_account<T: Trait>(
	caller: T::AccountId,
	module: WasmModule<T>,
	data: Vec<u8>,
	endowment: Endow,
) -> Result<Contract<T>, &'static str>
{
	let (storage_size, endowment) = match endowment {
		Endow::CollectRent => {
			// storage_size cannot be zero because otherwise a contract that is just above
			// the subsistence threshold does not pay rent given a large enough subsistence
			// threshold. But we need rent payments to occur in order to benchmark for worst cases.
			let storage_size = Config::<T>::subsistence_threshold_uncached()
				.checked_div(&T::RentDepositOffset::get())
				.unwrap_or_else(Zero::zero);

			// Endowment should be large but not as large to inhibit rent payments.
			let endowment = T::RentDepositOffset::get()
				.saturating_mul(storage_size + T::StorageSizeOffset::get().into())
				.saturating_sub(1.into());

			(storage_size, endowment)
		},
		Endow::Max => (0.into(), max_endowment::<T>()),
	};
	T::Currency::make_free_balance_be(&caller, funding::<T>());
	let addr = T::DetermineContractAddress::contract_address_for(&module.hash, &data, &caller);
	init_block_number::<T>();
	Contracts::<T>::put_code_raw(module.code)?;
	Contracts::<T>::instantiate(
		RawOrigin::Signed(caller.clone()).into(),
		endowment,
		Weight::max_value(),
		module.hash,
		data,
	)?;
	let mut contract = get_alive::<T>(&addr)?;
	contract.storage_size = storage_size.saturated_into::<u32>();
	ContractInfoOf::<T>::insert(&addr, ContractInfo::Alive(contract));
	Ok(Contract {
		caller,
		account_id: addr.clone(),
		addr: T::Lookup::unlookup(addr),
		endowment,
		code_hash: module.hash.clone(),
	})
}

fn instantiate_contract_from_index<T: Trait>(
	index: u32,
	module: WasmModule<T>,
	data: Vec<u8>,
	endowment: Endow,
) -> Result<Contract<T>, &'static str> {
	instantiate_contract_from_account(account("instantiator", index, 0), module, data, endowment)
}

fn instantiate_contract<T: Trait>(
	module: WasmModule<T>,
	data: Vec<u8>,
	endowment: Endow,
) -> Result<Contract<T>, &'static str> {
	instantiate_contract_from_index(0, module, data, endowment)
}

fn store_items<T: Trait>(
	account: &T::AccountId,
	items: &Vec<(StorageKey, Vec<u8>)>
) -> Result<(), &'static str> {
	let info = get_alive::<T>(account)?;
	for item in items {
		crate::storage::write_contract_storage::<T>(
			account,
			&info.trie_id,
			&item.0,
			Some(item.1.clone()),
		)
		.map_err(|_| "Failed to write storage to restoration dest")?;
	}
	Ok(())
}

fn create_storage<T: Trait>(
	stor_num: u32,
	stor_size: u32
) -> Result<Vec<(StorageKey, Vec<u8>)>, &'static str> {
	(0..stor_num).map(|i| {
		let hash = T::Hashing::hash_of(&i)
			.as_ref()
			.try_into()
			.map_err(|_| "Hash too big for storage key")?;
		Ok((hash, vec![42u8; stor_size as usize]))
	}).collect::<Result<Vec<_>, &'static str>>()
}

fn create_tombstone<T: Trait>(stor_num: u32, stor_size: u32) -> Result<Tombstone<T>, &'static str> {
	let contract = instantiate_contract::<T>(dummy_code(), vec![], Endow::CollectRent)?;
	let storage_items = create_storage::<T>(stor_num, stor_size)?;
	store_items::<T>(&contract.account_id, &storage_items)?;
	System::<T>::set_block_number(
		eviction_at::<T>(&contract.account_id)? + T::SignedClaimHandicap::get() + 5.into()
	);
	crate::rent::collect_rent::<T>(&contract.account_id);
	ensure_tombstone::<T>(&contract.account_id)?;

	Ok(Tombstone {
		contract,
		storage: storage_items,
	})
}

fn get_alive<T: Trait>(addr: &T::AccountId) -> Result<AliveContractInfo<T>, &'static str> {
	ContractInfoOf::<T>::get(&addr).and_then(|c| c.get_alive())
		.ok_or("Expected contract to be alive at this point.")
}

fn ensure_alive<T: Trait>(addr: &T::AccountId) -> Result<(), &'static str> {
	get_alive::<T>(addr).map(|_| ())
}

fn ensure_tombstone<T: Trait>(addr: &T::AccountId) -> Result<(), &'static str> {
	ContractInfoOf::<T>::get(&addr).and_then(|c| c.get_tombstone())
		.ok_or("Expected contract to be a tombstone at this point.")
		.map(|_| ())
}

fn max_pages<T: Trait>() -> u32 {
	Contracts::<T>::current_schedule().max_memory_pages
}

fn funding<T: Trait>() -> BalanceOf<T> {
	BalanceOf::<T>::max_value() / 2.into()
}

fn max_endowment<T: Trait>() -> BalanceOf<T> {
	funding::<T>().saturating_sub(T::Currency::minimum_balance())
}

fn create_funded_user<T: Trait>(string: &'static str, n: u32) -> T::AccountId {
	let user = account(string, n, 0);
	T::Currency::make_free_balance_be(&user, funding::<T>());
	user
}

fn eviction_at<T: Trait>(addr: &T::AccountId) -> Result<T::BlockNumber, &'static str> {
	match crate::rent::compute_rent_projection::<T>(addr).map_err(|_| "Invalid acc for rent")? {
		RentProjection::EvictionAt(at) => Ok(at),
		_ => Err("Account does not pay rent.")?,
	}
}

/// Set the block number to one.
///
/// The default block number is zero. The benchmarking system bumps the block number
/// to one for the benchmarking closure when it is set to zero. In order to prevent this
/// undesired implicit bump (which messes with rent collection), wo do the bump ourselfs
/// in the setup closure so that both the instantiate and subsequent call are run with the
/// same block number.
fn init_block_number<T: Trait>() {
	System::<T>::set_block_number(1.into());
}

benchmarks! {
	_ {
	}

	// This extrinsic is pretty much constant as it is only a simple setter.
	update_schedule {
		let schedule = Schedule {
			version: 1,
			.. Default::default()
		};
	}: _(RawOrigin::Root, schedule)

	// This constructs a contract that is maximal expensive to instrument.
	// It creates a maximum number of metering blocks per byte.
	// `n`: Size of the code in kilobytes.
	put_code {
		let n in 0 .. Contracts::<T>::current_schedule().max_code_size / 1024;
		let caller = create_funded_user::<T>("caller", 0);
		let module = sized_code::<T>(n * 1024);
		let origin = RawOrigin::Signed(caller);
	}: _(origin, module.code)

	// Instantiate uses a dummy contract constructor to measure the overhead of the instantiate.
	// The size of the input data influences the runtime because it is hashed in order to determine
	// the contract address. 
	// `n`: Size of the data passed to constructor in kilobytes.
	instantiate {
		let n in 0 .. max_pages::<T>() * 64;
		let data = vec![42u8; (n * 1024) as usize];
		let endowment = Config::<T>::subsistence_threshold_uncached();
		let caller = create_funded_user::<T>("caller", 0);
		let WasmModule { code, hash } = dummy_code::<T>();
		let origin = RawOrigin::Signed(caller.clone());
		let addr = T::DetermineContractAddress::contract_address_for(&hash, &data, &caller);
		Contracts::<T>::put_code_raw(code)?;
	}: _(origin, endowment, Weight::max_value(), hash, data)
	verify {
		// endowment was removed from the caller
		assert_eq!(T::Currency::free_balance(&caller), funding::<T>() - endowment);
		// contract has the full endowment because no rent collection happended
		assert_eq!(T::Currency::free_balance(&addr), endowment);
		// instantiate should leave a alive contract
		ensure_alive::<T>(&addr)?;
	}

	// We just call a dummy contract to measure to overhead of the call extrinsic.
	// The size of the data has no influence on the costs of this extrinsic as long as the contract
	// won't call `seal_input` in its constructor to copy the data to contract memory.
	// The dummy contract used here does not do this. The costs for the data copy is billed as
	// part of `seal_input`.
	call {
		let data = vec![42u8; 1024];
		let instance = instantiate_contract::<T>(dummy_code(), vec![], Endow::CollectRent)?;
		let value = T::Currency::minimum_balance() * 100.into();
		let origin = RawOrigin::Signed(instance.caller.clone());

		// trigger rent collection for worst case performance of call
		System::<T>::set_block_number(eviction_at::<T>(&instance.account_id)? - 5.into());
		let before = T::Currency::free_balance(&instance.account_id);
	}: _(origin, instance.addr, value, Weight::max_value(), data)
	verify {
		// endowment and value transfered via call should be removed from the caller
		assert_eq!(
			T::Currency::free_balance(&instance.caller),
			funding::<T>() - instance.endowment - value,
		);
		// rent should have lowered the amount of balance of the contract
		assert!(T::Currency::free_balance(&instance.account_id) < before + value);
		// but it should not have been evicted by the rent collection
		ensure_alive::<T>(&instance.account_id)?;
	}

	// We benchmark the costs for sucessfully evicting an empty contract.
	// The actual costs are depending on how many storage items the evicted contract
	// does have. However, those costs are not to be payed by the sender but
	// will be distributed over multiple blocks using a scheduler. Otherwise there is
	// no incentive to remove large contracts when the removal is more expensive than
	// the reward for removing them.
	claim_surcharge {
		let instance = instantiate_contract::<T>(dummy_code(), vec![], Endow::CollectRent)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
		let account_id = instance.account_id.clone();

		// instantiate should leave us with an alive contract
		ensure_alive::<T>(&instance.account_id)?;

		// generate enough rent so that the contract is evicted
		System::<T>::set_block_number(
			eviction_at::<T>(&instance.account_id)? + T::SignedClaimHandicap::get() + 5.into()
		);
	}: _(origin, account_id, None)
	verify {
		// the claim surcharge should have evicted the contract
		ensure_tombstone::<T>(&instance.account_id)?;

		// the caller should get the reward for being a good snitch
		assert_eq!(
			T::Currency::free_balance(&instance.caller),
			funding::<T>() - instance.endowment + <T as Trait>::SurchargeReward::get(),
		);
	}

	seal_caller {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_caller", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_address {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_address", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_gas_left {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_gas_left", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_balance {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_balance", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_value_transferred {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_value_transferred", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_minimum_balance {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_minimum_balance", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_tombstone_deposit {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_tombstone_deposit", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_rent_allowance {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_rent_allowance", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_block_number {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_block_number", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_now {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(getter_code(
			"seal_now", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_weight_to_fee {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let pages = max_pages::<T>();
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_weight_to_fee",
				params: vec![ValueType::I64, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![DataSegment {
				offset: 0,
				value: (pages * 64 * 1024 - 4).to_le_bytes().to_vec(),
			}],
			call_body: Some(body_repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I64Const(500_000),
				Instruction::I32Const(4),
				Instruction::I32Const(0),
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_gas {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let code = create_code(ModuleDefinition {
			imported_functions: vec![ImportedFunction {
				name: "gas",
				params: vec![ValueType::I32],
				return_type: None,
			}],
			call_body: Some(body_repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(42),
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());

	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// We cannot call seal_input multiple times. Therefore our weight determination is not
	// as precise as with other APIs. Because this function can only be called once per
	// contract it cannot be used for Dos.
	seal_input {
		let r in 0 .. 1;
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_input",
				params: vec![ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: 0u32.to_le_bytes().to_vec(),
				},
			],
			call_body: Some(body_repeated(r, &[
				Instruction::I32Const(4), // ptr where to store output
				Instruction::I32Const(0), // ptr to length
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_input_per_kb {
		let n in 0 .. max_pages::<T>() * 64;
		let pages = max_pages::<T>();
		let buffer_size = pages * 64 * 1024 - 4;
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_input",
				params: vec![ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: buffer_size.to_le_bytes().to_vec(),
				},
			],
			call_body: Some(body(vec![
				Instruction::I32Const(4), // ptr where to store output
				Instruction::I32Const(0), // ptr to length
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let data = vec![42u8; (n * 1024).min(buffer_size) as usize];
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), data)

	// The same argument as for `seal_input` is true here.
	seal_return {
		let r in 0 .. 1;
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_return",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			call_body: Some(body_repeated(r, &[
				Instruction::I32Const(0), // flags
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const(0), // data_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_return_per_kb {
		let n in 0 .. max_pages::<T>() * 64;
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_return",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			call_body: Some(body(vec![
				Instruction::I32Const(0), // flags
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const((n * 1024) as i32), // data_len
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// The same argument as for `seal_input` is true here.
	seal_terminate {
		let r in 0 .. 1;
		let beneficiary = account::<T::AccountId>("beneficiary", 0, 0);
		let beneficiary_bytes = beneficiary.encode();
		let beneficiary_len = beneficiary_bytes.len();
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_terminate",
				params: vec![ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: beneficiary_bytes,
				},
			],
			call_body: Some(body_repeated(r, &[
				Instruction::I32Const(0), // beneficiary_ptr
				Instruction::I32Const(beneficiary_len as i32), // beneficiary_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
		assert_eq!(T::Currency::total_balance(&beneficiary), 0.into());
		assert_eq!(T::Currency::total_balance(&instance.account_id), max_endowment::<T>());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])
	verify {
		if r > 0 {
			assert_eq!(T::Currency::total_balance(&instance.account_id), 0.into());
			assert_eq!(T::Currency::total_balance(&beneficiary), max_endowment::<T>());
		}
	}

	seal_restore_to {
		let r in 0 .. 1;

		// Restore just moves the trie id from origin to destination and therefore
		// does not depend on the size of the destination contract. However, to not
		// trigger any edge case we won't use an empty contract as destination.
		let tombstone = create_tombstone::<T>(10, T::MaxValueSize::get())?;

		let dest = tombstone.contract.account_id.encode();
		let dest_len = dest.len();
		let code_hash = tombstone.contract.code_hash.encode();
		let code_hash_len = code_hash.len();
		let rent_allowance = BalanceOf::<T>::max_value().encode();
		let rent_allowance_len = rent_allowance.len();

		let dest_offset = 0;
		let code_hash_offset = dest_offset + dest_len;
		let rent_allowance_offset = code_hash_offset + code_hash_len;

		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_restore_to",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: dest_offset as u32,
					value: dest,
				},
				DataSegment {
					offset: code_hash_offset as u32,
					value: code_hash,
				},
				DataSegment {
					offset: rent_allowance_offset as u32,
					value: rent_allowance,
				},
			],
			call_body: Some(body_repeated(r, &[
				Instruction::I32Const(dest_offset as i32),
				Instruction::I32Const(dest_len as i32),
				Instruction::I32Const(code_hash_offset as i32),
				Instruction::I32Const(code_hash_len as i32),
				Instruction::I32Const(rent_allowance_offset as i32),
				Instruction::I32Const(rent_allowance_len as i32),
				Instruction::I32Const(0), // delta_ptr
				Instruction::I32Const(0), // delta_count
				Instruction::Call(0),
			])),
			.. Default::default()
		});

		let instance = instantiate_contract_from_account::<T>(
			account("origin", 0, 0), code, vec![], Endow::Max
		)?;
		store_items::<T>(&instance.account_id, &tombstone.storage)?;
		System::<T>::set_block_number(System::<T>::block_number() + 1.into());

		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])
	verify {
		if r > 0 {
			ensure_alive::<T>(&tombstone.contract.account_id)?;
		}
	}

	seal_restore_to_per_delta {
		let d in 0 .. API_BENCHMARK_BATCHES;
		let tombstone = create_tombstone::<T>(0, 0)?;
		let delta = create_storage::<T>(d * API_BENCHMARK_BATCH_SIZE, T::MaxValueSize::get())?;

		let dest = tombstone.contract.account_id.encode();
		let dest_len = dest.len();
		let code_hash = tombstone.contract.code_hash.encode();
		let code_hash_len = code_hash.len();
		let rent_allowance = BalanceOf::<T>::max_value().encode();
		let rent_allowance_len = rent_allowance.len();
		let delta_keys = delta.iter().flat_map(|(key, _)| key).cloned().collect::<Vec<_>>();

		let dest_offset = 0;
		let code_hash_offset = dest_offset + dest_len;
		let rent_allowance_offset = code_hash_offset + code_hash_len;
		let delta_keys_offset = rent_allowance_offset + rent_allowance_len;

		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_restore_to",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: dest_offset as u32,
					value: dest,
				},
				DataSegment {
					offset: code_hash_offset as u32,
					value: code_hash,
				},
				DataSegment {
					offset: rent_allowance_offset as u32,
					value: rent_allowance,
				},
				DataSegment {
					offset: delta_keys_offset as u32,
					value: delta_keys,
				},
			],
			call_body: Some(body(vec![
				Instruction::I32Const(dest_offset as i32),
				Instruction::I32Const(dest_len as i32),
				Instruction::I32Const(code_hash_offset as i32),
				Instruction::I32Const(code_hash_len as i32),
				Instruction::I32Const(rent_allowance_offset as i32),
				Instruction::I32Const(rent_allowance_len as i32),
				Instruction::I32Const(delta_keys_offset as i32), // delta_ptr
				Instruction::I32Const(delta.len() as i32), // delta_count
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});

		let instance = instantiate_contract_from_account::<T>(
			account("origin", 0, 0), code, vec![], Endow::Max
		)?;
		store_items::<T>(&instance.account_id, &tombstone.storage)?;
		store_items::<T>(&instance.account_id, &delta)?;
		System::<T>::set_block_number(System::<T>::block_number() + 1.into());

		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])
	verify {
		ensure_alive::<T>(&tombstone.contract.account_id)?;
	}

	// We benchmark only for the maximum subject length. We assume that this is some lowish
	// number (< 1 KB). Therefore we are not overcharging too much in case a smaller subject is
	// used.
	seal_random {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let pages = max_pages::<T>();
		let subject_len = Contracts::<T>::current_schedule().max_subject_len;
		assert!(subject_len < 1024);
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_random",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: (pages * 64 * 1024 - subject_len - 4).to_le_bytes().to_vec(),
				},
			],
			call_body: Some(body_repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(4), // subject_ptr
				Instruction::I32Const(subject_len as i32), // subject_len
				Instruction::I32Const((subject_len + 4) as i32), // out_ptr
				Instruction::I32Const(0),	// out_len_ptr
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// Overhead of calling the function without any topic.
	// We benchmark for the worst case (largest event).
	seal_deposit_event {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_deposit_event",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			call_body: Some(body_repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(0), // topics_ptr
				Instruction::I32Const(0), // topics_len
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const(0), // data_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// Benchmark the overhead that topics generate.
	// `t`: Number of topics
	// `n`: Size of event payload in kb
	seal_deposit_event_per_topic_and_kb {
		let t in 0 .. Contracts::<T>::current_schedule().max_event_topics;
		let n in 0 .. T::MaxValueSize::get() / 1024;
		let mut topics = (0..API_BENCHMARK_BATCH_SIZE)
			.map(|n| (n * t..n * t + t).map(|i| T::Hashing::hash_of(&i)).collect::<Vec<_>>().encode())
			.peekable();
		let topics_len = topics.peek().map(|i| i.len()).unwrap_or(0);
		let topics = topics.flatten().collect();
		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_deposit_event",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: topics,
				},
			],
			call_body: Some(body_counted(API_BENCHMARK_BATCH_SIZE, vec![
				Counter(0, topics_len as u32), // topics_ptr
				Regular(Instruction::I32Const(topics_len as i32)), // topics_len
				Regular(Instruction::I32Const(0)), // data_ptr
				Regular(Instruction::I32Const((n * 1024) as i32)), // data_len
				Regular(Instruction::Call(0)),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_set_rent_allowance {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let allowance = funding::<T>().encode();
		let allowance_len = allowance.len();
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory { min_pages: 1, max_pages: 1 }),
			imported_functions: vec![ImportedFunction {
				name: "seal_set_rent_allowance",
				params: vec![ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: allowance,
				},
			],
			call_body: Some(body_repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(0), // value_ptr
				Instruction::I32Const(allowance_len as i32), // value_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	// The contract is a bit more complex because I needs to use different keys in order
	// to generate unique storage accesses. However, it is still dominated by the storage
	// accesses.
	seal_set_storage {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let keys = (0 .. r * API_BENCHMARK_BATCH_SIZE)
			.flat_map(|n| T::Hashing::hash_of(&n).as_ref().to_vec())
			.collect::<Vec<_>>();
		let key_len = sp_std::mem::size_of::<<T::Hashing as sp_runtime::traits::Hash>::Output>();
		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_set_storage",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: keys,
				},
			],
			call_body: Some(body_counted(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(0, key_len as u32), // key_ptr
				Regular(Instruction::I32Const(0)), // value_ptr
				Regular(Instruction::I32Const(0)), // value_len
				Regular(Instruction::Call(0)),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_set_storage_per_kb {
		let n in 0 .. T::MaxValueSize::get() / 1024;
		let key = T::Hashing::hash_of(&1u32).as_ref().to_vec();
		let key_len = key.len();
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_set_storage",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: key,
				},
			],
			call_body: Some(body_repeated(API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(0), // key_ptr
				Instruction::I32Const(0), // value_ptr
				Instruction::I32Const((n * 1024) as i32), // value_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// Similar to seal_set_storage. However, we store all the keys that we are about to
	// delete beforehand in order to prevent any optimizations that could occur when
	// deleting a non existing key.
	seal_clear_storage {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let keys = (0 .. r * API_BENCHMARK_BATCH_SIZE)
			.map(|n| T::Hashing::hash_of(&n).as_ref().to_vec())
			.collect::<Vec<_>>();
		let key_bytes = keys.iter().flatten().cloned().collect::<Vec<_>>();
		let key_len = sp_std::mem::size_of::<<T::Hashing as sp_runtime::traits::Hash>::Output>();
		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_clear_storage",
				params: vec![ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: key_bytes,
				},
			],
			call_body: Some(body_counted(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(0, key_len as u32),
				Regular(Instruction::Call(0)),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let trie_id = get_alive::<T>(&instance.account_id)?.trie_id;
		for key in keys {
			crate::storage::write_contract_storage::<T>(
				&instance.account_id,
				&trie_id,
				key.as_slice().try_into().map_err(|e| "Key has wrong length")?,
				Some(vec![42; T::MaxValueSize::get() as usize])
			)
			.map_err(|_| "Failed to write to storage during setup.")?;
		}
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// We make sure that all storage accesses are to unique keys.
	seal_get_storage {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let keys = (0 .. r * API_BENCHMARK_BATCH_SIZE)
			.map(|n| T::Hashing::hash_of(&n).as_ref().to_vec())
			.collect::<Vec<_>>();
		let key_len = sp_std::mem::size_of::<<T::Hashing as sp_runtime::traits::Hash>::Output>();
		let key_bytes = keys.iter().flatten().cloned().collect::<Vec<_>>();
		let key_bytes_len = key_bytes.len();
		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_get_storage",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: key_bytes,
				},
			],
			call_body: Some(body_counted(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(0, key_len as u32), // key_ptr
				Regular(Instruction::I32Const((key_bytes_len + 4) as i32)), // out_ptr
				Regular(Instruction::I32Const(key_bytes_len as i32)), // out_len_ptr
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let trie_id = get_alive::<T>(&instance.account_id)?.trie_id;
		for key in keys {
			crate::storage::write_contract_storage::<T>(
				&instance.account_id,
				&trie_id,
				key.as_slice().try_into().map_err(|e| "Key has wrong length")?,
				Some(vec![])
			)
			.map_err(|_| "Failed to write to storage during setup.")?;
		}
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_get_storage_per_kb {
		let n in 0 .. T::MaxValueSize::get() / 1024;
		let key = T::Hashing::hash_of(&1u32).as_ref().to_vec();
		let key_len = key.len();
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_get_storage",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: key.clone(),
				},
				DataSegment {
					offset: key_len as u32,
					value: T::MaxValueSize::get().to_le_bytes().into(),
				},
			],
			call_body: Some(body_repeated(API_BENCHMARK_BATCH_SIZE, &[
				// call at key_ptr
				Instruction::I32Const(0), // key_ptr
				Instruction::I32Const((key_len + 4) as i32), // out_ptr
				Instruction::I32Const(key_len as i32), // out_len_ptr
				Instruction::Call(0),
				Instruction::Drop,
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let trie_id = get_alive::<T>(&instance.account_id)?.trie_id;
		crate::storage::write_contract_storage::<T>(
			&instance.account_id,
			&trie_id,
			key.as_slice().try_into().map_err(|e| "Key has wrong length")?,
			Some(vec![42u8; (n * 1024) as usize])
		)
		.map_err(|_| "Failed to write to storage during setup.")?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// We transfer to unique accounts.
	seal_transfer {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let accounts = (0..r * API_BENCHMARK_BATCH_SIZE)
			.map(|i| account::<T::AccountId>("receiver", i, 0))
			.collect::<Vec<_>>();
		let account_len = accounts.get(0).map(|i| i.encode().len()).unwrap_or(0);
		let account_bytes = accounts.iter().flat_map(|x| x.encode()).collect();
		let value = Config::<T>::subsistence_threshold_uncached();
		assert!(value > 0.into());
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_transfer",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: value_bytes,
				},
				DataSegment {
					offset: value_len as u32,
					value: account_bytes,
				},
			],
			call_body: Some(body_counted(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(value_len as u32, account_len as u32), // account_ptr
				Regular(Instruction::I32Const(account_len as i32)), // account_len
				Regular(Instruction::I32Const(0)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
		for account in &accounts {
			assert_eq!(T::Currency::total_balance(account), 0.into());
		}
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])
	verify {
		for account in &accounts {
			assert_eq!(T::Currency::total_balance(account), value);
		}
	}

	// We call unique accounts.
	seal_call {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let dummy_code = dummy_code::<T>();
		let callees = (0..r * API_BENCHMARK_BATCH_SIZE)
			.map(|i| instantiate_contract_from_index(i + 1, dummy_code.clone(), vec![], Endow::Max))
			.collect::<Result<Vec<_>, _>>()?;
		let callee_len = callees.get(0).map(|i| i.account_id.encode().len()).unwrap_or(0);
		let callee_bytes = callees.iter().flat_map(|x| x.account_id.encode()).collect();
		let value: BalanceOf<T> = 0.into();
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_call",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I64,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: value_bytes,
				},
				DataSegment {
					offset: value_len as u32,
					value: callee_bytes,
				},
			],
			call_body: Some(body_counted(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(value_len as u32, callee_len as u32), // callee_ptr
				Regular(Instruction::I32Const(callee_len as i32)), // callee_len
				Regular(Instruction::I64Const(0)), // gas
				Regular(Instruction::I32Const(0)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Regular(Instruction::I32Const(0)), // input_data_ptr
				Regular(Instruction::I32Const(0)), // input_data_len
				Regular(Instruction::I32Const(u32::max_value() as i32)), // output_ptr
				Regular(Instruction::I32Const(0)), // output_len_ptr
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	seal_call_per_transfer_input_output_kb {
		let t in 0 .. 1;
		let i in 0 .. max_pages::<T>() * 64;
		let o in 0 .. (max_pages::<T>() - 1) * 64;
		let callee_code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_return",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: None,
			}],
			call_body: Some(body(vec![
				Instruction::I32Const(0), // flags
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const((o * 1024) as i32), // data_len
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});
		let callees = (0..API_BENCHMARK_BATCH_SIZE)
			.map(|i| instantiate_contract_from_index(i + 1, callee_code.clone(), vec![], Endow::Max))
			.collect::<Result<Vec<_>, _>>()?;
		let callee_len = callees.get(0).map(|i| i.account_id.encode().len()).unwrap_or(0);
		let callee_bytes = callees.iter().flat_map(|x| x.account_id.encode()).collect::<Vec<_>>();
		let callees_len = callee_bytes.len();
		let value: BalanceOf<T> = t.into();
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_call",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I64,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: value_bytes,
				},
				DataSegment {
					offset: value_len as u32,
					value: callee_bytes,
				},
				DataSegment {
					offset: (value_len + callees_len) as u32,
					value: (o * 1024).to_le_bytes().into(),
				},
			],
			call_body: Some(body_counted(API_BENCHMARK_BATCH_SIZE, vec![
				Counter(value_len as u32, callee_len as u32), // callee_ptr
				Regular(Instruction::I32Const(callee_len as i32)), // callee_len
				Regular(Instruction::I64Const(0)), // gas
				Regular(Instruction::I32Const(0)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Regular(Instruction::I32Const(0)), // input_data_ptr
				Regular(Instruction::I32Const((i * 1024) as i32)), // input_data_len
				Regular(Instruction::I32Const((value_len + callees_len + 4) as i32)), // output_ptr
				Regular(Instruction::I32Const((value_len + callees_len) as i32)), // output_len_ptr
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// We assume that every instantiate sends at least the subsistence amount.
	seal_instantiate {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let hashes = (0..r * API_BENCHMARK_BATCH_SIZE)
			.map(|i| {
				let code = create_code::<T>(ModuleDefinition {
					call_body: Some(body(vec![
						Instruction::I32Const(i as i32),
						Instruction::Drop,
						Instruction::End,
					])),
					.. Default::default()
				});
				Contracts::<T>::put_code_raw(code.code)?;
				Ok(code.hash)
			})
			.collect::<Result<Vec<_>, &'static str>>()?;
		let hash_len = hashes.get(0).map(|x| x.encode().len()).unwrap_or(0);
		let hashes_bytes = hashes.iter().flat_map(|x| x.encode()).collect::<Vec<_>>();
		let hashes_len = hashes_bytes.len();
		let value = Config::<T>::subsistence_threshold_uncached();
		assert!(value > 0.into());
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		let addr_len = sp_std::mem::size_of::<T::AccountId>();

		// offsets where to place static data in contract memory
		let value_offset = 0;
		let hashes_offset = value_offset + value_len;
		let addr_len_offset = hashes_offset + hashes_len;
		let addr_offset = addr_len_offset + addr_len;

		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_instantiate",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I64,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32
				],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: value_offset as u32,
					value: value_bytes,
				},
				DataSegment {
					offset: hashes_offset as u32,
					value: hashes_bytes,
				},
				DataSegment {
					offset: addr_len_offset as u32,
					value: addr_len.to_le_bytes().into(),
				},
			],
			call_body: Some(body_counted(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(hashes_offset as u32, hash_len as u32), // code_hash_ptr
				Regular(Instruction::I32Const(hash_len as i32)), // code_hash_len
				Regular(Instruction::I64Const(0)), // gas
				Regular(Instruction::I32Const(value_offset as i32)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Regular(Instruction::I32Const(0)), // input_data_ptr
				Regular(Instruction::I32Const(0)), // input_data_len
				Regular(Instruction::I32Const(addr_offset as i32)), // address_ptr
				Regular(Instruction::I32Const(addr_len_offset as i32)), // address_len_ptr
				Regular(Instruction::I32Const(u32::max_value() as i32)), // output_ptr
				Regular(Instruction::I32Const(0)), // output_len_ptr
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
		let addresses = hashes
			.iter()
			.map(|hash| T::DetermineContractAddress::contract_address_for(
				hash, &[], &instance.account_id
			))
			.collect::<Vec<_>>();

		for addr in &addresses {
			if let Some(_) = ContractInfoOf::<T>::get(&addr) {
				return Err("Expected that contract does not exist at this point.");
			}
		}
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])
	verify {
		for addr in &addresses {
			ensure_alive::<T>(addr)?;
		}
	}

	seal_instantiate_per_input_output_kb {
		let i in 0 .. (max_pages::<T>() - 1) * 64;
		let o in 0 .. (max_pages::<T>() - 1) * 64;
		let callee_code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_return",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: None,
			}],
			deploy_body: Some(body(vec![
				Instruction::I32Const(0), // flags
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const((o * 1024) as i32), // data_len
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});
		let hash = callee_code.hash.clone();
		let hash_bytes = callee_code.hash.encode();
		let hash_len = hash_bytes.len();
		Contracts::<T>::put_code_raw(callee_code.code)?;
		let inputs = (0..API_BENCHMARK_BATCH_SIZE).map(|x| x.encode()).collect::<Vec<_>>();
		let input_len = inputs.get(0).map(|x| x.len()).unwrap_or(0);
		let input_bytes = inputs.iter().cloned().flatten().collect::<Vec<_>>();
		let inputs_len = input_bytes.len();
		let value = Config::<T>::subsistence_threshold_uncached();
		assert!(value > 0.into());
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		let addr_len = sp_std::mem::size_of::<T::AccountId>();

		// offsets where to place static data in contract memory
		let input_offset = 0;
		let value_offset = inputs_len;
		let hash_offset = value_offset + value_len;
		let addr_len_offset = hash_offset + hash_len;
		let output_len_offset = addr_len_offset + 4;
		let output_offset = output_len_offset + 4;

		use CountedInstruction::{Counter, Regular};
		let code = create_code::<T>(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_instantiate",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I64,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32
				],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: input_offset as u32,
					value: input_bytes,
				},
				DataSegment {
					offset: value_offset as u32,
					value: value_bytes,
				},
				DataSegment {
					offset: hash_offset as u32,
					value: hash_bytes,
				},
				DataSegment {
					offset: addr_len_offset as u32,
					value: (addr_len as u32).to_le_bytes().into(),
				},
				DataSegment {
					offset: output_len_offset as u32,
					value: (o * 1024).to_le_bytes().into(),
				},
			],
			call_body: Some(body_counted(API_BENCHMARK_BATCH_SIZE, vec![
				Regular(Instruction::I32Const(hash_offset as i32)), // code_hash_ptr
				Regular(Instruction::I32Const(hash_len as i32)), // code_hash_len
				Regular(Instruction::I64Const(0)), // gas
				Regular(Instruction::I32Const(value_offset as i32)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Counter(input_offset as u32, input_len as u32), // input_data_ptr
				Regular(Instruction::I32Const((i * 1024).max(input_len as u32) as i32)), // input_data_len
				Regular(Instruction::I32Const((addr_len_offset + addr_len) as i32)), // address_ptr
				Regular(Instruction::I32Const(addr_len_offset as i32)), // address_len_ptr
				Regular(Instruction::I32Const(output_offset as i32)), // output_ptr
				Regular(Instruction::I32Const(output_len_offset as i32)), // output_len_ptr
				Regular(Instruction::Call(0)),
				Regular(Instruction::I32Eqz),
				Regular(Instruction::If(BlockType::NoResult)),
				Regular(Instruction::Nop),
				Regular(Instruction::Else),
				Regular(Instruction::Unreachable),
				Regular(Instruction::End),
			])),
			.. Default::default()
		});
		let instance = instantiate_contract::<T>(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	seal_hash_sha2_256 {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(hasher_code(
			"seal_hash_sha2_256", r * API_BENCHMARK_BATCH_SIZE, 0,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// `n`: Input to hash in kilobytes
	seal_hash_sha2_256_per_kb {
		let n in 0 .. max_pages::<T>() * 64;
		let instance = instantiate_contract::<T>(hasher_code(
			"seal_hash_sha2_256", API_BENCHMARK_BATCH_SIZE, n * 1024,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	seal_hash_keccak_256 {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(hasher_code(
			"seal_hash_keccak_256", r * API_BENCHMARK_BATCH_SIZE, 0,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// `n`: Input to hash in kilobytes
	seal_hash_keccak_256_per_kb {
		let n in 0 .. max_pages::<T>() * 64;
		let instance = instantiate_contract::<T>(hasher_code(
			"seal_hash_keccak_256", API_BENCHMARK_BATCH_SIZE, n * 1024,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	seal_hash_blake2_256 {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(hasher_code(
			"seal_hash_blake2_256", r * API_BENCHMARK_BATCH_SIZE, 0,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// `n`: Input to hash in kilobytes
	seal_hash_blake2_256_per_kb {
		let n in 0 .. max_pages::<T>() * 64;
		let instance = instantiate_contract::<T>(hasher_code(
			"seal_hash_blake2_256", API_BENCHMARK_BATCH_SIZE, n * 1024,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	seal_hash_blake2_128 {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = instantiate_contract::<T>(hasher_code(
			"seal_hash_blake2_128", r * API_BENCHMARK_BATCH_SIZE, 0,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])

	// `n`: Input to hash in kilobytes
	seal_hash_blake2_128_per_kb {
		let n in 0 .. max_pages::<T>() * 64;
		let instance = instantiate_contract::<T>(hasher_code(
			"seal_hash_blake2_128", API_BENCHMARK_BATCH_SIZE, n * 1024,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0.into(), Weight::max_value(), vec![])
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tests::{ExtBuilder, Test};
	use frame_support::assert_ok;
	use paste::paste;

	macro_rules! create_test {
		($name:ident) => {
			#[test]
			fn $name() {
				ExtBuilder::default().build().execute_with(|| {
					assert_ok!(paste!{
						[<test_benchmark_ $name>]::<Test>()
					});
				});
			}
		}
	}

	create_test!(update_schedule);
	create_test!(put_code);
	create_test!(instantiate);
	create_test!(call);
	create_test!(claim_surcharge);
	create_test!(seal_caller);
	create_test!(seal_address);
	create_test!(seal_gas_left);
	create_test!(seal_balance);
	create_test!(seal_value_transferred);
	create_test!(seal_minimum_balance);
	create_test!(seal_tombstone_deposit);
	create_test!(seal_rent_allowance);
	create_test!(seal_block_number);
	create_test!(seal_now);
	create_test!(seal_weight_to_fee);
	create_test!(seal_gas);
	create_test!(seal_input);
	create_test!(seal_input_per_kb);
	create_test!(seal_return);
	create_test!(seal_return_per_kb);
	create_test!(seal_terminate);
	create_test!(seal_restore_to);
	create_test!(seal_restore_to_per_delta);
	create_test!(seal_random);
	create_test!(seal_deposit_event);
	create_test!(seal_deposit_event_per_topic_and_kb);
	create_test!(seal_set_rent_allowance);
	create_test!(seal_set_storage);
	create_test!(seal_set_storage_per_kb);
	create_test!(seal_get_storage);
	create_test!(seal_get_storage_per_kb);
	create_test!(seal_transfer);
	create_test!(seal_call);
	create_test!(seal_call_per_transfer_input_output_kb);
	create_test!(seal_clear_storage);
	create_test!(seal_hash_sha2_256);
	create_test!(seal_hash_sha2_256_per_kb);
	create_test!(seal_hash_keccak_256);
	create_test!(seal_hash_keccak_256_per_kb);
	create_test!(seal_hash_blake2_256);
	create_test!(seal_hash_blake2_256_per_kb);
	create_test!(seal_hash_blake2_128);
	create_test!(seal_hash_blake2_128_per_kb);
}
