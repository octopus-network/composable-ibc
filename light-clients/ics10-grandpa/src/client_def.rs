// Copyright (C) 2022 ComposableFi.
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

use crate::{client_state::ClientState, consensus_state::ConsensusState, error::Error};
use ibc::core::ics02_client::{
	client_consensus::ConsensusState as _, client_state::ClientState as _,
};

use crate::{
	client_message::{ClientMessage, RelayChainHeader},
	client_state::{
		AuthoritiesChange, AUTHORITIES_CHANGE_ITEM_LIFETIME, AUTHORITIES_CHANGE_ITEM_MIN_COUNT,
	},
};
use alloc::{format, string::ToString, vec, vec::Vec};
use codec::Decode;
use core::{convert::identity, marker::PhantomData};
use finality_grandpa::Chain;
use grandpa_client_primitives::{
	justification::{
		find_forced_change, find_scheduled_change, AncestryChain, GrandpaJustification,
	},
	ParachainHeadersWithFinalityProof,
};
use ibc::{
	core::{
		ics02_client::{
			client_def::{ClientDef, ConsensusUpdateResult},
			error::Error as Ics02Error,
		},
		ics03_connection::connection::ConnectionEnd,
		ics04_channel::{
			channel::ChannelEnd,
			commitment::{AcknowledgementCommitment, PacketCommitment},
			packet::Sequence,
		},
		ics23_commitment::commitment::{CommitmentPrefix, CommitmentProofBytes, CommitmentRoot},
		ics24_host::{
			identifier::{ChannelId, ClientId, ConnectionId, PortId},
			path::{
				AcksPath, ChannelEndsPath, ClientConsensusStatePath, ClientStatePath,
				CommitmentsPath, ConnectionsPath, ReceiptsPath, SeqRecvsPath,
			},
		},
		ics26_routing::context::ReaderContext,
	},
	timestamp::{Expiry, Timestamp},
	Height,
};
use ibc_proto::google::protobuf::Any;
use light_client_common::{
	state_machine, verify_delay_passed, verify_membership, verify_non_membership,
};
use sp_core::H256;
use sp_runtime::traits::Header;
use sp_trie::StorageProof;
use tendermint_proto::Protobuf;
use vec1::Vec1;

pub const CLIENT_STATE_UPGRADE_PATH: &[u8] = b"client-state-upgrade-path";
pub const CONSENSUS_STATE_UPGRADE_PATH: &[u8] = b"consensus-state-upgrade-path";

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GrandpaClient<T>(PhantomData<T>);

impl<H> ClientDef for GrandpaClient<H>
where
	H: grandpa_client_primitives::HostFunctions<Header = RelayChainHeader>,
{
	type ClientMessage = ClientMessage;
	type ClientState = ClientState<H>;
	type ConsensusState = ConsensusState;

	fn verify_client_message<Ctx: ReaderContext>(
		&self,
		_ctx: &Ctx,
		_client_id: ClientId,
		client_state: Self::ClientState,
		client_message: Self::ClientMessage,
	) -> Result<(), Ics02Error> {
		match client_message {
			ClientMessage::Header(header) => {
				if client_state.para_id as u64 != header.height.revision_number {
					return Err(Error::Custom(format!(
						"Para id mismatch: expected {}, got {}",
						client_state.para_id, header.height.revision_number
					))
					.into())
				}
				let headers_with_finality_proof = ParachainHeadersWithFinalityProof {
					finality_proof: header.finality_proof,
					parachain_headers: header.parachain_headers,
					latest_para_height: header.height.revision_height as u32,
				};

				grandpa_client::verify_parachain_headers_with_grandpa_finality_proof::<
					RelayChainHeader,
					H,
				>(client_state.into(), headers_with_finality_proof)
				.map_err(Error::GrandpaPrimitives)?;
			},
			ClientMessage::Misbehaviour(misbehavior) => {
				let first_proof = misbehavior.first_finality_proof;
				let second_proof = misbehavior.second_finality_proof;

				if first_proof.block == second_proof.block {
					return Err(
						Error::Custom("Misbehaviour proofs are for the same block".into()).into()
					)
				}

				let first_headers =
					AncestryChain::<RelayChainHeader>::new(&first_proof.unknown_headers);
				let first_target =
					first_proof.unknown_headers.iter().max_by_key(|h| *h.number()).ok_or_else(
						|| Error::Custom("Unknown headers can't be empty!".to_string()),
					)?;

				let second_headers =
					AncestryChain::<RelayChainHeader>::new(&second_proof.unknown_headers);
				let second_target =
					second_proof.unknown_headers.iter().max_by_key(|h| *h.number()).ok_or_else(
						|| Error::Custom("Unknown headers can't be empty!".to_string()),
					)?;

				if first_target.hash() != first_proof.block ||
					second_target.hash() != second_proof.block
				{
					return Err(Error::Custom(
						"Misbehaviour proofs are not for the same chain".into(),
					)
					.into())
				}

				let first_base =
					first_proof.unknown_headers.iter().min_by_key(|h| *h.number()).ok_or_else(
						|| Error::Custom("Unknown headers can't be empty!".to_string()),
					)?;
				let first_finalized = first_headers
					.ancestry(first_base.hash(), first_target.hash())
					.map_err(|_| Error::Custom("Invalid ancestry!".to_string()))?;

				let second_base =
					second_proof.unknown_headers.iter().min_by_key(|h| *h.number()).ok_or_else(
						|| Error::Custom("Unknown headers can't be empty!".to_string()),
					)?;
				let second_finalized = second_headers
					.ancestry(second_base.hash(), second_target.hash())
					.map_err(|_| Error::Custom("Invalid ancestry!".to_string()))?;

				let first_parent = first_base.parent_hash;
				let second_parent = second_base.parent_hash;

				if first_parent != second_parent {
					return Err(Error::Custom(
						"Misbehaviour proofs are not for the same ancestor".into(),
					)
					.into())
				}

				let chain_diverges =
					first_finalized.iter().zip(&second_finalized).any(|(a, b)| a != b);
				if !chain_diverges {
					return Err(Error::Custom("Chains should diverge".into()).into())
				}

				// TODO: should we handle genesis block here somehow?
				if !H::contains_relay_header_hash(first_parent) {
					Err(Error::Custom(
						"Could not find the known header for first finality proof".to_string(),
					))?
				}

				let first_justification = GrandpaJustification::<RelayChainHeader>::decode(
					&mut &first_proof.justification[..],
				)
				.map_err(|_| Error::Custom("Could not decode first justification".to_string()))?;
				let second_justification = GrandpaJustification::<RelayChainHeader>::decode(
					&mut &second_proof.justification[..],
				)
				.map_err(|_| Error::Custom("Could not decode second justification".to_string()))?;

				if first_proof.block != first_justification.commit.target_hash ||
					second_proof.block != second_justification.commit.target_hash
				{
					Err(Error::Custom(
						"First or second finality proof block hash does not match justification target hash"
							.to_string(),
					))?
				}

				// we don't know which of the number is canonical, so we will try to verify both
				// if the two bases are not equal
				let base_numbers = if first_base.number == second_base.number {
					vec![first_base.number]
				} else {
					vec![first_base.number, second_base.number]
				};
				for base_number in base_numbers {
					// we can't trust block numbers, because they may be changed arbitrary
					let first_height = base_number + first_finalized.len() as u32 - 1;
					let second_height = base_number + second_finalized.len() as u32 - 1;

					let get_authorities = |height| {
						let key = client_state
							.authorities_changes
							.binary_search_by_key(&height, |x| x.height)
							.unwrap_or_else(identity);
						client_state
							.authorities_changes
							.get(key)
							.map(|change| (change.set_id, &change.authorities))
							.unwrap_or_else(|| {
								let change = client_state.authorities_changes.last();
								(change.set_id, &change.authorities)
							})
					};
					let (first_set_id, first_current_authorities) = get_authorities(first_height);
					let (second_set_id, second_current_authorities) =
						get_authorities(second_height);

					let first_valid = first_justification
						.verify::<H>(first_set_id, first_current_authorities)
						.is_ok();
					let second_valid = second_justification
						.verify::<H>(second_set_id, second_current_authorities)
						.is_ok();

					// whoops equivocation is valid.
					if first_valid && second_valid {
						return Ok(())
					}
				}

				return Err(Error::Custom("Invalid justification".to_string()).into())
			},
		}

		Ok(())
	}

	fn update_state<Ctx: ReaderContext>(
		&self,
		ctx: &Ctx,
		client_id: ClientId,
		mut client_state: Self::ClientState,
		client_message: Self::ClientMessage,
	) -> Result<(Self::ClientState, ConsensusUpdateResult<Ctx>), Ics02Error> {
		let header = match client_message {
			ClientMessage::Header(header) => header,
			_ => unreachable!(
				"02-client will check for misbehaviour before calling update_state; qed"
			),
		};
		let ancestry =
			AncestryChain::<RelayChainHeader>::new(&header.finality_proof.unknown_headers);
		let mut consensus_states = vec![];

		let from = client_state.latest_relay_hash;

		let finalized = ancestry
			.ancestry(from, header.finality_proof.block)
			.map_err(|_| Error::Custom(format!("[update_state] Invalid ancestry!")))?;

		let mut finalized_sorted = finalized.clone();
		finalized_sorted.sort();

		for (relay_hash, parachain_header_proof) in header.parachain_headers {
			// we really shouldn't set consensus states for parachain headers not in the finalized
			// chain.
			if finalized_sorted.binary_search(&relay_hash).is_err() {
				continue
			}

			let header = ancestry.header(&relay_hash).ok_or_else(|| {
				Error::Custom(format!("No relay chain header found for hash: {relay_hash:?}"))
			})?;

			let (height, consensus_state) = ConsensusState::from_header::<H>(
				parachain_header_proof,
				client_state.para_id,
				header.state_root.clone(),
			)?;

			// Skip duplicate consensus states
			if ctx.consensus_state(&client_id, height).is_ok() {
				continue
			}

			let wrapped = Ctx::AnyConsensusState::wrap(&consensus_state)
				.expect("AnyConsenusState is type checked; qed");
			consensus_states.push((height, wrapped));
		}

		// updates
		let target = ancestry
			.header(&header.finality_proof.block)
			.expect("target header has already been checked in verify_client_message; qed");

		// check that the block number is correct, because it will be used later for
		// finding the authorities set
		let expected_target_height = client_state.latest_relay_height + finalized.len() as u32 - 1;
		if expected_target_height != target.number {
			return Err(Error::Custom(format!(
				"[update_state] unexpected block number: {expected_target_height} != {}",
				target.number
			)))?
		}

		// can't try to rewind relay chain
		if target.number <= client_state.latest_relay_height {
			Err(Ics02Error::implementation_specific(format!(
				"Light client can only be updated to new relay chain height."
			)))?
		}

		let mut heights = consensus_states
			.iter()
			.map(|(h, ..)| {
				// this cast is safe, see [`ConsensusState::from_header`]
				h.revision_height as u32
			})
			.collect::<Vec<_>>();

		heights.sort();

		if let Some((min_height, max_height)) = heights.first().zip(heights.last()) {
			// can't try to rewind parachain.
			if *min_height <= client_state.latest_para_height {
				Err(Ics02Error::implementation_specific(format!(
					"Light client can only be updated to new parachain height."
				)))?
			}
			client_state.latest_para_height = *max_height
		}

		client_state.latest_relay_hash = header.finality_proof.block;
		client_state.latest_relay_height = target.number;

		if let Some(scheduled_change) = find_scheduled_change(target) {
			let now = ctx.host_timestamp();
			let len = client_state.authorities_changes.len();
			let next_set_id = client_state.last_set_id() + 1;
			let mut xs = client_state
				.authorities_changes
				.into_iter()
				.enumerate()
				.filter(|(i, x)| {
					// we keep at least AUTHORITIES_CHANGE_ITEM_MIN_COUNT changes
					if len - i < AUTHORITIES_CHANGE_ITEM_MIN_COUNT {
						return true
					}
					// prune expired changes
					!matches!(
						now.check_expiry(
							&(x.timestamp + AUTHORITIES_CHANGE_ITEM_LIFETIME)
								.unwrap_or_else(|_| Timestamp::from_nanoseconds(1).unwrap()) // do not remove the entry on overflow, because it's a sign that something went wrong
						),
						Expiry::Expired
					)
				})
				.map(|(_, x)| x)
				.collect::<Vec<_>>();
			xs.push(AuthoritiesChange {
				height: target.number + scheduled_change.delay + 1, /* we start using the set id
				                                                     * at the next block + delay */
				timestamp: now,
				set_id: next_set_id,
				authorities: scheduled_change.next_authorities,
			});
			xs.sort_by_key(|change| change.height);
			client_state.authorities_changes =
				Vec1::try_from_vec(xs).expect("we've just added one item to the vector above; qed");
		}

		let now = ctx.host_timestamp();
		let now_ms = now.nanoseconds() / 1_000_000;
		H::insert_relay_header_hashes(now_ms, &finalized);
		Ok((client_state, ConsensusUpdateResult::Batch(consensus_states)))
	}

	fn update_state_on_misbehaviour(
		&self,
		mut client_state: Self::ClientState,
		_client_message: Self::ClientMessage,
	) -> Result<Self::ClientState, Ics02Error> {
		client_state.frozen_height =
			Some(Height::new(client_state.para_id as u64, client_state.latest_para_height as u64));
		Ok(client_state)
	}

	fn check_for_misbehaviour<Ctx: ReaderContext>(
		&self,
		ctx: &Ctx,
		client_id: ClientId,
		client_state: Self::ClientState,
		client_message: Self::ClientMessage,
	) -> Result<bool, Ics02Error> {
		if matches!(client_message, ClientMessage::Misbehaviour(_)) {
			return Ok(true)
		}

		// we also check that this update doesn't include competing consensus states for heights we
		// already processed.
		let header = match client_message {
			ClientMessage::Header(header) => header,
			_ => unreachable!("We've checked for misbehavior in line 180; qed"),
		};
		//forced authority set change is handled as a misbehaviour

		let ancestry =
			AncestryChain::<RelayChainHeader>::new(&header.finality_proof.unknown_headers);

		for (relay_hash, parachain_header_proof) in header.parachain_headers {
			let header = ancestry.header(&relay_hash).ok_or_else(|| {
				Error::Custom(format!("No relay chain header found for hash: {relay_hash:?}"))
			})?;

			if find_forced_change(header).is_some() {
				return Ok(true)
			}

			let (height, consensus_state) = ConsensusState::from_header::<H>(
				parachain_header_proof,
				client_state.para_id,
				header.state_root.clone(),
			)?;

			match ctx.maybe_consensus_state(&client_id, height)? {
				Some(cs) => {
					let cs: ConsensusState = cs
						.downcast()
						.ok_or(Ics02Error::client_args_type_mismatch(client_state.client_type()))?;

					if cs != consensus_state {
						// Houston we have a problem
						return Ok(true)
					}
				},
				None => {},
			};
		}

		Ok(false)
	}

	fn verify_upgrade_and_update_state<Ctx: ReaderContext>(
		&self,
		ctx: &Ctx,
		client_id: ClientId,
		old_client_state: &Self::ClientState,
		upgrade_client_state: &Self::ClientState,
		upgrade_consensus_state: &Self::ConsensusState,
		proof_upgrade_client: Vec<u8>,
		proof_upgrade_consensus_state: Vec<u8>,
	) -> Result<(Self::ClientState, ConsensusUpdateResult<Ctx>), Ics02Error> {
		use prost::Message;

		let height = old_client_state.latest_height();

		if upgrade_client_state.latest_height() <= height {
			return Err(Ics02Error::implementation_specific(format!(
				"Upgrade client state height must be greater than current client state height: {} <= {height}", upgrade_client_state.latest_height()
			)))
		}

		let consenus_state = ctx.consensus_state(&client_id, height)?
			.downcast::<Self::ConsensusState>()
			.ok_or_else(|| Error::Custom(format!("Wrong consensus state type stored for Grandpa client with {client_id} at {height}")))?;

		let root = H256::from_slice(consenus_state.root.as_bytes());

		// verify client state upgrade proof
		{
			let proof_upgrade_client = {
				let nodes: Vec<Vec<u8>> =
					Decode::decode(&mut &proof_upgrade_client[..]).map_err(Error::Codec)?;
				StorageProof::new(nodes)
			};

			let encoded = Ctx::AnyClientState::wrap(&upgrade_client_state.clone())
				.expect("AnyConsensusState is type-checked; qed")
				.encode_to_vec()
				.map_err(Ics02Error::encode)?;

			let value = state_machine::read_proof_check::<H::BlakeTwo256, _>(
				&root,
				proof_upgrade_client,
				vec![CLIENT_STATE_UPGRADE_PATH],
			)
			.map_err(|err| Error::Custom(format!("{err}")))?
			.remove(CLIENT_STATE_UPGRADE_PATH)
			.flatten()
			.ok_or_else(|| Error::Custom(format!("Invalid proof for client state upgrade")))?;
			let value = Any::decode(&mut &value[..])
				.map_err(|e| {
					Error::Custom(format!("Invalid proof for consensus state upgrade: {e}"))
				})?
				.value;

			let value_s = hex::encode(&value);
			let encoded_s = hex::encode(&encoded);

			if value != encoded {
				Err(Error::Custom(format!("Invalid proof for client state upgrade: values are not equal {value_s} != {encoded_s}")))?
			}
		}

		// verify consensus state upgrade proof
		{
			let proof_upgrade_consensus_state = {
				let nodes: Vec<Vec<u8>> = Decode::decode(&mut &proof_upgrade_consensus_state[..])
					.map_err(Error::Codec)?;
				StorageProof::new(nodes)
			};

			let encoded = Ctx::AnyConsensusState::wrap(upgrade_consensus_state)
				.expect("AnyConsensusState is type-checked; qed")
				.encode_to_vec()
				.map_err(Ics02Error::encode)?;

			let value = state_machine::read_proof_check::<H::BlakeTwo256, _>(
				&root,
				proof_upgrade_consensus_state,
				vec![CONSENSUS_STATE_UPGRADE_PATH],
			)
			.map_err(|err| Error::Custom(format!("{err}")))?
			.remove(CONSENSUS_STATE_UPGRADE_PATH)
			.flatten()
			.ok_or_else(|| Error::Custom(format!("Invalid proof for consensus state upgrade")))?;
			let value = Any::decode(&mut &value[..])
				.map_err(|e| {
					Error::Custom(format!("Invalid proof for consensus state upgrade: {e}"))
				})?
				.value;

			let value_s = hex::encode(&value);
			let encoded_s = hex::encode(&encoded);

			if value != encoded {
				Err(Error::Custom(format!("Invalid proof for consensus state upgrade: values are not equal {value_s} != {encoded_s}")))?
			}
		}

		let mixed_upgrade_client_state = ClientState::<H> {
			relay_chain: old_client_state.relay_chain,
			latest_relay_height: upgrade_client_state.latest_relay_height,
			latest_relay_hash: upgrade_client_state.latest_relay_hash,
			frozen_height: None,
			latest_para_height: upgrade_client_state.latest_para_height,
			para_id: upgrade_client_state.para_id,
			authorities_changes: upgrade_client_state.authorities_changes.clone(),
			_phantom: Default::default(),
		};

		Ok((
			mixed_upgrade_client_state,
			ConsensusUpdateResult::Single(
				Ctx::AnyConsensusState::wrap(upgrade_consensus_state)
					.expect("AnyConsensusState is type-checked; qed"),
			),
		))
	}

	/// Will try to update the client with the state of the substitute.
	///
	/// The following must always be true:
	///   - The substitute client is the same type as the subject client
	///   - The subject and substitute client states match in all parameters (expect `relay_chain`,
	/// `para_id`, `latest_para_height`, `latest_relay_height`, `latest_relay_hash`,
	/// `frozen_height`, `latest_para_height`, `current_set_id` and `current_authorities`).
	fn check_substitute_and_update_state<Ctx: ReaderContext>(
		&self,
		_ctx: &Ctx,
		_subject_client_id: ClientId,
		_substitute_client_id: ClientId,
		_old_client_state: Self::ClientState,
		_substitute_client_state: Self::ClientState,
	) -> Result<(Self::ClientState, ConsensusUpdateResult<Ctx>), Ics02Error> {
		unimplemented!("check_substitute_and_update_state not implemented for Grandpa client")
	}

	fn verify_client_consensus_state<Ctx: ReaderContext>(
		&self,
		_ctx: &Ctx,
		client_state: &Self::ClientState,
		height: Height,
		prefix: &CommitmentPrefix,
		proof: &CommitmentProofBytes,
		root: &CommitmentRoot,
		client_id: &ClientId,
		consensus_height: Height,
		expected_consensus_state: &Ctx::AnyConsensusState,
	) -> Result<(), Ics02Error> {
		client_state.verify_height(height)?;
		let path = ClientConsensusStatePath {
			client_id: client_id.clone(),
			epoch: consensus_height.revision_number,
			height: consensus_height.revision_height,
		};
		let value = expected_consensus_state.encode_to_vec().map_err(Ics02Error::encode)?;
		verify_membership::<H::BlakeTwo256, _>(prefix, proof, root, path, value)
			.map_err(Error::Anyhow)?;
		Ok(())
	}

	fn verify_connection_state<Ctx: ReaderContext>(
		&self,
		_ctx: &Ctx,
		_client_id: &ClientId,
		client_state: &Self::ClientState,
		height: Height,
		prefix: &CommitmentPrefix,
		proof: &CommitmentProofBytes,
		root: &CommitmentRoot,
		connection_id: &ConnectionId,
		expected_connection_end: &ConnectionEnd,
	) -> Result<(), Ics02Error> {
		client_state.verify_height(height)?;
		let path = ConnectionsPath(connection_id.clone());
		let value = expected_connection_end.encode_vec().map_err(Ics02Error::encode)?;
		verify_membership::<H::BlakeTwo256, _>(prefix, proof, root, path, value)
			.map_err(Error::Anyhow)?;
		Ok(())
	}

	fn verify_channel_state<Ctx: ReaderContext>(
		&self,
		_ctx: &Ctx,
		_client_id: &ClientId,
		client_state: &Self::ClientState,
		height: Height,
		prefix: &CommitmentPrefix,
		proof: &CommitmentProofBytes,
		root: &CommitmentRoot,
		port_id: &PortId,
		channel_id: &ChannelId,
		expected_channel_end: &ChannelEnd,
	) -> Result<(), Ics02Error> {
		client_state.verify_height(height)?;
		let path = ChannelEndsPath(port_id.clone(), *channel_id);
		let value = expected_channel_end.encode_vec().map_err(Ics02Error::encode)?;
		verify_membership::<H::BlakeTwo256, _>(prefix, proof, root, path, value)
			.map_err(Error::Anyhow)?;
		Ok(())
	}

	fn verify_client_full_state<Ctx: ReaderContext>(
		&self,
		_ctx: &Ctx,
		client_state: &Self::ClientState,
		height: Height,
		prefix: &CommitmentPrefix,
		proof: &CommitmentProofBytes,
		root: &CommitmentRoot,
		client_id: &ClientId,
		expected_client_state: &Ctx::AnyClientState,
	) -> Result<(), Ics02Error> {
		client_state.verify_height(height)?;
		let path = ClientStatePath(client_id.clone());
		let value = expected_client_state.encode_to_vec().map_err(Ics02Error::encode)?;
		verify_membership::<H::BlakeTwo256, _>(prefix, proof, root, path, value)
			.map_err(Error::Anyhow)?;
		Ok(())
	}

	fn verify_packet_data<Ctx: ReaderContext>(
		&self,
		ctx: &Ctx,
		_client_id: &ClientId,
		client_state: &Self::ClientState,
		height: Height,
		connection_end: &ConnectionEnd,
		proof: &CommitmentProofBytes,
		root: &CommitmentRoot,
		port_id: &PortId,
		channel_id: &ChannelId,
		sequence: Sequence,
		commitment: PacketCommitment,
	) -> Result<(), Ics02Error> {
		client_state.verify_height(height)?;
		verify_delay_passed::<H, _>(ctx, height, connection_end).map_err(Error::Anyhow)?;

		let commitment_path =
			CommitmentsPath { port_id: port_id.clone(), channel_id: *channel_id, sequence };

		verify_membership::<H::BlakeTwo256, _>(
			connection_end.counterparty().prefix(),
			proof,
			root,
			commitment_path,
			commitment.into_vec(),
		)
		.map_err(Error::Anyhow)?;
		Ok(())
	}

	fn verify_packet_acknowledgement<Ctx: ReaderContext>(
		&self,
		ctx: &Ctx,
		_client_id: &ClientId,
		client_state: &Self::ClientState,
		height: Height,
		connection_end: &ConnectionEnd,
		proof: &CommitmentProofBytes,
		root: &CommitmentRoot,
		port_id: &PortId,
		channel_id: &ChannelId,
		sequence: Sequence,
		ack: AcknowledgementCommitment,
	) -> Result<(), Ics02Error> {
		client_state.verify_height(height)?;
		verify_delay_passed::<H, _>(ctx, height, connection_end).map_err(Error::Anyhow)?;

		let ack_path = AcksPath { port_id: port_id.clone(), channel_id: *channel_id, sequence };
		verify_membership::<H::BlakeTwo256, _>(
			connection_end.counterparty().prefix(),
			proof,
			root,
			ack_path,
			ack.into_vec(),
		)
		.map_err(Error::Anyhow)?;
		Ok(())
	}

	fn verify_next_sequence_recv<Ctx: ReaderContext>(
		&self,
		ctx: &Ctx,
		_client_id: &ClientId,
		client_state: &Self::ClientState,
		height: Height,
		connection_end: &ConnectionEnd,
		proof: &CommitmentProofBytes,
		root: &CommitmentRoot,
		port_id: &PortId,
		channel_id: &ChannelId,
		sequence: Sequence,
	) -> Result<(), Ics02Error> {
		client_state.verify_height(height)?;
		verify_delay_passed::<H, _>(ctx, height, connection_end).map_err(Error::Anyhow)?;

		let seq_bytes = codec::Encode::encode(&u64::from(sequence));

		let seq_path = SeqRecvsPath(port_id.clone(), *channel_id);
		verify_membership::<H::BlakeTwo256, _>(
			connection_end.counterparty().prefix(),
			proof,
			root,
			seq_path,
			seq_bytes,
		)
		.map_err(Error::Anyhow)?;
		Ok(())
	}

	fn verify_packet_receipt_absence<Ctx: ReaderContext>(
		&self,
		ctx: &Ctx,
		_client_id: &ClientId,
		client_state: &Self::ClientState,
		height: Height,
		connection_end: &ConnectionEnd,
		proof: &CommitmentProofBytes,
		root: &CommitmentRoot,
		port_id: &PortId,
		channel_id: &ChannelId,
		sequence: Sequence,
	) -> Result<(), Ics02Error> {
		client_state.verify_height(height)?;
		verify_delay_passed::<H, _>(ctx, height, connection_end).map_err(Error::Anyhow)?;

		let receipt_path =
			ReceiptsPath { port_id: port_id.clone(), channel_id: *channel_id, sequence };
		verify_non_membership::<H::BlakeTwo256, _>(
			connection_end.counterparty().prefix(),
			proof,
			root,
			receipt_path,
		)
		.map_err(Error::Anyhow)?;
		Ok(())
	}
}
