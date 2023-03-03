use common::types::{
    AckKind, EntryIndex, IngressId, InvocationId, PartitionId, PeerId, ServiceInvocationId,
};
use futures::{stream, Sink, SinkExt, Stream, StreamExt};
use network::PartitionProcessorSender;
use std::collections::HashSet;
use std::convert::Infallible;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::pin::Pin;
use tracing::{debug, info};

mod effects;
mod leadership;
pub mod shuffle;
mod state_machine;
mod storage;

use crate::partition::effects::{Effects, Interpreter};
use crate::partition::leadership::{ActuatorOutput, LeadershipState};
use crate::partition::storage::PartitionStorage;
pub(crate) use state_machine::Command;

#[derive(Debug)]
pub(super) struct PartitionProcessor<
    CmdStream,
    ProposalSink,
    RawEntryCodec,
    InvokerInputSender,
    NetworkHandle,
    Storage,
> {
    peer_id: PeerId,
    partition_id: PartitionId,

    storage: Storage,

    command_stream: CmdStream,
    proposal_sink: ProposalSink,

    invoker_tx: InvokerInputSender,

    state_machine: state_machine::StateMachine<RawEntryCodec>,

    network_handle: NetworkHandle,

    ack_tx: PartitionProcessorSender<AckResponse>,

    _entry_codec: PhantomData<RawEntryCodec>,
}

#[derive(Debug, Clone)]
pub(super) struct RocksDBJournalReader;

impl invoker::JournalReader for RocksDBJournalReader {
    type JournalStream = stream::Empty<journal::raw::RawEntry>;
    type Error = Infallible;
    type Future = futures::future::Pending<
        Result<(invoker::JournalMetadata, Self::JournalStream), Self::Error>,
    >;

    fn read_journal(&self, _sid: &ServiceInvocationId) -> Self::Future {
        // TODO implement this
        unimplemented!("Implement JournalReader")
    }
}

impl<CmdStream, ProposalSink, RawEntryCodec, InvokerInputSender, NetworkHandle, Storage>
    PartitionProcessor<
        CmdStream,
        ProposalSink,
        RawEntryCodec,
        InvokerInputSender,
        NetworkHandle,
        Storage,
    >
where
    CmdStream: Stream<Item = consensus::Command<AckCommand>>,
    ProposalSink: Sink<AckCommand>,
    RawEntryCodec: journal::raw::RawEntryCodec + Default + Debug,
    InvokerInputSender: invoker::InvokerInputSender + Clone,
    NetworkHandle: network::NetworkHandle<shuffle::ShuffleInput, shuffle::ShuffleOutput>,
    Storage: storage_api::Storage + Clone + Send + Sync + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        peer_id: PeerId,
        partition_id: PartitionId,
        command_stream: CmdStream,
        proposal_sink: ProposalSink,
        invoker_tx: InvokerInputSender,
        storage: Storage,
        network_handle: NetworkHandle,
        ack_tx: PartitionProcessorSender<AckResponse>,
    ) -> Self {
        Self {
            peer_id,
            partition_id,
            command_stream,
            proposal_sink,
            invoker_tx,
            state_machine: Default::default(),
            storage,
            network_handle,
            ack_tx,
            _entry_codec: Default::default(),
        }
    }

    pub(super) async fn run(self) -> anyhow::Result<()> {
        let PartitionProcessor {
            peer_id,
            partition_id,
            command_stream,
            mut state_machine,
            invoker_tx,
            network_handle,
            storage,
            proposal_sink,
            ack_tx,
            ..
        } = self;
        tokio::pin!(command_stream);
        tokio::pin!(proposal_sink);

        // The max number of effects should be 2 atm (e.g. RegisterTimer and AppendJournalEntry)
        let mut effects = Effects::with_capacity(2);

        let (mut actuator_stream, mut leadership_state) =
            LeadershipState::follower(peer_id, partition_id, invoker_tx, network_handle);

        let mut partition_storage = PartitionStorage::new(partition_id, storage);

        loop {
            tokio::select! {
                command = command_stream.next() => {
                    if let Some(command) = command {
                        match command {
                            consensus::Command::Apply(ackable_command) => {
                                let (ack_target, fsm_command) = ackable_command.into_inner();

                                effects.clear();
                                state_machine.on_apply(fsm_command, &mut effects, &partition_storage).await?;

                                let message_collector = leadership_state.into_message_collector();

                                let transaction = partition_storage.create_transaction();
                                let result = Interpreter::<RawEntryCodec>::interpret_effects(&mut effects, transaction, message_collector).await?;

                                let message_collector = result.commit().await?;
                                leadership_state = message_collector.send().await?;

                                if let Some(ack_target) = ack_target {
                                    ack_tx.send(ack_target.acknowledge()).await?;
                                }
                            }
                            consensus::Command::BecomeLeader(leader_epoch) => {
                                info!(%peer_id, %partition_id, %leader_epoch, "Become leader.");
                                (actuator_stream, leadership_state) = leadership_state.become_leader(leader_epoch, partition_storage.clone()).await?;
                            }
                            consensus::Command::BecomeFollower => {
                                info!(%peer_id, %partition_id, "Become follower.");
                                (actuator_stream, leadership_state) = leadership_state.become_follower().await?;
                            },
                            consensus::Command::ApplySnapshot => {
                                unimplemented!("Not supported yet.");
                            }
                            consensus::Command::CreateSnapshot => {
                                unimplemented!("Not supported yet.");
                            }
                        }
                    } else {
                        break;
                    }
                },
                actuator_message = actuator_stream.next() => {
                    let actuator_message = actuator_message.ok_or(anyhow::anyhow!("actuator stream is closed"))?;
                    Self::propose_actuator_message(actuator_message, &mut proposal_sink).await;
                },
                task_result = leadership_state.run_tasks() => {
                    Err(task_result)?
                }
            }
        }

        debug!(%peer_id, %partition_id, "Shutting partition processor down.");
        let _ = leadership_state.become_follower().await;

        Ok(())
    }

    async fn propose_actuator_message(
        actuator_message: ActuatorOutput,
        proposal_sink: &mut Pin<&mut ProposalSink>,
    ) {
        match actuator_message {
            ActuatorOutput::Invoker(invoker_output) => {
                // Err only if the consensus module is shutting down
                let _ = proposal_sink
                    .send(AckCommand::no_ack(Command::Invoker(invoker_output)))
                    .await;
            }
            ActuatorOutput::Shuffle(outbox_truncation) => {
                // Err only if the consensus module is shutting down
                let _ = proposal_sink
                    .send(AckCommand::no_ack(Command::OutboxTruncation(
                        outbox_truncation.index(),
                    )))
                    .await;
            }
        };
    }
}

#[derive(Debug, PartialEq, Clone)]
pub(crate) enum InvocationStatus {
    Invoked(InvocationId),
    Suspended {
        invocation_id: InvocationId,
        waiting_for_completed_entries: HashSet<EntryIndex>,
    },
    Free,
}

#[derive(Debug)]
pub(super) struct AckCommand {
    command: Command,
    ack_target: Option<AckTarget>,
}

impl AckCommand {
    pub(super) fn ack(command: Command, ack_target: AckTarget) -> Self {
        Self {
            command,
            ack_target: Some(ack_target),
        }
    }

    pub(super) fn no_ack(command: Command) -> Self {
        Self {
            command,
            ack_target: None,
        }
    }

    fn into_inner(self) -> (Option<AckTarget>, Command) {
        (self.ack_target, self.command)
    }
}

#[derive(Debug)]
pub(super) enum AckTarget {
    Shuffle {
        shuffle_target: PeerId,
        msg_index: u64,
    },
    #[allow(dead_code)]
    Ingress {
        ingress_id: IngressId,
        msg_index: u64,
    },
}

impl AckTarget {
    pub(super) fn shuffle(shuffle_target: PeerId, msg_index: u64) -> Self {
        AckTarget::Shuffle {
            shuffle_target,
            msg_index,
        }
    }

    #[allow(dead_code)]
    pub(super) fn ingress(ingress_id: IngressId, msg_index: u64) -> Self {
        AckTarget::Ingress {
            ingress_id,
            msg_index,
        }
    }

    fn acknowledge(self) -> AckResponse {
        match self {
            AckTarget::Shuffle {
                shuffle_target,
                msg_index,
            } => AckResponse::Shuffle(ShuffleAckResponse {
                shuffle_target,
                kind: AckKind::Acknowledge(msg_index),
            }),
            AckTarget::Ingress {
                ingress_id,
                msg_index,
            } => AckResponse::Ingress(IngressAckResponse {
                _ingress_id: ingress_id,
                kind: AckKind::Acknowledge(msg_index),
            }),
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(super) enum AckResponse {
    Shuffle(ShuffleAckResponse),
    Ingress(IngressAckResponse),
}

#[derive(Debug)]
pub(super) struct ShuffleAckResponse {
    pub(crate) shuffle_target: PeerId,
    pub(crate) kind: AckKind,
}

#[derive(Debug)]
pub(super) struct IngressAckResponse {
    pub(crate) _ingress_id: IngressId,
    pub(crate) kind: AckKind,
}
