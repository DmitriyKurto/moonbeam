// Copyright 2019-2021 PureStake Inc.
// This file is part of Moonbeam.

// Moonbeam is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Moonbeam is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Moonbeam.  If not, see <http://www.gnu.org/licenses/>.

//! Moonbeam Runtime Integration Tests

#![cfg(test)]

use cumulus_primitives_parachain_inherent::ParachainInherentData;
use frame_support::{
	assert_noop, assert_ok,
	dispatch::Dispatchable,
	traits::{GenesisBuild, OnFinalize, OnInitialize},
};
use moonbeam_runtime::{
	AccountId, AuthorInherent, Balance, Balances, Call, Event, InflationInfo, ParachainStaking,
	Range, Runtime, System, GLMR,
};
use sp_runtime::{DispatchError, Perbill};

fn run_to_block(n: u32) {
	while System::block_number() < n {
		AuthorInherent::on_finalize(System::block_number());
		ParachainStaking::on_finalize(System::block_number());
		System::set_block_number(System::block_number() + 1);
		AuthorInherent::on_initialize(System::block_number());
	}
}

pub type EVMCall = pallet_evm::Call<Runtime>;
pub type AuthorInherentCall = author_inherent::Call<Runtime>;
pub type ParachainSystemCall = cumulus_pallet_parachain_system::Call<Runtime>;

fn last_event() -> Event {
	System::events().pop().expect("Event expected").event
}

struct ExtBuilder {
	endowed_accounts: Vec<(AccountId, Balance)>,
	stakers: Vec<(AccountId, Option<AccountId>, Balance)>,
}

impl Default for ExtBuilder {
	fn default() -> ExtBuilder {
		ExtBuilder {
			endowed_accounts: vec![],
			stakers: vec![],
		}
	}
}

impl ExtBuilder {
	fn balances(mut self, endowed_accounts: Vec<(AccountId, Balance)>) -> Self {
		self.endowed_accounts = endowed_accounts;
		self
	}

	fn staking(mut self, stakers: Vec<(AccountId, Option<AccountId>, Balance)>) -> Self {
		self.stakers = stakers;
		self
	}

	fn build(self) -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::default()
			.build_storage::<Runtime>()
			.unwrap();

		pallet_balances::GenesisConfig::<Runtime> {
			balances: self.endowed_accounts,
		}
		.assimilate_storage(&mut t)
		.unwrap();

		parachain_staking::GenesisConfig::<Runtime> {
			stakers: self.stakers,
			inflation_config: InflationInfo {
				expect: Range {
					min: 100_000 * GLMR,
					ideal: 200_000 * GLMR,
					max: 500_000 * GLMR,
				},
				// 8766 rounds (hours) in a year
				round: Range {
					min: Perbill::from_parts(Perbill::from_percent(4).deconstruct() / 8766),
					ideal: Perbill::from_parts(Perbill::from_percent(5).deconstruct() / 8766),
					max: Perbill::from_parts(Perbill::from_percent(5).deconstruct() / 8766),
				},
			},
		}
		.assimilate_storage(&mut t)
		.unwrap();

		let mut ext = sp_io::TestExternalities::new(t);
		ext.execute_with(|| System::set_block_number(1));
		ext
	}
}

const ALICE: [u8; 20] = [4u8; 20];
const BOB: [u8; 20] = [5u8; 20];
const CHARLIE: [u8; 20] = [6u8; 20];
const DAVE: [u8; 20] = [7u8; 20];

fn origin_of(account_id: AccountId) -> <Runtime as frame_system::Config>::Origin {
	<Runtime as frame_system::Config>::Origin::signed(account_id)
}

fn inherent_origin() -> <Runtime as frame_system::Config>::Origin {
	<Runtime as frame_system::Config>::Origin::none()
}

#[test]
fn join_collator_candidates() {
	ExtBuilder::default()
		.balances(vec![
			(AccountId::from(ALICE), 2_000 * GLMR),
			(AccountId::from(BOB), 2_000 * GLMR),
			(AccountId::from(CHARLIE), 1_100 * GLMR),
			(AccountId::from(DAVE), 1_000 * GLMR),
		])
		.staking(vec![
			// collators
			(AccountId::from(ALICE), None, 1_000 * GLMR),
			(AccountId::from(BOB), None, 1_000 * GLMR),
			// nominators
			(
				AccountId::from(CHARLIE),
				Some(AccountId::from(ALICE)),
				50 * GLMR,
			),
			(
				AccountId::from(CHARLIE),
				Some(AccountId::from(BOB)),
				50 * GLMR,
			),
		])
		.build()
		.execute_with(|| {
			assert_noop!(
				ParachainStaking::join_candidates(origin_of(AccountId::from(ALICE)), 1_000 * GLMR,),
				parachain_staking::Error::<Runtime>::CandidateExists
			);
			assert_noop!(
				ParachainStaking::join_candidates(
					origin_of(AccountId::from(CHARLIE)),
					1_000 * GLMR
				),
				parachain_staking::Error::<Runtime>::NominatorExists
			);
			assert!(System::events().is_empty());
			assert_ok!(ParachainStaking::join_candidates(
				origin_of(AccountId::from(DAVE)),
				1_000 * GLMR,
			));
			assert_eq!(
				last_event(),
				Event::parachain_staking(parachain_staking::Event::JoinedCollatorCandidates(
					AccountId::from(DAVE),
					1_000 * GLMR,
					3_100 * GLMR
				))
			);
		});
}

#[test]
fn transfer_through_evm_to_stake() {
	ExtBuilder::default()
		.balances(vec![(AccountId::from(ALICE), 3_000 * GLMR)])
		.build()
		.execute_with(|| {
			// Charlie has no balance => fails to stake
			assert_noop!(
				ParachainStaking::join_candidates(
					origin_of(AccountId::from(CHARLIE)),
					1_000 * GLMR,
				),
				DispatchError::Module {
					index: 3,
					error: 3,
					message: Some("InsufficientBalance")
				}
			);
			// Alice stakes to become a collator candidate
			assert_ok!(ParachainStaking::join_candidates(
				origin_of(AccountId::from(ALICE)),
				1_000 * GLMR,
			));
			// Alice transfer from free balance 1000 GLMR to Bob
			assert_ok!(Balances::transfer(
				origin_of(AccountId::from(ALICE)),
				AccountId::from(BOB),
				2_000 * GLMR,
			));
			assert_eq!(Balances::free_balance(AccountId::from(BOB)), 2_000 * GLMR,);
			use sp_core::U256;
			let gas_limit = 100000u64;
			let gas_price: U256 = 1000.into();
			// Bob transfers 1000 GLMR to Charlie via EVM
			assert_ok!(Call::EVM(EVMCall::call(
				AccountId::from(BOB),
				AccountId::from(CHARLIE),
				Vec::new(),
				(1_000 * GLMR).into(),
				gas_limit,
				gas_price,
				None
			))
			.dispatch(origin_of(AccountId::from(BOB))));
			assert_eq!(
				Balances::free_balance(AccountId::from(CHARLIE)),
				1_000 * GLMR,
			);
			// Charlie can stake now
			assert_ok!(ParachainStaking::join_candidates(
				origin_of(AccountId::from(CHARLIE)),
				1_000 * GLMR,
			),);
		});
}

#[test]
fn reward_block_authors() {
	ExtBuilder::default()
		.balances(vec![
			(AccountId::from(ALICE), 2_000 * GLMR),
			(AccountId::from(BOB), 1_000 * GLMR),
		])
		.staking(vec![
			// collator
			(AccountId::from(ALICE), None, 1_000 * GLMR),
			// nominators
			(
				AccountId::from(BOB),
				Some(AccountId::from(ALICE)),
				500 * GLMR,
			),
		])
		.build()
		.execute_with(|| {
			// set parachain inherent data
			use cumulus_primitives_core::PersistedValidationData;
			use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;
			let sproof_builder = RelayStateSproofBuilder::default();
			let (relay_parent_storage_root, relay_chain_state) =
				sproof_builder.into_state_root_and_proof();
			let vfp = PersistedValidationData {
				relay_parent_number: 1u32,
				relay_parent_storage_root,
				..Default::default()
			};
			let parachain_inherent_data = ParachainInherentData {
				validation_data: vfp,
				relay_chain_state: relay_chain_state,
				downward_messages: Default::default(),
				horizontal_messages: Default::default(),
			};
			assert_ok!(
				Call::ParachainSystem(ParachainSystemCall::set_validation_data(
					parachain_inherent_data
				))
				.dispatch(inherent_origin())
			);
			fn set_alice_as_author() {
				assert_ok!(
					Call::AuthorInherent(AuthorInherentCall::set_author(AccountId::from(ALICE)))
						.dispatch(inherent_origin())
				);
			}
			for x in 2..1201 {
				set_alice_as_author();
				run_to_block(x);
			}
			// no rewards doled out yet
			assert_eq!(Balances::free_balance(AccountId::from(ALICE)), 1_000 * GLMR,);
			assert_eq!(Balances::free_balance(AccountId::from(BOB)), 500 * GLMR,);
			set_alice_as_author();
			run_to_block(1201);
			// rewards minted and distributed
			assert_eq!(
				Balances::free_balance(AccountId::from(ALICE)),
				1000010038599992699200,
			);
			assert_eq!(
				Balances::free_balance(AccountId::from(BOB)),
				500003650399996349600,
			);
		});
}