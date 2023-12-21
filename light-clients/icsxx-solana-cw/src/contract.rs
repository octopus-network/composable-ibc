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

use crate::{
	context::Context,
	error::ContractError,
	helpers::{
		check_substitute_and_update_state, prune_oldest_consensus_state, verify_delay_passed,
		verify_upgrade_and_update_state,
	},
	ics23::ReadonlyProcessedStates,
	msg::{
		CheckForMisbehaviourMsg, ContractResult, ExecuteMsg, ExportMetadataMsg, InstantiateMsg,
		QueryMsg, QueryResponse, StatusMsg, UpdateStateMsg, UpdateStateOnMisbehaviourMsg,
		VerifyClientMessage, VerifyMembershipMsg, VerifyNonMembershipMsg,
		VerifyUpgradeAndUpdateStateMsg,
	},
	state::{get_client_state, get_consensus_state},
};
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{to_binary, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdResult};
use ed25519_consensus::VerificationKey;
use ibc::core::{
	ics02_client::{
		client_def::{ClientDef, ConsensusUpdateResult},
		context::{ClientKeeper, ClientReader},
	},
	ics24_host::identifier::ClientId,
};
use ics07_tendermint::{
	client_def::{verify_membership, verify_non_membership, TendermintClient},
	HostFunctionsProvider,
};
use ics08_wasm::SUBJECT_PREFIX;
use sha2::{Digest, Sha256};
use std::str::FromStr;
use tendermint::{
	crypto::{
		signature::{Error as TendermintCryptoError, Verifier},
		Sha256 as TendermintSha256,
	},
	merkle::{Hash, MerkleHash, NonIncremental, HASH_SIZE},
	PublicKey, Signature,
};
use tendermint_light_client_verifier::operations::CommitValidator;

#[derive(Clone, Copy, Debug, PartialEq, Default, Eq)]
pub struct HostFunctions;

impl ics23::HostFunctionsProvider for HostFunctions {
	fn sha2_256(message: &[u8]) -> [u8; 32] {
		let mut hasher = Sha256::default();
		hasher.update(message);
		hasher.finalize().as_slice().try_into().expect("slice with incorrect length")
	}

	fn sha2_512(_message: &[u8]) -> [u8; 64] {
		unimplemented!()
	}

	fn sha2_512_truncated(_message: &[u8]) -> [u8; 32] {
		unimplemented!()
	}

	fn sha3_512(_message: &[u8]) -> [u8; 64] {
		unimplemented!()
	}

	fn ripemd160(_message: &[u8]) -> [u8; 20] {
		unimplemented!()
	}
}

impl TendermintSha256 for HostFunctions {
	fn digest(data: impl AsRef<[u8]>) -> [u8; HASH_SIZE] {
		<Self as ics23::HostFunctionsProvider>::sha2_256(data.as_ref())
	}
}

impl MerkleHash for HostFunctions {
	fn empty_hash(&mut self) -> Hash {
		NonIncremental::<Self>::default().empty_hash()
	}

	fn leaf_hash(&mut self, bytes: &[u8]) -> Hash {
		NonIncremental::<Self>::default().leaf_hash(bytes)
	}

	fn inner_hash(&mut self, left: Hash, right: Hash) -> Hash {
		NonIncremental::<Self>::default().inner_hash(left, right)
	}
}

impl Verifier for HostFunctions {
	fn verify(
		pubkey: PublicKey,
		msg: &[u8],
		signature: &Signature,
	) -> Result<(), TendermintCryptoError> {
		let vk = pubkey.ed25519().expect("");
		let pubkey2 = VerificationKey::try_from(vk.as_bytes())
			.map_err(|_| TendermintCryptoError::MalformedPublicKey)?;
		let sig = ed25519_consensus::Signature::try_from(signature.as_bytes())
			.map_err(|_| TendermintCryptoError::MalformedSignature)?;
		pubkey2.verify(&sig, msg).map_err(|_| TendermintCryptoError::VerificationFailed)
	}
}
impl CommitValidator for HostFunctions {}
impl HostFunctionsProvider for HostFunctions {}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
	deps: DepsMut,
	env: Env,
	_info: MessageInfo,
	_msg: InstantiateMsg,
) -> Result<Response, ContractError> {
	let _client = TendermintClient::<HostFunctions>::default();
	let mut ctx = Context::<HostFunctions>::new(deps, env);
	let client_id = ClientId::from_str("08-wasm-0").expect("client id is valid");
	let client_state = ctx
		.client_state(&client_id)
		.map_err(|e| ContractError::Tendermint(e.to_string()))?;
	ctx.store_update_height(client_id.clone(), client_state.latest_height, ctx.host_height())
		.map_err(|e| ContractError::Tendermint(e.to_string()))?;
	ctx.store_update_time(client_id, client_state.latest_height, ctx.host_timestamp())
		.map_err(|e| ContractError::Tendermint(e.to_string()))?;

	Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
	deps: DepsMut,
	env: Env,
	_info: MessageInfo,
	msg: ExecuteMsg,
) -> Result<Response, ContractError> {
	let client = TendermintClient::<HostFunctions>::default();
	let mut ctx = Context::<HostFunctions>::new(deps, env);
	let client_id = ClientId::from_str("08-wasm-0").expect("client id is valid");
	let data = process_message(msg, client, &mut ctx, client_id)?;
	let mut response = Response::default();
	response.data = Some(data);
	Ok(response)
}

fn process_message(
	msg: ExecuteMsg,
	client: TendermintClient<HostFunctions>,
	ctx: &mut Context<HostFunctions>,
	client_id: ClientId,
) -> Result<Binary, ContractError> {
	//log!(ctx, "process_message: {:?}", msg);
	let result = match msg {
		ExecuteMsg::VerifyMembership(msg) => {
			Ok(()).map(|_| to_binary(&ContractResult::success()))
		},
		ExecuteMsg::VerifyNonMembership(msg) => {
			Ok(()).map(|_| to_binary(&ContractResult::success()))
		},
		ExecuteMsg::VerifyClientMessage(msg) => {
			Ok(()).map(|_| to_binary(&ContractResult::success()))
		},
		ExecuteMsg::CheckForMisbehaviour(msg) => {
			unimplemented!("Not needed for now")
		},
		ExecuteMsg::UpdateStateOnMisbehaviour(msg_raw) => {
			unimplemented!("Not needed for now")
		},
		ExecuteMsg::UpdateState(msg_raw) => {
			// To be implementted
			let client_state = ctx
				.client_state(&client_id)
				.map_err(|e| ContractError::Tendermint(e.to_string()))?;
			let msg = UpdateStateMsg::try_from(msg_raw)?;
			let latest_revision_height = client_state.latest_height().revision_height;
			prune_oldest_consensus_state(ctx, &client_state, ctx.host_timestamp().nanoseconds());
			client
				.update_state(ctx, client_id.clone(), client_state, msg.client_message)
				.map_err(|e| ContractError::Tendermint(e.to_string()))
				.and_then(|(cs, cu)| {
					let height = cs.latest_height();
					match cu {
						ConsensusUpdateResult::Single(cs) => {
							ctx.store_consensus_state(client_id.clone(), height, cs)
								.map_err(|e| ContractError::Tendermint(e.to_string()))?;
						},
						ConsensusUpdateResult::Batch(css) =>
							for (height, cs) in css {
								ctx.store_consensus_state(client_id.clone(), height, cs)
									.map_err(|e| ContractError::Tendermint(e.to_string()))?;
							},
					}
					if cs.latest_height().revision_height > latest_revision_height {
						ctx.store_client_state(client_id, cs)
							.map_err(|e| ContractError::Tendermint(e.to_string()))?;
					}
					Ok(to_binary(&ContractResult::success()))
				})
		},
		ExecuteMsg::CheckSubstituteAndUpdateState(_msg) =>
			Ok(()).map(|_| to_binary(&ContractResult::success())),
		ExecuteMsg::VerifyUpgradeAndUpdateState(msg) => 
			Ok(()).map(|_| to_binary(&ContractResult::success()))
	};
	Ok(result??)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
	let client_id = ClientId::from_str("08-wasm-0").expect("client id is valid");
	//deps.api.debug("In query");
	match msg {
		QueryMsg::ClientTypeMsg(_) => unimplemented!("ClientTypeMsg"),
		QueryMsg::GetLatestHeightsMsg(_) => unimplemented!("GetLatestHeightsMsg"),
		QueryMsg::ExportMetadata(ExportMetadataMsg {}) => {
			let ro_proceeded_state = ReadonlyProcessedStates::new(deps.storage);
			to_binary(&QueryResponse::genesis_metadata(ro_proceeded_state.get_metadata()))
		},
		QueryMsg::Status(StatusMsg {}) => 
			to_binary(&QueryResponse::status("Active".to_string()))
	}
}
