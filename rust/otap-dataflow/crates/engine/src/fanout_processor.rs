use crate::{
    Interests,
    error::Error,
    local::processor::{EffectHandler as LocalEffectHandler, Processor},
    message::Message,
    readonly::ReadonlyMarkable,
};

/// FanoutProcessor
///
/// Mutation-aware fan-out semantics matching OTel Collector behavior:
///
/// **Processing Order** (matches OTel Collector):
/// 1. **Mutating consumers** (MUTATION interest): All but last receive `clone()`
/// 2. **Last mutator**: Receives owned original ONLY if no readonly consumers exist; otherwise clone
/// 3. **Read-only consumers** (no MUTATION interest): Receive the original data
///    - Phase 1: Each gets `clone_read_only()` (full clone)
///    - Phase 2 TODO: Mark original as readonly, share among all readonly consumers (COW)
///
/// **Key Insight**: Readonly consumers can **share** the original data (just mark it readonly),
/// while mutators need **independent clones** for isolation. This ordering enables Phase 2 to
/// eliminate clones for readonly consumers entirely.
///
/// **Isolation Guarantees**:
/// - Each mutating consumer operates on independent data (cannot see others' mutations)
/// - Readonly consumers receive semantically identical data
/// - Phase 1: Isolation via cloning (safe, correct, but extra copies)
/// - Phase 2: Isolation via COW + mutation guards (optimal)
///
/// Control messages are currently ignored (TODO: revisit if control broadcast is needed).
///
/// TODO:
/// - Phase 2: Implement true COW with `is_read_only()` and `mark_read_only()`
/// - Shared (Send) pipeline parity
/// - Integrate with pipeline factory auto-insertion for multi-destination edges
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

    // Optimization: Single readonly consumer with no mutators can receive original (MOVE)
    if mutator_ports.is_empty() && read_only_ports.len() == 1 {
        let port = &read_only_ports[0];
        eprintln!(
            "[FanoutProcessor:{}]   MOVE (no clone) -> single readonly port '{}'",
            node_id, port
        );
        effect
            .send_message_to(port.clone(), data)
            .await
            .map_err(|e| Error::ChannelSendError {
                error: format!("fanout send (single readonly) to port '{port}' failed: {e}"),
            })?;
        return Ok(());
    }

    // === OTel Collector Ordering ===
    // 1. Send clones to all mutators except last
    // 2. Last mutator: original ONLY if no readonly consumers; otherwise clone
    // 3. Readonly consumers: share the original (Phase 1: clone each; Phase 2: mark and share)

    // Step 1: Clone for all mutators except last
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

    // Step 2: Last mutator gets original ONLY if no readonly consumers exist
    if !mutator_ports.is_empty() {
        let last_port = mutator_ports.last().unwrap().clone();

        if read_only_ports.is_empty() {
            // No readonly consumers - safe to give original to last mutator
            // This matches OTel Collector: `if len(lsc.readonly) == 0 && !ld.IsReadOnly()`
            //
            // Note: We defer `!data.is_read_only()` check to Phase 2 because:
            // 1. No upstream processor can pre-mark data readonly yet
            // 2. Blanket Clone impl ensures all clones are independent
            // 3. Phase 1 focuses on mutation isolation, not COW optimization
            effect
                .send_message_to(last_port.clone(), data)
                .await
                .map_err(|e| Error::ChannelSendError {
                    error: format!("fanout send (last mutator) to port '{last_port}' failed: {e}"),
                })?;
            return Ok(());
        } else {
            // Readonly consumers exist - must clone for last mutator to preserve original
            // This matches OTel Collector behavior: clone when readonly consumers need the data
            let cloned = data.clone();
            effect
                .send_message_to(last_port.clone(), cloned)
                .await
                .map_err(|e| Error::ChannelSendError {
                    error: format!("fanout send (last mutator) to port '{last_port}' failed: {e}"),
                })?;
        }
    }

    // Step 3: Readonly consumers receive the original data
    // Phase 1: Clone for each readonly consumer (safe but inefficient)
    // Phase 2 TODO: Mark original as readonly once, then share among all readonly consumers
    //   if read_only_ports.len() > 1 && !data.is_read_only() {
    //       data.mark_read_only();
    //   }
    //   for port in &read_only_ports {
    //       effect.send_message_to(port.clone(), data.clone()).await?;  // Cheap Arc clone
    //   }
    if !read_only_ports.is_empty() {
        for port in &read_only_ports {
            let cloned = data.clone_read_only();
            effect
                .send_message_to(port.clone(), cloned)
                .await
                .map_err(|e| Error::ChannelSendError {
                    error: format!("fanout send (read-only) to port '{port}' failed: {e}"),
                })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(missing_docs)]
    use super::*;
    use crate::local::message::LocalSender;
    use crate::local::processor::EffectHandler;
    use crate::testing::test_node;
    use otap_df_channel::mpsc;
    use otap_df_telemetry::reporter::MetricsReporter;
    use tokio::time::{Duration, timeout};

    // Simple pdata type
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct PTest(u64);

    // Helper to make a local channel
    fn channel<T>(capacity: usize) -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
        mpsc::Channel::new(capacity)
    }

    // Build an EffectHandler with given ports.
    fn make_effect_handler(ports: &[&str]) -> (EffectHandler<PTest>, Vec<mpsc::Receiver<PTest>>) {
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
        proc.process(Message::PData(PTest(7)), &mut eh)
            .await
            .unwrap();

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
        proc.process(Message::PData(PTest(11)), &mut eh)
            .await
            .unwrap();

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
        // Test: Single mutator with NO readonly consumers receives the original (no clone).
        // This matches OTel Collector: `if len(lsc.readonly) == 0 && !ld.IsReadOnly()`
        //
        // Phase 1 behavior:
        // - No readonly consumers exist, so original safely transferred to mutator
        // - No is_read_only() check needed (no upstream pre-marking capability exists)
        // - Phase 2 will add is_read_only() when COW layer is implemented
        let ports = ["mut_only"];
        let (mut eh, receivers) = make_effect_handler(&ports);
        eh.set_port_interests("mut_only", Interests::MUTATION)
            .unwrap();

        let mut proc = FanoutProcessor;
        proc.process(Message::PData(PTest(3)), &mut eh)
            .await
            .unwrap();

        let v = timeout(Duration::from_millis(50), receivers[0].recv())
            .await
            .expect("message")
            .expect("value");
        assert_eq!(v, PTest(3));
    }

    #[tokio::test]
    async fn fanout_mutator_with_readonly_consumers() {
        // Test: When readonly consumers exist, even the last (only) mutator must receive a clone.
        // This matches OTel Collector behavior where original is preserved for readonly consumers.
        //
        // Expected behavior:
        // - Mutator receives clone (because readonly consumers need the original)
        // - Readonly consumers receive clones (Phase 1) or shared original (Phase 2)
        // - All consumers receive semantically identical data
        let ports = ["mut1", "ro1", "ro2"];
        let (mut eh, receivers) = make_effect_handler(&ports);
        eh.set_port_interests("mut1", Interests::MUTATION).unwrap();
        // ro1 and ro2 have no MUTATION interest (default)

        let mut proc = FanoutProcessor;
        proc.process(Message::PData(PTest(42)), &mut eh)
            .await
            .unwrap();

        // All consumers should receive the data
        for (idx, rx) in receivers.iter().enumerate() {
            let v = timeout(Duration::from_millis(50), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("message timeout for receiver {}", idx))
                .unwrap_or_else(|_| panic!("no value for receiver {}", idx));
            assert_eq!(v, PTest(42), "receiver {} didn't get expected value", idx);
        }
    }

    #[tokio::test]
    async fn fanout_all_mutators_no_readonly() {
        // Test: Multiple mutators with NO readonly consumers
        // Expected: All but last receive clones, last receives original
        let ports = ["mut1", "mut2", "mut3"];
        let (mut eh, receivers) = make_effect_handler(&ports);
        eh.set_port_interests("mut1", Interests::MUTATION).unwrap();
        eh.set_port_interests("mut2", Interests::MUTATION).unwrap();
        eh.set_port_interests("mut3", Interests::MUTATION).unwrap();

        let mut proc = FanoutProcessor;
        proc.process(Message::PData(PTest(99)), &mut eh)
            .await
            .unwrap();

        // All mutators should receive the data
        for (idx, rx) in receivers.iter().enumerate() {
            let v = timeout(Duration::from_millis(50), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("message timeout for receiver {}", idx))
                .unwrap_or_else(|_| panic!("no value for receiver {}", idx));
            assert_eq!(v, PTest(99), "receiver {} didn't get expected value", idx);
        }
    }
}
