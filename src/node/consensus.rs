use time::{get_time};

use super::super::events::{Events, Event, Timeout, EventsConfiguration};
use super::super::crypto::{Hash};
use super::super::messages::{ConsensusMessage, Propose, Prevote, Precommit, Commit, Message};
use super::super::storage::{Changes};
use super::{NodeContext, Round, Height};

pub struct ConsensusService;

pub trait ConsensusHandler {
    fn handle(&mut self, ctx: &mut NodeContext, msg: ConsensusMessage) {
        // Ignore messages from previous height
        if msg.height() < ctx.state.height() + 1 {
            return
        }

        // Queued messages from future height or round
        if msg.height() > ctx.state.height() + 1 ||
           msg.round() > ctx.state.round() {
            ctx.state.add_queued(msg);
            return
        }

        match ctx.state.public_key_of(msg.validator()) {
            // Incorrect signature of message
            Some(public_key) => if !msg.verify() {
                return
            },
            // Incorrect validator id
            None => return
        }

        match msg {
            ConsensusMessage::Propose(msg) => {
                // Check prev_hash
                if msg.prev_hash() != ctx.state.prev_hash() {
                    return
                }

                // Check leader
                if msg.validator() != ctx.state.leader(msg.round()) {
                    return
                }

                // TODO: check time
                // TODO: check that transactions are not commited yet

                self.handle_propose(ctx, msg)
            },
            ConsensusMessage::Prevote(msg) => self.handle_prevote(ctx, msg),
            ConsensusMessage::Precommit(msg) => self.handle_precommit(ctx, msg),
            ConsensusMessage::Commit(msg) => self.handle_commit(ctx, msg),
        }
    }

    fn handle_propose(&mut self, ctx: &mut NodeContext, propose: Propose) {
        // Add propose
        let hash = propose.hash();
        let added = ctx.state.add_propose(hash, &propose);

        if added {
            // TODO: Temp (Check that we "have block")
            for h in propose.transactions() {
                if !ctx.state.tx_pool().contains_key(h) {
                    panic!("unknown transactions into propose");
                }
            }

            self.have_block(ctx, hash);
        }
    }

    fn have_block(&mut self, ctx: &mut NodeContext, hash: Hash) {
        // Send prevote
        if ctx.state.locked_round() == 0 {
            self.send_prevote(ctx, ctx.state.propose(hash).round(), hash);
        }

        // Lock to propose
        let start_round = ::std::cmp::max(ctx.state.locked_round() + 1,
                                          ctx.state.proposal(hash).round());
        for round in start_round ... ctx.state.round() {
            if ctx.state.has_majority_prevotes(round, hash) {
                self.lock(ctx, round, hash);
            }
        }

        // FIXME: Commit if we have +2/3 precommits?
        // for round in propose.round() ... ctx.state.round() {
        //     if ctx.state.has_majority_precommits(round, hash) {
        //         self.commit
        //     }
        // }

        //     self.lock(ctx, hash);
        // }
    }

    fn handle_prevote(&mut self, ctx: &mut NodeContext, prevote: Prevote) {
        // Add prevote
        let has_consensus = ctx.state.add_prevote(&prevote);

        // Lock to propose
        if has_consensus && ctx.state.locked_round() < prevote.round() {
            let hash = prevote.block_hash();
            if ctx.state.propose(hash) != None {
                self.lock(ctx, prevote.round(), hash);
            }
        }
    }

    fn lock(&mut self, ctx: &mut NodeContext,
            round: Round, block_hash: Hash) {
        // Change lock
        ctx.state.lock(round, block_hash);

        // Execute block and get state hash
        let state_hash = match ctx.state.state_hash(block_hash) {
            Some(state_hash) => state_hash,
            None => self.execute(ctx, block_hash)
        };

        // Send precommit
        self.send_precommit(ctx, round, block_hash, state_hash);

        // Commit if has consensus
        if ctx.state.has_majority_precommits(round,
                                             block_hash,
                                             state_hash) {
            self.commit(ctx, round, block_hash);
            return
        }

        // Send prevotes
        if !ctx.state.have_incompatible_prevotes() {
            for round in ctx.state.locked_round() + 1 ... ctx.state.round() {
                if !ctx.state.have_prevote(round) {
                    self.send_prevote(ctx, round, block_hash);
                    if ctx.state.has_majority_prevotes(round, block_hash) {
                        self.lock(ctx, round, block_hash);
                    }
                }
            }
        }
    }

    fn handle_precommit(&mut self, ctx: &mut NodeContext, msg: Precommit) {
        // Add precommit
        let has_consensus = ctx.state.add_precommit(&msg);

        if has_consensus {
            let block_hash = msg.block_hash();
            if ctx.state.proposal(block_hash) != None {
                // Execute block and get state hash
                let state_hash = match ctx.state.state_hash(block_hash) {
                    Some(state_hash) => state_hash,
                    None => self.execute(ctx, block_hash)
                };

                if state_hash != msg.state_hash() {
                    panic!("We are fucked up...");
                }

                self.commit(ctx, msg.round(), block_hash);
            }
        }
    }

    fn commit(&mut self, ctx: &mut NodeContext,
              round: Round, hash: Hash, changes: &Changes) {
        // Merge changes into storage
        ctx.storage.merge(changes);

        // Update state to new height
        ctx.state.new_height(hash);

        // Generate new transactions
        for tx in (&mut ctx.tx_generator).take(100) {
            ctx.state.add_tx(tx.hash(), tx);
        }

        // Send commit
        self.send_commit(ctx, ctx.state.height() - 1, round, hash);

        // Handle queued messages
        for msg in ctx.state.queue() {
            self.handle(ctx, msg);
        }

        // Send propose
        if self.is_leader(ctx) {
            self.send_propose(ctx);
        }

        // Add timeout for first round
        ctx.add_timeout();
    }

    fn handle_commit(&mut self, _: &mut NodeContext, _: Commit) {
    }

    fn handle_timeout(&mut self, ctx: &mut NodeContext, timeout: Timeout) {
        if timeout.height != ctx.state.height() {
            return
        }

        if timeout.round != ctx.state.round() {
            return
        }

        // Add timeout for this round
        self.context.add_timeout();

        // Update state to new round
        ctx.state.new_round();

        // Send prevote if we are locked or propose if we are leader
        if let Some(hash) = self.locked_propose() {
            self.send_prevote(ctx, hash);
        } else if self.is_leader(ctx) {
            self.send_propose(ctx);
        }

        // Handle queued messages
        for msg in ctx.state.queue() {
            self.handle(ctx, msg);
        }
    }

    fn is_leader(&self, ctx: &NodeContext) -> bool {
        ctx.state.leader(ctx.state.round()) == ctx.state.id()
    }

    fn execute(&mut self, ctx: &mut NodeContext, hash: Hash) -> Hash {
        // let fork = Fork::new(ctx.storage);

        // fork.put_block(msg);

        // let changes = fork.changes();
        // let hash = changes.hash();
        // ctx.add_changes(hash, changes);
        // hash
    }

    fn send_propose(&mut self, ctx: &mut NodeContext) {
        let propose = Propose::new(ctx.state.id(),
                                   ctx.state.height(),
                                   ctx.state.round(),
                                   get_time(),
                                   ctx.storage.prev_hash(),
                                   &ctx.state.transactions(),
                                   &ctx.secret_key);
        ctx.broadcast(&propose);

        let hash = propose.hash();
        ctx.state.add_propose(hash, propose);

        // Send prevote
        self.send_prevote(ctx.state.round(), hash);
    }

    fn send_prevote(&mut self, ctx: &mut NodeContext,
                    round: Round, block_hash: Hash) {
        let prevote = Prevote::new(ctx.state.id(),
                                   ctx.height(),
                                   round,
                                   block_hash,
                                   &ctx.secret_key);
        ctx.state.add_prevote(&prevote);
        ctx.broadcast(prevote);
    }

    fn send_precommit(&mut self, ctx: &mut NodeContext,
                      round: Round, block_hash: Hash, state_hash: Hash) {
        let precommit = Precommit::new(ctx.state.id(),
                                       ctx.state.height(),
                                       round,
                                       block_hash,
                                       state_hash,
                                       &ctx.secret_key);
        ctx.state.add_precommit(&precommit);
        ctx.broadcast(&precommit);
    }

    fn send_commit(&mut self, ctx: &mut NodeContext,
                   height: Height, round: Round, block_hash: Hash) {
        // Send commit
        let commit = Commit::new(ctx.state.id,
                                 height,
                                 round,
                                 block_hash,
                                 &ctx.secret_key);
        ctx.broadcast(commit);
    }
}

impl ConsensusHandler for ConsensusService {
    // default implementation
}