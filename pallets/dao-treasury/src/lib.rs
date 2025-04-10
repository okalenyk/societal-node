// This file is part of Substrate.

// Copyright (C) 2017-2022 Parity Technologies (UK) Ltd.
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

//! # Treasury Pallet
//!
//! The Treasury pallet provides a "pot" of funds that can be managed by stakeholders in the system
//! and a structure for making spending proposals from this pot.
//!
//! - [`Config`]
//! - [`Call`]
//!
//! ## Overview
//!
//! The Treasury Pallet itself provides the pot to store funds, and a means for stakeholders to
//! propose, approve, and deny expenditures. The chain will need to provide a method (e.g.
//! inflation, fees) for collecting funds.
//!
//! By way of example, the Council could vote to fund the Treasury with a portion of the block
//! reward and use the funds to pay developers.
//!
//!
//! ### Terminology
//!
//! - **Proposal:** A suggestion to allocate funds from the pot to a beneficiary.
//! - **Beneficiary:** An account who will receive the funds from a proposal iff the proposal is
//!   approved.
//! - **Deposit:** Funds that a proposer must lock when making a proposal. The deposit will be
//!   returned or slashed if the proposal is approved or rejected respectively.
//! - **Pot:** Unspent funds accumulated by the treasury pallet.
//!
//! ## Interface
//!
//! ### Dispatchable Functions
//!
//! General spending/proposal protocol:
//! - `propose_spend` - Make a spending proposal and stake the required deposit.
//! - `reject_proposal` - Reject a proposal, slashing the deposit.
//! - `approve_proposal` - Accept the proposal, returning the deposit.
//! - `remove_approval` - Remove an approval, the deposit will no longer be returned.
//!
//! ## GenesisConfig
//!
//! The Treasury pallet depends on the [`GenesisConfig`].

#![cfg_attr(not(feature = "std"), no_std)]

mod benchmarking;
#[cfg(test)]
mod tests;
pub mod weights;

use codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;

use sp_runtime::{
	traits::{AccountIdConversion, Saturating, StaticLookup, Zero},
	Permill, RuntimeDebug,
};
use sp_std::prelude::*;

use frame_support::{
	print,
	traits::{
		Currency, EnsureOriginWithArg, ExistenceRequirement::KeepAlive, Get, Imbalance,
		OnUnbalanced, ReservableCurrency, WithdrawReasons,
	},
	weights::Weight,
	BoundedVec, PalletId,
};

use dao_primitives::{DaoPolicy, DaoProvider};

pub use pallet::*;
pub use weights::WeightInfo;

pub type BalanceOf<T, I = ()> =
	<<T as Config<I>>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;
pub type PositiveImbalanceOf<T, I = ()> = <<T as Config<I>>::Currency as Currency<
	<T as frame_system::Config>::AccountId,
>>::PositiveImbalance;
pub type NegativeImbalanceOf<T, I = ()> = <<T as Config<I>>::Currency as Currency<
	<T as frame_system::Config>::AccountId,
>>::NegativeImbalance;

/// A trait to allow the Treasury Pallet to spend it's funds for other purposes.
/// There is an expectation that the implementer of this trait will correctly manage
/// the mutable variables passed to it:
/// * `budget_remaining`: How much available funds that can be spent by the treasury. As funds are
///   spent, you must correctly deduct from this value.
/// * `imbalance`: Any imbalances that you create should be subsumed in here to maximize efficiency
///   of updating the total issuance. (i.e. `deposit_creating`)
/// * `total_weight`: Track any weight that your `spend_fund` implementation uses by updating this
///   value.
/// * `missed_any`: If there were items that you want to spend on, but there were not enough funds,
///   mark this value as `true`. This will prevent the treasury from burning the excess funds.
#[impl_trait_for_tuples::impl_for_tuples(30)]
pub trait SpendFunds<T: Config<I>, I: 'static = ()> {
	fn spend_funds(
		budget_remaining: &mut BalanceOf<T, I>,
		imbalance: &mut PositiveImbalanceOf<T, I>,
		total_weight: &mut Weight,
		missed_any: &mut bool,
	);
}

/// Dao ID. Just a `u32`.
pub type DaoId = u32;

/// An index of a proposal. Just a `u32`.
pub type ProposalIndex = u32;

// TODO: revise this
pub type AssetId = u32;

/// A spending proposal.
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
#[derive(Encode, Decode, Clone, PartialEq, Eq, MaxEncodedLen, RuntimeDebug, TypeInfo)]
pub struct Proposal<AccountId, Balance> {
	/// Dao ID
	dao_id: DaoId,
	/// The account proposing it.
	proposer: AccountId,
	/// The (total) amount that should be paid if the proposal is accepted.
	value: Balance,
	/// The account to whom the payment should be made if the proposal is accepted.
	beneficiary: AccountId,
	/// The amount held on deposit (reserved) for making this proposal.
	bond: Balance,
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T, I = ()>(PhantomData<(T, I)>);

	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// The staking balance.
		type Currency: Currency<Self::AccountId> + ReservableCurrency<Self::AccountId>;

		/// Origin from which approvals must come.
		type ApproveOrigin: EnsureOriginWithArg<Self::Origin, (u32, u32)>;

		/// Origin from which rejections must come.
		type RejectOrigin: EnsureOriginWithArg<Self::Origin, (u32, u32)>;

		/// The overarching event type.
		type Event: From<Event<Self, I>> + IsType<<Self as frame_system::Config>::Event>;

		/// Handler for the unbalanced decrease when slashing for a rejected proposal or bounty.
		type OnSlash: OnUnbalanced<NegativeImbalanceOf<Self, I>>;

		/// Period between successive spends.
		#[pallet::constant]
		type SpendPeriod: Get<Self::BlockNumber>;

		/// Percentage of spare funds (if any) that are burnt per spend period.
		#[pallet::constant]
		type Burn: Get<Permill>;

		/// The treasury's pallet id, used for deriving its sovereign account ID.
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Handler for the unbalanced decrease when treasury funds are burned.
		type BurnDestination: OnUnbalanced<NegativeImbalanceOf<Self, I>>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// Runtime hooks to external pallet using treasury to compute spend funds.
		type SpendFunds: SpendFunds<Self, I>;

		/// The maximum number of approvals that can wait in the spending queue.
		///
		/// NOTE: This parameter is also used within the Bounties Pallet extension if enabled.
		#[pallet::constant]
		type MaxApprovals: Get<u32>;

		/// The origin required for approving spends from the treasury outside of the proposal
		/// process. The `Success` value is the maximum amount that this origin is allowed to
		/// spend at a time.
		type SpendOrigin: EnsureOrigin<Self::Origin, Success = BalanceOf<Self, I>>;

		// TODO: rework providers
		type DaoProvider: DaoProvider<
			Id = u32,
			AccountId = Self::AccountId,
			Policy = DaoPolicy<Self::AccountId>,
		>;
	}

	/// Number of proposals that have been made.
	#[pallet::storage]
	#[pallet::getter(fn proposal_count)]
	pub(crate) type ProposalCount<T, I = ()> =
		StorageMap<_, Twox64Concat, DaoId, ProposalIndex, ValueQuery>;

	/// Proposals that have been made.
	#[pallet::storage]
	#[pallet::getter(fn proposals)]
	pub type Proposals<T: Config<I>, I: 'static = ()> = StorageDoubleMap<
		_,
		Twox64Concat,
		DaoId,
		Twox64Concat,
		ProposalIndex,
		Proposal<T::AccountId, BalanceOf<T, I>>,
		OptionQuery,
	>;

	/// Proposal indices that have been approved but not yet awarded.
	#[pallet::storage]
	#[pallet::getter(fn approvals)]
	pub type Approvals<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, DaoId, BoundedVec<ProposalIndex, T::MaxApprovals>, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// New proposal.
		Proposed { dao_id: DaoId, proposal_index: ProposalIndex },
		/// We have ended a spend period and will now allocate funds.
		Spending { dao_id: DaoId, budget_remaining: BalanceOf<T, I> },
		/// Some funds have been allocated.
		Awarded {
			dao_id: DaoId,
			proposal_index: ProposalIndex,
			award: BalanceOf<T, I>,
			account: T::AccountId,
		},
		/// A proposal was rejected; funds were slashed.
		Rejected { dao_id: DaoId, proposal_index: ProposalIndex, slashed: BalanceOf<T, I> },
		/// Some of our funds have been burnt.
		Burnt { dao_id: DaoId, burnt_funds: BalanceOf<T, I> },
		/// Spending has finished; this is the amount that rolls over until next spend.
		Rollover { dao_id: DaoId, rollover_balance: BalanceOf<T, I> },
		/// Some funds have been deposited.
		Deposit { value: BalanceOf<T, I> },
		/// A new spend proposal has been approved.
		SpendApproved {
			dao_id: DaoId,
			proposal_index: ProposalIndex,
			amount: BalanceOf<T, I>,
			beneficiary: T::AccountId,
		},
	}

	/// Error for the treasury pallet.
	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// Proposer's balance is too low.
		InsufficientProposersBalance,
		/// No proposal or bounty at that index.
		InvalidIndex,
		/// Too many approvals in the queue.
		TooManyApprovals,
		/// The spend origin is valid but the amount it is allowed to spend is lower than the
		/// amount to be spent.
		InsufficientPermission,
		/// Proposal has not been approved.
		ProposalNotApproved,
	}

	// TODO: beware of huge DAO count - use chunk spend instead
	#[pallet::hooks]
	impl<T: Config<I>, I: 'static> Hooks<BlockNumberFor<T>> for Pallet<T, I> {
		/// # <weight>
		/// - Complexity: `O(A)` where `A` is the number of approvals
		/// - Db reads and writes: `Approvals`, `pot account data`
		/// - Db reads and writes per approval: `Proposals`, `proposer account data`, `beneficiary
		///   account data`
		/// - The weight is overestimated if some approvals got missed.
		/// # </weight>
		fn on_initialize(n: T::BlockNumber) -> Weight {
			let mut weight: u64 = 0;
			// Check to see if we should spend some funds!
			if (n % T::SpendPeriod::get()).is_zero() {
				let dao_count = T::DaoProvider::count();

				for dao_id in 0..dao_count {
					weight += Self::spend_funds(dao_id);
				}

				weight
			} else {
				0
			}
		}
	}

	#[pallet::call]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Put forward a suggestion for spending. A deposit proportional to the value
		/// is reserved and slashed if the proposal is rejected. It is returned once the
		/// proposal is awarded.
		///
		/// # <weight>
		/// - Complexity: O(1)
		/// - DbReads: `ProposalCount`, `origin account`
		/// - DbWrites: `ProposalCount`, `Proposals`, `origin account`
		/// # </weight>
		#[pallet::weight(T::WeightInfo::propose_spend())]
		pub fn propose_spend(
			origin: OriginFor<T>,
			dao_id: DaoId,
			#[pallet::compact] value: BalanceOf<T, I>,
			beneficiary: <T::Lookup as StaticLookup>::Source,
		) -> DispatchResult {
			let proposer = ensure_signed(origin.clone())?;
			let beneficiary = T::Lookup::lookup(beneficiary)?;

			let bond = Self::calculate_bond(T::DaoProvider::policy(dao_id)?, value);
			T::Currency::reserve(&proposer, bond)
				.map_err(|_| Error::<T, I>::InsufficientProposersBalance)?;

			let c = Self::proposal_count(dao_id);
			<ProposalCount<T, I>>::insert(dao_id, c + 1);
			<Proposals<T, I>>::insert(
				dao_id,
				c,
				Proposal { dao_id, proposer, value, beneficiary, bond },
			);

			Self::deposit_event(Event::Proposed { dao_id, proposal_index: c });
			Ok(())
		}

		/// Reject a proposed spend. The original deposit will be slashed.
		///
		/// May only be called from `T::RejectOrigin`.
		///
		/// # <weight>
		/// - Complexity: O(1)
		/// - DbReads: `Proposals`, `rejected proposer account`
		/// - DbWrites: `Proposals`, `rejected proposer account`
		/// # </weight>
		#[pallet::weight((T::WeightInfo::reject_proposal(), DispatchClass::Operational))]
		pub fn reject_proposal(
			origin: OriginFor<T>,
			#[pallet::compact] dao_id: DaoId,
			#[pallet::compact] proposal_id: ProposalIndex,
		) -> DispatchResult {
			T::RejectOrigin::ensure_origin(origin, &T::DaoProvider::policy(dao_id)?.reject_origin)?;

			let proposal = <Proposals<T, I>>::take(&dao_id, &proposal_id)
				.ok_or(Error::<T, I>::InvalidIndex)?;
			let value = proposal.bond;
			let imbalance = T::Currency::slash_reserved(&proposal.proposer, value).0;
			T::OnSlash::on_unbalanced(imbalance);

			Self::deposit_event(Event::<T, I>::Rejected {
				dao_id,
				proposal_index: proposal_id,
				slashed: value,
			});
			Ok(())
		}

		/// Approve a proposal. At a later time, the proposal will be allocated to the beneficiary
		/// and the original deposit will be returned.
		///
		/// May only be called from `T::ApproveOrigin`.
		///
		/// # <weight>
		/// - Complexity: O(1).
		/// - DbReads: `Proposals`, `Approvals`
		/// - DbWrite: `Approvals`
		/// # </weight>
		#[pallet::weight((T::WeightInfo::approve_proposal(T::MaxApprovals::get()), DispatchClass::Operational))]
		pub fn approve_proposal(
			origin: OriginFor<T>,
			#[pallet::compact] dao_id: DaoId,
			#[pallet::compact] proposal_id: ProposalIndex,
		) -> DispatchResult {
			T::ApproveOrigin::ensure_origin(
				origin,
				&T::DaoProvider::policy(dao_id)?.approve_origin,
			)?;

			ensure!(
				<Proposals<T, I>>::contains_key(dao_id, proposal_id),
				Error::<T, I>::InvalidIndex
			);

			Approvals::<T, I>::try_append(dao_id, proposal_id)
				.map_err(|_| Error::<T, I>::TooManyApprovals)?;
			Ok(())
		}

		/// Propose and approve a spend of treasury funds.
		///
		/// - `origin`: Must be `SpendOrigin` with the `Success` value being at least `amount`.
		/// - `amount`: The amount to be transferred from the treasury to the `beneficiary`.
		/// - `beneficiary`: The destination account for the transfer.
		///
		/// NOTE: For record-keeping purposes, the proposer is deemed to be equivalent to the
		/// beneficiary.
		#[pallet::weight(T::WeightInfo::spend())]
		pub fn spend(
			origin: OriginFor<T>,
			dao_id: DaoId,
			#[pallet::compact] amount: BalanceOf<T, I>,
			beneficiary: <T::Lookup as StaticLookup>::Source,
		) -> DispatchResult {
			let max_amount = T::SpendOrigin::ensure_origin(origin)?;
			let beneficiary = T::Lookup::lookup(beneficiary)?;

			T::DaoProvider::exists(dao_id)?;

			ensure!(amount <= max_amount, Error::<T, I>::InsufficientPermission);
			let proposal_index = Self::proposal_count(dao_id);
			Approvals::<T, I>::try_append(dao_id, proposal_index)
				.map_err(|_| Error::<T, I>::TooManyApprovals)?;
			let proposal = Proposal {
				dao_id,
				proposer: beneficiary.clone(),
				value: amount,
				beneficiary: beneficiary.clone(),
				bond: Default::default(),
			};
			Proposals::<T, I>::insert(dao_id, proposal_index, proposal);
			ProposalCount::<T, I>::insert(dao_id, proposal_index + 1);

			Self::deposit_event(Event::SpendApproved {
				dao_id,
				proposal_index,
				amount,
				beneficiary,
			});
			Ok(())
		}

		/// Force a previously approved proposal to be removed from the approval queue.
		/// The original deposit will no longer be returned.
		///
		/// May only be called from `T::RejectOrigin`.
		/// - `proposal_id`: The index of a proposal
		///
		/// # <weight>
		/// - Complexity: O(A) where `A` is the number of approvals
		/// - Db reads and writes: `Approvals`
		/// # </weight>
		///
		/// Errors:
		/// - `ProposalNotApproved`: The `proposal_id` supplied was not found in the approval queue,
		/// i.e., the proposal has not been approved. This could also mean the proposal does not
		/// exist altogether, thus there is no way it would have been approved in the first place.
		#[pallet::weight((T::WeightInfo::remove_approval(), DispatchClass::Operational))]
		pub fn remove_approval(
			origin: OriginFor<T>,
			#[pallet::compact] dao_id: DaoId,
			#[pallet::compact] proposal_id: ProposalIndex,
		) -> DispatchResult {
			T::RejectOrigin::ensure_origin(origin, &T::DaoProvider::policy(dao_id)?.reject_origin)?;

			Approvals::<T, I>::try_mutate(dao_id, |v| -> DispatchResult {
				if let Some(index) = v.iter().position(|x| x == &proposal_id) {
					v.remove(index);
					Ok(())
				} else {
					Err(Error::<T, I>::ProposalNotApproved.into())
				}
			})?;

			Ok(())
		}
	}
}

impl<T: Config<I>, I: 'static> Pallet<T, I> {
	// Add public immutables and private mutables.

	/// The account ID of the treasury pot.
	///
	/// This actually does computation. If you need to keep using it, then make sure you cache the
	/// value and only call this once.
	pub fn account_id() -> T::AccountId {
		T::PalletId::get().into_account_truncating()
	}

	fn u128_to_balance_of(cost: u128) -> BalanceOf<T, I> {
		TryInto::<BalanceOf<T, I>>::try_into(cost).ok().unwrap()
	}

	/// The needed bond for a proposal whose spend is `value`.
	fn calculate_bond(policy: DaoPolicy<T::AccountId>, value: BalanceOf<T, I>) -> BalanceOf<T, I> {
		let proposal_bond = Permill::from_percent(policy.proposal_bond);

		let mut r = Self::u128_to_balance_of(policy.proposal_bond_min).max(proposal_bond * value);
		if let Some(m) = policy.proposal_bond_max {
			r = r.min(Self::u128_to_balance_of(m));
		}
		r
	}

	/// Spend some money! returns number of approvals before spend.
	pub fn spend_funds(dao_id: DaoId) -> Weight {
		let mut total_weight: Weight = Zero::zero();

		let mut budget_remaining = Self::pot(dao_id);
		log::info!("budget remaining: {:?}", budget_remaining);

		Self::deposit_event(Event::Spending { dao_id, budget_remaining });
		let account_id = T::DaoProvider::dao_account_id(dao_id);

		let mut missed_any = false;
		let mut imbalance = <PositiveImbalanceOf<T, I>>::zero();
		let proposals_len = Approvals::<T, I>::mutate(dao_id, |v| {
			let proposals_approvals_len = v.len() as u32;
			v.retain(|&index| {
				// Should always be true, but shouldn't panic if false or we're screwed.
				if let Some(p) = Self::proposals(dao_id, index) {
					if p.value <= budget_remaining {
						budget_remaining -= p.value;
						<Proposals<T, I>>::remove(dao_id, index);

						// return their deposit.
						let err_amount = T::Currency::unreserve(&p.proposer, p.bond);
						debug_assert!(err_amount.is_zero());

						// provide the allocation.
						imbalance.subsume(T::Currency::deposit_creating(&p.beneficiary, p.value));

						Self::deposit_event(Event::Awarded {
							dao_id,
							proposal_index: index,
							award: p.value,
							account: p.beneficiary,
						});
						false
					} else {
						missed_any = true;
						true
					}
				} else {
					false
				}
			});
			proposals_approvals_len
		});

		total_weight += T::WeightInfo::on_initialize_proposals(proposals_len);

		// Call Runtime hooks to external pallet using treasury to compute spend funds.
		T::SpendFunds::spend_funds(
			&mut budget_remaining,
			&mut imbalance,
			&mut total_weight,
			&mut missed_any,
		);

		if !missed_any {
			// burn some proportion of the remaining budget if we run a surplus.
			let burn = (T::Burn::get() * budget_remaining).min(budget_remaining);
			budget_remaining -= burn;

			let (debit, credit) = T::Currency::pair(burn);
			imbalance.subsume(debit);
			T::BurnDestination::on_unbalanced(credit);
			Self::deposit_event(Event::Burnt { dao_id, burnt_funds: burn })
		}

		// Must never be an error, but better to be safe.
		// proof: budget_remaining is account free balance minus ED;
		// Thus we can't spend more than account free balance minus ED;
		// Thus account is kept alive; qed;
		if let Err(problem) =
			T::Currency::settle(&account_id, imbalance, WithdrawReasons::TRANSFER, KeepAlive)
		{
			print("Inconsistent state - couldn't settle imbalance for funds spent by treasury");
			// Nothing else to do here.
			drop(problem);
		}

		Self::deposit_event(Event::Rollover { dao_id, rollover_balance: budget_remaining });

		total_weight
	}

	/// Return the amount of money in the pot.
	// The existential deposit is not part of the pot so treasury account never gets deleted.
	pub fn pot(dao_id: DaoId) -> BalanceOf<T, I> {
		T::Currency::free_balance(&T::DaoProvider::dao_account_id(dao_id))
			// Must never be less than 0 but better be safe.
			.saturating_sub(T::Currency::minimum_balance())
	}
}

impl<T: Config<I>, I: 'static> OnUnbalanced<NegativeImbalanceOf<T, I>> for Pallet<T, I> {
	// TODO: revise for dao_id
	fn on_nonzero_unbalanced(amount: NegativeImbalanceOf<T, I>) {
		let numeric_amount = amount.peek();

		T::Currency::resolve_creating(&Self::account_id(), amount);

		Self::deposit_event(Event::Deposit { value: numeric_amount });
	}
}
