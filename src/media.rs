use std::io;
use std::ops::Deref;
use std::sync::mpsc;

use crate::config::Media as MediaConfig;
use crate::platform::{VirtualPresenterEndpoint, VirtualPresenterListener};

#[allow(unused_imports)]
pub use vivid_gateway::{
    AudioSourceConfig, ClipRect, KeyframeRequestOutcome, MediaEvent, NodeConfig,
    OuterMediaProjection, PlayRequest, ProducerId, ProjectionSnapshot, RetainedRaster, SceneNode,
    SceneNodeConfig, SemanticDescriptor, SnapshotOverlayWindow, SnapshotSource, SnapshotSurface,
    SourceDescriptor, SourceKey,
};

impl vivid_gateway::PresenterListener for VirtualPresenterListener {
    fn endpoint(&self) -> String {
        VirtualPresenterListener::endpoint(self)
    }

    fn accept(&self) -> io::Result<vivid_gateway::Transport> {
        VirtualPresenterListener::accept(self)
    }
}

pub struct VirtualVivid(vivid_gateway::VirtualVivid);

impl VirtualVivid {
    #[allow(dead_code)]
    pub fn start(endpoint: VirtualPresenterEndpoint, config: MediaConfig) -> io::Result<Self> {
        // Terminate media at the runtime: no caller hosts a media-event consumer, so successful
        // validation must return the ingress flow directly rather than park a delivery.
        let listener = VirtualPresenterListener::bind(endpoint)?;
        vivid_gateway::VirtualVivid::start_eventless(listener, config).map(Self)
    }

    pub fn start_with_events(
        endpoint: VirtualPresenterEndpoint,
        config: MediaConfig,
        events: Option<mpsc::SyncSender<MediaEvent>>,
    ) -> io::Result<Self> {
        let listener = VirtualPresenterListener::bind(endpoint)?;
        // Overlay hosting is advertised for every pane rather than gated per pane: one presenter
        // serves them all, and negotiation is producer-driven, so a plain shell that never asks
        // for the overlay profiles is unaffected by their being on offer.
        let presenter = vivid_gateway::VirtualVivid::start_configured(
            listener,
            vivid_gateway::PresenterConfig::terminal_with_overlay(config),
            events,
        )?;
        presenter.enable_overlay_host_relay();
        Ok(Self(presenter))
    }
}

impl Deref for VirtualVivid {
    type Target = vivid_gateway::VirtualVivid;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
