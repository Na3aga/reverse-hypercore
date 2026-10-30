//! Per-validator application actor.
//!
//! The actor bridges simplex (which only sees digests) and the application
//! (which only sees opaque payload bytes). It implements, through its
//! [`Mailbox`], the three interfaces the simplex engine needs:
//!
//! - [`Automaton`] / [`CertifiableAutomaton`]: build proposals (by calling
//!   [`PayloadSource::build`]) and verify peers' proposals (by waiting for the
//!   full envelope, checking it against the consensus context, then calling
//!   [`PayloadSource::validate`]).
//! - [`Relay`]: disseminate full envelopes on a dedicated p2p channel.
//! - [`Reporter`]: observe finalization certificates and turn them into an
//!   ordered, gap-free stream of [`Finalized`] blocks.
//!
//! All internal maps are `BTreeMap`/`BTreeSet` so that iteration order (and
//! therefore message order) is identical across runs with the same seed.

use crate::{
    block::{self, Block, Wire, MAX_PAYLOAD_BYTES},
    Finalized, PayloadSource,
};
use bytes::Bytes;
use commonware_actor::Feedback;
use commonware_consensus::{
    simplex::{scheme::ed25519, types::Activity, types::Context, Plan},
    Automaton, CertifiableAutomaton, Relay, Reporter, Viewable,
};
use commonware_cryptography::{ed25519::PublicKey, sha256::Digest};
use commonware_macros::select_loop;
use commonware_p2p::{Receiver, Recipients, Sender};
use commonware_runtime::{Clock, Spawner};
use commonware_utils::channel::{fallible::OneshotExt, oneshot};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub(crate) type Ctx = Context<Digest, PublicKey>;

/// Callback invoked for each block this validator finalizes, in height order.
pub(crate) type FinalizedSink = Arc<dyn Fn(Finalized) + Send + Sync>;

/// How often outstanding block requests are retried.
const FETCH_RETRY: Duration = Duration::from_millis(250);
/// Upper bound on digests re-requested per retry tick.
const MAX_REQUESTS_PER_TICK: usize = 32;
/// Number of views of block history kept behind the finalized tip (used to
/// answer peers' fetch requests).
const RETAIN_VIEWS: u64 = 1024;

pub(crate) enum Message {
    Propose {
        context: Ctx,
        response: oneshot::Sender<Digest>,
    },
    /// Internal: a delayed proposal is due (see `block_interval`).
    Build {
        context: Ctx,
        response: oneshot::Sender<Digest>,
    },
    Verify {
        context: Ctx,
        payload: Digest,
        response: oneshot::Sender<bool>,
    },
    Broadcast {
        payload: Digest,
        plan: Plan<PublicKey>,
    },
    Finalized {
        view: u64,
        digest: Digest,
        certificate: Vec<u8>,
    },
}

/// Handle given to the simplex engine (as automaton, relay and reporter).
#[derive(Clone)]
pub(crate) struct Mailbox {
    sender: mpsc::UnboundedSender<Message>,
}

impl Mailbox {
    fn send(&self, msg: Message) -> Feedback {
        if self.sender.send(msg).is_ok() {
            Feedback::Ok
        } else {
            Feedback::Closed
        }
    }
}

impl Automaton for Mailbox {
    type Context = Ctx;
    type Digest = Digest;

    async fn propose(&mut self, context: Ctx) -> oneshot::Receiver<Digest> {
        let (response, rx) = oneshot::channel();
        self.send(Message::Propose { context, response });
        rx
    }

    async fn verify(&mut self, context: Ctx, payload: Digest) -> oneshot::Receiver<bool> {
        let (response, rx) = oneshot::channel();
        self.send(Message::Verify {
            context,
            payload,
            response,
        });
        rx
    }
}

// Default certification (always certify): validity is fully decided in `verify`.
impl CertifiableAutomaton for Mailbox {}

impl Relay for Mailbox {
    type Digest = Digest;
    type PublicKey = PublicKey;
    type Plan = Plan<PublicKey>;

    fn broadcast(&mut self, payload: Digest, plan: Plan<PublicKey>) -> Feedback {
        self.send(Message::Broadcast { payload, plan })
    }
}

impl Reporter for Mailbox {
    type Activity = Activity<ed25519::Scheme, Digest>;

    fn report(&mut self, activity: Self::Activity) -> Feedback {
        // Only finalization certificates matter to the application. Finalization
        // of view v implies all its ancestors are final, so the actor walks the
        // parent chain to fill in any views it was not told about directly.
        if let Activity::Finalization(f) = activity {
            use commonware_codec::Encode as _;
            return self.send(Message::Finalized {
                view: f.view().get(),
                digest: f.proposal.payload,
                certificate: f.encode().to_vec(),
            });
        }
        Feedback::Ok
    }
}

struct Stored {
    block: Block,
    bytes: Bytes,
}

pub(crate) struct Config {
    pub index: u32,
    /// Validator public keys, indexed by validator index.
    pub keys: Vec<PublicKey>,
    pub source: Arc<dyn PayloadSource>,
    pub sink: FinalizedSink,
    /// Delay between being asked to propose and building the payload.
    pub block_interval: Duration,
    /// The last block the application already has: (view, digest). Genesis,
    /// or the floor certificate's block when rejoining a running chain.
    pub tip: (u64, Digest),
}

pub(crate) struct Actor<E: Clock + Spawner> {
    context: E,
    cfg: Config,
    self_sender: mpsc::UnboundedSender<Message>,
    mailbox: mpsc::UnboundedReceiver<Message>,

    blocks: BTreeMap<Digest, Stored>,
    verify_waiters: BTreeMap<Digest, Vec<(Ctx, oneshot::Sender<bool>)>>,
    wanted: BTreeSet<Digest>,

    /// Last block delivered to the sink: (view, digest).
    tip: (u64, Digest),
    height: u64,
    /// Highest finalization certificate not yet fully delivered, encoded.
    target: Option<(u64, Digest, Vec<u8>)>,
}

impl<E: Clock + Spawner> Actor<E> {
    pub fn new(context: E, cfg: Config) -> (Self, Mailbox) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tip: cfg.tip,
                context,
                cfg,
                self_sender: tx.clone(),
                mailbox: rx,
                blocks: BTreeMap::new(),
                verify_waiters: BTreeMap::new(),
                wanted: BTreeSet::new(),
                height: 0,
                target: None,
            },
            Mailbox { sender: tx },
        )
    }

    pub fn start<NS, NR>(self, net: (NS, NR)) -> commonware_runtime::Handle<()>
    where
        NS: Sender<PublicKey = PublicKey>,
        NR: Receiver<PublicKey = PublicKey>,
    {
        let context = self.context.child("app");
        context.spawn(move |ctx| self.run(ctx, net))
    }

    async fn run<NS, NR>(mut self, context: E, (mut net_tx, mut net_rx): (NS, NR))
    where
        NS: Sender<PublicKey = PublicKey>,
        NR: Receiver<PublicKey = PublicKey>,
    {
        self.context = context;
        let mut next_retry = self.context.current() + FETCH_RETRY;
        select_loop! {
            self.context,
            on_stopped => {
                debug!(index = self.cfg.index, "application actor stopping");
            },
            Some(msg) = self.mailbox.recv() else break => {
                self.handle(msg, &mut net_tx);
            },
            Ok((peer, buf)) = net_rx.recv() else break => {
                self.handle_wire(peer, buf.as_ref(), &mut net_tx);
            },
            _ = self.context.sleep_until(next_retry) => {
                next_retry = self.context.current() + FETCH_RETRY;
                for d in self.wanted.iter().take(MAX_REQUESTS_PER_TICK) {
                    net_tx.send(Recipients::All, Wire::Request(*d).encode(), false);
                }
            },
        }
    }

    fn now_ms(&self) -> u64 {
        let now = self.context.current();
        now.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    fn index_of(&self, key: &PublicKey) -> Option<u32> {
        self.cfg
            .keys
            .iter()
            .position(|k| k == key)
            .map(|i| i as u32)
    }

    fn handle<NS: Sender<PublicKey = PublicKey>>(&mut self, msg: Message, net: &mut NS) {
        match msg {
            Message::Propose { context, response } => {
                if self.cfg.block_interval.is_zero() {
                    self.build(context, response);
                } else {
                    // Delay the build without blocking the actor loop.
                    let delay = self.cfg.block_interval;
                    let tx = self.self_sender.clone();
                    self.context.child("delay").spawn(move |ctx| async move {
                        ctx.sleep(delay).await;
                        let _ = tx.send(Message::Build { context, response });
                    });
                }
            }
            Message::Build { context, response } => self.build(context, response),
            Message::Verify {
                context,
                payload,
                response,
            } => match self.blocks.get(&payload) {
                Some(stored) => {
                    let ok = self.check(&context, &stored.block);
                    response.send_lossy(ok);
                }
                None => {
                    // Keep the request pending until the envelope arrives (by
                    // broadcast or fetch): "missing data" is not "invalid".
                    self.verify_waiters
                        .entry(payload)
                        .or_default()
                        .push((context, response));
                    self.want(payload, net);
                }
            },
            Message::Broadcast { payload, plan } => {
                let Some(stored) = self.blocks.get(&payload) else {
                    debug!(?payload, "asked to broadcast unknown block");
                    return;
                };
                let recipients = match plan {
                    Plan::Propose { .. } => Recipients::All,
                    Plan::Forward { recipients, .. } => recipients,
                };
                net.send(
                    recipients,
                    Wire::Block(stored.bytes.clone()).encode(),
                    false,
                );
            }
            Message::Finalized { view, digest, certificate } => {
                if view <= self.tip.0 {
                    return;
                }
                if self.target.as_ref().is_none_or(|(v, _, _)| view > *v) {
                    self.target = Some((view, digest, certificate));
                }
                self.advance(net);
            }
        }
    }

    /// Build a proposal for `context` by asking the application for a payload.
    fn build(&mut self, context: Ctx, response: oneshot::Sender<Digest>) {
        let view = context.view().get();
        let (parent_view, parent) = (context.parent.0.get(), context.parent.1);
        let payload = self.cfg.source.build(view, parent.0, self.now_ms());
        if payload.len() > MAX_PAYLOAD_BYTES {
            warn!(
                view,
                len = payload.len(),
                "payload too large; declining to propose"
            );
            return; // dropping `response` declines the proposal
        }
        let block = Block {
            view,
            parent_view,
            parent: parent.0,
            proposer: self.cfg.index,
            payload,
        };
        let bytes = block.encode();
        let digest = block::digest(&bytes);
        self.blocks.insert(digest, Stored { block, bytes });
        response.send_lossy(digest);
    }

    /// Validate an envelope against the consensus context it is proposed in,
    /// then defer to the application for payload validation.
    fn check(&self, context: &Ctx, block: &Block) -> bool {
        let leader = self.index_of(&context.leader);
        block.view == context.view().get()
            && block.parent_view == context.parent.0.get()
            && block.parent == context.parent.1 .0
            && Some(block.proposer) == leader
            && self.cfg.source.validate(block.view, &block.payload)
    }

    fn want<NS: Sender<PublicKey = PublicKey>>(&mut self, digest: Digest, net: &mut NS) {
        if self.wanted.insert(digest) {
            net.send(Recipients::All, Wire::Request(digest).encode(), false);
        }
    }

    fn handle_wire<NS: Sender<PublicKey = PublicKey>>(
        &mut self,
        peer: PublicKey,
        buf: &[u8],
        net: &mut NS,
    ) {
        match Wire::decode(buf) {
            Some(Wire::Request(digest)) => {
                if let Some(stored) = self.blocks.get(&digest) {
                    net.send(
                        Recipients::One(peer),
                        Wire::Block(stored.bytes.clone()).encode(),
                        false,
                    );
                }
            }
            Some(Wire::Block(bytes)) => {
                let digest = block::digest(&bytes);
                if self.blocks.contains_key(&digest) {
                    return;
                }
                let Some(block) = Block::decode(&bytes) else {
                    debug!(?peer, "undecodable block");
                    return;
                };
                // Ignore stale blocks we no longer need (bounded memory), unless
                // something is explicitly waiting on them.
                if block.view <= self.tip.0 && !self.wanted.contains(&digest) {
                    return;
                }
                self.wanted.remove(&digest);
                if let Some(waiters) = self.verify_waiters.remove(&digest) {
                    for (ctx, response) in waiters {
                        let ok = self.check(&ctx, &block);
                        response.send_lossy(ok);
                    }
                }
                self.blocks.insert(digest, Stored { block, bytes });
                self.advance(net);
            }
            None => debug!(?peer, "undecodable wire message"),
        }
    }

    /// Deliver every block between the current tip and the highest known
    /// finalization, walking parent links backwards. If an ancestor is missing,
    /// request it and resume when it arrives.
    fn advance<NS: Sender<PublicKey = PublicKey>>(&mut self, net: &mut NS) {
        let Some((_, target, _)) = self.target else {
            return;
        };
        let mut chain = Vec::new();
        let mut cur = target;
        while cur != self.tip.1 {
            let Some(stored) = self.blocks.get(&cur) else {
                self.want(cur, net);
                return;
            };
            if stored.block.view <= self.tip.0 {
                // Would imply two conflicting finalized chains: a safety
                // violation that BFT consensus with f < n/3 rules out.
                panic!("finalized chain does not extend local tip");
            }
            chain.push(cur);
            cur = Digest(stored.block.parent);
        }
        let certificate = self.target.take().map(|t| t.2).unwrap_or_default();
        for digest in chain.into_iter().rev() {
            let block = &self.blocks[&digest].block;
            self.height += 1;
            self.tip = (block.view, digest);
            (self.cfg.sink)(Finalized {
                view: block.view,
                height: self.height,
                proposer: block.proposer,
                digest: digest.0,
                payload: block.payload.clone(),
                // Only the target block is named by the certificate.
                certificate: if digest == target { certificate.clone() } else { Vec::new() },
            });
        }
        self.prune();
    }

    fn prune(&mut self) {
        let tip_view = self.tip.0;
        // Verification for views at or below the finalized tip is moot; dropping
        // the responder closes the request.
        self.verify_waiters.retain(|_, waiters| {
            waiters.retain(|(ctx, _)| ctx.view().get() > tip_view);
            !waiters.is_empty()
        });
        let waiters = &self.verify_waiters;
        self.wanted.retain(|d| waiters.contains_key(d));
        let floor = tip_view.saturating_sub(RETAIN_VIEWS);
        self.blocks.retain(|_, s| s.block.view >= floor);
    }
}
