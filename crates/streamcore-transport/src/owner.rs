//! The transport owner (STREAMCORE-VIDEO 8.7): the one place on a client that
//! decides when to connect, when to move a live session to another transport
//! and when to resume, so that no operation ever races a live transport.
//!
//! It is a pure state machine. The consumer feeds it events with the current
//! time and carries out the actions it returns: dialing, probing, sending its
//! control hello, swapping channels, closing them and arming timers. It does no
//! I/O and keeps no clock, so every rule here is tested without a device.
//!
//! The rules it enforces:
//! - one control hello at a time, and no switch while connecting or resuming;
//! - a cable is adopted make-before-break: a stability hold, one dial with no
//!   retry, a probe that must pass, then the hello; the old channel is closed
//!   only after the host's answer;
//! - a rejected or failed candidate leaves the live transport untouched;
//! - a live transport that dies while switching resumes on the candidate only
//!   if it had passed its probe;
//! - a cable that keeps failing is left alone for a while.

/// Which kind of transport a channel is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Network,
    /// A relayed pipe over a cable.
    Usb,
}

/// The rules' numbers. The defaults are the design's.
#[derive(Clone, Copy, Debug)]
pub struct OwnerPolicy {
    /// No detach or flap for this long before a cable is dialed.
    pub stability_hold_ms: u64,
    pub probe_count: u8,
    pub probe_spacing_ms: u32,
    pub probe_deadline_ms: u32,
    /// Pongs a probe needs.
    pub probe_min_answered: u32,
    /// The candidate's round-trip p50 may exceed the live transport's by this.
    pub probe_rtt_margin_us: u64,
    /// How long to wait for the host's answer to the control hello before
    /// keeping both channels until one answers or closes.
    pub hello_answer_ms: u64,
    /// After a move off an unhealthy cable, ignore cable triggers this long.
    pub trigger_hold_ms: u64,
    /// This many cable losses within `flap_window_ms` keep the session on the
    /// network for `flap_stay_ms`, doubling each time it happens again.
    pub flap_limit: usize,
    pub flap_window_ms: u64,
    pub flap_stay_ms: u64,
    /// A resume that fails is tried again after this pause, doubling up to
    /// `resume_retry_max_ms`, and given up after `resume_attempts` in a row.
    pub resume_retry_ms: u64,
    pub resume_retry_max_ms: u64,
    pub resume_attempts: u32,
}

impl Default for OwnerPolicy {
    fn default() -> Self {
        Self {
            stability_hold_ms: 2_000,
            probe_count: 5,
            probe_spacing_ms: 20,
            probe_deadline_ms: 500,
            probe_min_answered: 4,
            probe_rtt_margin_us: 2_000,
            hello_answer_ms: 5_000,
            trigger_hold_ms: 10_000,
            flap_limit: 3,
            flap_window_ms: 60_000,
            flap_stay_ms: 300_000,
            resume_retry_ms: 150,
            resume_retry_max_ms: 30_000,
            resume_attempts: 8,
        }
    }
}

/// How far a switch has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Waiting out the stability hold (cable only).
    Holding,
    Dialing,
    /// The candidate is connected and being probed (cable only).
    Probing,
    /// The control hello is on the candidate; the answer is awaited.
    AwaitingAnswer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Connecting,
    Live {
        route: Route,
    },
    /// A candidate on `to` is being brought up beside the live transport.
    /// `live_closed`: the live one died after the candidate passed its probe,
    /// so the candidate is the only channel left.
    Switching {
        from: Route,
        to: Route,
        stage: Stage,
        live_closed: bool,
    },
    Resuming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    StabilityHold,
    HelloAnswer,
    /// The pause before the next resume attempt.
    ResumeRetry,
}

/// What happened. Events about a candidate are ignored when there is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The consumer wants a connection.
    ConnectRequested,
    /// The consumer wants none: close everything.
    Disconnect,
    /// A connect or resume finished: a channel on `route` is live and its
    /// control hello was answered.
    Connected {
        route: Route,
    },
    ConnectFailed,
    /// A cable pipe is there to dial: a plug, or a pipe accepted on the
    /// device's listener.
    UsbPipeReady,
    /// The cable's state changed without going away: restarts the hold.
    UsbFlapped,
    /// The cable pipe went away before it was used.
    UsbPipeGone,
    /// The live cable is backlogged or slow (the health rule).
    UsbUnhealthy,
    TimerFired(Timer),
    /// The candidate's dial finished.
    CandidateDialed,
    CandidateDialFailed,
    /// The candidate's probe finished. `live_rtt_p50_us` is the live
    /// transport's current round-trip p50, when known.
    Probed {
        answered: u32,
        rtt_p50_us: Option<u64>,
        elapsed_ms: u32,
        live_rtt_p50_us: Option<u64>,
    },
    /// The host answered the candidate's control hello: resumed, same session,
    /// a newer generation.
    HelloAccepted,
    HelloRejected,
    LiveClosed,
    CandidateClosed,
}

/// What the consumer does next, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Dial with no live transport, racing the network routes; with
    /// `usb_first` a cable pipe is one more route, ahead of them.
    Connect {
        usb_first: bool,
    },
    /// Dial one candidate on `route`, pinned to the current host: no retry.
    DialCandidate {
        route: Route,
    },
    ProbeCandidate {
        count: u8,
        spacing_ms: u32,
        deadline_ms: u32,
    },
    /// Send the control hello, with the resume reference and the generation
    /// last seen, on the candidate.
    SendHelloOnCandidate,
    /// Make the candidate the live transport in one step (channel, receiver,
    /// keepalive), then close the old channel and ignore what still arrives
    /// on it.
    AdoptCandidate,
    CloseCandidate,
    CloseLive,
    SetTimer(Timer, u64),
    CancelTimer(Timer),
    Report(Status),
}

/// What the consumer shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Live(Route),
    /// The session moved to `Route` without a break: not an interruption.
    TransportChanged(Route),
    /// The transport was lost and the session is being resumed: the last
    /// picture stays, input is dropped.
    Resuming,
    Disconnected,
}

pub struct TransportOwner {
    policy: OwnerPolicy,
    state: State,
    /// A cable pipe is there and unused.
    pipe_ready: bool,
    /// Cable triggers are ignored until this time.
    usb_barred_until: u64,
    /// When recent cable losses happened.
    usb_losses: Vec<u64>,
    /// How long the next flap bar lasts.
    next_flap_stay_ms: u64,
    /// Resume attempts that failed in a row.
    resume_failures: u32,
}

impl TransportOwner {
    pub fn new(policy: OwnerPolicy) -> Self {
        Self {
            policy,
            state: State::Idle,
            pipe_ready: false,
            usb_barred_until: 0,
            usb_losses: Vec::new(),
            next_flap_stay_ms: policy.flap_stay_ms,
            resume_failures: 0,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    fn usb_allowed(&self, now_ms: u64) -> bool {
        now_ms >= self.usb_barred_until
    }

    /// A cable transport was lost or left for health: count it, and bar the
    /// cable for a while when it keeps happening.
    fn usb_lost(&mut self, now_ms: u64) {
        let window = self.policy.flap_window_ms;
        self.usb_losses
            .retain(|at| now_ms.saturating_sub(*at) < window);
        self.usb_losses.push(now_ms);
        if self.usb_losses.len() >= self.policy.flap_limit {
            self.usb_losses.clear();
            self.usb_barred_until = self
                .usb_barred_until
                .max(now_ms.saturating_add(self.next_flap_stay_ms));
            self.next_flap_stay_ms = self.next_flap_stay_ms.saturating_mul(2);
        }
    }

    /// On a live network transport with a cable pipe waiting: start the
    /// make-before-break switch, if the cable is not barred.
    fn maybe_switch_to_usb(&mut self, now_ms: u64, actions: &mut Vec<Action>) {
        if self.state
            == (State::Live {
                route: Route::Network,
            })
            && self.pipe_ready
            && self.usb_allowed(now_ms)
        {
            self.state = State::Switching {
                from: Route::Network,
                to: Route::Usb,
                stage: Stage::Holding,
                live_closed: false,
            };
            actions.push(Action::SetTimer(
                Timer::StabilityHold,
                self.policy.stability_hold_ms,
            ));
        }
    }

    fn resume(&mut self, actions: &mut Vec<Action>) {
        self.state = State::Resuming;
        self.resume_failures = 0;
        actions.push(Action::Report(Status::Resuming));
        actions.push(Action::Connect {
            usb_first: self.pipe_ready,
        });
    }

    /// Ends a switch whose candidate failed: back on the live transport, or,
    /// when that died meanwhile, resuming.
    ///
    /// A cable candidate that fails (its dial, its probe, a rejected hello, or
    /// closing early) counts as a cable loss and holds cable triggers for a
    /// while: a cable that is plugged in but never good enough must not be
    /// dialed and probed every few seconds for as long as it stays plugged in.
    fn candidate_failed(
        &mut self,
        now_ms: u64,
        from: Route,
        to: Route,
        live_closed: bool,
        actions: &mut Vec<Action>,
    ) {
        if to == Route::Usb {
            self.usb_lost(now_ms);
            self.usb_barred_until = self
                .usb_barred_until
                .max(now_ms.saturating_add(self.policy.trigger_hold_ms));
        }
        if live_closed {
            // Already reported as resuming when the live transport closed.
            self.state = State::Resuming;
            self.resume_failures = 0;
            actions.push(Action::Connect {
                usb_first: self.pipe_ready && self.usb_allowed(now_ms),
            });
        } else {
            self.state = State::Live { route: from };
        }
    }

    /// Feeds one event at `now_ms` (any monotonic milliseconds) and returns
    /// what to do. Events that do not apply in the current state do nothing.
    pub fn handle(&mut self, now_ms: u64, event: Event) -> Vec<Action> {
        let mut actions = Vec::new();
        // The cable's presence is tracked in every state.
        match event {
            Event::UsbPipeReady => self.pipe_ready = true,
            Event::UsbPipeGone => self.pipe_ready = false,
            _ => {}
        }
        if event == Event::Disconnect {
            match self.state {
                State::Idle => {}
                State::Switching { live_closed, .. } => {
                    actions.push(Action::CancelTimer(Timer::StabilityHold));
                    actions.push(Action::CancelTimer(Timer::HelloAnswer));
                    actions.push(Action::CloseCandidate);
                    if !live_closed {
                        actions.push(Action::CloseLive);
                    }
                }
                State::Live { .. } => actions.push(Action::CloseLive),
                State::Resuming => actions.push(Action::CancelTimer(Timer::ResumeRetry)),
                State::Connecting => {}
            }
            if self.state != State::Idle {
                actions.push(Action::Report(Status::Disconnected));
            }
            self.state = State::Idle;
            return actions;
        }
        match self.state {
            State::Idle => {
                if event == Event::ConnectRequested {
                    self.state = State::Connecting;
                    actions.push(Action::Report(Status::Connecting));
                    actions.push(Action::Connect {
                        usb_first: self.pipe_ready && self.usb_allowed(now_ms),
                    });
                }
            }
            State::Connecting | State::Resuming => match event {
                Event::Connected { route } => {
                    if route == Route::Usb {
                        // The pipe is now the live transport.
                        self.pipe_ready = false;
                    }
                    self.state = State::Live { route };
                    actions.push(Action::Report(Status::Live(route)));
                    // A pipe that lost the connect race is still a trigger.
                    self.maybe_switch_to_usb(now_ms, &mut actions);
                }
                // A first connect that fails is the consumer's to ask for again.
                // A resume keeps trying with a growing pause, and the status
                // stays "resuming" until it gives up.
                Event::ConnectFailed if self.state == State::Resuming => {
                    self.resume_failures += 1;
                    if self.resume_failures >= self.policy.resume_attempts {
                        self.state = State::Idle;
                        actions.push(Action::Report(Status::Disconnected));
                    } else {
                        let doublings = (self.resume_failures - 1).min(32);
                        let pause = self
                            .policy
                            .resume_retry_ms
                            .saturating_mul(1u64 << doublings)
                            .min(self.policy.resume_retry_max_ms);
                        actions.push(Action::SetTimer(Timer::ResumeRetry, pause));
                    }
                }
                Event::TimerFired(Timer::ResumeRetry) if self.state == State::Resuming => {
                    actions.push(Action::Connect {
                        usb_first: self.pipe_ready && self.usb_allowed(now_ms),
                    });
                }
                Event::ConnectFailed => {
                    self.state = State::Idle;
                    actions.push(Action::Report(Status::Disconnected));
                }
                _ => {}
            },
            State::Live { route } => match event {
                Event::UsbPipeReady => self.maybe_switch_to_usb(now_ms, &mut actions),
                Event::UsbUnhealthy if route == Route::Usb => {
                    self.usb_lost(now_ms);
                    self.usb_barred_until = self
                        .usb_barred_until
                        .max(now_ms.saturating_add(self.policy.trigger_hold_ms));
                    self.state = State::Switching {
                        from: Route::Usb,
                        to: Route::Network,
                        stage: Stage::Dialing,
                        live_closed: false,
                    };
                    actions.push(Action::DialCandidate {
                        route: Route::Network,
                    });
                }
                Event::LiveClosed => {
                    if route == Route::Usb {
                        self.usb_lost(now_ms);
                    }
                    self.resume(&mut actions);
                }
                _ => {}
            },
            State::Switching {
                from,
                to,
                stage,
                live_closed,
            } => {
                let stay = |stage| State::Switching {
                    from,
                    to,
                    stage,
                    live_closed,
                };
                match (stage, event) {
                    // The live transport died before the candidate proved
                    // itself: cancel the candidate and resume.
                    (Stage::Holding | Stage::Dialing | Stage::Probing, Event::LiveClosed) => {
                        actions.push(Action::CancelTimer(Timer::StabilityHold));
                        if stage != Stage::Holding {
                            actions.push(Action::CloseCandidate);
                        }
                        if from == Route::Usb {
                            self.usb_lost(now_ms);
                        }
                        self.resume(&mut actions);
                    }
                    // It died after the probe passed: the candidate, with its
                    // hello already sent, is the first route of the resume.
                    (Stage::AwaitingAnswer, Event::LiveClosed) if !live_closed => {
                        if from == Route::Usb {
                            self.usb_lost(now_ms);
                        }
                        self.state = State::Switching {
                            from,
                            to,
                            stage,
                            live_closed: true,
                        };
                        actions.push(Action::Report(Status::Resuming));
                    }
                    (Stage::Holding, Event::UsbFlapped) => {
                        actions.push(Action::SetTimer(
                            Timer::StabilityHold,
                            self.policy.stability_hold_ms,
                        ));
                    }
                    (Stage::Holding, Event::UsbPipeGone) => {
                        actions.push(Action::CancelTimer(Timer::StabilityHold));
                        self.state = State::Live { route: from };
                    }
                    (Stage::Holding, Event::TimerFired(Timer::StabilityHold)) => {
                        self.state = stay(Stage::Dialing);
                        actions.push(Action::DialCandidate { route: to });
                    }
                    (Stage::Dialing, Event::CandidateDialed) => {
                        if to == Route::Usb {
                            // The pipe is in use from here on.
                            self.pipe_ready = false;
                            self.state = stay(Stage::Probing);
                            actions.push(Action::ProbeCandidate {
                                count: self.policy.probe_count,
                                spacing_ms: self.policy.probe_spacing_ms,
                                deadline_ms: self.policy.probe_deadline_ms,
                            });
                        } else {
                            self.state = stay(Stage::AwaitingAnswer);
                            actions.push(Action::SendHelloOnCandidate);
                            actions.push(Action::SetTimer(
                                Timer::HelloAnswer,
                                self.policy.hello_answer_ms,
                            ));
                        }
                    }
                    // One dial, no retry: nothing more until the next trigger.
                    (Stage::Dialing, Event::CandidateDialFailed) => {
                        if to == Route::Usb {
                            self.pipe_ready = false;
                        }
                        self.candidate_failed(now_ms, from, to, live_closed, &mut actions);
                    }
                    (
                        Stage::Probing,
                        Event::Probed {
                            answered,
                            rtt_p50_us,
                            elapsed_ms,
                            live_rtt_p50_us,
                        },
                    ) => {
                        let fast_enough = match (rtt_p50_us, live_rtt_p50_us) {
                            (Some(candidate), Some(live)) => {
                                candidate <= live.saturating_add(self.policy.probe_rtt_margin_us)
                            }
                            // Nothing to compare with: the pongs decide.
                            (Some(_), None) => true,
                            (None, _) => false,
                        };
                        if answered >= self.policy.probe_min_answered
                            && fast_enough
                            && elapsed_ms <= self.policy.probe_deadline_ms
                        {
                            self.state = stay(Stage::AwaitingAnswer);
                            actions.push(Action::SendHelloOnCandidate);
                            actions.push(Action::SetTimer(
                                Timer::HelloAnswer,
                                self.policy.hello_answer_ms,
                            ));
                        } else {
                            actions.push(Action::CloseCandidate);
                            self.candidate_failed(now_ms, from, to, live_closed, &mut actions);
                        }
                    }
                    (Stage::AwaitingAnswer, Event::HelloAccepted) => {
                        actions.push(Action::CancelTimer(Timer::HelloAnswer));
                        actions.push(Action::AdoptCandidate);
                        actions.push(Action::Report(if live_closed {
                            Status::Live(to)
                        } else {
                            Status::TransportChanged(to)
                        }));
                        self.state = State::Live { route: to };
                        // Back on the network with a cable waiting (a health
                        // move): the bar decides whether to try it again.
                        self.maybe_switch_to_usb(now_ms, &mut actions);
                    }
                    (Stage::AwaitingAnswer, Event::HelloRejected) => {
                        actions.push(Action::CancelTimer(Timer::HelloAnswer));
                        actions.push(Action::CloseCandidate);
                        self.candidate_failed(now_ms, from, to, live_closed, &mut actions);
                    }
                    (
                        Stage::Dialing | Stage::Probing | Stage::AwaitingAnswer,
                        Event::CandidateClosed,
                    ) => {
                        actions.push(Action::CancelTimer(Timer::HelloAnswer));
                        self.candidate_failed(now_ms, from, to, live_closed, &mut actions);
                    }
                    // No answer in time. With the live transport still up,
                    // keep both channels until one answers or closes. With it
                    // gone the candidate is the only channel: give it up and
                    // resume, or the client would wait on it for ever.
                    (Stage::AwaitingAnswer, Event::TimerFired(Timer::HelloAnswer))
                        if live_closed =>
                    {
                        actions.push(Action::CloseCandidate);
                        self.candidate_failed(now_ms, from, to, live_closed, &mut actions);
                    }
                    _ => {}
                }
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action as A;
    use Event as E;

    fn owner() -> TransportOwner {
        TransportOwner::new(OwnerPolicy::default())
    }

    /// An owner live on the network at time 0.
    fn live_on_network() -> TransportOwner {
        let mut owner = owner();
        owner.handle(0, E::ConnectRequested);
        owner.handle(
            0,
            E::Connected {
                route: Route::Network,
            },
        );
        owner
    }

    fn good_probe() -> Event {
        E::Probed {
            answered: 5,
            rtt_p50_us: Some(900),
            elapsed_ms: 120,
            live_rtt_p50_us: Some(3_000),
        }
    }

    /// Drives a network-live owner through hold, dial and probe to the hello.
    fn to_awaiting_answer(owner: &mut TransportOwner, now: u64) {
        owner.handle(now, E::UsbPipeReady);
        owner.handle(now + 2_000, E::TimerFired(Timer::StabilityHold));
        owner.handle(now + 2_010, E::CandidateDialed);
        owner.handle(now + 2_200, good_probe());
    }

    #[test]
    fn connect_then_a_cable_is_adopted_make_before_break() {
        let mut owner = owner();
        assert_eq!(
            owner.handle(0, E::ConnectRequested),
            [
                A::Report(Status::Connecting),
                A::Connect { usb_first: false }
            ]
        );
        assert_eq!(
            owner.handle(
                50,
                E::Connected {
                    route: Route::Network
                }
            ),
            [A::Report(Status::Live(Route::Network))]
        );
        // The plug starts the hold; nothing is dialed yet.
        assert_eq!(
            owner.handle(1_000, E::UsbPipeReady),
            [A::SetTimer(Timer::StabilityHold, 2_000)]
        );
        // A flap restarts it.
        assert_eq!(
            owner.handle(2_500, E::UsbFlapped),
            [A::SetTimer(Timer::StabilityHold, 2_000)]
        );
        assert_eq!(
            owner.handle(4_500, E::TimerFired(Timer::StabilityHold)),
            [A::DialCandidate { route: Route::Usb }]
        );
        assert_eq!(
            owner.handle(4_510, E::CandidateDialed),
            [A::ProbeCandidate {
                count: 5,
                spacing_ms: 20,
                deadline_ms: 500
            }]
        );
        assert_eq!(
            owner.handle(4_700, good_probe()),
            [
                A::SendHelloOnCandidate,
                A::SetTimer(Timer::HelloAnswer, 5_000)
            ]
        );
        // Only the host's answer swaps the channels: a change, not a break.
        assert_eq!(
            owner.handle(4_720, E::HelloAccepted),
            [
                A::CancelTimer(Timer::HelloAnswer),
                A::AdoptCandidate,
                A::Report(Status::TransportChanged(Route::Usb))
            ]
        );
        assert_eq!(owner.state(), State::Live { route: Route::Usb });
    }

    #[test]
    fn a_candidate_that_fails_leaves_the_live_transport_untouched() {
        let live = State::Live {
            route: Route::Network,
        };
        // The pipe goes during the hold.
        let mut owner = live_on_network();
        owner.handle(0, E::UsbPipeReady);
        assert_eq!(
            owner.handle(500, E::UsbPipeGone),
            [A::CancelTimer(Timer::StabilityHold)]
        );
        assert_eq!(owner.state(), live);
        // The dial fails: one attempt, nothing more until the next trigger.
        let mut owner = live_on_network();
        owner.handle(0, E::UsbPipeReady);
        owner.handle(2_000, E::TimerFired(Timer::StabilityHold));
        assert_eq!(owner.handle(2_100, E::CandidateDialFailed), []);
        assert_eq!(owner.state(), live);
        assert_eq!(owner.handle(9_000, E::TimerFired(Timer::StabilityHold)), []);
        // A probe that is too lossy, too slow, or late closes the candidate.
        for probe in [
            E::Probed {
                answered: 3,
                rtt_p50_us: Some(900),
                elapsed_ms: 120,
                live_rtt_p50_us: Some(3_000),
            },
            E::Probed {
                answered: 5,
                rtt_p50_us: Some(5_001),
                elapsed_ms: 120,
                live_rtt_p50_us: Some(3_000),
            },
            E::Probed {
                answered: 5,
                rtt_p50_us: Some(900),
                elapsed_ms: 501,
                live_rtt_p50_us: Some(3_000),
            },
            E::Probed {
                answered: 0,
                rtt_p50_us: None,
                elapsed_ms: 500,
                live_rtt_p50_us: None,
            },
        ] {
            let mut owner = live_on_network();
            owner.handle(0, E::UsbPipeReady);
            owner.handle(2_000, E::TimerFired(Timer::StabilityHold));
            owner.handle(2_010, E::CandidateDialed);
            assert_eq!(owner.handle(2_300, probe), [A::CloseCandidate]);
            assert_eq!(owner.state(), live);
        }
        // The host rejects the hello (a stale generation): keep the old one.
        let mut owner = live_on_network();
        to_awaiting_answer(&mut owner, 0);
        assert_eq!(
            owner.handle(2_300, E::HelloRejected),
            [A::CancelTimer(Timer::HelloAnswer), A::CloseCandidate]
        );
        assert_eq!(owner.state(), live);
    }

    #[test]
    fn no_answer_keeps_both_channels_until_one_answers_or_closes() {
        let mut owner = live_on_network();
        to_awaiting_answer(&mut owner, 0);
        // Five seconds with no answer: nothing is closed.
        assert_eq!(owner.handle(7_200, E::TimerFired(Timer::HelloAnswer)), []);
        assert!(matches!(owner.state(), State::Switching { .. }));
        // A late acceptance still swaps.
        let mut accepted = TransportOwner::new(OwnerPolicy::default());
        accepted.handle(0, E::ConnectRequested);
        accepted.handle(
            0,
            E::Connected {
                route: Route::Network,
            },
        );
        to_awaiting_answer(&mut accepted, 0);
        accepted.handle(7_200, E::TimerFired(Timer::HelloAnswer));
        assert!(
            accepted
                .handle(9_000, E::HelloAccepted)
                .contains(&A::AdoptCandidate)
        );
        // The candidate closing leaves the live one; the live one closing too
        // means resuming.
        owner.handle(8_000, E::LiveClosed);
        assert_eq!(
            owner.handle(8_100, E::CandidateClosed),
            [
                A::CancelTimer(Timer::HelloAnswer),
                A::Connect { usb_first: false }
            ]
        );
        assert_eq!(owner.state(), State::Resuming);
    }

    #[test]
    fn a_live_transport_that_dies_while_switching_uses_the_candidate_only_if_it_passed() {
        // Before the probe passed: cancel the candidate and resume.
        let mut owner = live_on_network();
        owner.handle(0, E::UsbPipeReady);
        owner.handle(2_000, E::TimerFired(Timer::StabilityHold));
        owner.handle(2_010, E::CandidateDialed);
        assert_eq!(
            owner.handle(2_100, E::LiveClosed),
            [
                A::CancelTimer(Timer::StabilityHold),
                A::CloseCandidate,
                A::Report(Status::Resuming),
                A::Connect { usb_first: false }
            ]
        );
        assert_eq!(owner.state(), State::Resuming);
        // After it passed: the candidate, hello already sent, carries the
        // resume, and its acceptance makes it live.
        let mut owner = live_on_network();
        to_awaiting_answer(&mut owner, 0);
        assert_eq!(
            owner.handle(2_250, E::LiveClosed),
            [A::Report(Status::Resuming)]
        );
        assert_eq!(
            owner.handle(2_300, E::HelloAccepted),
            [
                A::CancelTimer(Timer::HelloAnswer),
                A::AdoptCandidate,
                A::Report(Status::Live(Route::Usb))
            ]
        );
    }

    #[test]
    fn nothing_switches_while_connecting_or_resuming_and_a_waiting_pipe_is_used_after() {
        let mut owner = owner();
        owner.handle(0, E::ConnectRequested);
        // The cable appears mid-connect: remembered, not acted on.
        assert_eq!(owner.handle(10, E::UsbPipeReady), []);
        assert_eq!(owner.state(), State::Connecting);
        // The network won the race: the pipe is still a trigger.
        assert_eq!(
            owner.handle(
                40,
                E::Connected {
                    route: Route::Network
                }
            ),
            [
                A::Report(Status::Live(Route::Network)),
                A::SetTimer(Timer::StabilityHold, 2_000)
            ]
        );
        // With a pipe ready before connecting, it is the first route.
        let mut owner = TransportOwner::new(OwnerPolicy::default());
        owner.handle(0, E::UsbPipeReady);
        assert_eq!(
            owner.handle(5, E::ConnectRequested)[1],
            A::Connect { usb_first: true }
        );
        // Connected over the cable: no switch follows.
        assert_eq!(
            owner.handle(40, E::Connected { route: Route::Usb }),
            [A::Report(Status::Live(Route::Usb))]
        );
    }

    #[test]
    fn an_unplug_resumes_on_the_network_and_an_unhealthy_cable_is_left_make_before_break() {
        let mut owner = owner();
        owner.handle(0, E::UsbPipeReady);
        owner.handle(0, E::ConnectRequested);
        owner.handle(40, E::Connected { route: Route::Usb });
        // Unplugged: resuming, not disconnected.
        assert_eq!(
            owner.handle(5_000, E::LiveClosed),
            [A::Report(Status::Resuming), A::Connect { usb_first: false }]
        );
        owner.handle(
            5_100,
            E::Connected {
                route: Route::Network,
            },
        );
        assert_eq!(
            owner.state(),
            State::Live {
                route: Route::Network
            }
        );

        // Health: move to the network without a break, then hold triggers.
        let mut owner = TransportOwner::new(OwnerPolicy::default());
        owner.handle(0, E::UsbPipeReady);
        owner.handle(0, E::ConnectRequested);
        owner.handle(40, E::Connected { route: Route::Usb });
        assert_eq!(
            owner.handle(10_000, E::UsbUnhealthy),
            [A::DialCandidate {
                route: Route::Network
            }]
        );
        // A network candidate needs no probe: the hello goes at once.
        assert_eq!(
            owner.handle(10_050, E::CandidateDialed),
            [
                A::SendHelloOnCandidate,
                A::SetTimer(Timer::HelloAnswer, 5_000)
            ]
        );
        assert_eq!(
            owner.handle(10_080, E::HelloAccepted),
            [
                A::CancelTimer(Timer::HelloAnswer),
                A::AdoptCandidate,
                A::Report(Status::TransportChanged(Route::Network))
            ]
        );
        // A pipe that arrives inside the 10 s hold stays idle.
        assert_eq!(owner.handle(12_000, E::UsbPipeReady), []);
        // After it, the next trigger starts a switch again.
        assert_eq!(
            owner.handle(20_100, E::UsbPipeReady),
            [A::SetTimer(Timer::StabilityHold, 2_000)]
        );
    }

    #[test]
    fn three_cable_losses_in_a_minute_keep_the_session_on_the_network_longer_each_time() {
        let mut owner = owner();
        let mut now = 0;
        owner.handle(now, E::ConnectRequested);
        owner.handle(
            now,
            E::Connected {
                route: Route::Network,
            },
        );
        // Adopt the cable and lose it, three times within a minute.
        for round in 0..3 {
            now += 1_000;
            assert_eq!(
                owner.handle(now, E::UsbPipeReady),
                [A::SetTimer(Timer::StabilityHold, 2_000)],
                "round {round}"
            );
            owner.handle(now + 2_000, E::TimerFired(Timer::StabilityHold));
            owner.handle(now + 2_010, E::CandidateDialed);
            owner.handle(now + 2_200, good_probe());
            owner.handle(now + 2_220, E::HelloAccepted);
            now += 5_000;
            owner.handle(now, E::LiveClosed);
            owner.handle(
                now + 100,
                E::Connected {
                    route: Route::Network,
                },
            );
        }
        // Barred for five minutes: a plug does nothing.
        assert_eq!(owner.handle(now + 60_000, E::UsbPipeReady), []);
        assert_eq!(owner.handle(now + 299_000, E::UsbPipeReady), []);
        // Then it is tried again.
        assert_eq!(
            owner.handle(now + 301_000, E::UsbPipeReady),
            [A::SetTimer(Timer::StabilityHold, 2_000)]
        );
    }

    #[test]
    fn disconnect_closes_whatever_is_open_and_stray_events_do_nothing() {
        let mut owner = live_on_network();
        to_awaiting_answer(&mut owner, 0);
        assert_eq!(
            owner.handle(2_300, E::Disconnect),
            [
                A::CancelTimer(Timer::StabilityHold),
                A::CancelTimer(Timer::HelloAnswer),
                A::CloseCandidate,
                A::CloseLive,
                A::Report(Status::Disconnected)
            ]
        );
        assert_eq!(owner.state(), State::Idle);
        // Late events from the closed channels and timers are ignored.
        for stray in [
            E::HelloAccepted,
            E::CandidateClosed,
            E::LiveClosed,
            E::TimerFired(Timer::HelloAnswer),
            E::CandidateDialed,
            good_probe(),
            E::Disconnect,
        ] {
            assert_eq!(owner.handle(3_000, stray), []);
            assert_eq!(owner.state(), State::Idle);
        }
        // A failed connect ends disconnected; the consumer asks again.
        owner.handle(4_000, E::ConnectRequested);
        assert_eq!(
            owner.handle(9_000, E::ConnectFailed),
            [A::Report(Status::Disconnected)]
        );
    }

    #[test]
    fn with_the_live_transport_gone_an_unanswered_candidate_is_given_up() {
        let mut owner = live_on_network();
        to_awaiting_answer(&mut owner, 0);
        owner.handle(2_250, E::LiveClosed);
        // The candidate is the only channel, and the host never answers.
        assert_eq!(
            owner.handle(7_200, E::TimerFired(Timer::HelloAnswer)),
            [A::CloseCandidate, A::Connect { usb_first: false }]
        );
        assert_eq!(owner.state(), State::Resuming);
    }

    #[test]
    fn a_resume_is_retried_with_a_growing_pause_before_it_is_given_up() {
        let mut owner = live_on_network();
        assert_eq!(
            owner.handle(1_000, E::LiveClosed),
            [A::Report(Status::Resuming), A::Connect { usb_first: false }]
        );
        // Seven failures: a pause that doubles, and no status change.
        let mut now = 1_000;
        for pause in [150, 300, 600, 1_200, 2_400, 4_800, 9_600] {
            now += 100;
            assert_eq!(
                owner.handle(now, E::ConnectFailed),
                [A::SetTimer(Timer::ResumeRetry, pause)]
            );
            assert_eq!(owner.state(), State::Resuming);
            now += pause;
            assert_eq!(
                owner.handle(now, E::TimerFired(Timer::ResumeRetry)),
                [A::Connect { usb_first: false }]
            );
        }
        // The eighth ends it.
        assert_eq!(
            owner.handle(now + 100, E::ConnectFailed),
            [A::Report(Status::Disconnected)]
        );
        assert_eq!(owner.state(), State::Idle);

        // A resume that succeeds starts the count again, and the pause is capped.
        let mut owner = TransportOwner::new(OwnerPolicy {
            resume_attempts: 20,
            ..OwnerPolicy::default()
        });
        owner.handle(0, E::ConnectRequested);
        owner.handle(
            0,
            E::Connected {
                route: Route::Network,
            },
        );
        owner.handle(10, E::LiveClosed);
        owner.handle(20, E::ConnectFailed);
        owner.handle(
            30,
            E::Connected {
                route: Route::Network,
            },
        );
        owner.handle(40, E::LiveClosed);
        assert_eq!(
            owner.handle(50, E::ConnectFailed),
            [A::SetTimer(Timer::ResumeRetry, 150)]
        );
        let mut last = 0;
        for _ in 0..12 {
            if let [A::SetTimer(Timer::ResumeRetry, pause)] = owner.handle(60, E::ConnectFailed)[..]
            {
                last = pause;
            }
        }
        assert_eq!(last, 30_000);
        // A disconnect while waiting to retry cancels the timer.
        assert_eq!(
            owner.handle(70, E::Disconnect),
            [
                A::CancelTimer(Timer::ResumeRetry),
                A::Report(Status::Disconnected)
            ]
        );
    }

    #[test]
    fn a_cable_that_is_never_good_enough_is_not_tried_every_few_seconds() {
        let slow = E::Probed {
            answered: 5,
            rtt_p50_us: Some(9_000),
            elapsed_ms: 120,
            live_rtt_p50_us: Some(3_000),
        };
        let mut owner = live_on_network();
        let mut now = 0;
        // Each failed candidate holds the cable for ten seconds.
        for round in 0..3 {
            assert_eq!(
                owner.handle(now, E::UsbPipeReady),
                [A::SetTimer(Timer::StabilityHold, 2_000)],
                "round {round}"
            );
            owner.handle(now + 2_000, E::TimerFired(Timer::StabilityHold));
            owner.handle(now + 2_010, E::CandidateDialed);
            assert_eq!(owner.handle(now + 2_200, slow), [A::CloseCandidate]);
            // The keeper's next pipe, two seconds later: ignored.
            assert_eq!(owner.handle(now + 4_200, E::UsbPipeReady), []);
            now += 2_200 + 10_000;
        }
        // Three in a minute: left alone for five minutes.
        assert_eq!(owner.handle(now, E::UsbPipeReady), []);
        assert_eq!(owner.handle(now + 280_000, E::UsbPipeReady), []);
        assert_eq!(
            owner.handle(now + 301_000, E::UsbPipeReady),
            [A::SetTimer(Timer::StabilityHold, 2_000)]
        );
        // The session stayed on the network throughout.
        assert!(matches!(
            owner.state(),
            State::Switching {
                from: Route::Network,
                ..
            }
        ));
    }
}
