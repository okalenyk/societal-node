// This file is part of Substrate.

// Copyright (C) 2019-2022 Parity Technologies (UK) Ltd.
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

//! # Membership Module
//!
//! Allows control of membership of a set of `AccountId`s, useful for managing membership of of a
//! collective. A prime member may be set

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{
	dispatch::DispatchError,
	traits::{EnsureOriginWithArg, Get},
	BoundedVec,
};
use sp_std::prelude::*;

pub mod weights;

pub use pallet::*;
pub use weights::WeightInfo;

use dao_primitives::{ChangeDaoMembers, DaoPolicy, DaoProvider, InitializeDaoMembers};

/// Dao ID. Just a `u32`.
pub type DaoId = u32;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	/// The current storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(4);

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T, I = ()>(PhantomData<(T, I)>);

	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// The overarching event type.
		type Event: From<Event<Self, I>> + IsType<<Self as frame_system::Config>::Event>;

		/// Required origin for adding a member (though can always be Root).
		type ApproveOrigin: EnsureOriginWithArg<Self::Origin, (u32, u32)>;

		/// The receiver of the signal for when the membership has been initialized. This happens
		/// pre-genesis and will usually be the same as `MembershipChanged`. If you need to do
		/// something different on initialization, then you can change this accordingly.
		type MembershipInitialized: InitializeDaoMembers<DaoId, Self::AccountId>;

		/// The receiver of the signal for when the membership has changed.
		type MembershipChanged: ChangeDaoMembers<DaoId, Self::AccountId>;

		/// The maximum number of members that this membership can have.
		///
		/// This is used for benchmarking. Re-run the benchmarks if this changes.
		///
		/// This is enforced in the code; the membership size can not exceed this limit.
		type MaxMembers: Get<u32>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		// TODO: rework providers
		type DaoProvider: DaoProvider<
			Id = u32,
			AccountId = Self::AccountId,
			Policy = DaoPolicy<Self::AccountId>,
		>;
	}

	/// The current membership, stored as an ordered Vec.
	#[pallet::storage]
	#[pallet::getter(fn members)]
	pub type Members<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, DaoId, BoundedVec<T::AccountId, T::MaxMembers>, ValueQuery>;

	/// The current prime member, if one exists.
	#[pallet::storage]
	#[pallet::getter(fn prime)]
	pub type Prime<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, DaoId, T::AccountId, OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// The given member was added; see the transaction for who.
		MemberAdded,
		/// The given member was removed; see the transaction for who.
		MemberRemoved,
		/// Two members were swapped; see the transaction for who.
		MembersSwapped,
		/// The membership was reset; see the transaction for who the new set is.
		MembersReset,
		/// One of the members' keys changed.
		KeyChanged,
		/// Phantom member, never used.
		Dummy { _phantom_data: PhantomData<(T::AccountId, <T as Config<I>>::Event)> },
	}

	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// Already a member.
		AlreadyMember,
		/// Not a member.
		NotMember,
		/// Too many members.
		TooManyMembers,
	}

	#[pallet::call]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Add a member `who` to the set.
		///
		/// May only be called from `T::AddOrigin`.
		#[pallet::weight(50_000_000)]
		pub fn add_member(
			origin: OriginFor<T>,
			dao_id: DaoId,
			who: T::AccountId,
		) -> DispatchResult {
			T::ApproveOrigin::ensure_origin(origin, &T::DaoProvider::policy(dao_id)?.add_origin)?;

			let mut members = <Members<T, I>>::get(dao_id);
			let location = members.binary_search(&who).err().ok_or(Error::<T, I>::AlreadyMember)?;
			members
				.try_insert(location, who.clone())
				.map_err(|_| Error::<T, I>::TooManyMembers)?;

			<Members<T, I>>::insert(dao_id, &members);

			T::MembershipChanged::change_members_sorted(dao_id, &[who], &[], &members[..]);

			Self::deposit_event(Event::MemberAdded);
			Ok(())
		}

		/// Remove a member `who` from the set.
		///
		/// May only be called from `T::RemoveOrigin`.
		#[pallet::weight(50_000_000)]
		pub fn remove_member(
			origin: OriginFor<T>,
			dao_id: DaoId,
			who: T::AccountId,
		) -> DispatchResult {
			T::ApproveOrigin::ensure_origin(
				origin,
				&T::DaoProvider::policy(dao_id)?.remove_origin,
			)?;

			let mut members = <Members<T, I>>::get(dao_id);
			let location = members.binary_search(&who).ok().ok_or(Error::<T, I>::NotMember)?;
			members.remove(location);

			<Members<T, I>>::insert(dao_id, &members);

			T::MembershipChanged::change_members_sorted(dao_id, &[], &[who], &members[..]);
			Self::rejig_prime(dao_id, &members);

			Self::deposit_event(Event::MemberRemoved);
			Ok(())
		}

		/// Swap out one member `remove` for another `add`.
		///
		/// May only be called from `T::SwapOrigin`.
		///
		/// Prime membership is *not* passed from `remove` to `add`, if extant.
		#[pallet::weight(50_000_000)]
		pub fn swap_member(
			origin: OriginFor<T>,
			dao_id: DaoId,
			remove: T::AccountId,
			add: T::AccountId,
		) -> DispatchResult {
			T::ApproveOrigin::ensure_origin(origin, &T::DaoProvider::policy(dao_id)?.swap_origin)?;

			if remove == add {
				return Ok(())
			}

			let mut members = <Members<T, I>>::get(dao_id);
			let location = members.binary_search(&remove).ok().ok_or(Error::<T, I>::NotMember)?;
			let _ = members.binary_search(&add).err().ok_or(Error::<T, I>::AlreadyMember)?;
			members[location] = add.clone();
			members.sort();

			<Members<T, I>>::insert(dao_id, &members);

			T::MembershipChanged::change_members_sorted(dao_id, &[add], &[remove], &members[..]);
			Self::rejig_prime(dao_id, &members);

			Self::deposit_event(Event::MembersSwapped);
			Ok(())
		}

		/// Change the membership to a new set, disregarding the existing membership. Be nice and
		/// pass `members` pre-sorted.
		///
		/// May only be called from `T::ResetOrigin`.
		#[pallet::weight(50_000_000)]
		pub fn reset_members(
			origin: OriginFor<T>,
			dao_id: DaoId,
			members: Vec<T::AccountId>,
		) -> DispatchResult {
			T::ApproveOrigin::ensure_origin(origin, &T::DaoProvider::policy(dao_id)?.reset_origin)?;

			let mut members: BoundedVec<T::AccountId, T::MaxMembers> =
				BoundedVec::try_from(members).map_err(|_| Error::<T, I>::TooManyMembers)?;
			members.sort();
			<Members<T, I>>::mutate(dao_id, |m| {
				T::MembershipChanged::set_members_sorted(dao_id, &members[..], m);
				Self::rejig_prime(dao_id, &members);
				*m = members;
			});

			Self::deposit_event(Event::MembersReset);
			Ok(())
		}

		/// Swap out the sending member for some other key `new`.
		///
		/// May only be called from `Signed` origin of a current member.
		///
		/// Prime membership is passed from the origin account to `new`, if extant.
		#[pallet::weight(50_000_000)]
		pub fn change_key(
			origin: OriginFor<T>,
			dao_id: DaoId,
			new: T::AccountId,
		) -> DispatchResult {
			let remove = ensure_signed(origin)?;

			if remove != new {
				let mut members = <Members<T, I>>::get(dao_id);
				let location =
					members.binary_search(&remove).ok().ok_or(Error::<T, I>::NotMember)?;
				let _ = members.binary_search(&new).err().ok_or(Error::<T, I>::AlreadyMember)?;
				members[location] = new.clone();
				members.sort();

				<Members<T, I>>::insert(dao_id, &members);

				T::MembershipChanged::change_members_sorted(
					dao_id,
					&[new.clone()],
					&[remove.clone()],
					&members[..],
				);

				if Prime::<T, I>::get(dao_id) == Some(remove) {
					Prime::<T, I>::insert(dao_id, &new);

					T::MembershipChanged::set_prime(dao_id, Some(new));
				}
			}

			Self::deposit_event(Event::KeyChanged);
			Ok(())
		}

		/// Set the prime member. Must be a current member.
		///
		/// May only be called from `T::PrimeOrigin`.
		#[pallet::weight(50_000_000)]
		pub fn set_prime(origin: OriginFor<T>, dao_id: DaoId, who: T::AccountId) -> DispatchResult {
			T::ApproveOrigin::ensure_origin(origin, &T::DaoProvider::policy(dao_id)?.prime_origin)?;

			Self::members(dao_id).binary_search(&who).ok().ok_or(Error::<T, I>::NotMember)?;
			Prime::<T, I>::insert(dao_id, &who);

			T::MembershipChanged::set_prime(dao_id, Some(who));
			Ok(())
		}

		/// Remove the prime member if it exists.
		///
		/// May only be called from `T::PrimeOrigin`.
		#[pallet::weight(50_000_000)]
		pub fn clear_prime(origin: OriginFor<T>, dao_id: DaoId) -> DispatchResult {
			T::ApproveOrigin::ensure_origin(origin, &T::DaoProvider::policy(dao_id)?.prime_origin)?;

			Prime::<T, I>::remove(dao_id);

			T::MembershipChanged::set_prime(dao_id, None);
			Ok(())
		}
	}
}

impl<T: Config<I>, I: 'static> Pallet<T, I> {
	fn rejig_prime(dao_id: DaoId, members: &[T::AccountId]) {
		if let Some(prime) = Prime::<T, I>::get(dao_id) {
			match members.binary_search(&prime) {
				Ok(_) => {
					// TODO
					// T::MembershipChanged::set_prime(Some(prime))
				},
				Err(_) => Prime::<T, I>::remove(dao_id),
			}
		}
	}
}

// TODO
// impl<T: Config<I>, I: 'static> Contains<T::AccountId> for Pallet<T, I> {
// 	fn contains(dao_id: DaoId, t: &T::AccountId) -> bool {
// 		Self::members(dao_id).binary_search(t).is_ok()
// 	}
// }

/// A trait for a set which can enumerate its members in order.
pub trait DaoSortedMembers<T: Ord> {
	/// Get a vector of all members in the set, ordered.
	fn sorted_members(dao_id: DaoId) -> Vec<T>;

	/// Return `true` if this "contains" the given value `t`.
	fn contains(dao_id: DaoId, t: &T) -> bool {
		Self::sorted_members(dao_id).binary_search(t).is_ok()
	}

	/// Get the number of items in the set.
	fn count(dao_id: DaoId) -> usize {
		Self::sorted_members(dao_id).len()
	}

	/// Add an item that would satisfy `contains`. It does not make sure any other
	/// state is correctly maintained or generated.
	///
	/// **Should be used for benchmarking only!!!**
	#[cfg(feature = "runtime-benchmarks")]
	fn add(_t: &T) {
		unimplemented!()
	}
}
impl<T: Config<I>, I: 'static> DaoSortedMembers<T::AccountId> for Pallet<T, I> {
	fn sorted_members(dao_id: DaoId) -> Vec<T::AccountId> {
		Self::members(dao_id).to_vec()
	}

	fn count(dao_id: DaoId) -> usize {
		Members::<T, I>::decode_len(dao_id).unwrap_or(0)
	}
}

// TODO: make abstraction to frame InitializeMembers
impl<T: Config<I>, I: 'static> InitializeDaoMembers<DaoId, T::AccountId> for Pallet<T, I> {
	fn initialize_members(
		dao_id: DaoId,
		source_members: Vec<T::AccountId>,
	) -> Result<(), DispatchError> {
		if !source_members.is_empty() {
			assert!(<Members<T, I>>::get(dao_id).is_empty(), "Members are already initialized!");

			let mut members: BoundedVec<T::AccountId, T::MaxMembers> =
				BoundedVec::try_from(source_members).map_err(|_| Error::<T, I>::TooManyMembers)?;
			members.sort();
			T::MembershipInitialized::initialize_members(dao_id, members.clone().into())?;
			<Members<T, I>>::insert(dao_id, members);
		}

		Ok(())
	}
}

#[cfg(feature = "runtime-benchmarks")]
mod benchmark {
	use super::{Pallet as Membership, *};
	use frame_benchmarking::{account, benchmarks_instance_pallet, whitelist};
	use frame_support::{assert_ok, traits::EnsureOrigin};
	use frame_system::RawOrigin;

	const SEED: u32 = 0;

	fn set_members<T: Config<I>, I: 'static>(members: Vec<T::AccountId>, prime: Option<usize>) {
		let approve_origin = T::ApproveOrigin::successful_origin(&(1, 1));
		// let prime_origin = T::PrimeOrigin::successful_origin();

		assert_ok!(<Membership<T, I>>::reset_members(approve_origin.clone(), 0, members.clone()));
		if let Some(prime) = prime.map(|i| members[i].clone()) {
			assert_ok!(<Membership<T, I>>::set_prime(approve_origin.clone(), 0, prime));
		} else {
			assert_ok!(<Membership<T, I>>::clear_prime(approve_origin, 0));
		}
	}

	benchmarks_instance_pallet! {
		add_member {
			let m in 1 .. (T::MaxMembers::get() - 1);

			let members = (0..m).map(|i| account("member", i, SEED)).collect::<Vec<T::AccountId>>();
			set_members::<T, I>(members, None);
			let new_member = account::<T::AccountId>("add", m, SEED);
		}: {
			assert_ok!(<Membership<T, I>>::add_member(T::ApproveOrigin::successful_origin(&(1, 1)), 0, new_member.clone()));
		}
		verify {
			assert!(<Members<T, I>>::get(0).contains(&new_member));
			#[cfg(test)] crate::tests::clean();
		}

		// the case of no prime or the prime being removed is surely cheaper than the case of
		// reporting a new prime via `MembershipChanged`.
		remove_member {
			let m in 2 .. T::MaxMembers::get();

			let members = (0..m).map(|i| account("member", i, SEED)).collect::<Vec<T::AccountId>>();
			set_members::<T, I>(members.clone(), Some(members.len() - 1));

			let to_remove = members.first().cloned().unwrap();
		}: {
			assert_ok!(<Membership<T, I>>::remove_member(T::ApproveOrigin::successful_origin(&(1, 1)), 0, to_remove.clone()));
		} verify {
			assert!(!<Members<T, I>>::get(0).contains(&to_remove));
			// prime is rejigged
			assert!(<Prime<T, I>>::get(0).is_some() && T::MembershipChanged::get_prime(0).is_some());
			#[cfg(test)] crate::tests::clean();
		}

		// we remove a non-prime to make sure it needs to be set again.
		swap_member {
			let m in 2 .. T::MaxMembers::get();

			let members = (0..m).map(|i| account("member", i, SEED)).collect::<Vec<T::AccountId>>();
			set_members::<T, I>(members.clone(), Some(members.len() - 1));
			let add = account::<T::AccountId>("member", m, SEED);
			let remove = members.first().cloned().unwrap();
		}: {
			assert_ok!(<Membership<T, I>>::swap_member(
				T::ApproveOrigin::successful_origin(&(1, 1)),
				0,
				remove.clone(),
				add.clone(),
			));
		} verify {
			assert!(!<Members<T, I>>::get(0).contains(&remove));
			assert!(<Members<T, I>>::get(0).contains(&add));
			// prime is rejigged
			assert!(<Prime<T, I>>::get(0).is_some() && T::MembershipChanged::get_prime(0).is_some());
			#[cfg(test)] crate::tests::clean();
		}

		// er keep the prime common between incoming and outgoing to make sure it is rejigged.
		reset_member {
			let m in 1 .. T::MaxMembers::get();

			let members = (1..m+1).map(|i| account("member", i, SEED)).collect::<Vec<T::AccountId>>();
			set_members::<T, I>(members.clone(), Some(members.len() - 1));
			let mut new_members = (m..2*m).map(|i| account("member", i, SEED)).collect::<Vec<T::AccountId>>();
		}: {
			assert_ok!(<Membership<T, I>>::reset_members(T::ApproveOrigin::successful_origin(&(1, 1)), 0, new_members.clone()));
		} verify {
			new_members.sort();
			assert_eq!(<Members<T, I>>::get(0), new_members);
			// prime is rejigged
			assert!(<Prime<T, I>>::get(0).is_some() && T::MembershipChanged::get_prime(0).is_some());
			#[cfg(test)] crate::tests::clean();
		}

		change_key {
			let m in 1 .. T::MaxMembers::get();

			// worse case would be to change the prime
			let members = (0..m).map(|i| account("member", i, SEED)).collect::<Vec<T::AccountId>>();
			let prime = members.last().cloned().unwrap();
			set_members::<T, I>(members.clone(), Some(members.len() - 1));

			let add = account::<T::AccountId>("member", m, SEED);
			whitelist!(prime);
		}: {
			assert_ok!(<Membership<T, I>>::change_key(RawOrigin::Signed(prime.clone()).into(), 0, add.clone()));
		} verify {
			assert!(!<Members<T, I>>::get(0).contains(&prime));
			assert!(<Members<T, I>>::get(0).contains(&add));
			// prime is rejigged
			assert_eq!(<Prime<T, I>>::get(0).unwrap(), add);
			#[cfg(test)] crate::tests::clean();
		}

		set_prime {
			let m in 1 .. T::MaxMembers::get();
			let members = (0..m).map(|i| account("member", i, SEED)).collect::<Vec<T::AccountId>>();
			let prime = members.last().cloned().unwrap();
			set_members::<T, I>(members, None);
		}: {
			assert_ok!(<Membership<T, I>>::set_prime(T::ApproveOrigin::successful_origin(&(1, 1)), 0, prime));
		} verify {
			assert!(<Prime<T, I>>::get(0).is_some());
			assert!(<T::MembershipChanged>::get_prime(0).is_some());
			#[cfg(test)] crate::tests::clean();
		}

		clear_prime {
			let m in 1 .. T::MaxMembers::get();
			let members = (0..m).map(|i| account("member", i, SEED)).collect::<Vec<T::AccountId>>();
			let prime = members.last().cloned().unwrap();
			set_members::<T, I>(members, None);
		}: {
			assert_ok!(<Membership<T, I>>::clear_prime(T::ApproveOrigin::successful_origin(&(1, 1)), 0));
		} verify {
			assert!(<Prime<T, I>>::get(0).is_none());
			assert!(<T::MembershipChanged>::get_prime(0).is_none());
			#[cfg(test)] crate::tests::clean();
		}

		impl_benchmark_test_suite!(Membership, crate::tests::new_bench_ext(), crate::tests::Test);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate as pallet_membership;

	use sp_core::H256;
	use sp_runtime::{
		testing::Header,
		traits::{BadOrigin, BlakeTwo256, IdentityLookup},
	};

	use frame_support::{
		assert_noop, assert_ok, bounded_vec, ord_parameter_types, parameter_types,
		traits::{ConstU32, ConstU64, GenesisBuild, StorageVersion},
	};
	use frame_system::EnsureSignedBy;

	type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
	type Block = frame_system::mocking::MockBlock<Test>;

	frame_support::construct_runtime!(
		pub enum Test where
			Block = Block,
			NodeBlock = Block,
			UncheckedExtrinsic = UncheckedExtrinsic,
		{
			System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
			Membership: pallet_membership::{Pallet, Call, Storage, Config<T>, Event<T>},
		}
	);

	parameter_types! {
		pub BlockWeights: frame_system::limits::BlockWeights =
			frame_system::limits::BlockWeights::simple_max(1024);
		pub static Members: Vec<u64> = vec![];
		pub static Prime: Option<u64> = None;
	}

	impl frame_system::Config for Test {
		type BaseCallFilter = frame_support::traits::Everything;
		type BlockWeights = ();
		type BlockLength = ();
		type DbWeight = ();
		type Origin = Origin;
		type Index = u64;
		type BlockNumber = u64;
		type Hash = H256;
		type Call = Call;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = Event;
		type BlockHashCount = ConstU64<250>;
		type Version = ();
		type PalletInfo = PalletInfo;
		type AccountData = ();
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
		type SS58Prefix = ();
		type OnSetCode = ();
		type MaxConsumers = ConstU32<16>;
	}
	ord_parameter_types! {
		pub const One: u64 = 1;
		pub const Two: u64 = 2;
		pub const Three: u64 = 3;
		pub const Four: u64 = 4;
		pub const Five: u64 = 5;
	}

	pub struct TestChangeMembers;
	impl ChangeMembers<u64> for TestChangeMembers {
		fn change_members_sorted(incoming: &[u64], outgoing: &[u64], new: &[u64]) {
			let mut old_plus_incoming = Members::get();
			old_plus_incoming.extend_from_slice(incoming);
			old_plus_incoming.sort();
			let mut new_plus_outgoing = new.to_vec();
			new_plus_outgoing.extend_from_slice(outgoing);
			new_plus_outgoing.sort();
			assert_eq!(old_plus_incoming, new_plus_outgoing);

			Members::set(new.to_vec());
			Prime::set(None);
		}
		fn set_prime(who: Option<u64>) {
			Prime::set(who);
		}
		fn get_prime() -> Option<u64> {
			Prime::get()
		}
	}

	impl InitializeMembers<u64> for TestChangeMembers {
		fn initialize_members(members: &[u64]) {
			MEMBERS.with(|m| *m.borrow_mut() = members.to_vec());
		}
	}

	impl Config for Test {
		type Event = Event;
		type AddOrigin = EnsureSignedBy<One, u64>;
		type RemoveOrigin = EnsureSignedBy<Two, u64>;
		type SwapOrigin = EnsureSignedBy<Three, u64>;
		type ResetOrigin = EnsureSignedBy<Four, u64>;
		type PrimeOrigin = EnsureSignedBy<Five, u64>;
		type MembershipInitialized = TestChangeMembers;
		type MembershipChanged = TestChangeMembers;
		type MaxMembers = ConstU32<10>;
		type WeightInfo = ();
	}

	pub(crate) fn new_test_ext() -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
		// We use default for brevity, but you can configure as desired if needed.
		pallet_membership::GenesisConfig::<Test> {
			members: bounded_vec![10, 20, 30],
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();
		t.into()
	}

	#[cfg(feature = "runtime-benchmarks")]
	pub(crate) fn new_bench_ext() -> sp_io::TestExternalities {
		frame_system::GenesisConfig::default().build_storage::<Test>().unwrap().into()
	}

	#[cfg(feature = "runtime-benchmarks")]
	pub(crate) fn clean() {
		Members::set(vec![]);
		Prime::set(None);
	}

	#[test]
	fn query_membership_works() {
		new_test_ext().execute_with(|| {
			assert_eq!(Membership::members(), vec![10, 20, 30]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), vec![10, 20, 30]);
		});
	}

	#[test]
	fn prime_member_works() {
		new_test_ext().execute_with(|| {
			assert_noop!(Membership::set_prime(Origin::signed(4), 20), BadOrigin);
			assert_noop!(Membership::set_prime(Origin::signed(5), 15), Error::<Test, _>::NotMember);
			assert_ok!(Membership::set_prime(Origin::signed(5), 20));
			assert_eq!(Membership::prime(), Some(20));
			assert_eq!(PRIME.with(|m| *m.borrow()), Membership::prime());

			assert_ok!(Membership::clear_prime(Origin::signed(5)));
			assert_eq!(Membership::prime(), None);
			assert_eq!(PRIME.with(|m| *m.borrow()), Membership::prime());
		});
	}

	#[test]
	fn add_member_works() {
		new_test_ext().execute_with(|| {
			assert_noop!(Membership::add_member(Origin::signed(5), 15), BadOrigin);
			assert_noop!(
				Membership::add_member(Origin::signed(1), 10),
				Error::<Test, _>::AlreadyMember
			);
			assert_ok!(Membership::add_member(Origin::signed(1), 15));
			assert_eq!(Membership::members(), vec![10, 15, 20, 30]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), Membership::members().to_vec());
		});
	}

	#[test]
	fn remove_member_works() {
		new_test_ext().execute_with(|| {
			assert_noop!(Membership::remove_member(Origin::signed(5), 20), BadOrigin);
			assert_noop!(
				Membership::remove_member(Origin::signed(2), 15),
				Error::<Test, _>::NotMember
			);
			assert_ok!(Membership::set_prime(Origin::signed(5), 20));
			assert_ok!(Membership::remove_member(Origin::signed(2), 20));
			assert_eq!(Membership::members(), vec![10, 30]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), Membership::members().to_vec());
			assert_eq!(Membership::prime(), None);
			assert_eq!(PRIME.with(|m| *m.borrow()), Membership::prime());
		});
	}

	#[test]
	fn swap_member_works() {
		new_test_ext().execute_with(|| {
			assert_noop!(Membership::swap_member(Origin::signed(5), 10, 25), BadOrigin);
			assert_noop!(
				Membership::swap_member(Origin::signed(3), 15, 25),
				Error::<Test, _>::NotMember
			);
			assert_noop!(
				Membership::swap_member(Origin::signed(3), 10, 30),
				Error::<Test, _>::AlreadyMember
			);

			assert_ok!(Membership::set_prime(Origin::signed(5), 20));
			assert_ok!(Membership::swap_member(Origin::signed(3), 20, 20));
			assert_eq!(Membership::members(), vec![10, 20, 30]);
			assert_eq!(Membership::prime(), Some(20));
			assert_eq!(PRIME.with(|m| *m.borrow()), Membership::prime());

			assert_ok!(Membership::set_prime(Origin::signed(5), 10));
			assert_ok!(Membership::swap_member(Origin::signed(3), 10, 25));
			assert_eq!(Membership::members(), vec![20, 25, 30]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), Membership::members().to_vec());
			assert_eq!(Membership::prime(), None);
			assert_eq!(PRIME.with(|m| *m.borrow()), Membership::prime());
		});
	}

	#[test]
	fn swap_member_works_that_does_not_change_order() {
		new_test_ext().execute_with(|| {
			assert_ok!(Membership::swap_member(Origin::signed(3), 10, 5));
			assert_eq!(Membership::members(), vec![5, 20, 30]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), Membership::members().to_vec());
		});
	}

	#[test]
	fn change_key_works() {
		new_test_ext().execute_with(|| {
			assert_ok!(Membership::set_prime(Origin::signed(5), 10));
			assert_noop!(
				Membership::change_key(Origin::signed(3), 0, 25),
				Error::<Test, _>::NotMember
			);
			assert_noop!(
				Membership::change_key(Origin::signed(10), 0, 20),
				Error::<Test, _>::AlreadyMember
			);
			assert_ok!(Membership::change_key(Origin::signed(10), 0, 40));
			assert_eq!(Membership::members(), vec![20, 30, 40]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), Membership::members().to_vec());
			assert_eq!(Membership::prime(), Some(40));
			assert_eq!(PRIME.with(|m| *m.borrow()), Membership::prime());
		});
	}

	#[test]
	fn change_key_works_that_does_not_change_order() {
		new_test_ext().execute_with(|| {
			assert_ok!(Membership::change_key(Origin::signed(10), 0, 5));
			assert_eq!(Membership::members(), vec![5, 20, 30]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), Membership::members().to_vec());
		});
	}

	#[test]
	fn reset_members_works() {
		new_test_ext().execute_with(|| {
			assert_ok!(Membership::set_prime(Origin::signed(5), 20));
			assert_noop!(
				Membership::reset_members(Origin::signed(1), bounded_vec![20, 40, 30]),
				BadOrigin
			);

			assert_ok!(Membership::reset_members(Origin::signed(4), vec![20, 40, 30]));
			assert_eq!(Membership::members(), vec![20, 30, 40]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), Membership::members().to_vec());
			assert_eq!(Membership::prime(), Some(20));
			assert_eq!(PRIME.with(|m| *m.borrow()), Membership::prime());

			assert_ok!(Membership::reset_members(Origin::signed(4), vec![10, 40, 30]));
			assert_eq!(Membership::members(), vec![10, 30, 40]);
			assert_eq!(MEMBERS.with(|m| m.borrow().clone()), Membership::members().to_vec());
			assert_eq!(Membership::prime(), None);
			assert_eq!(PRIME.with(|m| *m.borrow()), Membership::prime());
		});
	}

	#[test]
	#[should_panic(expected = "Members cannot contain duplicate accounts.")]
	fn genesis_build_panics_with_duplicate_members() {
		pallet_membership::GenesisConfig::<Test> {
			members: bounded_vec![1, 2, 3, 1],
			phantom: Default::default(),
		}
		.build_storage()
		.unwrap();
	}

	#[test]
	fn migration_v4() {
		new_test_ext().execute_with(|| {
			use frame_support::traits::PalletInfo;
			let old_pallet_name = "OldMembership";
			let new_pallet_name =
				<Test as frame_system::Config>::PalletInfo::name::<Membership>().unwrap();

			frame_support::storage::migration::move_pallet(
				new_pallet_name.as_bytes(),
				old_pallet_name.as_bytes(),
			);

			StorageVersion::new(0).put::<Membership>();

			crate::migrations::v4::pre_migrate::<Membership, _>(old_pallet_name, new_pallet_name);
			crate::migrations::v4::migrate::<Test, Membership, _>(old_pallet_name, new_pallet_name);
			crate::migrations::v4::post_migrate::<Membership, _>(old_pallet_name, new_pallet_name);
		});
	}
}
