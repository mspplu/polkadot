// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! The Statement Distribution Subsystem.
//!
//! This is responsible for distributing signed statements about candidate
//! validity amongst validators.

use polkadot_subsystem::{
	Subsystem, SubsystemResult, SubsystemContext, SpawnedSubsystem,
	FromOverseer, OverseerSignal,
};
use polkadot_subsystem::messages::{
	AllMessages, NetworkBridgeMessage, NetworkBridgeEvent, StatementDistributionMessage,
	PeerId, ReputationChange as Rep, CandidateBackingMessage, RuntimeApiMessage,
	RuntimeApiRequest,
};
use node_primitives::{ProtocolId, View, SignedFullStatement};
use polkadot_primitives::Hash;
use polkadot_primitives::parachain::{
	CompactStatement, ValidatorIndex, ValidatorId, SigningContext, ValidatorSignature,
};
use parity_scale_codec::{Encode, Decode};

use futures::prelude::*;
use futures::channel::oneshot;

use std::collections::{HashMap, HashSet};

const PROTOCOL_V1: ProtocolId = *b"sdn1";

const COST_UNEXPECTED_STATEMENT: Rep = Rep::new(-100, "Unexpected Statement");
const COST_INVALID_SIGNATURE: Rep = Rep::new(-500, "Invalid Statement Signature");
const COST_INVALID_MESSAGE: Rep = Rep::new(-500, "Invalid message");
const COST_DUPLICATE_STATEMENT: Rep = Rep::new(-250, "Statement sent more than once by peer");
const COST_APPARENT_FLOOD: Rep = Rep::new(-1000, "Peer appears to be flooding us with statements");

const BENEFIT_VALID_STATEMENT: Rep = Rep::new(5, "Peer provided a valid statement");
const BENEFIT_VALID_STATEMENT_FIRST: Rep = Rep::new(
	25,
	"Peer was the first to provide a valid statement",
);

/// The maximum amount of candidates each validator is allowed to second at any relay-parent.
/// Short for "Validator Candidate Threshold".
///
/// This is the amount of candidates we keep per validator at any relay-parent.
/// Typically we will only keep 1, but when a validator equivocates we will need to track 2.
const VC_THRESHOLD: usize = 2;

/// The statement distribution subsystem.
pub struct StatementDistribution;

impl<C> Subsystem<C> for StatementDistribution
	where C: SubsystemContext<Message=StatementDistributionMessage>
{
	fn start(self, ctx: C) -> SpawnedSubsystem {
		// Swallow error because failure is fatal to the node and we log with more precision
		// within `run`.
		SpawnedSubsystem(run(ctx).map(|_| ()).boxed())
	}
}

fn network_update_message(n: NetworkBridgeEvent) -> AllMessages {
	AllMessages::StatementDistribution(StatementDistributionMessage::NetworkBridgeUpdate(n))
}

/// Tracks our impression of a single peer's view of the candidates a validator has seconded
/// for a given relay-parent.
///
/// It is expected to receive at most `VC_THRESHOLD` from us and be aware of at most `VC_THRESHOLD`
/// via other means.
#[derive(Default)]
struct VcPerPeerTracker {
	local_observed: arrayvec::ArrayVec<[Hash; VC_THRESHOLD]>,
	remote_observed: arrayvec::ArrayVec<[Hash; VC_THRESHOLD]>,
}

impl VcPerPeerTracker {
	// Note that the remote should now be aware that a validator has seconded a given candidate (by hash)
	// based on a message that we have sent it from our local pool.
	fn note_local(&mut self, h: Hash) {
		if !note_hash(&mut self.local_observed, h) {
			log::warn!("Statement distribution is erroneously attempting to distribute more \
				than {} candidate(s) per validator index. Ignoring", VC_THRESHOLD);
		}
	}

	// Note that the remote should now be aware that a validator has seconded a given candidate (by hash)
	// based on a message that it has sent us.
	//
	// Returns `true` if the peer was allowed to send us such a message, `false` otherwise.
	fn note_remote(&mut self, h: Hash) -> bool {
		note_hash(&mut self.remote_observed, h)
	}
}

fn note_hash(
	observed: &mut arrayvec::ArrayVec<[Hash; VC_THRESHOLD]>,
	h: Hash,
) -> bool {
	if observed.contains(&h) { return true; }

	if observed.is_full() {
		false
	} else {
		observed.try_push(h).expect("length of storage guarded above; \
			only panics if length exceeds capacity; qed");

		true
	}
}

// knowledge that a peer has about goings-on in a relay parent.
#[derive(Default)]
struct PeerRelayParentKnowledge {
	// candidates that the peer is aware of. This indicates that we can
	// send other statements pertaining to that candidate.
	known_candidates: HashSet<Hash>,
	// fingerprints of all statements a peer should be aware of: those that
	// were sent to the peer by us.
	sent_statements: HashSet<(CompactStatement, ValidatorIndex)>,
	// fingerprints of all statements a peer should be aware of: those that
	// were sent to us by the peer.
	received_statements: HashSet<(CompactStatement, ValidatorIndex)>,
	// How many candidates this peer is aware of for each given validator index.
	seconded_counts: HashMap<ValidatorIndex, VcPerPeerTracker>,
	// How many statements we've received for each candidate that we're aware of.
	received_message_count: HashMap<Hash, usize>,
}

impl PeerRelayParentKnowledge {
	/// Attempt to update our view of the peer's knowledge with this statement's fingerprint based
	/// on something that we would like to send to the peer.
	///
	/// This returns `None` if the peer cannot accept this statement, without altering internal
	/// state.
	///
	/// If the peer can accept the statement, this returns `Some` and updates the internal state.
	/// Once the knowledge has incorporated a statement, it cannot be incorporated again.
	///
	/// This returns `Some(true)` if this is the first time the peer has become aware of a
	/// candidate with the given hash.
	fn send(&mut self, fingerprint: &(CompactStatement, ValidatorIndex)) -> Option<bool> {
		let already_known = self.sent_statements.contains(fingerprint)
			|| self.received_statements.contains(fingerprint);

		if already_known {
			return None;
		}

		let new_known = match fingerprint.0 {
			CompactStatement::Candidate(ref h) => {
				self.seconded_counts.entry(fingerprint.1)
					.or_default()
					.note_local(h.clone());

				self.known_candidates.insert(h.clone())
			},
			CompactStatement::Valid(ref h) | CompactStatement::Invalid(ref h) => {
				// The peer can only accept Valid and Invalid statements for which it is aware
				// of the corresponding candidate.
				if !self.known_candidates.contains(h) {
					return None;
				}

				false
			}
		};

		self.sent_statements.insert(fingerprint.clone());

		Some(new_known)
	}

	/// Attempt to update our view of the peer's knowledge with this statement's fingerprint based on
	/// a message we are receiving from the peer.
	///
	/// Provide the maximum message count that we can receive per candidate. In practice we should
	/// not receive more statements for any one candidate than there are members in the group assigned
	/// to that para, but this maximum needs to be lenient to account for equivocations that may be
	/// cross-group. As such, a maximum of 2 * n_validators is recommended.
	///
	/// This returns an error if the peer should not have sent us this message according to protocol
	/// rules for flood protection.
	///
	/// If this returns `Ok`, the internal state has been altered. After `receive`ing a new
	/// candidate, we are then cleared to send the peer further statements about that candidate.
	///
	/// This returns `Ok(true)` if this is the first time the peer has become aware of a
	/// candidate with given hash.
	fn receive(
		&mut self,
		fingerprint: &(CompactStatement, ValidatorIndex),
		max_message_count: usize,
	) -> Result<bool, Rep> {
		// We don't check `sent_statements` because a statement could be in-flight from both
		// sides at the same time.
		if self.received_statements.contains(fingerprint) {
			return Err(COST_DUPLICATE_STATEMENT);
		}

		let candidate_hash = match fingerprint.0 {
			CompactStatement::Candidate(ref h) => {
				let allowed_remote = self.seconded_counts.entry(fingerprint.1)
					.or_insert_with(Default::default)
					.note_remote(h.clone());

				if !allowed_remote {
					return Err(COST_UNEXPECTED_STATEMENT);
				}

				h
			}
			CompactStatement::Valid(ref h)| CompactStatement::Invalid(ref h) => {
				if !self.known_candidates.contains(&h) {
					return Err(COST_UNEXPECTED_STATEMENT);
				}

				h
			}
		};

		{
			let received_per_candidate = self.received_message_count
				.entry(candidate_hash.clone())
				.or_insert(0);

			if *received_per_candidate + 1 >= max_message_count {
				return Err(COST_APPARENT_FLOOD);
			}

			*received_per_candidate += 1;
		}

		self.received_statements.insert(fingerprint.clone());
		Ok(self.known_candidates.insert(candidate_hash.clone()))
	}
}

struct PeerData {
	view: View,
	view_knowledge: HashMap<Hash, PeerRelayParentKnowledge>,
}

impl PeerData {
	/// Attempt to update our view of the peer's knowledge with this statement's fingerprint based
	/// on something that we would like to send to the peer.
	///
	/// This returns `None` if the peer cannot accept this statement, without altering internal
	/// state.
	///
	/// If the peer can accept the statement, this returns `Some` and updates the internal state.
	/// Once the knowledge has incorporated a statement, it cannot be incorporated again.
	///
	/// This returns `Some(true)` if this is the first time the peer has become aware of a
	/// candidate with the given hash.
	fn send(
		&mut self,
		relay_parent: &Hash,
		fingerprint: &(CompactStatement, ValidatorIndex),
	) -> Option<bool> {
		self.view_knowledge.get_mut(relay_parent).map_or(None, |k| k.send(fingerprint))
	}

	/// Attempt to update our view of the peer's knowledge with this statement's fingerprint based on
	/// a message we are receiving from the peer.
	///
	/// Provide the maximum message count that we can receive per candidate. In practice we should
	/// not receive more statements for any one candidate than there are members in the group assigned
	/// to that para, but this maximum needs to be lenient to account for equivocations that may be
	/// cross-group. As such, a maximum of 2 * n_validators is recommended.
	///
	/// This returns an error if the peer should not have sent us this message according to protocol
	/// rules for flood protection.
	///
	/// If this returns `Ok`, the internal state has been altered. After `receive`ing a new
	/// candidate, we are then cleared to send the peer further statements about that candidate.
	///
	/// This returns `Ok(true)` if this is the first time the peer has become aware of a
	/// candidate with given hash.
	fn receive(
		&mut self,
		relay_parent: &Hash,
		fingerprint: &(CompactStatement, ValidatorIndex),
		max_message_count: usize,
	) -> Result<bool, Rep> {
		self.view_knowledge.get_mut(relay_parent).ok_or(COST_UNEXPECTED_STATEMENT)?
			.receive(fingerprint, max_message_count)
	}
}

// A statement stored while a relay chain head is active.
struct StoredStatement {
	comparator: StoredStatementComparator,
	statement: SignedFullStatement,
}

// A value used for comparison of stored statements to each other.
//
// The compact version of the statement, the validator index, and the signature of the validator
// is enough to differentiate between all types of equivocations, as long as the signature is
// actually checked to be valid. The same statement with 2 signatures and 2 statements with
// different (or same) signatures wll all be correctly judged to be unequal with this comparator.
#[derive(PartialEq, Eq, Hash, Clone)]
struct StoredStatementComparator {
	compact: CompactStatement,
	validator_index: ValidatorIndex,
	signature: ValidatorSignature,
}

impl StoredStatement {
	fn compact(&self) -> &CompactStatement {
		&self.comparator.compact
	}

	fn fingerprint(&self) -> (CompactStatement, ValidatorIndex) {
		(self.comparator.compact.clone(), self.statement.validator_index())
	}
}

impl std::borrow::Borrow<StoredStatementComparator> for StoredStatement {
	fn borrow(&self) -> &StoredStatementComparator {
		&self.comparator
	}
}

impl std::hash::Hash for StoredStatement {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.comparator.hash(state)
	}
}

impl std::cmp::PartialEq for StoredStatement {
	fn eq(&self, other: &Self) -> bool {
		&self.comparator == &other.comparator
	}
}

impl std::cmp::Eq for StoredStatement {}

enum NotedStatement<'a> {
	NotUseful,
	Fresh(&'a StoredStatement),
	UsefulButKnown
}

struct ActiveHeadData {
	/// All candidates we are aware of for this head, keyed by hash.
	candidates: HashSet<Hash>,
	/// Stored seconded statements for circulation to peers.
	seconded_statements: HashSet<StoredStatement>,
	/// Stored other statements for circulation to peers.
	other_statements: HashSet<StoredStatement>,
	/// The validators at this head.
	validators: Vec<ValidatorId>,
	/// The session index this head is at.
	session_index: sp_staking::SessionIndex,
	/// How many `Seconded` statements we've seen per validator.
	seconded_counts: HashMap<ValidatorIndex, usize>,
}

impl ActiveHeadData {
	fn new(validators: Vec<ValidatorId>, session_index: sp_staking::SessionIndex) -> Self {
		ActiveHeadData {
			candidates: Default::default(),
			seconded_statements: Default::default(),
			other_statements: Default::default(),
			validators,
			session_index,
			seconded_counts: Default::default(),
		}
	}

	/// Note the given statement.
	///
	/// If it was not already known and can be accepted,  returns `Some`,
	/// with a handle to the statement.
	///
	/// We accept up to `VC_THRESHOLD` (2 at time of writing) `Seconded` statements
	/// per validator. These will be the first ones we see. The statement is assumed
	/// to have been checked, including that the validator index is not out-of-bounds and
	/// the signature is valid.
	///
	/// Any other statements or those that reference a candidate we are not aware of cannot be accepted.
	fn note_statement(&mut self, statement: SignedFullStatement) -> NotedStatement {
		let validator_index = statement.validator_index();
		let comparator = StoredStatementComparator {
			compact: statement.payload().to_compact(),
			validator_index,
			signature: statement.signature().clone(),
		};

		let stored = StoredStatement {
			comparator: comparator.clone(),
			statement,
		};

		match comparator.compact {
			CompactStatement::Candidate(h) => {
				let seconded_so_far = self.seconded_counts.entry(validator_index).or_insert(0);
				if *seconded_so_far >= 2 {
					return NotedStatement::NotUseful;
				} else {
					*seconded_so_far += 1;
				}

				self.candidates.insert(h);
				if self.seconded_statements.insert(stored) {
					// This will always return `Some` because it was just inserted.
					NotedStatement::Fresh(self.seconded_statements.get(&comparator)
						.expect("Statement was just inserted; qed"))
				} else {
					NotedStatement::UsefulButKnown
				}
			}
			CompactStatement::Valid(h) | CompactStatement::Invalid(h) => {
				if !self.candidates.contains(&h) {
					return NotedStatement::NotUseful;
				}

				if self.other_statements.insert(stored) {
					// This will always return `Some` because it was just inserted.
					NotedStatement::Fresh(self.other_statements.get(&comparator)
						.expect("Statement was just inserted; qed"))
				} else {
					NotedStatement::UsefulButKnown
				}
			}
		}
	}

	/// Get an iterator over all statements for the active head. Seconded statements come first.
	fn statements(&self) -> impl Iterator<Item = &'_ StoredStatement> + '_ {
		self.seconded_statements.iter().chain(self.other_statements.iter())
	}

	/// Get an iterator over all statements for the active head that are for a particular candidate.
	fn statements_about(&self, candidate_hash: Hash)
		-> impl Iterator<Item = &'_ StoredStatement> + '_
	{
		self.statements().filter(move |s| s.compact().candidate_hash() == &candidate_hash)
	}
}

/// Check a statement signature under this parent hash.
fn check_statement_signature(
	head: &ActiveHeadData,
	relay_parent: Hash,
	statement: &SignedFullStatement,
) -> Result<(), ()> {
	let signing_context = SigningContext {
		session_index: head.session_index,
		parent_hash: relay_parent,
	};

	match head.validators.get(statement.validator_index() as usize) {
		None => return Err(()),
		Some(v) => statement.check_signature(&signing_context, v),
	}
}

#[derive(Encode, Decode)]
enum WireMessage {
	/// relay-parent, full statement.
	Statement(Hash, SignedFullStatement),
}

/// Places the statement in storage if it is new, and then
/// circulates the statement to all peers who have not seen it yet, and
/// sends all statements dependent on that statement to peers who could previously not receive
/// them but now can.
async fn circulate_statement_and_dependents(
	peers: &mut HashMap<PeerId, PeerData>,
	active_heads: &mut HashMap<Hash, ActiveHeadData>,
	ctx: &mut impl SubsystemContext<Message = StatementDistributionMessage>,
	relay_parent: Hash,
	statement: SignedFullStatement,
) -> SubsystemResult<()> {
	if let Some(active_head)= active_heads.get_mut(&relay_parent) {

		// First circulate the statement directly to all peers needing it.
		// The borrow of `active_head` needs to encompass only this (Rust) statement.
		let outputs: Option<(Hash, Vec<PeerId>)> = {
			match active_head.note_statement(statement) {
				NotedStatement::Fresh(stored) => Some((
					stored.compact().candidate_hash().clone(),
					circulate_statement(peers, ctx, relay_parent, stored).await?,
				)),
				_ => None,
			}
		};

		// Now send dependent statements to all peers needing them, if any.
		if let Some((candidate_hash, peers_needing_dependents)) = outputs {
			for peer in peers_needing_dependents {
				if let Some(peer_data) = peers.get_mut(&peer) {
					// defensive: the peer data should always be some because the iterator
					// of peers is derived from the set of peers.
					send_statements_about(
						peer,
						peer_data,
						ctx,
						relay_parent,
						candidate_hash,
						&*active_head
					).await?;
				}
			}
		}
	}

	Ok(())
}

/// Circulates a statement to all peers who have not seen it yet, and returns
/// an iterator over peers who need to have dependent statements sent.
async fn circulate_statement(
	peers: &mut HashMap<PeerId, PeerData>,
	ctx: &mut impl SubsystemContext<Message = StatementDistributionMessage>,
	relay_parent: Hash,
	stored: &StoredStatement,
) -> SubsystemResult<Vec<PeerId>> {
	let fingerprint = stored.fingerprint();

	let mut peers_to_send = HashMap::new();

	for (peer, data) in peers.iter_mut() {
		if let Some(new_known) = data.send(&relay_parent, &fingerprint) {
			peers_to_send.insert(peer.clone(), new_known);
		}
	}

	// Send all these peers the initial statement.
	if !peers_to_send.is_empty() {
		let payload = WireMessage::Statement(relay_parent, stored.statement.clone()).encode();
		ctx.send_message(AllMessages::NetworkBridge(NetworkBridgeMessage::SendMessage(
			peers_to_send.keys().cloned().collect(),
			PROTOCOL_V1,
			payload,
		))).await?;
	}

	Ok(peers_to_send.into_iter().filter_map(|(peer, needs_dependent)| if needs_dependent {
		Some(peer)
	} else {
		None
	}).collect())
}

/// Send all statements about a given candidate hash to a peer.
async fn send_statements_about(
	peer: PeerId,
	peer_data: &mut PeerData,
	ctx: &mut impl SubsystemContext<Message = StatementDistributionMessage>,
	relay_parent: Hash,
	candidate_hash: Hash,
	active_head: &ActiveHeadData,
) -> SubsystemResult<()> {
	for statement in active_head.statements_about(candidate_hash) {
		if peer_data.send(&relay_parent, &statement.fingerprint()).is_some() {
			let payload = WireMessage::Statement(
				relay_parent,
				statement.statement.clone(),
			).encode();

			ctx.send_message(AllMessages::NetworkBridge(NetworkBridgeMessage::SendMessage(
				vec![peer.clone()],
				PROTOCOL_V1,
				payload,
			))).await?;
		}
	}

	Ok(())
}

/// Send all statements at a given relay-parent to a peer.
async fn send_statements(
	peer: PeerId,
	peer_data: &mut PeerData,
	ctx: &mut impl SubsystemContext<Message = StatementDistributionMessage>,
	relay_parent: Hash,
	active_head: &ActiveHeadData
) -> SubsystemResult<()> {
	for statement in active_head.statements() {
		if peer_data.send(&relay_parent, &statement.fingerprint()).is_some() {
			let payload = WireMessage::Statement(
				relay_parent,
				statement.statement.clone(),
			).encode();

			ctx.send_message(AllMessages::NetworkBridge(NetworkBridgeMessage::SendMessage(
				vec![peer.clone()],
				PROTOCOL_V1,
				payload,
			))).await?;
		}
	}

	Ok(())
}

async fn report_peer(
	ctx: &mut impl SubsystemContext,
	peer: PeerId,
	rep: Rep,
) -> SubsystemResult<()> {
	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::ReportPeer(peer, rep)
	)).await
}

// Handle an incoming wire message. Returns a reference to a newly-stored statement
// if we were not already aware of it, along with the corresponding relay-parent.
//
// This function checks the signature and ensures the statement is compatible with our
// view.
async fn handle_incoming_message<'a>(
	peer: PeerId,
	peer_data: &mut PeerData,
	our_view: &View,
	active_heads: &'a mut HashMap<Hash, ActiveHeadData>,
	ctx: &mut impl SubsystemContext<Message = StatementDistributionMessage>,
	message: Vec<u8>,
) -> SubsystemResult<Option<(Hash, &'a StoredStatement)>> {
	let (relay_parent, statement) = match WireMessage::decode(&mut &message[..]) {
		Err(_) => return report_peer(ctx, peer, COST_INVALID_MESSAGE).await.map(|_| None),
		Ok(WireMessage::Statement(r, s)) => (r, s),
	};

	if !our_view.contains(&relay_parent) {
		return report_peer(ctx, peer, COST_UNEXPECTED_STATEMENT).await.map(|_| None);
	}

	let active_head = match active_heads.get_mut(&relay_parent) {
		Some(h) => h,
		None => {
			// This should never be out-of-sync with our view if the view updates
			// correspond to actual `StartWork` messages. So we just log and ignore.
			log::warn!("Our view out-of-sync with active heads. Head {} not found", relay_parent);
			return Ok(None);
		}
	};

	// check the signature on the statement.
	if let Err(()) = check_statement_signature(&active_head, relay_parent, &statement) {
		return report_peer(ctx, peer, COST_INVALID_SIGNATURE).await.map(|_| None);
	}

	// Ensure the statement is stored in the peer data.
	//
	// Note that if the peer is sending us something that is not within their view,
	// it will not be kept within their log.
	let fingerprint = (statement.payload().to_compact(), statement.validator_index());
	let max_message_count = active_head.validators.len() * 2;
	match peer_data.receive(&relay_parent, &fingerprint, max_message_count) {
		Err(rep) => {
			report_peer(ctx, peer, rep).await?;
			return Ok(None)
		}
		Ok(true) => {
			// Send the peer all statements concerning the candidate that we have,
			// since it appears to have just learned about the candidate.
			send_statements_about(
				peer.clone(),
				peer_data,
				ctx,
				relay_parent,
				fingerprint.0.candidate_hash().clone(),
				&*active_head,
			).await?
		}
		Ok(false) => {}
	}

	// Note: `peer_data.receive` already ensures that the statement is not an unbounded equivocation
	// or unpinned to a seconded candidate. So it is safe to place it into the storage.
	match active_head.note_statement(statement) {
		NotedStatement::NotUseful => Ok(None),
		NotedStatement::UsefulButKnown => {
			report_peer(ctx, peer, BENEFIT_VALID_STATEMENT).await?;
			Ok(None)
		}
		NotedStatement::Fresh(statement) => {
			report_peer(ctx, peer, BENEFIT_VALID_STATEMENT_FIRST).await?;
			Ok(Some((relay_parent, statement)))
		}
	}
}

/// Update a peer's view. Sends all newly unlocked statements based on the previous
async fn update_peer_view_and_send_unlocked(
	peer: PeerId,
	peer_data: &mut PeerData,
	ctx: &mut impl SubsystemContext<Message = StatementDistributionMessage>,
	active_heads: &HashMap<Hash, ActiveHeadData>,
	new_view: View,
) -> SubsystemResult<()> {
	let old_view = std::mem::replace(&mut peer_data.view, new_view);

	// Remove entries for all relay-parents in the old view but not the new.
	for removed in old_view.difference(&peer_data.view) {
		let _ = peer_data.view_knowledge.remove(&removed);
	}

	// Add entries for all relay-parents in the new view but not the old.
	// Furthermore, send all statements we have for those relay parents.
	let new_view = peer_data.view.difference(&old_view).collect::<Vec<_>>();
	for new in new_view.iter().copied() {
		peer_data.view_knowledge.insert(new, Default::default());

		if let Some(active_head) = active_heads.get(&new) {
			send_statements(
				peer.clone(),
				peer_data,
				ctx,
				new,
				active_head,
			).await?;
		}
	}

	Ok(())
}

async fn handle_network_update(
	peers: &mut HashMap<PeerId, PeerData>,
	active_heads: &mut HashMap<Hash, ActiveHeadData>,
	ctx: &mut impl SubsystemContext<Message = StatementDistributionMessage>,
	our_view: &mut View,
	update: NetworkBridgeEvent,
) -> SubsystemResult<()> {
	match update {
		NetworkBridgeEvent::PeerConnected(peer, _role) => {
			peers.insert(peer, PeerData {
				view: Default::default(),
				view_knowledge: Default::default(),
			});

			Ok(())
		}
		NetworkBridgeEvent::PeerDisconnected(peer) => {
			peers.remove(&peer);
			Ok(())
		}
		NetworkBridgeEvent::PeerMessage(peer, message) => {
			match peers.get_mut(&peer) {
				Some(data) => {
					let new_stored = handle_incoming_message(
						peer,
						data,
						&*our_view,
						active_heads,
						ctx,
						message,
					).await?;

					if let Some((relay_parent, new)) = new_stored {
						// When we receive a new message from a peer, we forward it to the
						// candidate backing subsystem.
						let message = AllMessages::CandidateBacking(
							CandidateBackingMessage::Statement(relay_parent, new.statement.clone())
						);
						ctx.send_message(message).await?;
					}

					Ok(())
				}
				None => Ok(()),
			}

		}
		NetworkBridgeEvent::PeerViewChange(peer, view) => {
			match peers.get_mut(&peer) {
				Some(data) => {
					update_peer_view_and_send_unlocked(
						peer,
						data,
						ctx,
						&*active_heads,
						view,
					).await
				}
				None => Ok(()),
			}
		}
		NetworkBridgeEvent::OurViewChange(view) => {
			let old_view = std::mem::replace(our_view, view);
			active_heads.retain(|head, _| our_view.contains(head));

			for new in our_view.difference(&old_view) {
				if !active_heads.contains_key(&new) {
					log::warn!(target: "statement_distribution", "Our network bridge view update \
						inconsistent with `StartWork` messages we have received from overseer. \
						Contains unknown hash {}", new);
				}
			}

			Ok(())
		}
	}

}

async fn run(
	mut ctx: impl SubsystemContext<Message = StatementDistributionMessage>,
) -> SubsystemResult<()> {
	// startup: register the network protocol with the bridge.
	ctx.send_message(AllMessages::NetworkBridge(NetworkBridgeMessage::RegisterEventProducer(
		PROTOCOL_V1,
		network_update_message,
	))).await?;

	let mut peers: HashMap<PeerId, PeerData> = HashMap::new();
	let mut our_view = View::default();
	let mut active_heads: HashMap<Hash, ActiveHeadData> = HashMap::new();

	loop {
		let message = ctx.recv().await?;
		match message {
			FromOverseer::Signal(OverseerSignal::StartWork(relay_parent)) => {
				let (validators, session_index) = {
					let (val_tx, val_rx) = oneshot::channel();
					let (session_tx, session_rx) = oneshot::channel();

					let val_message = AllMessages::RuntimeApi(
						RuntimeApiMessage::Request(relay_parent, RuntimeApiRequest::Validators(val_tx)),
					);
					let session_message = AllMessages::RuntimeApi(
						RuntimeApiMessage::Request(relay_parent, RuntimeApiRequest::SigningContext(session_tx)),
					);

					ctx.send_messages(
						std::iter::once(val_message).chain(std::iter::once(session_message))
					).await?;

					(val_rx.await?, session_rx.await?.session_index)
				};

				active_heads.entry(relay_parent)
					.or_insert(ActiveHeadData::new(validators, session_index));
			}
			FromOverseer::Signal(OverseerSignal::StopWork(_relay_parent)) => {
				// do nothing - we will handle this when our view changes.
			}
			FromOverseer::Signal(OverseerSignal::Conclude) => break,
			FromOverseer::Communication { msg } => match msg {
				StatementDistributionMessage::Share(relay_parent, statement) =>
					circulate_statement_and_dependents(
						&mut peers,
						&mut active_heads,
						&mut ctx,
						relay_parent,
						statement,
					).await?,
				StatementDistributionMessage::NetworkBridgeUpdate(event) => handle_network_update(
					&mut peers,
					&mut active_heads,
					&mut ctx,
					&mut our_view,
					event,
				).await?,
			}
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_keyring::Sr25519Keyring;
	use node_primitives::Statement;
	use polkadot_primitives::parachain::{AbridgedCandidateReceipt};

	#[test]
	fn active_head_accepts_only_2_seconded_per_validator() {
		let validators = vec![
			Sr25519Keyring::Alice.public().into(),
			Sr25519Keyring::Bob.public().into(),
			Sr25519Keyring::Charlie.public().into(),
		];
		let parent_hash: Hash = [1; 32].into();

		let session_index = 1;
		let signing_context = SigningContext {
			parent_hash,
			session_index,
		};

		let candidate_a = {
			let mut c = AbridgedCandidateReceipt::default();
			c.relay_parent = parent_hash;
			c.parachain_index = 1.into();
			c
		};

		let candidate_b = {
			let mut c = AbridgedCandidateReceipt::default();
			c.relay_parent = parent_hash;
			c.parachain_index = 2.into();
			c
		};

		let candidate_c = {
			let mut c = AbridgedCandidateReceipt::default();
			c.relay_parent = parent_hash;
			c.parachain_index = 3.into();
			c
		};

		let mut head_data = ActiveHeadData::new(validators, session_index);

		head_data.note_statement(SignedFullStatement::sign(
			Statement::Seconded(candidate_a),
			&signing_context,
			0,
			&Sr25519Keyring::Alice.pair().into(),
		));
	}

	#[test]
	fn note_local_works() {
		// TODO [now]
	}

	#[test]
	fn note_remote_works() {
		// TODO [now]
	}

	#[test]
	fn peer_view_update_sends_messages() {
		// TODO [now]
	}

	#[test]
	fn circulated_statement_goes_to_all_peers_with_view() {
		// TODO [now]
	}

	#[test]
	fn smoke() {
		// TODO [now]
	}
}
