use super::clientside::SelectionEvents;
use super::{InnerServerState, MyWorld};
use crate::X11Selection;
use log::{info, warn};
use smithay_client_toolkit::data_device_manager::ReadPipe;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Proxy, QueueHandle};

use smithay_client_toolkit::data_device_manager::{
    DataDeviceManagerState, data_device::DataDevice,
    data_offer::SelectionOffer as WlSelectionOffer, data_source::CopyPasteSource,
};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::PrimarySelectionDevice;
use smithay_client_toolkit::primary_selection::offer::PrimarySelectionOffer;
use smithay_client_toolkit::primary_selection::selection::PrimarySelectionSource;
use std::rc::{Rc, Weak};
use std::sync::OnceLock;

/// Extra mime type offered on every Wayland source this process creates for an X11 selection.
///
/// The compositor can silently ignore a set_selection request (for example when it judges that
/// none of our surfaces has keyboard focus), and no `cancelled` event follows. Without a way to
/// tell our own source apart from somebody else's, the X11 source would then be kept forever
/// and every later Wayland selection would be ignored. An offer carrying this marker is ours;
/// one without it means another client owns the selection now.
pub(crate) fn own_source_marker() -> &'static str {
    static MARKER: OnceLock<String> = OnceLock::new();
    MARKER.get_or_init(|| {
        format!(
            "application/x-xwayland-satellite-source;pid={}",
            std::process::id()
        )
    })
}

pub(super) struct SelectionStates<S: X11Selection> {
    clipboard: Option<SelectionState<S, Clipboard>>,
    primary: Option<SelectionState<S, Primary>>,
}

impl<S: X11Selection> SelectionStates<S> {
    pub fn new(global_list: &GlobalList, qh: &QueueHandle<MyWorld>) -> Self {
        Self {
            clipboard: DataDeviceManagerState::bind(global_list, qh)
                .inspect_err(|e| {
                    warn!("Could not bind data device manager ({e:?}). Clipboard will not work.")
                })
                .ok()
                .map(SelectionState::new),
            primary: PrimarySelectionManagerState::bind(global_list, qh)
                .inspect_err(|_| info!("Primary selection unsupported."))
                .ok()
                .map(SelectionState::new),
        }
    }

    pub fn seat_created(&mut self, qh: &QueueHandle<MyWorld>, seat: &WlSeat) {
        if let Some(c) = &mut self.clipboard {
            c.device = Some(c.manager.get_data_device(qh, seat));
        }

        if let Some(d) = &mut self.primary {
            d.device = Some(d.manager.get_selection_device(qh, seat));
        }
    }
}

enum SelectionData<S: X11Selection, T: SelectionType> {
    X11 { inner: T::Source, data: Weak<S> },
    Foreign(ForeignSelection<T>),
}

struct SelectionState<S: X11Selection, T: SelectionType> {
    manager: T::Manager,
    device: Option<T::DataDevice>,
    source: Option<SelectionData<S, T>>,
}

impl<S: X11Selection, T: SelectionType> SelectionState<S, T> {
    fn new(manager: T::Manager) -> Self {
        Self {
            manager,
            device: None,
            source: None,
        }
    }
}

impl<S: X11Selection> InnerServerState<S> {
    pub(super) fn handle_selection_events(&mut self) {
        self.handle_impl::<Clipboard>();
        self.handle_impl::<Primary>();
    }

    fn handle_impl<T: SelectionType>(&mut self) {
        let Some(state) = T::selection_state(&mut self.selection_states) else {
            return;
        };

        let events = T::get_events(&mut self.world);

        for (mime_type, fd) in std::mem::take(&mut events.requests) {
            let SelectionData::X11 { data, .. } = state.source.as_ref().unwrap() else {
                unreachable!("Got selection request without having set the selection?")
            };
            if mime_type == own_source_marker() {
                // The marker carries no data; dropping the pipe ends the transfer.
                continue;
            }
            if let Some(data) = data.upgrade() {
                data.write_to(&mime_type, fd);
            }
        }

        if events.cancelled {
            state.source = None;
            events.cancelled = false;
        }

        let take_offer = match &state.source {
            None => true,
            // An offer without our marker while we think the X11 side owns the selection means
            // the compositor did not keep our source; follow the compositor.
            Some(SelectionData::X11 { .. }) => events
                .offer
                .as_ref()
                .is_some_and(|offer| !T::get_mimes(offer).iter().any(|m| m == own_source_marker())),
            Some(SelectionData::Foreign(_)) => false,
        };
        if take_offer {
            if let Some(offer) = T::take_offer(&mut events.offer) {
                let mime_types = T::get_mimes(&offer);
                if mime_types.iter().any(|m| m == own_source_marker()) {
                    // Our own (already replaced) source echoed back; nothing new to forward.
                    return;
                }
                if matches!(state.source, Some(SelectionData::X11 { .. })) {
                    info!(
                        "Compositor did not keep our X11 selection source; using the new Wayland selection"
                    );
                }
                let foreign = ForeignSelection {
                    mime_types,
                    inner: offer,
                };
                state.source = Some(SelectionData::Foreign(foreign));
            }
        }
    }

    pub(crate) fn set_selection_source<T: SelectionType>(&mut self, selection: &Rc<S>) {
        if let Some(state) = T::selection_state(&mut self.selection_states) {
            let mut mime_types = selection.mime_types();
            mime_types.push(own_source_marker());
            let src = T::create_source(&state.manager, &self.qh, mime_types);
            let data = SelectionData::X11 {
                inner: src,
                data: Rc::downgrade(selection),
            };
            let SelectionData::X11 { inner, .. } = state.source.insert(data) else {
                unreachable!();
            };
            if let Some(serial) = self
                .last_kb_serial
                .as_ref()
                .map(|(_seat, serial)| serial)
                .copied()
            {
                T::set_selection(inner, state.device.as_ref().unwrap(), serial);
            }
        }
    }

    pub(crate) fn new_selection<T: SelectionType>(&mut self) -> Option<ForeignSelection<T>> {
        T::selection_state(&mut self.selection_states)
            .as_mut()
            .and_then(|state| {
                state.source.take().and_then(|s| match s {
                    SelectionData::Foreign(f) => Some(f),
                    SelectionData::X11 { .. } => {
                        state.source = Some(s);
                        None
                    }
                })
            })
    }
}

pub struct ForeignSelection<T: SelectionType> {
    pub mime_types: Box<[String]>,
    inner: T::Offer,
}

#[allow(private_bounds)]
impl<T: SelectionType> ForeignSelection<T> {
    /// Request the transfer without waiting for its producer. The main loop flushes
    /// the Wayland queue and drives the returned pipe asynchronously.
    pub(crate) fn receive(&self, mime_type: String) -> std::io::Result<ReadPipe> {
        T::receive_offer(&self.inner, mime_type)
    }
}

#[allow(private_bounds, private_interfaces)]
pub trait SelectionType: Sized {
    type Source;
    type Offer;
    type Manager;
    type DataDevice;

    // The methods in this trait shouldn't be used outside of this file.

    fn selection_state<S: X11Selection>(
        state: &mut SelectionStates<S>,
    ) -> &mut Option<SelectionState<S, Self>>;

    fn create_source(
        manager: &Self::Manager,
        qh: &QueueHandle<MyWorld>,
        mime_types: Vec<&str>,
    ) -> Self::Source;

    fn set_selection(source: &Self::Source, device: &Self::DataDevice, serial: u32);

    fn get_events(world: &mut MyWorld) -> &mut SelectionEvents<Self::Offer>;

    fn receive_offer(offer: &Self::Offer, mime_type: String) -> std::io::Result<ReadPipe>;

    fn take_offer(offer: &mut Option<Self::Offer>) -> Option<Self::Offer> {
        offer.take()
    }

    fn get_mimes(offer: &Self::Offer) -> Box<[String]>;
}

pub enum Clipboard {}
pub enum Primary {}

#[allow(private_bounds, private_interfaces)]
impl SelectionType for Clipboard {
    type Source = CopyPasteSource;
    type Offer = WlSelectionOffer;
    type Manager = DataDeviceManagerState;
    type DataDevice = DataDevice;

    fn selection_state<S: X11Selection>(
        state: &mut SelectionStates<S>,
    ) -> &mut Option<SelectionState<S, Self>> {
        &mut state.clipboard
    }

    fn create_source(
        manager: &Self::Manager,
        qh: &QueueHandle<MyWorld>,
        mime_types: Vec<&str>,
    ) -> Self::Source {
        manager.create_copy_paste_source(qh, mime_types)
    }

    fn set_selection(source: &Self::Source, device: &Self::DataDevice, serial: u32) {
        source.set_selection(device, serial);
    }

    fn get_events(world: &mut MyWorld) -> &mut SelectionEvents<Self::Offer> {
        &mut world.clipboard
    }

    fn take_offer(offer: &mut Option<Self::Offer>) -> Option<Self::Offer> {
        offer.take().filter(|offer| offer.inner().is_alive())
    }

    fn get_mimes(offer: &Self::Offer) -> Box<[String]> {
        offer.with_mime_types(|mimes| mimes.into())
    }

    fn receive_offer(offer: &Self::Offer, mime_type: String) -> std::io::Result<ReadPipe> {
        offer.receive(mime_type).map_err(|e| {
            use smithay_client_toolkit::data_device_manager::data_offer::DataOfferError;
            match e {
                DataOfferError::InvalidReceive => std::io::Error::from(std::io::ErrorKind::Other),
                DataOfferError::Io(e) => e,
            }
        })
    }
}

#[allow(private_bounds, private_interfaces)]
impl SelectionType for Primary {
    type Source = PrimarySelectionSource;
    type Offer = PrimarySelectionOffer;
    type Manager = PrimarySelectionManagerState;
    type DataDevice = PrimarySelectionDevice;

    fn selection_state<S: X11Selection>(
        state: &mut SelectionStates<S>,
    ) -> &mut Option<SelectionState<S, Self>> {
        &mut state.primary
    }

    fn create_source(
        manager: &Self::Manager,
        qh: &QueueHandle<MyWorld>,
        mime_types: Vec<&str>,
    ) -> Self::Source {
        manager.create_selection_source(qh, mime_types)
    }

    fn set_selection(source: &Self::Source, device: &Self::DataDevice, serial: u32) {
        source.set_selection(device, serial);
    }

    fn get_events(world: &mut MyWorld) -> &mut SelectionEvents<Self::Offer> {
        &mut world.primary
    }

    fn receive_offer(offer: &Self::Offer, mime_type: String) -> std::io::Result<ReadPipe> {
        offer.receive(mime_type)
    }

    fn get_mimes(offer: &Self::Offer) -> Box<[String]> {
        offer.with_mime_types(|mimes| mimes.into())
    }
}
