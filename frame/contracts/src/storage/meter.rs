// This file is part of Substrate.

// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
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

//! This module contains functions to meter the storage deposit.

use crate::{storage::ContractInfo, BalanceOf, Config, Error};
use frame_support::{
	dispatch::{DispatchError, DispatchResult},
	traits::{tokens::BalanceStatus, Get, ReservableCurrency},
	DefaultNoBound,
};
use pallet_contracts_primitives::StorageDeposit as Deposit;
use sp_core::crypto::UncheckedFrom;
use sp_runtime::traits::Saturating;
use sp_std::marker::PhantomData;

/// Deposit that uses the native currency's balance type.
pub type DepositOf<T> = Deposit<BalanceOf<T>>;

/// A production root storage meter that actually charges from its origin.
pub type Meter<T> = RawMeter<T, DefaultExt, Root>;

/// A poduction nested storage meter that actually charges from its origin.
pub type NestedMeter<T> = RawMeter<T, DefaultExt, Nested>;

/// A poduction storage meter that actually charges from its origin.
///
/// This can be used where we want to be generic over the state (Root vs. Nested).
pub type GenericMeter<T, S> = RawMeter<T, DefaultExt, S>;

/// A trait that allows to decouple the metering from the charging of balance.
///
/// This mostly exists for testing so that the charging can be mocked.
pub trait Ext<T: Config> {
	/// This will be called to inform the implementer about the `storage_limit` of the meter.
	///
	/// It is necessary to reserve the balance so that the charge won't fail later on. Should fail
	/// when `origin` does not have enough free balance.
	///
	/// `origin`: The origin of the call stack from which is reponsible for putting down a deposit.
	/// `limit`: The limit with which the meter was constructed.
	fn reserve_limit(origin: &T::AccountId, limit: &BalanceOf<T>) -> DispatchResult;
	/// This called to inform the implementer that the metering is finished.
	///
	/// This is should be used to unreserve the unused balance. The amount to unreserve can be
	/// calculated from `limit` and `deposit`.
	///
	/// `origin`: The origin of the call stack from which is reponsible for putting down a deposit.
	/// `limit`: The limit with which the meter was constructed.
	/// `deposit`: The amount of actually used balance during the life time of this meter.
	fn unreserve_limit(origin: &T::AccountId, limit: &BalanceOf<T>, deposit: &DepositOf<T>);
	/// This is called to inform the implementer that some balance should be charged due to
	/// some interaction of the `origin` with a `contract`.
	///
	/// The balance transfer can either flow from `origin` to `contract` or the other way
	/// around depending on whether `amount` constitues a `Charge` or a `Refund`.
	/// It is garantueed that that all the possible balance that can be charged from the `origin`
	/// was reserved by a call to `reserve_limit`. This is why this function is infallible.
	fn charge(origin: &T::AccountId, contract: &T::AccountId, amount: &DepositOf<T>);
}

/// The default ext is what is used for actual on-chain execution when balance needs to be charged.
pub enum DefaultExt {}

/// Used to implement a type state pattern for the meter.
///
/// It is sealed and cannot be implemented outside of this module.
pub trait State: private::Sealed {}

/// State parameter that constitutes a meter that is in its root state.
pub enum Root {}

/// State parameter that constitutes a meter that is in its nested state.
pub enum Nested {}

impl State for Root {}
impl State for Nested {}

/// A type that allows the metering of consumed or freed storage of a single contract call stack.
#[derive(DefaultNoBound)]
pub struct RawMeter<T: Config, E: Ext<T>, S: State> {
	/// The origin is the account that instantiates a call stack. This is where the balance is
	/// charged from and refunded to.
	///
	/// # Note
	///
	/// This is `Some` if and only if `S == Root`.
	origin: Option<T::AccountId>,
	/// The limit of how much balance this meter is allowed to consume.
	limit: BalanceOf<T>,
	/// The amount of balance that was used in this meter and all of its already absorbed children.
	total_deposit: DepositOf<T>,
	/// The amount of balance that was used in this meter alone.
	own_deposit: DepositOf<T>,
	/// Type parameters are only used in impls.
	_phantom: PhantomData<(E, S)>,
}

/// This type is used to describe a storage change when charging from the meter.
#[derive(Default)]
pub struct Diff {
	/// How many bytes were added to storage.
	pub bytes_added: u32,
	/// How many bytes were removed from storage.
	pub bytes_removed: u32,
	/// How many storage items were added to storage.
	pub items_added: u32,
	/// How many storage items were removed from storage.
	pub items_removed: u32,
}

impl Diff {
	pub fn to_deposit<T: Config>(&self) -> DepositOf<T> {
		let mut deposit = Deposit::default();
		let per_byte = T::DepositPerByte::get();
		let per_item = T::DepositPerItem::get();

		if self.bytes_added > self.bytes_removed {
			deposit = deposit.saturating_add(&Deposit::Charge(
				per_byte.saturating_mul((self.bytes_added - self.bytes_removed).into()),
			));
		} else if self.bytes_removed > self.bytes_added {
			deposit = deposit.saturating_add(&Deposit::Refund(
				per_byte.saturating_mul((self.bytes_removed - self.bytes_added).into()),
			));
		}

		if self.items_added > self.items_removed {
			deposit = deposit.saturating_add(&Deposit::Charge(
				per_item.saturating_mul((self.items_added - self.items_removed).into()),
			));
		} else if self.bytes_removed > self.bytes_added {
			deposit = deposit.saturating_add(&Deposit::Refund(
				per_item.saturating_mul((self.items_removed - self.items_added).into()),
			));
		}

		deposit
	}
}

impl<T, E, S> Drop for RawMeter<T, E, S>
where
	T: Config,
	E: Ext<T>,
	S: State,
{
	fn drop(&mut self) {
		// Drop cannot be specialized: We need to do a runtime check.
		// An origin exists if and only if this is a root meter.
		if let Some(origin) = self.origin.as_ref() {
			// you cannot charge to the root meter
			debug_assert_eq!(self.own_deposit, <DepositOf<T>>::default());
			E::unreserve_limit(origin, &self.limit, &self.total_deposit);
		}
	}
}

impl<T, E, S> RawMeter<T, E, S>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Ext<T>,
	S: State,
{
	pub fn nested(&mut self) -> RawMeter<T, E, Nested> {
		RawMeter {
			origin: None,
			limit: self.available(),
			total_deposit: Default::default(),
			own_deposit: Default::default(),
			_phantom: PhantomData,
		}
	}

	pub fn absorb(
		&mut self,
		mut absorbed: RawMeter<T, E, Nested>,
		origin: &T::AccountId,
		contract: &T::AccountId,
		info: Option<&mut ContractInfo<T>>,
	) {
		// Absorbing from an exisiting (non terminated) contract.
		if let Some(info) = info {
			match &mut absorbed.own_deposit {
				Deposit::Charge(amount) =>
					info.storage_deposit = info.storage_deposit.saturating_add(*amount),
				Deposit::Refund(amount) => {
					// We need to make sure to never refund more than what was deposited.
					// This is relevant on runtime upgrades.
					*amount = (*amount).min(info.storage_deposit);
					info.storage_deposit = info.storage_deposit.saturating_sub(*amount);
				},
			}
		}

		self.total_deposit = self.total_deposit.saturating_add(&absorbed.total_deposit);
		E::charge(origin, &contract, &absorbed.own_deposit);
	}

	pub fn total_deposit(&self) -> &DepositOf<T> {
		&self.total_deposit
	}

	fn available(&self) -> BalanceOf<T> {
		self.total_deposit.available(&self.limit)
	}
}

impl<T, E> RawMeter<T, E, Root>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Ext<T>,
{
	pub fn new(origin: T::AccountId, limit: BalanceOf<T>) -> Result<Self, DispatchError> {
		E::reserve_limit(&origin, &limit)?;
		Ok(Self {
			origin: Some(origin),
			limit,
			total_deposit: Default::default(),
			own_deposit: Default::default(),
			_phantom: PhantomData,
		})
	}
}

impl<T, E> RawMeter<T, E, Nested>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Ext<T>,
{
	pub fn charge(&mut self, diff: &Diff) -> Result<DepositOf<T>, DispatchError> {
		let deposit = diff.to_deposit::<T>();
		let total_deposit = self.total_deposit.saturating_add(&deposit);
		if let Deposit::Charge(amount) = total_deposit {
			if amount > self.limit {
				return Err(<Error<T>>::StorageExhausted.into())
			}
		}
		self.total_deposit = total_deposit;
		self.own_deposit = self.own_deposit.saturating_add(&deposit);
		Ok(deposit)
	}

	pub fn terminate(&mut self, contract_info: &ContractInfo<T>) {
		let refund = Deposit::Refund(contract_info.storage_deposit);

		// The deposit for `own_deposit` isn't persisted into the contract info until the current
		// frame is dropped. This means that whatever changes were introduced during the
		// current frame are dicarded when terminating.
		self.total_deposit =
			self.total_deposit.saturating_add(&refund).saturating_sub(&self.own_deposit);
		self.own_deposit = refund;
	}
}

impl<T: Config> Ext<T> for DefaultExt {
	fn reserve_limit(origin: &T::AccountId, limit: &BalanceOf<T>) -> DispatchResult {
		T::Currency::reserve(origin, *limit)
	}

	fn unreserve_limit(origin: &T::AccountId, limit: &BalanceOf<T>, deposit: &DepositOf<T>) {
		T::Currency::unreserve(origin, deposit.available(&limit));
	}

	fn charge(origin: &T::AccountId, contract: &T::AccountId, amount: &DepositOf<T>) {
		let (slashed, beneficiary, amount) = match amount {
			Deposit::Charge(amount) => (origin, contract, amount),
			Deposit::Refund(amount) => (contract, origin, amount),
		};

		// For charge `Err` can never happen as a contract's account is required to exist
		// at all times. The pallet enforces this invariant. Chain extensions or dispatchables
		// that allow the removal of the contract's account are defunct.
		//
		// For refund `Err` can't happen because the initial value transfer from the
		// origin to the contract has a keep alive existence requirement.
		//
		// There is nothing we can do when either `Err` or `Ok(> 0)` happens as this constitutes
		// a bug in the runtime: Either the runtime does not hold up the invariant of never
		// deleting a contract's account or it does not honor reserved balances.
		//
		// There is one exception:
		//
		// If a contract is terminated its account's free balance is completely removed and
		// sent to the beneficiary. This could lead to the removal of the contract's account if
		// the amount of reserved balance is below the existential deposit.
		let _ = T::Currency::repatriate_reserved(
			slashed,
			beneficiary,
			*amount,
			BalanceStatus::Reserved,
		);
	}
}

mod private {
	pub trait Sealed {}
	impl Sealed for super::Root {}
	impl Sealed for super::Nested {}
}
