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
/// readonly consumers share the same data instance.
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

/// Implementation for primitive integer types (used in tests).
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

    /// Asynchronously delivers `data` to all configured destinations.
    ///
    /// Clones are minimized by:
    /// - Sending the original to the final consumer (no clone needed)
    /// - Cloning for all other consumers
    /// - Marking data readonly once if multiple readonly consumers share it
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if any destination channel is closed or full.
    pub async fn send(&self, mut data: T) -> Result<(), SendError<T>> {
        let total = self.slots.len();
        let mut remaining = total;
        let mark_readonly = self.readonly_count > 1;
        let mut readonly_marked = !mark_readonly;

        for slot in &self.slots {
            remaining -= 1;
            if matches!(slot.role, FanoutRole::Readonly) && !readonly_marked {
                data.mark_readonly();
                readonly_marked = true;
            }

            if remaining == 0 {
                self.senders[slot.index].send(data).await?;
                return Ok(());
            } else {
                self.senders[slot.index].send(data.clone()).await?;
            }
        }

        Ok(())
    }

    /// Attempts to deliver `data` immediately without awaiting.
    ///
    /// Uses the same cloning strategy as [`LocalFanoutSender::send`] but returns
    /// immediately if any channel is full rather than awaiting.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if any destination channel is closed or full.
    pub fn try_send(&self, mut data: T) -> Result<(), SendError<T>> {
        let total = self.slots.len();
        let mut remaining = total;
        let mark_readonly = self.readonly_count > 1;
        let mut readonly_marked = !mark_readonly;

        for slot in &self.slots {
            remaining -= 1;
            if matches!(slot.role, FanoutRole::Readonly) && !readonly_marked {
                data.mark_readonly();
                readonly_marked = true;
            }

            if remaining == 0 {
                self.senders[slot.index].try_send(data)?;
                return Ok(());
            } else {
                self.senders[slot.index].try_send(data.clone())?;
            }
        }

        Ok(())
    }
}

/// Fanout sender implementation for `Send` (shared) pipelines.
///
/// Shares identical semantics with [`LocalFanoutSender`] but operates over
/// shared-channel primitives that are safe to move across threads.
#[must_use = "SharedFanoutSender should be used to send messages"]
pub struct SharedFanoutSender<T> {
    senders: Vec<SharedSender<T>>,
    slots: Vec<FanoutSlot>,
    readonly_count: usize,
}

impl<T: Clone + ReadonlyMarkable> SharedFanoutSender<T> {
    /// Constructs a new shared fanout sender that validates capability
    /// assignments and preserves the configured destination order.
    #[must_use = "SharedFanoutSender must be used after construction"]
    pub fn new(
        senders: Vec<SharedSender<T>>,
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

    /// Asynchronously delivers `data` to all configured destinations.
    ///
    /// Clones are minimized by:
    /// - Sending the original to the final consumer (no clone needed)
    /// - Cloning for all other consumers
    /// - Marking data readonly once if multiple readonly consumers share it
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if any destination channel is closed or full.
    pub async fn send(&self, mut data: T) -> Result<(), SendError<T>> {
        let total = self.slots.len();
        let mut remaining = total;
        let mark_readonly = self.readonly_count > 1;
        let mut readonly_marked = !mark_readonly;

        for slot in &self.slots {
            remaining -= 1;
            if matches!(slot.role, FanoutRole::Readonly) && !readonly_marked {
                data.mark_readonly();
                readonly_marked = true;
            }

            if remaining == 0 {
                self.senders[slot.index].send(data).await?;
                return Ok(());
            } else {
                self.senders[slot.index].send(data.clone()).await?;
            }
        }

        Ok(())
    }

    /// Attempts to deliver `data` immediately without awaiting.
    ///
    /// Uses the same cloning strategy as [`SharedFanoutSender::send`] but returns
    /// immediately if any channel is full rather than awaiting.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if any destination channel is closed or full.
    pub fn try_send(&self, mut data: T) -> Result<(), SendError<T>> {
        let total = self.slots.len();
        let mut remaining = total;
        let mark_readonly = self.readonly_count > 1;
        let mut readonly_marked = !mark_readonly;

        for slot in &self.slots {
            remaining -= 1;
            if matches!(slot.role, FanoutRole::Readonly) && !readonly_marked {
                data.mark_readonly();
                readonly_marked = true;
            }

            if remaining == 0 {
                self.senders[slot.index].try_send(data)?;
                return Ok(());
            } else {
                self.senders[slot.index].try_send(data.clone())?;
            }
        }

        Ok(())
    }
}

/// Sender enum for local (!Send) contexts.
///
/// This enum contains only !Send sender variants, preventing it from being
/// used in Send contexts. This provides compile-time safety for local pipelines.
#[must_use = "A `LocalSenderEnum` is requested but not used."]
pub enum LocalSenderEnum<T> {
    /// Single-destination local MPSC sender.
    Mpsc(LocalSender<T>),
    /// Multi-destination local MPMC sender.
    Mpmc(LocalSender<T>),
    /// Multi-destination local fanout sender with smart cloning.
    Fanout(LocalFanoutSender<T>),
}

impl<T: Clone + ReadonlyMarkable> LocalSenderEnum<T> {
    /// Sends a message through this local sender.
    pub async fn send(&self, msg: T) -> Result<(), SendError<T>> {
        match self {
            LocalSenderEnum::Mpsc(sender) => sender.send(msg).await,
            LocalSenderEnum::Mpmc(sender) => sender.send(msg).await,
            LocalSenderEnum::Fanout(fanout) => fanout.send(msg).await,
        }
    }

    /// Attempts to send a message without awaiting.
    pub fn try_send(&self, msg: T) -> Result<(), SendError<T>> {
        match self {
            LocalSenderEnum::Mpsc(sender) => sender.try_send(msg),
            LocalSenderEnum::Mpmc(sender) => sender.try_send(msg),
            LocalSenderEnum::Fanout(fanout) => fanout.try_send(msg),
        }
    }
}

impl<T> Clone for LocalSenderEnum<T> {
    fn clone(&self) -> Self {
        match self {
            LocalSenderEnum::Mpsc(sender) => LocalSenderEnum::Mpsc(sender.clone()),
            LocalSenderEnum::Mpmc(sender) => LocalSenderEnum::Mpmc(sender.clone()),
            LocalSenderEnum::Fanout(_) => {
                panic!("LocalFanoutSender cannot be cloned - it owns multiple senders")
            }
        }
    }
}

/// Sender enum for shared (Send) contexts.
///
/// This enum contains only Send sender variants, allowing it to be safely
/// moved across threads. This provides compile-time safety for shared pipelines.
#[must_use = "A `SharedSenderEnum` is requested but not used."]
pub enum SharedSenderEnum<T> {
    /// Single-destination shared MPSC sender.
    Mpsc(SharedSender<T>),
    /// Multi-destination shared MPMC sender.
    Mpmc(SharedSender<T>),
    /// Multi-destination shared fanout sender with smart cloning.
    Fanout(SharedFanoutSender<T>),
}

impl<T: Clone + ReadonlyMarkable> SharedSenderEnum<T> {
    /// Sends a message through this shared sender.
    pub async fn send(&self, msg: T) -> Result<(), SendError<T>> {
        match self {
            SharedSenderEnum::Mpsc(sender) => sender.send(msg).await,
            SharedSenderEnum::Mpmc(sender) => sender.send(msg).await,
            SharedSenderEnum::Fanout(fanout) => fanout.send(msg).await,
        }
    }

    /// Attempts to send a message without awaiting.
    pub fn try_send(&self, msg: T) -> Result<(), SendError<T>> {
        match self {
            SharedSenderEnum::Mpsc(sender) => sender.try_send(msg),
            SharedSenderEnum::Mpmc(sender) => sender.try_send(msg),
            SharedSenderEnum::Fanout(fanout) => fanout.try_send(msg),
        }
    }
}

impl<T> Clone for SharedSenderEnum<T> {
    fn clone(&self) -> Self {
        match self {
            SharedSenderEnum::Mpsc(sender) => SharedSenderEnum::Mpsc(sender.clone()),
            SharedSenderEnum::Mpmc(sender) => SharedSenderEnum::Mpmc(sender.clone()),
            SharedSenderEnum::Fanout(_) => {
                panic!("SharedFanoutSender cannot be cloned - it owns multiple senders")
            }
        }
    }
}

/// Abstraction over every sender variant used by the runtime, allowing call
/// sites to remain agnostic to transport (local/shared) or fanout semantics.
///
/// **Note**: This unified enum is !Send because it contains !Send variants.
/// For Send contexts, use `SharedSenderEnum` instead. For !Send contexts,
/// use `LocalSenderEnum` for better type safety.
#[must_use = "A `Sender` is requested but not used."]
pub enum Sender<T> {
    /// Sender of a local channel.
    Local(LocalSender<T>),
    /// Sender of a shared channel.
    Shared(SharedSender<T>),
    /// Fanout sender operating on local (`!Send`) channels.
    LocalFanout(LocalFanoutSender<T>),
    /// Fanout sender operating on shared (`Send`) channels.
    SharedFanout(SharedFanoutSender<T>),
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        match self {
            Sender::Local(sender) => Sender::Local(sender.clone()),
            Sender::Shared(sender) => Sender::Shared(sender.clone()),
            Sender::LocalFanout(_) => {
                panic!("LocalFanoutSender cannot be cloned - it owns multiple senders")
            }
            Sender::SharedFanout(_) => {
                panic!("SharedFanoutSender cannot be cloned - it owns multiple senders")
            }
        }
    }
}

impl<T> Sender<T> {
    /// Creates a new local MPSC sender.
    pub fn new_local_mpsc_sender(mpsc_sender: mpsc::Sender<T>) -> Self {
        Sender::Local(LocalSender::MpscSender(mpsc_sender))
    }

    /// Sends a message to the channel.
    pub async fn send(&self, msg: T) -> Result<(), SendError<T>>
    where
        T: Clone + ReadonlyMarkable,
    {
        match self {
            Sender::Local(sender) => sender.send(msg).await,
            Sender::Shared(sender) => sender.send(msg).await,
            Sender::LocalFanout(fanout) => fanout.send(msg).await,
            Sender::SharedFanout(fanout) => fanout.send(msg).await,
        }
    }

    /// Attempts to send a message without awaiting.
    pub fn try_send(&self, msg: T) -> Result<(), SendError<T>>
    where
        T: Clone + ReadonlyMarkable,
    {
        match self {
            Sender::Local(sender) => sender.try_send(msg),
            Sender::Shared(sender) => sender.try_send(msg),
            Sender::LocalFanout(fanout) => fanout.try_send(msg),
            Sender::SharedFanout(fanout) => fanout.try_send(msg),
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
