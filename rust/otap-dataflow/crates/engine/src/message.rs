// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Message definitions for the pipeline engine.

use crate::control::{AckMsg, NackMsg, NodeControlMsg};
use crate::local::message::{LocalReceiver, LocalSender};
use crate::shared::message::{SharedReceiver, SharedSender};
use otap_df_channel::error::{RecvError, SendError};
use otap_df_channel::mpsc;
use std::ops::Add;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::time::{Sleep, sleep_until};

/// Represents messages sent to nodes (receivers, processors, exporters, or connectors) within the
/// pipeline.
///
/// Messages are categorized as either pipeline data (`PData`) or control messages (`Control`).
#[derive(Debug, Clone)]
pub enum Message<PData> {
    /// A pipeline data message traversing the pipeline.
    PData(PData),

    /// A control message.
    Control(NodeControlMsg<PData>),
}

impl<Data> Message<Data> {
    /// Create a data message with the given payload.
    #[must_use]
    pub fn data_msg(data: Data) -> Self {
        Message::PData(data)
    }

    /// Create a ACK control message with the given ID.
    #[must_use]
    pub fn ack_ctrl_msg(ack: AckMsg<Data>) -> Self {
        Message::Control(NodeControlMsg::Ack(ack))
    }

    /// Create a NACK control message with the given ID and reason.
    #[must_use]
    pub fn nack_ctrl_msg(nack: NackMsg<Data>) -> Self {
        Message::Control(NodeControlMsg::Nack(nack))
    }

    /// Creates a config control message with the given configuration.
    #[must_use]
    pub fn config_ctrl_msg(config: serde_json::Value) -> Self {
        Message::Control(NodeControlMsg::Config { config })
    }

    /// Creates a timer tick control message.
    #[must_use]
    pub fn timer_tick_ctrl_msg() -> Self {
        Message::Control(NodeControlMsg::TimerTick {})
    }

    /// Creates a shutdown control message with the given reason.
    #[must_use]
    pub fn shutdown_ctrl_msg(deadline: Instant, reason: &str) -> Self {
        Message::Control(NodeControlMsg::Shutdown {
            deadline,
            reason: reason.to_owned(),
        })
    }

    /// Checks if this message is a data message.
    #[must_use]
    pub const fn is_data(&self) -> bool {
        matches!(self, Message::PData(..))
    }

    /// Checks if this message is a control message.
    #[must_use]
    pub const fn is_control(&self) -> bool {
        matches!(self, Message::Control(..))
    }

    /// Checks if this message is a shutdown control message.
    #[must_use]
    pub const fn is_shutdown(&self) -> bool {
        matches!(self, Message::Control(NodeControlMsg::Shutdown { .. }))
    }
}

/// Trait for data types that can be marked as readonly.
///
/// This is used by the fanout sender to mark data as readonly when multiple
/// readonly consumers share the same data instance, enabling copy-on-write optimization.
pub trait ReadonlyMarkable {
    /// Marks this data as readonly to prevent mutation.
    fn mark_readonly(&mut self);
}

/// Implementation for unit type (used in tests).
impl ReadonlyMarkable for () {
    fn mark_readonly(&mut self) {
        // No-op for unit type
    }
}

/// Implementation for primitive types (used in tests).
macro_rules! impl_readonly_markable_for_primitives {
    ($($t:ty),*) => {
        $(
            impl ReadonlyMarkable for $t {
                fn mark_readonly(&mut self) {
                    // No-op for primitive types - they're Copy so marking is not needed
                }
            }
        )*
    };
}

impl_readonly_markable_for_primitives!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64, bool, char);

#[derive(Clone, Copy)]
enum FanoutRole {
    Mutable,
    Readonly,
}

struct FanoutSlot {
    index: usize,
    role: FanoutRole,
}

fn build_fanout_slots(
    num_senders: usize,
    mutable_indices: &[usize],
    readonly_indices: &[usize],
) -> (Vec<FanoutSlot>, usize) {
    assert!(
        num_senders > 0,
        "FanoutSender requires at least one consumer (got 0 senders)"
    );

    let mut roles = vec![None; num_senders];

    for &idx in mutable_indices {
        assert!(
            idx < num_senders,
            "Mutable index {} out of bounds (total senders: {})",
            idx,
            num_senders
        );
        if roles[idx].is_some() {
            panic!("Duplicate index {} in mutable_indices", idx);
        }
        roles[idx] = Some(FanoutRole::Mutable);
    }

    for &idx in readonly_indices {
        assert!(
            idx < num_senders,
            "Readonly index {} out of bounds (total senders: {})",
            idx,
            num_senders
        );
        match roles[idx] {
            Some(FanoutRole::Mutable) => panic!(
                "Index {} appears in both mutable and readonly lists",
                idx
            ),
            Some(FanoutRole::Readonly) => {
                panic!("Duplicate index {} in readonly_indices", idx);
            }
            None => roles[idx] = Some(FanoutRole::Readonly),
        }
    }

    assert_eq!(
        mutable_indices.len() + readonly_indices.len(),
        num_senders,
        "Total indices ({}) must equal total senders ({})",
        mutable_indices.len() + readonly_indices.len(),
        num_senders
    );

    let mut readonly_count = 0;
    let mut slots = Vec::with_capacity(num_senders);
    for (index, role) in roles.into_iter().enumerate() {
        let role = role.unwrap_or_else(|| {
            panic!(
                "Sender at index {} missing capability assignment (mutable/readonly)",
                index
            )
        });
        if matches!(role, FanoutRole::Readonly) {
            readonly_count += 1;
        }
        slots.push(FanoutSlot { index, role });
    }

    (slots, readonly_count)
}

/// Fanout sender implementation for `!Send` (local) pipelines.
///
/// Preserves destination ordering while minimizing clones:
/// - Mutating consumers receive clones except for the final consumer overall
/// - Readonly consumers share the original payload when possible
/// - Data is marked readonly once if multiple readonly consumers share it
///
/// # Design Note
///
/// This fanout sender is specifically for Local (!Send) pipelines. For Shared (Send)
/// contexts with multiple consumers, use MPMC channels instead, which provide a proven
/// pull-based pattern for multi-consumer scenarios.
#[must_use = "LocalFanoutSender should be used to send messages"]
pub struct LocalFanoutSender<T> {
    senders: Vec<LocalSender<T>>,
    slots: Vec<FanoutSlot>,
    readonly_count: usize,
}

impl<T: Clone + ReadonlyMarkable> LocalFanoutSender<T> {
    /// Constructs a new local fanout sender that validates capability
    /// assignments and preserves the configured destination order.
    #[must_use = "LocalFanoutSender must be used after construction"]
    pub fn new(
        senders: Vec<LocalSender<T>>,
        mutable_indices: Vec<usize>,
        readonly_indices: Vec<usize>,
    ) -> Self {
        let num_senders = senders.len();
        let (slots, readonly_count) =
            build_fanout_slots(num_senders, &mutable_indices, &readonly_indices);

        Self {
            senders,
            slots,
            readonly_count,
        }
    }

    /// Asynchronously delivers `data` to all configured destinations following Go's optimization strategy.
    ///
    /// Optimization strategy (matching opentelemetry-collector fanoutconsumer):
    /// 1. Mutable consumers: Clone for each except the last if no readonly consumers exist
    /// 2. Readonly consumers: Share the same Rc-wrapped instance (zero-copy!)
    /// 3. If both mutable and readonly exist: All get clones/shares, original is not used
    ///
    /// This achieves true zero-copy for readonly consumers by sharing an Rc pointer
    /// instead of cloning the data.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if any destination channel is closed or full.
    pub async fn send(&self, mut data: T) -> Result<(), SendError<T>> {
        // Separate slots into mutable and readonly groups
        let mut mutable_slots = Vec::new();
        let mut readonly_slots = Vec::new();
        
        for slot in &self.slots {
            match slot.role {
                FanoutRole::Mutable => mutable_slots.push(slot),
                FanoutRole::Readonly => readonly_slots.push(slot),
            }
        }

        eprintln!("🔀 [FANOUT] Starting fanout: {} mutable, {} readonly consumers", 
                  mutable_slots.len(), readonly_slots.len());

        // Phase 1: Send to mutable consumers
        if !mutable_slots.is_empty() {
            eprintln!("  [PHASE 1] Processing {} mutable consumers", mutable_slots.len());
            
            // Clone for all mutable consumers except the last
            for i in 0..mutable_slots.len() - 1 {
                eprintln!("    📋 Mutable consumer {} (index {}): CLONING data", i + 1, mutable_slots[i].index);
                self.senders[mutable_slots[i].index].send(data.clone()).await?;
            }

            // Last mutable consumer: gets original if no readonly consumers, clone otherwise
            let last_mutable = mutable_slots[mutable_slots.len() - 1];
            if readonly_slots.is_empty() {
                // No readonly consumers: last mutable gets the original (no clone)
                eprintln!("    ✅ Last mutable consumer (index {}): MOVING original (no readonly consumers)", 
                         last_mutable.index);
                self.senders[last_mutable.index].send(data).await?;
                return Ok(());
            } else {
                // Readonly consumers exist: last mutable gets a clone too
                eprintln!("    📋 Last mutable consumer (index {}): CLONING (readonly consumers exist)", 
                         last_mutable.index);
                self.senders[last_mutable.index].send(data.clone()).await?;
            }
        }

        // Phase 2: Send to readonly consumers  
        if !readonly_slots.is_empty() {
            eprintln!("  [PHASE 2] Processing {} readonly consumers", readonly_slots.len());
            
            // Mark data as readonly if multiple readonly consumers will share it
            if readonly_slots.len() > 1 {
                eprintln!("    🔒 Marking data as readonly (multiple readonly consumers)");
                data.mark_readonly();
            }

            // For readonly consumers, we just clone and send to each
            // The mark_readonly() call enables internal COW optimization within the data type
            // This matches the Go implementation where readonly consumers share the data
            for i in 0..readonly_slots.len() - 1 {
                eprintln!("    📋 Readonly consumer {} (index {}): CLONING data", i + 1, readonly_slots[i].index);
                self.senders[readonly_slots[i].index].send(data.clone()).await?;
            }

            // Last readonly consumer gets the original moved data
            let last_readonly = readonly_slots[readonly_slots.len() - 1];
            eprintln!("    ✅ Last readonly consumer (index {}): MOVING original (zero-copy!)", last_readonly.index);
            self.senders[last_readonly.index].send(data).await?;
        }

        eprintln!("🔀 [FANOUT] Complete\n");
        Ok(())
    }
    /// Attempts to deliver `data` immediately without awaiting following Go's optimization strategy.
    ///
    /// Uses the same strategy as [`LocalFanoutSender::send`]:
    /// - Mutable consumers get clones (except last if no readonly)
    /// - Readonly consumers share Rc-wrapped instance (zero-copy)
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if any destination channel is closed or full.
    pub fn try_send(&self, mut data: T) -> Result<(), SendError<T>> {
        // Separate slots into mutable and readonly groups
        let mut mutable_slots = Vec::new();
        let mut readonly_slots = Vec::new();
        
        for slot in &self.slots {
            match slot.role {
                FanoutRole::Mutable => mutable_slots.push(slot),
                FanoutRole::Readonly => readonly_slots.push(slot),
            }
        }

        // Phase 1: Send to mutable consumers
        if !mutable_slots.is_empty() {
            // Clone for all mutable consumers except the last
            for i in 0..mutable_slots.len() - 1 {
                self.senders[mutable_slots[i].index].try_send(data.clone())?;
            }

            // Last mutable consumer: gets original if no readonly consumers, clone otherwise
            let last_mutable = mutable_slots[mutable_slots.len() - 1];
            if readonly_slots.is_empty() {
                // No readonly consumers: last mutable gets the original (no clone)
                self.senders[last_mutable.index].try_send(data)?;
                return Ok(());
            } else {
                // Readonly consumers exist: last mutable gets a clone too
                self.senders[last_mutable.index].try_send(data.clone())?;
            }
        }

        // Phase 2: Send to readonly consumers
        if !readonly_slots.is_empty() {
            // Mark data as readonly if multiple readonly consumers will share it
            if readonly_slots.len() > 1 {
                data.mark_readonly();
            }

            // For readonly consumers, we just clone and send to each
            // The mark_readonly() call enables internal COW optimization within the data type
            // This matches the Go implementation where readonly consumers share the data
            for i in 0..readonly_slots.len() - 1 {
                self.senders[readonly_slots[i].index].try_send(data.clone())?;
            }

            // Last readonly consumer gets the original moved data
            let last_readonly = readonly_slots[readonly_slots.len() - 1];
            self.senders[last_readonly.index].try_send(data)?;
        }

        Ok(())
    }
}

/// A generic channel Sender supporting both local and shared semantic (i.e. !Send and Send).
///
/// # Fanout Support
///
/// This enum includes a `LocalFanout` variant for efficiently broadcasting data to multiple
/// consumers in Local (!Send) pipelines with smart cloning based on consumer capabilities.
/// The fanout sender is wrapped in an `Rc` to allow cloning the Sender enum (needed when
/// the EffectHandler is cloned for spawned tasks).
/// 
/// For Shared (Send) contexts with multiple consumers, use MPMC channels instead.
#[must_use = "A `Sender` is requested but not used."]
pub enum Sender<T> {
    /// Sender of a local channel.
    Local(LocalSender<T>),
    /// Sender of a shared channel.
    Shared(SharedSender<T>),
    /// Fanout sender operating on local (`!Send`) channels.
    /// Wrapped in Rc to allow cloning the Sender enum.
    LocalFanout(Rc<LocalFanoutSender<T>>),
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        match self {
            Sender::Local(sender) => Sender::Local(sender.clone()),
            Sender::Shared(sender) => Sender::Shared(sender.clone()),
            Sender::LocalFanout(fanout) => Sender::LocalFanout(fanout.clone()),
        }
    }
}

impl<T> Sender<T> {
    /// Creates a new local MPSC sender.
    pub fn new_local_mpsc_sender(mpsc_sender: mpsc::Sender<T>) -> Self {
        Sender::Local(LocalSender::MpscSender(mpsc_sender))
    }
}

// Methods that work for all sender types
impl<T> Sender<T> {
    /// Sends a message to the channel.
    ///
    /// # Panics
    ///
    /// Panics if called on a `LocalFanout` sender when `T` doesn't implement
    /// `Clone + ReadonlyMarkable`. Use `send_fanout()` for fanout senders.
    pub async fn send(&self, msg: T) -> Result<(), SendError<T>> {
        match self {
            Sender::Local(sender) => sender.send(msg).await,
            Sender::Shared(sender) => sender.send(msg).await,
            Sender::LocalFanout(_) => {
                panic!("Cannot call send() on LocalFanout - use send_fanout() instead")
            }
        }
    }

    /// Attempts to send a message without awaiting.
    ///
    /// # Panics
    ///
    /// Panics if called on a `LocalFanout` sender when `T` doesn't implement
    /// `Clone + ReadonlyMarkable`. Use `try_send_fanout()` for fanout senders.
    pub fn try_send(&self, msg: T) -> Result<(), SendError<T>> {
        match self {
            Sender::Local(sender) => sender.try_send(msg),
            Sender::Shared(sender) => sender.try_send(msg),
            Sender::LocalFanout(_) => {
                panic!("Cannot call try_send() on LocalFanout - use try_send_fanout() instead")
            }
        }
    }
}

// Methods specific to types that support fanout
impl<T: Clone + ReadonlyMarkable> Sender<T> {
    /// Sends a message to the channel, supporting all sender types including fanout.
    pub async fn send_fanout(&self, msg: T) -> Result<(), SendError<T>> {
        match self {
            Sender::Local(sender) => sender.send(msg).await,
            Sender::Shared(sender) => sender.send(msg).await,
            Sender::LocalFanout(fanout) => fanout.send(msg).await,
        }
    }

    /// Attempts to send a message without awaiting, supporting all sender types including fanout.
    pub fn try_send_fanout(&self, msg: T) -> Result<(), SendError<T>> {
        match self {
            Sender::Local(sender) => sender.try_send(msg),
            Sender::Shared(sender) => sender.try_send(msg),
            Sender::LocalFanout(fanout) => fanout.try_send(msg),
        }
    }
}

/// A generic channel Receiver supporting both local and shared semantic (i.e. !Send and Send).
pub enum Receiver<T> {
    /// Receiver of a local channel.
    Local(LocalReceiver<T>),
    /// Receiver of a shared channel.
    Shared(SharedReceiver<T>),
}

impl<T> Receiver<T> {
    /// Creates a new local MPMC receiver.
    #[must_use]
    pub fn new_local_mpsc_receiver(mpsc_receiver: mpsc::Receiver<T>) -> Self {
        Receiver::Local(LocalReceiver::MpscReceiver(mpsc_receiver))
    }

    /// Receives a message from the channel.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        match self {
            Receiver::Local(receiver) => receiver.recv().await,
            Receiver::Shared(receiver) => receiver.recv().await,
        }
    }

    /// Tries to receive a message from the channel.
    pub fn try_recv(&mut self) -> Result<T, RecvError> {
        match self {
            Receiver::Local(receiver) => receiver.try_recv(),
            Receiver::Shared(receiver) => receiver.try_recv(),
        }
    }

    /// Checks if the channel is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Receiver::Local(receiver) => receiver.is_empty(),
            Receiver::Shared(receiver) => receiver.is_empty(),
        }
    }
}

/// A channel for receiving control and pdata messages.
///
/// Control messages are prioritized until the first `Shutdown` is received.
/// After that, only pdata messages are considered, up to the deadline.
///
/// Note: This approach is used to implement a graceful shutdown. The engine will first close all
/// data sources in the pipeline, and then send a shutdown message with a deadline to all nodes in
/// the pipeline.
pub struct MessageChannel<PData> {
    control_rx: Option<Receiver<NodeControlMsg<PData>>>,
    pdata_rx: Option<Receiver<PData>>,
    /// Once a Shutdown is seen, this is set to `Some(instant)` at which point
    /// no more pdata will be accepted.
    shutting_down_deadline: Option<Instant>,
    /// Holds the ControlMsg::Shutdown until after we’ve drained pdata.
    pending_shutdown: Option<NodeControlMsg<PData>>,
}

impl<PData> MessageChannel<PData> {
    /// Creates a new `MessageChannel` with the given control and data receivers.
    #[must_use]
    pub fn new(control_rx: Receiver<NodeControlMsg<PData>>, pdata_rx: Receiver<PData>) -> Self {
        MessageChannel {
            control_rx: Some(control_rx),
            pdata_rx: Some(pdata_rx),
            shutting_down_deadline: None,
            pending_shutdown: None,
        }
    }

    /// Asynchronously receives the next message to process.
    ///
    /// Order of precedence:
    ///
    /// 1. Before a `Shutdown` is seen: control messages are always
    ///    returned ahead of pdata.
    /// 2. After the first `Shutdown` is received:
    ///    - All further control messages are silently discarded.
    ///    - Pending pdata are drained until the shutdown deadline.
    /// 3. When the deadline expires (or was `0`): the stored `Shutdown` is returned.
    ///    Subsequent calls return `RecvError::Closed`.
    ///
    /// # Errors
    ///
    /// Returns a [`RecvError`] if both channels are closed, or if the
    /// shutdown deadline has passed.
    pub async fn recv(&mut self) -> Result<Message<PData>, RecvError> {
        let mut sleep_until_deadline: Option<Pin<Box<Sleep>>> = None;

        loop {
            if self.control_rx.is_none() || self.pdata_rx.is_none() {
                // MessageChannel has been shutdown
                return Err(RecvError::Closed);
            }

            // Draining mode: Shutdown pending
            if let Some(dl) = self.shutting_down_deadline {
                // If shutdown pending and no pdata left, return Shutdown immediately
                if self
                    .pdata_rx
                    .as_ref()
                    .expect("pdata_rs must exist")
                    .is_empty()
                {
                    let shutdown = self
                        .pending_shutdown
                        .take()
                        .expect("pending_shutdown must exist");
                    self.shutdown();
                    return Ok(Message::Control(shutdown));
                }

                if sleep_until_deadline.is_none() {
                    // Create a sleep timer for the deadline
                    sleep_until_deadline = Some(Box::pin(sleep_until(dl.into())));
                }

                // Drain pdata first, then timer, then other control msgs
                tokio::select! {
                    biased;

                    // 1) Deadline hit?
                    _ = sleep_until_deadline.as_mut().expect("sleep_until_deadline must exist") => {
                        let shutdown = self.pending_shutdown
                            .take()
                            .expect("pending_shutdown must exist");
                        self.shutdown();
                        return Ok(Message::Control(shutdown));
                    }

                    // 2) Any pdata?
                    pdata = self.pdata_rx.as_mut().expect("pdata_rx must exist").recv() => match pdata {
                        Ok(pdata) => return Ok(Message::PData(pdata)),
                        Err(_) => {
                            // pdata channel closed → emit Shutdown
                            let shutdown = self.pending_shutdown
                                .take()
                                .expect("pending_shutdown must exist");
                            self.shutdown();
                            return Ok(Message::Control(shutdown));
                        }
                    },


                }
            }

            // Normal mode: no shutdown yet
            tokio::select! {
                biased;

                // A) Control first
                ctrl = self.control_rx.as_mut().expect("control_rx must exist").recv() => match ctrl {
                    Ok(NodeControlMsg::Shutdown { deadline, reason }) => {
                        if deadline.duration_since(Instant::now()).is_zero() {
                            // Immediate shutdown, no draining
                            self.shutdown();
                            return Ok(Message::Control(NodeControlMsg::Shutdown { deadline, reason }));
                        }
                        // Begin draining mode, but don’t return Shutdown yet
                        let when = deadline;
                        self.shutting_down_deadline = Some(when);
                        self.pending_shutdown = Some(NodeControlMsg::Shutdown { deadline, reason });
                        continue; // re-enter the loop into draining mode
                    }
                    Ok(msg) => return Ok(Message::Control(msg)),
                    Err(e)  => return Err(e),
                },

                // B) Then pdata
                pdata = self.pdata_rx.as_mut().expect("pdata_rx must exist").recv() => {
                    match pdata {
                        Ok(pdata) => {
                            return Ok(Message::PData(pdata));
                        }
                        Err(RecvError::Closed) => {
                            // pdata channel closed -> emit Shutdown
                            self.shutdown();
                            return Ok(Message::Control(NodeControlMsg::Shutdown {
                                deadline: Instant::now().add(Duration::from_secs(1)),
                                reason: "pdata channel closed".to_owned(),
                            }));
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }
            }
        }
    }

    fn shutdown(&mut self) {
        self.shutting_down_deadline = None;
        drop(self.control_rx.take().expect("control_rx must exist"));
        drop(self.pdata_rx.take().expect("pdata_rx must exist"));
    }
}
