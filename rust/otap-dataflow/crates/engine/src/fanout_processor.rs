use crate::{
    error::Error,
    local::processor::{EffectHandler as LocalEffectHandler, Processor},
    message::Message,
    readonly::ReadonlyMarkable,
    Interests,
};

/// FanoutProcessor
///
/// Mutation-aware fan-out semantics:
/// - Read-only consumers (no MUTATION interest): receive `clone_read_only()` (may later be COW).
/// - Mutating consumers (MUTATION interest): all but the last mutator receive `clone()`;
///   the last mutator receives the owned original `PData`.
/// - Ordering independence: we first dispatch to all read-only ports, then to mutators so
///   the original is only moved after all read-only clones are created.
/// - If there are zero mutators, all consumers get read-only clones (non-mutators share).
///
/// Control messages are currently ignored (TODO: revisit if control broadcast is needed).
///
/// TODO:
/// - Shared (Send) pipeline parity.
/// - Optimize clone fan-out with real copy-on-write once available.
/// - Integrate with pipeline factory auto-insertion for multi-destination edges.
pub struct FanoutProcessor;

#[async_trait::async_trait(?Send)]
impl<PData: ReadonlyMarkable> Processor<PData> for FanoutProcessor {
    async fn process(
        &mut self,
        msg: Message<PData>,
        effect: &mut LocalEffectHandler<PData>,
    ) -> Result<(), Error> {
        match msg {
            Message::PData(data) => dispatch(effect, data).await,
            Message::Control(_ctrl) => {
                // TODO: optionally forward/broadcast control messages if fan-out insertion
                // bypasses normal control routing. For now, ignore.
                Ok(())
            }
        }
    }
}

async fn dispatch<PData: ReadonlyMarkable>(
    effect: &mut LocalEffectHandler<PData>,
    data: PData,
) -> Result<(), Error> {
    let metas = effect.connected_port_metas();
    if metas.is_empty() {
        return Ok(());
    }

    let mut read_only_ports = Vec::new();
    let mut mutator_ports = Vec::new();
    for (port, interests) in metas {
        if interests.contains(Interests::MUTATION) {
            mutator_ports.push(port);
        } else {
            read_only_ports.push(port);
        }
    }

    // Send read-only clones
    for port in &read_only_ports {
        let cloned = data.clone_read_only();
        effect
            .send_message_to(port.clone(), cloned)
            .await
            .map_err(|e| Error::ChannelSendError {
                error: format!("fanout send (read-only) to port '{port}' failed: {e}"),
            })?;
    }

    if mutator_ports.is_empty() {
        return Ok(());
    }

    // Send clones to all but last mutator
    if mutator_ports.len() > 1 {
        for port in &mutator_ports[..mutator_ports.len() - 1] {
            let cloned = data.clone();
            effect
                .send_message_to(port.clone(), cloned)
                .await
                .map_err(|e| Error::ChannelSendError {
                    error: format!("fanout send (mutator clone) to port '{port}' failed: {e}"),
                })?;
        }
    }

    // Send original to last mutator
    //
    // Note: OTel Collector checks `!ld.IsReadOnly()` here before sending original.
    // We defer this check to Phase 2 (COW implementation) because:
    // 1. No upstream processor can pre-mark data readonly yet (feature doesn't exist)
    // 2. Blanket Clone impl ensures all clones are independent (no shared state)
    // 3. Phase 1 focuses on mutation isolation semantics, not COW optimization
    // 4. Adding the check prematurely would complicate code without benefit
    //
    // When Phase 2 COW is implemented, add: `if !data.is_read_only() { ... }`
    let last_port = mutator_ports.last().unwrap().clone();
    effect
        .send_message_to(last_port.clone(), data)
        .await
        .map_err(|e| Error::ChannelSendError {
            error: format!("fanout send (last mutator) to port '{last_port}' failed: {e}"),
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(missing_docs)]
    use super::*;
    use crate::local::processor::EffectHandler;
    use crate::local::message::LocalSender;
    use crate::testing::test_node;
    use otap_df_channel::mpsc;
    use otap_df_telemetry::reporter::MetricsReporter;
    use tokio::time::{timeout, Duration};

    // Simple pdata type
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct PTest(u64);

    // Helper to make a local channel
    fn channel<T>(capacity: usize) -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
        mpsc::Channel::new(capacity)
    }

    // Build an EffectHandler with given ports.
    fn make_effect_handler(
        ports: &[&str],
    ) -> (EffectHandler<PTest>, Vec<mpsc::Receiver<PTest>>) {
        let mut senders = std::collections::HashMap::new();
        let mut receivers = Vec::new();
        for p in ports {
            let (tx, rx) = channel::<PTest>(10);
            let _ = senders.insert((*p).to_owned().into(), LocalSender::MpscSender(tx));
            receivers.push(rx);
        }
        let (_metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
        let eh = EffectHandler::new(test_node("fanout"), senders, None, metrics_reporter);
        (eh, receivers)
    }

    #[tokio::test]
    async fn fanout_all_read_only() {
        let ports = ["a", "b", "c"];
        let (mut eh, receivers) = make_effect_handler(&ports);
        // No MUTATION interests set (all read-only)
        let mut proc = FanoutProcessor;
        proc.process(Message::PData(PTest(7)), &mut eh).await.unwrap();

        // Expect one message per port
        for rx in receivers {
            let v = timeout(Duration::from_millis(50), rx.recv())
                .await
                .expect("message")
                .expect("value");
            assert_eq!(v, PTest(7));
        }
    }

    #[tokio::test]
    async fn fanout_mixed_read_only_and_mutators() {
        let ports = ["ro1", "mut1", "mut2", "ro2"];
        let (mut eh, receivers) = make_effect_handler(&ports);

        // Set MUTATION interests for mut1 & mut2
        eh.set_port_interests("mut1", Interests::MUTATION).unwrap();
        eh.set_port_interests("mut2", Interests::MUTATION).unwrap();

        let mut proc = FanoutProcessor;
        proc.process(Message::PData(PTest(11)), &mut eh).await.unwrap();

        // Collect received values
        let mut got = 0;
        for rx in receivers {
            let v = timeout(Duration::from_millis(50), rx.recv())
                .await
                .expect("message")
                .expect("value");
            assert_eq!(v, PTest(11));
            got += 1;
        }
        assert_eq!(got, ports.len());
    }

    #[tokio::test]
    async fn fanout_single_mutator_only() {
        // Phase 1 correctness: Single mutator receives original without cloning.
        // No is_read_only() check needed because:
        // - No upstream processor can pre-mark data readonly (feature doesn't exist)
        // - Blanket Clone impl means all previous clones are independent
        // - Phase 2 will add is_read_only() when COW layer is implemented
        let ports = ["mut_only"];
        let (mut eh, receivers) = make_effect_handler(&ports);
        eh.set_port_interests("mut_only", Interests::MUTATION).unwrap();

        let mut proc = FanoutProcessor;
        proc.process(Message::PData(PTest(3)), &mut eh).await.unwrap();

        let v = timeout(Duration::from_millis(50), receivers[0].recv())
            .await
            .expect("message")
            .expect("value");
        assert_eq!(v, PTest(3));
    }
}
