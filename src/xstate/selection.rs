use super::{XState, get_atom_name};
use crate::server::selection::{Clipboard, ForeignSelection, Primary, SelectionType};
use crate::transfer::{IO_BUDGET, ReadTransfer, TRANSFER_TIMEOUT, nonblocking};
use crate::{RealServerState, X11Selection};
use log::{debug, error, warn};
use smithay_client_toolkit::data_device_manager::{ReadPipe, WritePipe};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{ErrorKind, Write};
use std::rc::Rc;
use std::task::Poll;
use std::time::Instant;
use xcb::x;

#[derive(Debug)]
struct SelectionTargetId {
    name: String,
    target: x::Atom,
    property: x::Atom,
    source: Option<String>,
}

struct PendingSelectionData {
    target: x::Atom,
    property: x::Atom,
    pipe: WritePipe,
    incr: bool,
    active: bool,
    requestor: x::Window,
    buffer: Option<Vec<u8>>,
    written: usize,
    deadline: Instant,
}

pub struct Selection {
    mimes: Vec<SelectionTargetId>,
    connection: Rc<xcb::Connection>,
    window: x::Window,
    pending: RefCell<VecDeque<PendingSelectionData>>,
    selection: x::Atom,
    selection_time: u32,
    incr: x::Atom,
}

impl X11Selection for Selection {
    fn mime_types(&self) -> Vec<&str> {
        self.mimes
            .iter()
            .map(|target| target.name.as_str())
            .collect()
    }

    fn write_to(&self, mime: &str, pipe: WritePipe) {
        if let Err(error) = nonblocking(&pipe) {
            warn!("Cannot make selection destination nonblocking: {error}");
            return;
        }
        if let Some(target) = self.mimes.iter().find(|target| target.name == mime) {
            // A lot of X applications do not anticipate the possibility of multiple requests for
            // its owned selection to need the INCR transfer mechanism and will stop sending the
            // necessary `PropertyNotify` events, hanging Wayland transfer receivers.
            // To remedy this, every target requested by Wayland is put into a FIFO queue and the
            // `ConvertSelection` starting the next request is not sent until `next_conversion`
            // closes the `WritePipe`, marking termination of that request.
            self.pending.borrow_mut().push_back(PendingSelectionData {
                target: target.target,
                property: target.property,
                pipe,
                incr: false,
                active: false,
                requestor: x::WINDOW_NONE,
                buffer: None,
                written: 0,
                deadline: Instant::now() + TRANSFER_TIMEOUT,
            });
            if self.pending.borrow().len() == 1 {
                self.next_conversion();
            }
        } else {
            warn!("Could not find mime type {mime}");
        }
    }
}

impl Selection {
    /// Finish handling the current pending selection (if any) and queue the next one (if any).
    ///
    /// Regardless of whether the selection conversion succeeds or fails, this function must be
    /// called in order to drop the `WritePipe` to signal to the Wayland program no more data is
    /// coming and to start processing the next `PendingSelectionData`.
    ///
    /// # Panics:
    /// This function will panic if the `pending` `RefCell` is actively being borrowed.
    fn next_conversion(&self) {
        let mut pending = self.pending.borrow_mut();
        if pending.front().is_some_and(|p| p.active) {
            let finished = pending.pop_front().unwrap();
            self.connection.send_request(&x::DestroyWindow {
                window: finished.requestor,
            });
        }

        while let Some(psd) = pending.front_mut() {
            // A separate requestor is the conversion's identity. Reusing one
            // window after timeout lets a late same-target reply corrupt the
            // next FIFO entry, even when property/time are checked.
            let requestor = self.connection.generate_id();
            let result = self.connection.send_and_check_request(&x::CreateWindow {
                wid: requestor,
                parent: self.window,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                depth: 0,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: x::COPY_FROM_PARENT,
                value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
            });
            if let Err(error) = result {
                warn!("Cannot create selection request window: {error}");
                pending.pop_front();
                continue;
            }
            psd.requestor = requestor;
            if let Err(error) = self
                .connection
                .send_and_check_request(&x::ConvertSelection {
                    requestor,
                    selection: self.selection,
                    target: psd.target,
                    property: psd.property,
                    time: self.selection_time,
                })
            {
                warn!("Failed to request selection data: {error}");
                self.connection
                    .send_request(&x::DestroyWindow { window: requestor });
                pending.pop_front();
            } else {
                psd.active = true;
                psd.deadline = Instant::now() + TRANSFER_TIMEOUT;
                break;
            }
        }
        self.connection.flush().ok();
    }

    fn handle_refused_notify(&self, requestor: x::Window, target: x::Atom, time: u32) {
        if time == self.selection_time
            && self
                .pending
                .borrow()
                .front()
                .is_some_and(|p| p.active && p.requestor == requestor && p.target == target)
        {
            self.next_conversion();
        }
    }

    fn handle_notify(&self, target: x::Atom, requestor: x::Window, property: x::Atom) {
        let mut pending = self.pending.borrow_mut();
        let Some(psd) = pending.front_mut().filter(|p| {
            p.active && p.target == target && p.requestor == requestor && p.property == property
        }) else {
            warn!(
                "Got selection notify for unexpected target {}",
                get_atom_name(&self.connection, target),
            );
            return;
        };

        let request = self.connection.send_request(&x::GetProperty {
            delete: false,
            window: psd.requestor,
            property: psd.property,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: u32::MAX,
        });
        let reply = match self.connection.wait_for_reply(request) {
            Ok(reply) => reply,
            Err(e) => {
                warn!(
                    "Couldn't get mime type for {}: {e:?}",
                    get_atom_name(&self.connection, psd.target)
                );
                drop(pending);
                self.next_conversion();
                return;
            }
        };

        debug!(
            "got type {} for mime type {}",
            get_atom_name(&self.connection, reply.r#type()),
            get_atom_name(&self.connection, psd.target),
        );

        if reply.r#type() == self.incr {
            debug!(
                "beginning incr for {}",
                get_atom_name(&self.connection, psd.property)
            );
            psd.incr = true;
            psd.deadline = Instant::now() + TRANSFER_TIMEOUT;
            self.connection.send_request(&x::DeleteProperty {
                window: psd.requestor,
                property: psd.property,
            });
            self.connection.flush().ok();
            return;
        }

        let data = match reply.format() {
            8 => reply.value::<u8>(),
            32 => unsafe { reply.value::<u32>().align_to().1 },
            other => {
                warn!("Unexpected format {other} in selection reply");
                drop(pending);
                self.next_conversion();
                return;
            }
        };

        psd.buffer = Some(data.to_vec());
        psd.written = 0;
        drop(pending);
        self.advance_write(Instant::now());
    }

    fn advance_write(&self, now: Instant) {
        let mut pending = self.pending.borrow_mut();
        let Some(psd) = pending.front_mut() else {
            return;
        };
        let mut failed = now >= psd.deadline;
        if failed {
            warn!("X11 selection transfer timed out");
        }
        if !failed {
            let Some(buffer) = psd.buffer.as_ref() else {
                return;
            };
            let mut budget = IO_BUDGET;
            for _ in 0..16 {
                if psd.written == buffer.len() || budget == 0 {
                    break;
                }
                let end = buffer.len().min(psd.written + budget);
                match psd.pipe.write(&buffer[psd.written..end]) {
                    Ok(0) => {
                        failed = true;
                        break;
                    }
                    Ok(n) => {
                        psd.written += n;
                        budget -= n;
                        psd.deadline = now + TRANSFER_TIMEOUT;
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => {
                        warn!("Failed to write selection data: {error}");
                        failed = true;
                        break;
                    }
                }
            }
            if !failed && psd.written < buffer.len() {
                return;
            }
        }
        self.connection.send_request(&x::DeleteProperty {
            window: psd.requestor,
            property: psd.property,
        });
        self.connection.flush().ok();
        if !failed && psd.incr && psd.buffer.as_ref().is_some_and(|b| !b.is_empty()) {
            psd.buffer = None;
            psd.written = 0;
            return;
        }
        drop(pending);
        self.next_conversion();
    }

    fn check_for_incr(&self, event: &x::PropertyNotifyEvent) -> bool {
        debug_assert_eq!(event.state(), x::Property::NewValue);
        let target = self.pending.borrow().front().and_then(|pending| {
            (pending.requestor == event.window()
                && pending.property == event.atom()
                && pending.incr)
                .then_some(pending.target)
        });
        if let Some(target) = target {
            self.handle_notify(target, event.window(), event.atom());
            true
        } else {
            false
        }
    }
}

impl Drop for Selection {
    fn drop(&mut self) {
        for pending in self.pending.get_mut() {
            if pending.active {
                self.connection.send_request(&x::DestroyWindow {
                    window: pending.requestor,
                });
            }
        }
        self.connection.flush().ok();
    }
}

#[derive(Clone, Copy)]
struct SelectionReply {
    time: u32,
    requestor: x::Window,
    selection: x::Atom,
    target: x::Atom,
    property: x::Atom,
}

impl SelectionReply {
    fn from_request(request: &x::SelectionRequestEvent) -> Self {
        Self {
            time: request.time(),
            requestor: request.requestor(),
            selection: request.selection(),
            target: request.target(),
            property: request.property(),
        }
    }

    fn notify(&self, connection: &xcb::Connection, success: bool) {
        if let Err(error) = connection.send_and_check_request(&x::SendEvent {
            propagate: false,
            destination: x::SendEventDest::Window(self.requestor),
            event_mask: x::EventMask::empty(),
            event: &x::SelectionNotifyEvent::new(
                self.time,
                self.requestor,
                self.selection,
                self.target,
                if success { self.property } else { x::ATOM_NONE },
            ),
        }) {
            warn!("Failed to send selection request notify: {error:?}");
        }
    }
}

struct PendingWaylandRead {
    reply: SelectionReply,
    transfer: ReadTransfer<ReadPipe>,
    max_req_bytes: usize,
}

enum SelectionRequestOutcome {
    Immediate(bool),
    Pending(PendingWaylandRead),
}

pub struct WaylandIncrInfo {
    data: Vec<u8>,
    start: usize,
    reply: SelectionReply,
    max_req_bytes: usize,
    deadline: Instant,
}

pub struct WaylandSelection<T: SelectionType> {
    mimes: Vec<SelectionTargetId>,
    inner: ForeignSelection<T>,
}

impl WaylandIncrInfo {
    /// Return true once complete (including a failed/disappeared requestor).
    fn advance(&mut self, connection: &xcb::Connection) -> bool {
        let end = std::cmp::min(self.max_req_bytes + self.start, self.data.len());
        if let Err(error) = connection.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Append,
            window: self.reply.requestor,
            property: self.reply.property,
            r#type: self.reply.target,
            data: &self.data[self.start..end],
        }) {
            warn!("Failed to write incremental selection data: {error:?}");
            return true;
        }
        let finished = self.start == end;
        self.start = end;
        self.deadline = Instant::now() + TRANSFER_TIMEOUT;
        finished
    }
}

enum CurrentSelection<T: SelectionType> {
    X11(Rc<Selection>),
    Wayland(WaylandSelection<T>),
}

struct SelectionData<T: SelectionType> {
    last_selection_timestamp: u32,
    atom: x::Atom,
    targets_atom: x::Atom,
    current_selection: Option<CurrentSelection<T>>,
}

// This is a trait so that we can use &dyn
trait SelectionDataImpl {
    fn set_owner(&self, connection: &xcb::Connection, wm_window: x::Window) -> bool;
    fn handle_new_owner(
        &mut self,
        connection: &xcb::Connection,
        wm_window: x::Window,
        atoms: &super::Atoms,
        owner: x::Window,
        timestamp: u32,
    );
    fn handle_target_list(
        &mut self,
        connection: &Rc<xcb::Connection>,
        wm_window: x::Window,
        atoms: &super::Atoms,
        target_window: x::Window,
        dest_property: x::Atom,
        server_state: &mut RealServerState,
    );
    fn x11_selection(&self) -> Option<&Selection>;
    fn handle_selection_request(
        &mut self,
        connection: &xcb::Connection,
        atoms: &super::Atoms,
        request: &x::SelectionRequestEvent,
        max_req_bytes: usize,
        server_state: &mut RealServerState,
    ) -> SelectionRequestOutcome;
    fn atom(&self) -> x::Atom;
    fn selection_clear(&mut self);
}

impl<T: SelectionType> SelectionData<T> {
    fn new(atom: x::Atom, targets_atom: x::Atom) -> Self {
        Self {
            last_selection_timestamp: x::CURRENT_TIME,
            atom,
            targets_atom,
            current_selection: None,
        }
    }
}

impl<T: SelectionType> SelectionDataImpl for SelectionData<T> {
    fn atom(&self) -> x::Atom {
        self.atom
    }
    fn set_owner(&self, connection: &xcb::Connection, wm_window: x::Window) -> bool {
        if let Err(e) = connection.send_and_check_request(&x::SetSelectionOwner {
            owner: wm_window,
            selection: self.atom,
            // A Wayland selection is newer than every X11 one. Reusing the
            // last X11 owner's timestamp makes clients such as Qt keep their
            // cached TARGETS, so they keep pasting the old X11 data.
            time: x::CURRENT_TIME,
        }) {
            warn!(
                "Could not become owner of {}: {e:?}",
                get_atom_name(connection, self.atom)
            );
            return false;
        };

        match connection.wait_for_reply(connection.send_request(&x::GetSelectionOwner {
            selection: self.atom,
        })) {
            Ok(reply) if reply.owner() == wm_window => true,
            Ok(reply) => {
                warn!(
                    "Could not become owner of {} (owned by {:?})",
                    get_atom_name(connection, self.atom),
                    reply.owner()
                );
                false
            }
            Err(e) => {
                warn!(
                    "Could not validate ownership of {}: {e:?}",
                    get_atom_name(connection, self.atom)
                );
                false
            }
        }
    }

    fn selection_clear(&mut self) {
        self.current_selection = None;
    }

    fn handle_new_owner(
        &mut self,
        connection: &xcb::Connection,
        wm_window: x::Window,
        atoms: &super::Atoms,
        owner: x::Window,
        timestamp: u32,
    ) {
        // Grab targets
        match connection.send_and_check_request(&x::ConvertSelection {
            requestor: wm_window,
            selection: self.atom,
            target: atoms.targets,
            property: self.targets_atom,
            time: timestamp,
        }) {
            Ok(_) => {
                debug!(
                    "new {} owner: {owner:?}",
                    get_atom_name(connection, self.atom)
                );
                self.last_selection_timestamp = timestamp;
            }
            Err(e) => warn!(
                "could not set new {} owner: {e:?}",
                get_atom_name(connection, self.atom)
            ),
        }
    }

    fn handle_target_list(
        &mut self,
        connection: &Rc<xcb::Connection>,
        wm_window: x::Window,
        atoms: &super::Atoms,
        target_window: x::Window,
        dest_property: x::Atom,
        server_state: &mut RealServerState,
    ) {
        let reply = match connection.wait_for_reply(connection.send_request(&x::GetProperty {
            delete: true,
            window: wm_window,
            property: dest_property,
            r#type: x::ATOM_ATOM,
            long_offset: 0,
            long_length: 20,
        })) {
            Ok(reply) => reply,
            Err(e) => {
                warn!("Could not obtain target list: {e:?}");
                return;
            }
        };

        let targets: &[x::Atom] = reply.value();
        if targets.is_empty() {
            warn!("Got empty selection target list, trying again...");
            match connection.wait_for_reply(connection.send_request(&x::GetSelectionOwner {
                selection: self.atom,
            })) {
                Ok(reply) if reply.owner() != wm_window => {
                    self.handle_new_owner(
                        connection,
                        wm_window,
                        atoms,
                        reply.owner(),
                        self.last_selection_timestamp,
                    );
                }
                Ok(_) => {
                    warn!("We are unexpectedly the selection owner? Clipboard may be broken!");
                }
                Err(e) => {
                    error!("Couldn't grab selection owner: {e:?}. Clipboard is stale!");
                }
            }
            return;
        }
        if log::log_enabled!(log::Level::Debug) {
            let targets_str: Vec<String> = targets
                .iter()
                .map(|t| get_atom_name(connection, *t))
                .collect();
            debug!("got targets: {targets_str:?}");
        }

        let selection = get_atom_name(connection, self.atom);
        let mimes = targets
            .iter()
            .copied()
            .filter(|atom| ![atoms.targets, atoms.multiple, atoms.save_targets].contains(atom))
            .map(|target| {
                let name = get_atom_name(connection, target);
                let property = connection
                    .wait_for_reply(connection.send_request(&x::InternAtom {
                        only_if_exists: false,
                        name: &[name.as_bytes(), b"_", selection.as_bytes()].concat(),
                    }))
                    .unwrap()
                    .atom();
                SelectionTargetId {
                    name,
                    target,
                    property,
                    source: None,
                }
            })
            .collect();

        let selection = Rc::new(Selection {
            mimes,
            connection: connection.clone(),
            window: target_window,
            pending: RefCell::default(),
            selection: self.atom,
            selection_time: self.last_selection_timestamp,
            incr: atoms.incr,
        });

        server_state.set_selection_source::<T>(&selection);
        self.current_selection = Some(CurrentSelection::X11(selection));
        debug!("{} set from X11", get_atom_name(connection, self.atom));
    }

    fn x11_selection(&self) -> Option<&Selection> {
        match &self.current_selection {
            Some(CurrentSelection::X11(selection)) => Some(selection),
            _ => None,
        }
    }

    fn handle_selection_request(
        &mut self,
        connection: &xcb::Connection,
        atoms: &super::Atoms,
        request: &x::SelectionRequestEvent,
        max_req_bytes: usize,
        _server_state: &mut RealServerState,
    ) -> SelectionRequestOutcome {
        use SelectionRequestOutcome::{Immediate, Pending};
        let Some(CurrentSelection::Wayland(WaylandSelection { mimes, inner })) =
            &mut self.current_selection
        else {
            warn!("Got selection request, but we don't seem to be the selection owner");
            return Immediate(false);
        };
        if request.target() == atoms.targets {
            let targets: Box<[x::Atom]> = mimes.iter().map(|t| t.target).collect();
            return Immediate(
                connection
                    .send_and_check_request(&x::ChangeProperty {
                        mode: x::PropMode::Replace,
                        window: request.requestor(),
                        property: request.property(),
                        r#type: x::ATOM_ATOM,
                        data: &targets,
                    })
                    .is_ok(),
            );
        }
        let Some(target) = mimes.iter().find(|t| t.target == request.target()) else {
            return Immediate(false);
        };
        let mime = target.source.as_ref().unwrap_or(&target.name).clone();
        match inner
            .receive(mime)
            .and_then(|pipe| ReadTransfer::new(pipe, Instant::now()))
        {
            Ok(transfer) => Pending(PendingWaylandRead {
                reply: SelectionReply::from_request(request),
                transfer,
                max_req_bytes,
            }),
            Err(error) => {
                warn!("Failed to start Wayland selection transfer: {error}");
                Immediate(false)
            }
        }
    }
}

pub(super) struct SelectionState {
    clipboard: SelectionData<Clipboard>,
    primary: SelectionData<Primary>,
    target_window: x::Window,
    pending_reads: Vec<PendingWaylandRead>,
    incr_data: Vec<WaylandIncrInfo>,
}

impl SelectionState {
    pub fn new(connection: &xcb::Connection, root: x::Window, atoms: &super::Atoms) -> Self {
        let target_window = connection.generate_id();
        connection
            .send_and_check_request(&x::CreateWindow {
                wid: target_window,
                width: 1,
                height: 1,
                depth: 0,
                parent: root,
                x: 0,
                y: 0,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: x::COPY_FROM_PARENT,
                // Watch for INCR property changes.
                value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
            })
            .expect("Couldn't create window for selections");
        Self {
            target_window,
            pending_reads: Vec::new(),
            incr_data: Vec::new(),
            clipboard: SelectionData::new(atoms.clipboard, atoms.clipboard_targets),
            primary: SelectionData::new(atoms.primary, atoms.primary_targets),
        }
    }
}

impl XState {
    pub(crate) fn has_pending_selection_transfers(&self) -> bool {
        !self.selection_state.pending_reads.is_empty()
            || !self.selection_state.incr_data.is_empty()
            || self
                .selection_state
                .clipboard
                .x11_selection()
                .is_some_and(|s| !s.pending.borrow().is_empty())
            || self
                .selection_state
                .primary
                .x11_selection()
                .is_some_and(|s| !s.pending.borrow().is_empty())
    }

    pub(crate) fn advance_selection_transfers(&mut self) {
        let now = Instant::now();
        for selection in [
            self.selection_state.clipboard.x11_selection(),
            self.selection_state.primary.x11_selection(),
        ]
        .into_iter()
        .flatten()
        {
            selection.advance_write(now);
        }
        let mut pending = std::mem::take(&mut self.selection_state.pending_reads);
        for mut request in pending.drain(..) {
            match request.transfer.advance(now) {
                Poll::Pending => self.selection_state.pending_reads.push(request),
                Poll::Ready(Err(error)) => {
                    warn!("Wayland selection transfer failed: {error}");
                    request.reply.notify(&self.connection, false);
                }
                Poll::Ready(Ok(data)) => {
                    let reply = request.reply;
                    let success = if data.len() > request.max_req_bytes {
                        let setup = self
                            .connection
                            .send_and_check_request(&x::ChangeWindowAttributes {
                                window: reply.requestor,
                                value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
                            })
                            .and_then(|_| {
                                self.connection.send_and_check_request(&x::ChangeProperty {
                                    mode: x::PropMode::Replace,
                                    window: reply.requestor,
                                    property: reply.property,
                                    r#type: self.atoms.incr,
                                    data: &[data.len() as u32],
                                })
                            });
                        if setup.is_ok() {
                            self.selection_state.incr_data.push(WaylandIncrInfo {
                                data,
                                start: 0,
                                reply,
                                max_req_bytes: request.max_req_bytes,
                                deadline: now + TRANSFER_TIMEOUT,
                            });
                            true
                        } else {
                            false
                        }
                    } else {
                        self.connection
                            .send_and_check_request(&x::ChangeProperty {
                                mode: x::PropMode::Replace,
                                window: reply.requestor,
                                property: reply.property,
                                r#type: reply.target,
                                data: &data,
                            })
                            .is_ok()
                    };
                    reply.notify(&self.connection, success);
                }
            }
        }
        // An X11 requestor can disappear or stop acknowledging INCR chunks.
        self.selection_state.incr_data.retain(|p| p.deadline > now);
    }

    fn intern_target_property_atoms(&self, mime: &[u8], suffix: &[u8]) -> (x::Atom, x::Atom) {
        // A concatenation of the target and the selection type are used to create a distinct
        // property to write to
        let target = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::InternAtom {
                only_if_exists: false,
                name: mime,
            }))
            .unwrap()
            .atom();
        let property = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::InternAtom {
                only_if_exists: false,
                name: &[mime, suffix].concat(),
            }))
            .unwrap()
            .atom();
        (target, property)
    }

    pub(crate) fn set_clipboard(&mut self, selection: ForeignSelection<Clipboard>) {
        let mut utf8_xwl = false;
        let mut utf8_wl = false;
        let mut mimes: Vec<SelectionTargetId> = selection
            .mime_types
            .iter()
            .map(|mime| {
                match mime.as_str() {
                    "UTF8_STRING" => utf8_xwl = true,
                    "text/plain;charset=utf-8" => utf8_wl = true,
                    _ => {}
                }

                let (target, property) =
                    self.intern_target_property_atoms(mime.as_bytes(), b"_CLIPBOARD");
                SelectionTargetId {
                    name: mime.clone(),
                    target,
                    property,
                    source: None,
                }
            })
            .collect();

        if utf8_wl && !utf8_xwl {
            let name = "UTF8_STRING".to_string();
            let (target, property) =
                self.intern_target_property_atoms(name.as_bytes(), b"_CLIPBOARD");
            mimes.push(SelectionTargetId {
                name,
                target,
                property,
                source: Some("text/plain;charset=utf-8".to_string()),
            });
        }

        if self
            .selection_state
            .clipboard
            .set_owner(&self.connection, self.wm_window)
        {
            self.selection_state.clipboard.current_selection =
                Some(CurrentSelection::Wayland(WaylandSelection {
                    mimes,
                    inner: selection,
                }));

            debug!("Clipboard set from Wayland");
        }
    }

    pub(crate) fn set_primary_selection(&mut self, selection: ForeignSelection<Primary>) {
        let mut utf8_xwl = false;
        let mut utf8_wl = false;
        let mut mimes: Vec<SelectionTargetId> = selection
            .mime_types
            .iter()
            .map(|mime| {
                match mime.as_str() {
                    "UTF8_STRING" => utf8_xwl = true,
                    "text/plain;charset=utf-8" => utf8_wl = true,
                    _ => {}
                }

                let (target, property) =
                    self.intern_target_property_atoms(mime.as_bytes(), b"_PRIMARY");
                SelectionTargetId {
                    name: mime.clone(),
                    target,
                    property,
                    source: None,
                }
            })
            .collect();

        if utf8_wl && !utf8_xwl {
            let name = "UTF8_STRING".to_string();
            let (target, property) =
                self.intern_target_property_atoms(name.as_bytes(), b"_PRIMARY");
            mimes.push(SelectionTargetId {
                name,
                target,
                property,
                source: Some("text/plain;charset=utf-8".to_string()),
            });
        }

        if self
            .selection_state
            .primary
            .set_owner(&self.connection, self.wm_window)
        {
            self.selection_state.primary.current_selection =
                Some(CurrentSelection::Wayland(WaylandSelection {
                    mimes,
                    inner: selection,
                }));
            debug!("Primary set from Wayland");
        }
    }

    pub(super) fn handle_selection_event(
        &mut self,
        event: &xcb::Event,
        server_state: &mut RealServerState,
    ) -> bool {
        macro_rules! get_selection_data {
            ($selection:expr) => {
                match $selection {
                    x if x == self.atoms.clipboard => {
                        &mut self.selection_state.clipboard as &mut dyn SelectionDataImpl
                    }
                    x if x == self.atoms.primary => &mut self.selection_state.primary as _,
                    _ => return true,
                }
            };
        }
        match event {
            xcb::Event::X(x::Event::SelectionClear(e)) => {
                let data = get_selection_data!(e.selection());
                data.selection_clear();
            }
            xcb::Event::X(x::Event::SelectionNotify(e)) => {
                if e.property() == x::ATOM_NONE {
                    // Since the requested conversion could not be made, the request is invalid and
                    // should be removed, dropping the WritePipe to signal no data will be sent
                    let data = get_selection_data!(e.selection());
                    if let Some(selection) = data.x11_selection() {
                        selection.handle_refused_notify(e.requestor(), e.target(), e.time());
                    }
                    warn!(
                        "selection notify fail? {}",
                        get_atom_name(&self.connection, e.selection())
                    );
                    return true;
                }

                let data = get_selection_data!(e.selection());
                debug!(
                    "selection notify requestor: {:?} target: {} selection: {}",
                    e.requestor(),
                    get_atom_name(&self.connection, e.target()),
                    get_atom_name(&self.connection, e.selection()),
                );

                if e.requestor() == self.wm_window {
                    match e.target() {
                        x if x == self.atoms.targets => data.handle_target_list(
                            &self.connection,
                            self.wm_window,
                            &self.atoms,
                            self.selection_state.target_window,
                            e.property(),
                            server_state,
                        ),
                        other => warn!(
                            "got unexpected selection notify for target {}",
                            get_atom_name(&self.connection, other)
                        ),
                    }
                } else if let Some(selection) = data.x11_selection() {
                    if e.time() == selection.selection_time {
                        selection.handle_notify(e.target(), e.requestor(), e.property());
                    }
                }
            }
            xcb::Event::X(x::Event::SelectionRequest(e)) => {
                let data = get_selection_data!(e.selection());
                let reply = SelectionReply::from_request(e);
                if log::log_enabled!(log::Level::Debug) {
                    let target = get_atom_name(&self.connection, e.target());
                    let selection = get_atom_name(&self.connection, data.atom());
                    debug!("Got selection request for target {target} (selection: {selection})");
                }

                if e.property() == x::ATOM_NONE {
                    debug!("refusing - property is set to none");
                    reply.notify(&self.connection, false);
                    return true;
                }

                let outcome = data.handle_selection_request(
                    &self.connection,
                    &self.atoms,
                    e,
                    self.max_req_bytes,
                    server_state,
                );
                match outcome {
                    SelectionRequestOutcome::Immediate(success) => {
                        reply.notify(&self.connection, success)
                    }
                    SelectionRequestOutcome::Pending(pending) => {
                        // A property can represent only one transfer at a time.
                        let duplicate = self.selection_state.pending_reads.iter().any(|p| {
                            p.reply.requestor == reply.requestor
                                && p.reply.property == reply.property
                        }) || self.selection_state.incr_data.iter().any(|p| {
                            p.reply.requestor == reply.requestor
                                && p.reply.property == reply.property
                        });
                        if duplicate {
                            reply.notify(&self.connection, false);
                        } else {
                            self.selection_state.pending_reads.push(pending);
                        }
                    }
                }
            }

            xcb::Event::XFixes(xcb::xfixes::Event::SelectionNotify(e)) => match e.selection() {
                x if x == self.atoms.clipboard || x == self.atoms.primary => match e.subtype() {
                    xcb::xfixes::SelectionEvent::SetSelectionOwner => {
                        if e.owner() == self.wm_window {
                            return true;
                        }

                        let data = get_selection_data!(x);

                        data.handle_new_owner(
                            &self.connection,
                            self.wm_window,
                            &self.atoms,
                            e.owner(),
                            e.timestamp(),
                        );
                    }
                    xcb::xfixes::SelectionEvent::SelectionClientClose
                    | xcb::xfixes::SelectionEvent::SelectionWindowDestroy => {
                        debug!("Selection owner destroyed, selection will be unset");
                        get_selection_data!(x).selection_clear();
                    }
                },
                x if x == self.atoms.xsettings => match e.subtype() {
                    xcb::xfixes::SelectionEvent::SelectionClientClose
                    | xcb::xfixes::SelectionEvent::SelectionWindowDestroy => {
                        debug!("Xsettings owner disappeared, reacquiring");
                        self.set_xsettings_owner();
                    }
                    _ => {}
                },
                _ => {}
            },
            _ => return false,
        }

        true
    }

    /// Check if a PropertyNotifyEvent refers to a supported selection property, then make progress
    /// on an incremental data transfer if that property is in that process.
    /// Returns `true` if an attempt at progressing the data transfer was made, whether or not it
    /// succeeded, and `false` otherwise (e.g. the event does not target a selection property or
    /// that property is not in the process of an incremental data transfer)
    pub(super) fn handle_selection_property_change(
        &mut self,
        event: &x::PropertyNotifyEvent,
    ) -> bool {
        fn inner<T: SelectionType>(
            event: &x::PropertyNotifyEvent,
            data: &mut SelectionData<T>,
        ) -> bool {
            match event.state() {
                x::Property::NewValue => {
                    if let Some(selection) = &data.x11_selection() {
                        return selection.check_for_incr(event);
                    }
                }
                x::Property::Delete => {}
            }
            false
        }
        if event.state() == x::Property::Delete {
            if let Some(index) = self.selection_state.incr_data.iter().position(|p| {
                p.reply.requestor == event.window() && p.reply.property == event.atom()
            }) {
                if self.selection_state.incr_data[index].advance(&self.connection) {
                    self.selection_state.incr_data.remove(index);
                }
                return true;
            }
        }
        inner(event, &mut self.selection_state.primary)
            || inner(event, &mut self.selection_state.clipboard)
    }
}
