//! `HandlerState` to and from the handover state.
//!
//! Both directions destructure `HandlerState` whole, so a field added to it
//! does not compile until this file says what a handover does with it.

use std::collections::{HashMap, HashSet};

use rmux_core::handover::{restore_session_name, HandoverError, PaneTargetV1};
use rmux_core::{
    BufferStore, EnvironmentStore, HookStore, KeyBindingStore, OptionStore, PaneId, SessionStore,
};
use rmux_proto::{SessionName, RMUX_WIRE_VERSION};

use super::{
    AttachedSubmittedLine, HandlerState, MarkedPane, MessageEntry, PaneExitMetadata,
    PaneLifecycleState, WindowLinkGroup, WindowLinkOccurrenceId, WindowLinkSlot,
};
use crate::handover::{
    DeadPaneV1, ExitDetailsV1, HandoverStateV1, HeaderV1, MarkedPaneV1, MessageV1, PaneProcessV1,
    PaneStateV1, WindowLinkGroupV1, WindowLinksV1,
};
use crate::pane_transcript::PaneTranscript;

/// A pane process the restored state names, for the resume runtime to adopt
/// with the master fd it inherited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandedOverPaneProcess {
    pub(crate) session_name: SessionName,
    pub(crate) pane_id: PaneId,
    pub(crate) master_fd: i32,
    pub(crate) process: PaneProcessV1,
}

fn sorted_by_name<T>(mut entries: Vec<(String, T)>) -> Vec<(String, T)> {
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

impl HandlerState {
    /// Captures the whole state. Phase 3 calls this with every pane reader
    /// parked, so the transcripts hold everything read and nothing is half
    /// parsed beyond what the parser state records.
    pub(crate) fn to_handover(&self) -> Result<HandoverStateV1, HandoverError> {
        let Self {
            sessions,
            options,
            environment,
            hooks,
            buffers,
            key_bindings,
            // Not carried: leases held by in-flight commands, which the
            // quiesce has let finish.
            retained_lifecycle_targets: _,
            message_log,
            // Not carried: layout and resize notifications owed to clients.
            // Clients reattach after a handover and get a full redraw.
            applied_window_resizes: _,
            // Not carried: the order in which in-flight lifecycle events are
            // published. Quiesced, nothing is in flight.
            lifecycle_commit_order: _,
            // Not carried: running status jobs. A running job refuses the
            // handover (design, "Scope"), and the cache refills on the next
            // status redraw.
            status_jobs: _,
            startup_config_files,
            next_message_number,
            terminals,
            #[cfg(windows)]
                starting_panes: _,
            transcripts,
            // Not carried: the output rings attached clients read from.
            // Clients redraw from the screen.
            pane_outputs: _,
            // Not carried: reader tasks. The resume runtime starts new ones.
            #[cfg(unix)]
                pane_output_readers: _,
            pane_output_generations,
            pane_lifecycle,
            attached_submitted_rows,
            // Not carried: pixel sizes attached terminals reported. They
            // report them again when they reattach.
            attached_terminal_pixels: _,
            input_disabled_panes,
            #[cfg(test)]
                pane_input_captures: _,
            #[cfg(test)]
                window_runtime_resize_count: _,
            dead_panes,
            marked_pane,
            // Not carried: pipe-pane children. An active pipe refuses the
            // handover (design, "Scope").
            pipes: _,
            auto_named_windows,
            window_link_groups,
            window_link_slots,
            window_link_occurrences,
            next_window_link_group_id,
            next_window_link_occurrence_id,
            #[cfg(test)]
                fail_link_window_after_attach: _,
            // Not carried: the tokio runtime the readers run on.
            #[cfg(unix)]
                pane_reader_runtime: _,
        } = self;

        let (processes, session_environments) = terminals.to_handover();
        let mut header = HeaderV1 {
            writer_version: env!("CARGO_PKG_VERSION").to_owned(),
            wire_version: RMUX_WIRE_VERSION,
            pane_fds: Vec::with_capacity(processes.len()),
        };
        let mut processes: HashMap<(String, u32), PaneProcessV1> = processes
            .into_iter()
            .map(|(session, pane_id, fd, process)| {
                header.pane_fds.push((pane_id, fd));
                ((session, pane_id), process)
            })
            .collect();

        let mut panes: Vec<PaneStateV1> = transcripts
            .iter()
            .flat_map(|(session, panes)| {
                panes
                    .iter()
                    .map(move |(pane_id, transcript)| (session, *pane_id, transcript))
            })
            .map(|(session, pane_id, transcript)| {
                let key = (session.as_str().to_owned(), pane_id.as_u32());
                PaneStateV1 {
                    session: session.as_str().to_owned(),
                    pane_id: pane_id.as_u32(),
                    transcript: transcript
                        .lock()
                        .expect("pane transcript mutex must not be poisoned")
                        .to_handover(),
                    output_generation: pane_output_generations
                        .get(session)
                        .and_then(|panes| panes.get(&pane_id))
                        .copied(),
                    attached_submitted_line: attached_submitted_rows
                        .get(session)
                        .and_then(|panes| panes.get(&pane_id))
                        .map(AttachedSubmittedLine::to_handover),
                    process: processes.remove(&key),
                }
            })
            .collect();
        panes.sort_by(|left, right| {
            (&left.session, left.pane_id).cmp(&(&right.session, right.pane_id))
        });
        // A terminal without a transcript cannot happen today; if it ever
        // does, the process must still be handed over, or the exec would
        // orphan its fd.
        let mut orphans: Vec<_> = processes.into_iter().collect();
        orphans.sort_by(|left, right| left.0.cmp(&right.0));
        debug_assert!(orphans.is_empty(), "pane terminal without a transcript");
        for ((session, pane_id), process) in orphans {
            panes.push(PaneStateV1 {
                session,
                pane_id,
                transcript: crate::handover::TranscriptV1 {
                    terminal: rmux_core::TerminalScreen::new(
                        rmux_proto::TerminalSize { cols: 1, rows: 1 },
                        0,
                    )
                    .to_handover(),
                    output_sequence: 0,
                },
                output_generation: None,
                attached_submitted_line: None,
                process: Some(process),
            });
        }

        let mut pane_lifecycles = pane_lifecycle
            .values()
            .map(PaneLifecycleState::to_handover)
            .collect::<Result<Vec<_>, _>>()?;
        pane_lifecycles.sort_by_key(|lifecycle| lifecycle.pane_id);

        let mut dead: Vec<_> = dead_panes
            .iter()
            .flat_map(|(session, panes)| {
                panes.iter().map(move |(pane_id, metadata)| {
                    let PaneExitMetadata {
                        status,
                        signal,
                        time,
                    } = *metadata;
                    DeadPaneV1 {
                        session: session.as_str().to_owned(),
                        pane_id: pane_id.as_u32(),
                        exit: ExitDetailsV1 {
                            status,
                            signal,
                            time,
                        },
                    }
                })
            })
            .collect();
        dead.sort_by(|left, right| {
            (&left.session, left.pane_id).cmp(&(&right.session, right.pane_id))
        });

        let mut input_disabled: Vec<_> =
            input_disabled_panes.iter().map(|id| id.as_u32()).collect();
        input_disabled.sort_unstable();
        let mut auto_named: Vec<_> = auto_named_windows
            .iter()
            .map(|(session, index)| (session.as_str().to_owned(), *index))
            .collect();
        auto_named.sort();

        let mut groups: Vec<_> = window_link_groups
            .iter()
            .map(|(id, group)| {
                let WindowLinkGroup {
                    runtime_session_name,
                    slots,
                } = group;
                WindowLinkGroupV1 {
                    id: *id,
                    runtime_session: runtime_session_name.as_str().to_owned(),
                    slots: slots.iter().map(slot_to_handover).collect(),
                }
            })
            .collect();
        groups.sort_by_key(|group| group.id);
        let mut slots: Vec<_> = window_link_slots
            .iter()
            .map(|(slot, group)| {
                let (session, index) = slot_to_handover(slot);
                (session, index, *group)
            })
            .collect();
        slots.sort();
        let mut occurrences: Vec<_> = window_link_occurrences
            .iter()
            .map(|(slot, occurrence)| {
                let (session, index) = slot_to_handover(slot);
                (session, index, occurrence.as_u64())
            })
            .collect();
        occurrences.sort();

        Ok(HandoverStateV1 {
            header,
            sessions: sessions.to_handover(),
            options: options.to_handover(),
            environment: environment.to_handover(),
            hooks: hooks.to_handover(),
            buffers: buffers.to_handover(),
            key_bindings: key_bindings.to_handover(),
            message_log: message_log
                .iter()
                .map(|entry| {
                    let MessageEntry {
                        msg_time,
                        msg_num,
                        msg,
                    } = entry;
                    MessageV1 {
                        time: *msg_time,
                        number: *msg_num,
                        message: msg.clone(),
                    }
                })
                .collect(),
            next_message_number: *next_message_number,
            startup_config_files: startup_config_files.clone(),
            marked_pane: marked_pane.as_ref().map(|marked| {
                let MarkedPane { target, pane_id } = marked;
                MarkedPaneV1 {
                    target: PaneTargetV1::from_target(target),
                    pane_id: pane_id.as_u32(),
                }
            }),
            auto_named_windows: auto_named,
            window_links: WindowLinksV1 {
                groups,
                slots,
                occurrences,
                next_group_id: *next_window_link_group_id,
                next_occurrence_id: *next_window_link_occurrence_id,
            },
            input_disabled_panes: input_disabled,
            pane_lifecycles,
            dead_panes: dead,
            session_environments: sorted_by_name(session_environments),
            panes,
        })
    }

    /// Restores a captured state into this fresh state, which keeps its own
    /// runtime parts (the reader runtime, the job runtime).
    ///
    /// Everything but the pane processes comes back here. The processes are
    /// returned for the resume runtime to adopt with the fds it inherited:
    /// building a `PaneTerminal` takes a live fd and a child adopted by pid,
    /// which only the exec'd image has (design §3).
    pub(crate) fn restore_handover(
        &mut self,
        state: HandoverStateV1,
    ) -> Result<Vec<HandedOverPaneProcess>, HandoverError> {
        let Self {
            sessions,
            options,
            environment,
            hooks,
            buffers,
            key_bindings,
            retained_lifecycle_targets: _,
            message_log,
            applied_window_resizes: _,
            lifecycle_commit_order: _,
            status_jobs: _,
            startup_config_files,
            next_message_number,
            terminals,
            #[cfg(windows)]
                starting_panes: _,
            transcripts,
            pane_outputs: _,
            #[cfg(unix)]
                pane_output_readers: _,
            pane_output_generations,
            pane_lifecycle,
            attached_submitted_rows,
            attached_terminal_pixels: _,
            input_disabled_panes,
            #[cfg(test)]
                pane_input_captures: _,
            #[cfg(test)]
                window_runtime_resize_count: _,
            dead_panes,
            marked_pane,
            pipes: _,
            auto_named_windows,
            window_link_groups,
            window_link_slots,
            window_link_occurrences,
            next_window_link_group_id,
            next_window_link_occurrence_id,
            #[cfg(test)]
                fail_link_window_after_attach: _,
            #[cfg(unix)]
                pane_reader_runtime: _,
        } = self;
        // A resume starts from a fresh daemon; merging into a live one would
        // collide ids and is never what is meant.
        if !sessions.is_empty() || !transcripts.is_empty() || !terminals.is_empty_for_handover() {
            return Err(HandoverError::new(
                "a handover restores only into an empty daemon",
            ));
        }

        let HandoverStateV1 {
            header,
            sessions: session_store,
            options: option_store,
            environment: environment_store,
            hooks: hook_store,
            buffers: buffer_store,
            key_bindings: key_binding_store,
            message_log: messages,
            next_message_number: next_message,
            startup_config_files: config_files,
            marked_pane: marked,
            auto_named_windows: auto_named,
            window_links,
            input_disabled_panes: input_disabled,
            pane_lifecycles,
            dead_panes: dead,
            session_environments,
            panes,
        } = state;
        let HeaderV1 {
            // Diagnostics only; the schema already said how to read this.
            writer_version: _,
            // The wire version moves the socket, not the state (design §4).
            wire_version: _,
            pane_fds,
        } = header;
        let fds: HashMap<u32, i32> = pane_fds.into_iter().collect();

        // Every store first, into locals, so a refusal leaves `self` empty.
        let restored_sessions = SessionStore::from_handover(session_store)?;
        let restored_options = OptionStore::from_handover(option_store)?;
        let restored_environment = EnvironmentStore::from_handover(environment_store)?;
        let restored_hooks = HookStore::from_handover(hook_store)?;
        let restored_buffers = BufferStore::from_handover(buffer_store);
        let restored_key_bindings = KeyBindingStore::from_handover(key_binding_store)?;

        let mut restored_transcripts: HashMap<SessionName, HashMap<PaneId, _>> = HashMap::new();
        let mut generations: HashMap<SessionName, HashMap<PaneId, u64>> = HashMap::new();
        let mut submitted: HashMap<SessionName, HashMap<PaneId, AttachedSubmittedLine>> =
            HashMap::new();
        let mut processes = Vec::new();
        for pane in panes {
            let PaneStateV1 {
                session,
                pane_id,
                transcript,
                output_generation,
                attached_submitted_line,
                process,
            } = pane;
            let session_name = restore_session_name(&session)?;
            let id = PaneId::new(pane_id);
            let transcript = PaneTranscript::from_handover(transcript)?;
            if restored_transcripts
                .entry(session_name.clone())
                .or_default()
                .insert(id, std::sync::Arc::new(std::sync::Mutex::new(transcript)))
                .is_some()
            {
                return Err(HandoverError::new(format!(
                    "pane %{pane_id} appears twice in session {session}"
                )));
            }
            if let Some(generation) = output_generation {
                generations
                    .entry(session_name.clone())
                    .or_default()
                    .insert(id, generation);
            }
            if let Some(line) = attached_submitted_line {
                submitted
                    .entry(session_name.clone())
                    .or_default()
                    .insert(id, AttachedSubmittedLine::from_handover(line)?);
            }
            if let Some(process) = process {
                let master_fd = *fds.get(&pane_id).ok_or_else(|| {
                    HandoverError::new(format!("pane %{pane_id} has a process but no fd"))
                })?;
                processes.push(HandedOverPaneProcess {
                    session_name,
                    pane_id: id,
                    master_fd,
                    process,
                });
            }
        }

        let mut restored_dead: HashMap<SessionName, HashMap<PaneId, PaneExitMetadata>> =
            HashMap::new();
        for pane in dead {
            let DeadPaneV1 {
                session,
                pane_id,
                exit,
            } = pane;
            let ExitDetailsV1 {
                status,
                signal,
                time,
            } = exit;
            restored_dead
                .entry(restore_session_name(&session)?)
                .or_default()
                .insert(
                    PaneId::new(pane_id),
                    PaneExitMetadata {
                        status,
                        signal,
                        time,
                    },
                );
        }

        let WindowLinksV1 {
            groups,
            slots,
            occurrences,
            next_group_id,
            next_occurrence_id,
        } = window_links;
        let restored_groups = groups
            .into_iter()
            .map(|group| {
                let WindowLinkGroupV1 {
                    id,
                    runtime_session,
                    slots: group_slots,
                } = group;
                Ok((
                    id,
                    WindowLinkGroup {
                        runtime_session_name: restore_session_name(&runtime_session)?,
                        slots: group_slots
                            .into_iter()
                            .map(slot_from_handover)
                            .collect::<Result<_, HandoverError>>()?,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>, HandoverError>>()?;
        let restored_slots = slots
            .into_iter()
            .map(|(session, index, group)| Ok((slot_from_handover((session, index))?, group)))
            .collect::<Result<HashMap<_, _>, HandoverError>>()?;
        let restored_occurrences = occurrences
            .into_iter()
            .map(|(session, index, occurrence)| {
                Ok((
                    slot_from_handover((session, index))?,
                    WindowLinkOccurrenceId::from_handover(occurrence),
                ))
            })
            .collect::<Result<HashMap<_, _>, HandoverError>>()?;

        let restored_marked = marked
            .map(|marked| {
                let MarkedPaneV1 { target, pane_id } = marked;
                Ok::<_, HandoverError>(MarkedPane {
                    target: target.to_target()?,
                    pane_id: PaneId::new(pane_id),
                })
            })
            .transpose()?;
        let restored_auto_named = auto_named
            .into_iter()
            .map(|(session, index)| Ok((restore_session_name(&session)?, index)))
            .collect::<Result<HashSet<_>, HandoverError>>()?;

        let mut fresh_terminals = super::PaneTerminalStore::default();
        fresh_terminals.restore_session_environments(session_environments)?;

        // Nothing below can fail; the state is committed whole or not at all.
        *sessions = restored_sessions;
        *options = restored_options;
        *environment = restored_environment;
        *hooks = restored_hooks;
        *buffers = restored_buffers;
        *key_bindings = restored_key_bindings;
        *message_log = messages
            .into_iter()
            .map(|message| {
                let MessageV1 {
                    time,
                    number,
                    message,
                } = message;
                MessageEntry {
                    msg_time: time,
                    msg_num: number,
                    msg: message,
                }
            })
            .collect();
        *next_message_number = next_message;
        *startup_config_files = config_files;
        *terminals = fresh_terminals;
        *transcripts = restored_transcripts;
        *pane_output_generations = generations;
        *pane_lifecycle = pane_lifecycles
            .into_iter()
            .map(|lifecycle| {
                let lifecycle = PaneLifecycleState::from_handover(lifecycle);
                (lifecycle.pane_id, lifecycle)
            })
            .collect();
        *attached_submitted_rows = submitted;
        *input_disabled_panes = input_disabled.into_iter().map(PaneId::new).collect();
        *dead_panes = restored_dead;
        *marked_pane = restored_marked;
        *auto_named_windows = restored_auto_named;
        *window_link_groups = restored_groups;
        *window_link_slots = restored_slots;
        *window_link_occurrences = restored_occurrences;
        *next_window_link_group_id = next_group_id;
        *next_window_link_occurrence_id = next_occurrence_id;
        Ok(processes)
    }
}

/// What resuming did, for the log and the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct AdoptedPanes {
    pub(crate) processes: usize,
    pub(crate) readers: usize,
    pub(crate) ground_timers: usize,
    pub(crate) not_running: usize,
}

impl HandlerState {
    /// Checks, without taking anything, that every process the restore named
    /// can be adopted: its fd is open, its pid is real, and it has a
    /// transcript. A resume that fails here rolls back with every inherited
    /// fd still open and inheritable.
    pub(crate) fn check_handed_over_panes(
        &self,
        processes: &[HandedOverPaneProcess],
    ) -> Result<(), String> {
        for process in processes {
            let pane = process.pane_id.as_u32();
            if process.process.pid == 0 {
                return Err(format!("pane %{pane} has pid 0"));
            }
            rmux_os::handover::borrow_inherited_fd(process.master_fd)
                .map_err(|error| format!("pane %{pane}: {error}"))?;
            if !self
                .transcripts
                .get(&process.session_name)
                .is_some_and(|panes| panes.contains_key(&process.pane_id))
            {
                return Err(format!("pane %{pane} has a process but no transcript"));
            }
        }
        if self.pane_reader_runtime.is_none() {
            return Err("no runtime to run pane readers on".to_owned());
        }
        Ok(())
    }

    /// The second half of a resume: takes over every pane process with the
    /// fd it inherited, gives every pane its output channel back, starts a
    /// reader for every pane that was still reading, and restarts the ground
    /// timer of every parser left mid-sequence (§7).
    ///
    /// Call [`check_handed_over_panes`](Self::check_handed_over_panes) first;
    /// past that, nothing here can fail, because a failure halfway would
    /// leave fds adopted that a rollback needs inheritable.
    pub(crate) fn adopt_handed_over_panes(
        &mut self,
        processes: Vec<HandedOverPaneProcess>,
        pane_alert_callback: &crate::pane_io::PaneAlertCallback,
        pane_exit_callback: &crate::pane_io::PaneExitCallback,
    ) -> AdoptedPanes {
        let mut adopted = AdoptedPanes::default();
        let runtime = self
            .pane_reader_runtime
            .clone()
            .expect("checked by check_handed_over_panes");

        // Every pane gets its output channel, on its recorded generation, so
        // a client that subscribes sees the generation the pane had.
        let panes: Vec<(SessionName, PaneId)> = self
            .transcripts
            .iter()
            .flat_map(|(session, panes)| panes.keys().map(move |pane| (session.clone(), *pane)))
            .collect();
        for (session, pane_id) in &panes {
            let generation = self.pane_output_generation(session, *pane_id);
            let pane_output = crate::pane_io::pane_output_channel();
            pane_output.set_generation(generation);
            self.pane_outputs
                .entry(session.clone())
                .or_default()
                .insert(*pane_id, pane_output);
        }

        for process in processes {
            let HandedOverPaneProcess {
                session_name,
                pane_id,
                master_fd,
                process,
            } = process;
            let fd = rmux_os::handover::adopt_inherited_fd(master_fd)
                .expect("checked by check_handed_over_panes");
            let master = rmux_pty::PtyMaster::from_owned_fd(fd);
            // O_NONBLOCK is on the open file description, which the exec
            // kept; setting it again only matters if something cleared it.
            if let Err(error) = master.io().set_nonblocking() {
                tracing::warn!(pane = pane_id.as_u32(), "adopted pty master: {error}");
            }
            let child = rmux_pty::PtyChild::adopt(process.pid).expect("checked: pid is not 0");
            let mut terminal =
                crate::pane_terminal_process::PaneTerminal::from_handover(master, child, process);
            // A pane already kept dead by remain-on-exit had its exit
            // handled; its reader ended long ago and must not report it
            // again. Every other pane reads: one whose process died during
            // the exec reads EOF and is handled as any exit is.
            let dead = self
                .dead_panes
                .get(&session_name)
                .is_some_and(|panes| panes.contains_key(&pane_id));
            let reader = if dead {
                None
            } else {
                terminal.clone_master_for_output_reader().ok()
            };
            self.terminals
                .insert_handed_over_pane(session_name.clone(), pane_id, terminal);
            adopted.processes += 1;
            let Some(reader) = reader else {
                continue;
            };
            let transcript = self
                .transcripts
                .get(&session_name)
                .and_then(|panes| panes.get(&pane_id))
                .cloned()
                .expect("checked by check_handed_over_panes");
            let pane_output = self
                .pane_outputs
                .get(&session_name)
                .and_then(|panes| panes.get(&pane_id))
                .cloned()
                .expect("every pane got a channel above");
            let generation = self.pane_output_generation(&session_name, pane_id);
            let task = crate::pane_io::spawn_pane_output_reader(
                session_name.clone(),
                pane_id,
                reader,
                transcript,
                pane_output,
                Some(generation),
                Some(pane_alert_callback.clone()),
                Some(pane_exit_callback.clone()),
                runtime.clone(),
            );
            self.pane_output_readers
                .entry(session_name)
                .or_default()
                .insert(pane_id, task);
            adopted.readers += 1;
        }

        for (session, pane_id) in &panes {
            let (Some(transcript), Some(pane_output)) = (
                self.transcripts
                    .get(session)
                    .and_then(|panes| panes.get(pane_id)),
                self.pane_outputs
                    .get(session)
                    .and_then(|panes| panes.get(pane_id)),
            ) else {
                continue;
            };
            if crate::pane_io::resume_pane_ground_timer(session, *pane_id, transcript, pane_output)
            {
                adopted.ground_timers += 1;
            }
        }

        adopted.not_running = self.terminals.reap_handed_over_panes();
        adopted
    }
}

impl HandlerState {
    /// What in the state itself refuses a handover.
    pub(crate) fn handover_refusal(&self) -> Option<String> {
        let pipes = self.pipes.active_count();
        if pipes > 0 {
            return Some(format!(
                "{pipes} pane(s) piped with pipe-pane: the pipe's process cannot be handed over"
            ));
        }
        let jobs = self.status_jobs.active_job_count();
        if jobs > 0 {
            return Some(format!(
                "{jobs} status job(s) running: a #() job's process cannot be handed over"
            ));
        }
        None
    }

    pub(crate) fn pane_reader_gate(
        &self,
    ) -> Option<std::sync::Arc<crate::pane_reader_runtime::PaneReaderGate>> {
        self.pane_reader_runtime
            .as_ref()
            .map(|runtime| std::sync::Arc::clone(runtime.gate()))
    }
}

fn slot_to_handover(slot: &WindowLinkSlot) -> (String, u32) {
    let WindowLinkSlot {
        session_name,
        window_index,
    } = slot;
    (session_name.as_str().to_owned(), *window_index)
}

fn slot_from_handover(
    (session, window_index): (String, u32),
) -> Result<WindowLinkSlot, HandoverError> {
    Ok(WindowLinkSlot {
        session_name: restore_session_name(&session)?,
        window_index,
    })
}
