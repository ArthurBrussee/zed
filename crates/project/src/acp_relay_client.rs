//! quiet-ui: the laptop's end of the links a remote project's agents run
//! through. The relay on the box owns the agents; a link carries one agent
//! connection's JSON-RPC lines both ways and survives the connection to the
//! box dropping, and the server on the box being replaced, without the
//! connection above it noticing anything but a pause.

use anyhow::{Context as _, Result};
use collections::{BTreeMap, HashMap, VecDeque};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{AsyncApp, Context, Entity, Subscription, Task};
use remote::{RemoteClient, RemoteClientEvent};
use rpc::{AnyProtoClient, TypedEnvelope, proto};

pub enum RelayEvent {
    Line(String),
    Stderr(String),
    Exited(Option<i32>),
    /// The connection to the box is down; the agent is still running there.
    Reconnecting,
    Reconnected,
}

/// One agent connection through the relay. Both channels are `Send`, since
/// the ACP connection drives its transport off the main thread.
pub struct RelayLink {
    pub link_id: u64,
    pub incoming: mpsc::UnboundedReceiver<RelayEvent>,
    pub outgoing: mpsc::UnboundedSender<String>,
}

struct LinkState {
    events: mpsc::UnboundedSender<RelayEvent>,
    last_lseq: u64,
    held: BTreeMap<u64, proto::acp_relay_frame::Kind>,
    unacked: VecDeque<(u64, String)>,
    next_cseq: u64,
    acked_lseq: u64,
    _sender: Task<()>,
}

pub struct AcpRelayClient {
    project_id: u64,
    client: AnyProtoClient,
    links: HashMap<u64, LinkState>,
    _remote_subscription: Option<Subscription>,
}

const ACK_EVERY: u64 = 64;

impl AcpRelayClient {
    pub fn new(
        project_id: u64,
        client: AnyProtoClient,
        remote: Option<&Entity<RemoteClient>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let remote_subscription = remote.map(|remote| {
            cx.subscribe(remote, |this, _, event, cx| match event {
                RemoteClientEvent::Reconnected => this.reattach(cx),
                RemoteClientEvent::Disconnected { .. } => this.broadcast(|| RelayEvent::Reconnecting),
            })
        });
        Self {
            project_id,
            client,
            links: HashMap::default(),
            _remote_subscription: remote_subscription,
        }
    }

    pub fn init(client: &AnyProtoClient) {
        client.add_entity_message_handler(Self::handle_frame);
    }

    pub fn open(
        &mut self,
        agent_name: String,
        command: crate::agent_server_store::AgentServerCommand,
        root_dir: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<RelayLink>> {
        let request = self.client.request(proto::AcpRelayOpen {
            project_id: self.project_id,
            agent_name,
            path: command.path.to_string_lossy().into_owned(),
            args: command.args,
            env: command.env.unwrap_or_default().into_iter().collect(),
            root_dir,
        });
        cx.spawn(async move |this, cx| {
            let link_id = request.await?.link_id;
            let (events_tx, incoming) = mpsc::unbounded();
            let (outgoing, mut outgoing_rx) = mpsc::unbounded::<String>();
            this.update(cx, |this, cx| {
                let sender = cx.spawn(async move |this, cx| {
                    while let Some(line) = outgoing_rx.next().await {
                        if this
                            .update(cx, |this, cx| this.send_line(link_id, line, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                });
                this.links.insert(
                    link_id,
                    LinkState {
                        events: events_tx,
                        last_lseq: 0,
                        held: BTreeMap::new(),
                        unacked: VecDeque::new(),
                        next_cseq: 1,
                        acked_lseq: 0,
                        _sender: sender,
                    },
                );
            })?;
            log::info!("quiet-ui relay: opened link {link_id}");
            Ok(RelayLink {
                link_id,
                incoming,
                outgoing,
            })
        })
    }

    /// The agent connection is done with this link. Its sessions keep running
    /// on the box until they are closed.
    pub fn close(&mut self, link_id: u64) {
        if self.links.remove(&link_id).is_some() {
            self.client
                .send(proto::AcpRelayClose {
                    project_id: self.project_id,
                    link_id,
                })
                .ok();
        }
    }

    /// The sessions the relay on the box is running, attached or not.
    pub fn list_sessions(
        &self,
    ) -> impl Future<Output = Result<Vec<proto::AcpRelaySession>>> + use<> {
        let request = self.client.request(proto::AcpRelayListSessions {
            project_id: self.project_id,
        });
        async move { Ok(request.await?.sessions) }
    }

    fn send_line(&mut self, link_id: u64, line: String, cx: &mut Context<Self>) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        let cseq = link.next_cseq;
        link.next_cseq += 1;
        link.unacked.push_back((cseq, line.clone()));
        self.post(link_id, cseq, line, cx);
    }

    fn post(&self, link_id: u64, cseq: u64, line: String, cx: &mut Context<Self>) {
        let request = self.client.request(proto::AcpRelaySend {
            project_id: self.project_id,
            link_id,
            cseq,
            line,
        });
        cx.spawn(async move |this, cx| {
            match request.await {
                Ok(_) => {
                    this.update(cx, |this, _| {
                        if let Some(link) = this.links.get_mut(&link_id) {
                            link.unacked.retain(|(sent, _)| *sent > cseq);
                        }
                    })
                    .ok();
                }
                // The server no longer carries the link (it lost the relay,
                // or is a new one): attach again, which resends what is
                // unacked or, when the relay is gone too, ends the link.
                Err(error) => {
                    log::warn!("quiet-ui relay: sending on link {link_id} failed: {error:#}");
                    this.update(cx, |this, cx| this.reattach_link(link_id, cx)).ok();
                }
            }
        })
        .detach();
    }

    fn broadcast(&self, event: impl Fn() -> RelayEvent) {
        for link in self.links.values() {
            link.events.unbounded_send(event()).ok();
        }
    }

    /// After any reconnect, ask the box to resume each link where this side
    /// left off. When the server on the box is the same one, nothing was lost
    /// and it says so; when it is a new one, the relay replays what this side
    /// missed and says which of this side's lines to send again.
    fn reattach(&mut self, cx: &mut Context<Self>) {
        let links = self.links.keys().copied().collect::<Vec<_>>();
        for link_id in links {
            self.reattach_link(link_id, cx);
        }
    }

    fn reattach_link(&mut self, link_id: u64, cx: &mut Context<Self>) {
        {
            let Some(link) = self.links.get(&link_id) else {
                return;
            };
            let request = self.client.request(proto::AcpRelayAttach {
                project_id: self.project_id,
                link_id,
                last_lseq: link.last_lseq,
            });
            cx.spawn(async move |this, cx| {
                let result = request.await;
                this.update(cx, |this, cx| {
                    let Some(link) = this.links.get_mut(&link_id) else {
                        return;
                    };
                    match result {
                        Ok(attached) => {
                            let resend = if attached.next_cseq == 0 {
                                Vec::new()
                            } else {
                                link.unacked
                                    .iter()
                                    .filter(|(cseq, _)| *cseq >= attached.next_cseq)
                                    .cloned()
                                    .collect()
                            };
                            link.events.unbounded_send(RelayEvent::Reconnected).ok();
                            log::info!(
                                "quiet-ui relay: link {link_id} reattached, resending {}",
                                resend.len()
                            );
                            for (cseq, line) in resend {
                                this.post(link_id, cseq, line, cx);
                            }
                        }
                        Err(error) => {
                            log::warn!("quiet-ui relay: link {link_id} is gone: {error:#}");
                            link.events.unbounded_send(RelayEvent::Exited(None)).ok();
                            this.links.remove(&link_id);
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    async fn handle_frame(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AcpRelayFrame>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, _| {
            let frame = envelope.payload;
            let kind = frame.kind.context("a relay frame without a kind")?;
            let Some(link) = this.links.get_mut(&frame.link_id) else {
                return Ok(());
            };
            // Numbered 0 by a server that lost the relay and with it the
            // link's sequence: it comes out of order, and ends the link.
            if frame.lseq == 0 {
                if let proto::acp_relay_frame::Kind::Exited(exit) = kind {
                    log::warn!("quiet-ui relay: link {} lost its relay", frame.link_id);
                    link.events.unbounded_send(RelayEvent::Exited(exit.code)).ok();
                    this.links.remove(&frame.link_id);
                }
                return Ok(());
            }
            if frame.lseq <= link.last_lseq {
                return Ok(());
            }
            link.held.insert(frame.lseq, kind);
            while let Some(kind) = link.held.remove(&(link.last_lseq + 1)) {
                link.last_lseq += 1;
                let event = match kind {
                    proto::acp_relay_frame::Kind::Line(line) => RelayEvent::Line(line),
                    proto::acp_relay_frame::Kind::Stderr(line) => RelayEvent::Stderr(line),
                    proto::acp_relay_frame::Kind::Exited(exit) => RelayEvent::Exited(exit.code),
                };
                link.events.unbounded_send(event).ok();
            }
            if link.last_lseq >= link.acked_lseq + ACK_EVERY {
                link.acked_lseq = link.last_lseq;
                this.client
                    .send(proto::AcpRelayAck {
                        project_id: this.project_id,
                        link_id: frame.link_id,
                        lseq: link.last_lseq,
                    })
                    .ok();
            }
            Ok(())
        })
    }
}

impl Drop for AcpRelayClient {
    fn drop(&mut self) {
        let links = self.links.keys().copied().collect::<Vec<_>>();
        for link_id in links {
            self.close(link_id);
        }
    }
}
