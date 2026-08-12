use std::io;
use std::ops::Deref;
use std::sync::mpsc;

use crate::config::Media as MediaConfig;
use crate::platform::{VirtualPresenterEndpoint, VirtualPresenterListener};

#[allow(unused_imports)]
pub use vivid_gateway::{
    AudioSourceConfig, ClipRect, KeyframeRequestOutcome, MediaEvent, NodeConfig,
    OuterMediaProjection, PlayRequest, ProducerId, ProjectionSnapshot, RetainedRaster, SceneNode,
    SceneNodeConfig, SemanticDescriptor, SnapshotSource, SnapshotSurface, SourceDescriptor,
    SourceKey,
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
        Self::start_with_events(endpoint, config, None)
    }

    pub fn start_with_events(
        endpoint: VirtualPresenterEndpoint,
        config: MediaConfig,
        events: Option<mpsc::SyncSender<MediaEvent>>,
    ) -> io::Result<Self> {
        let listener = VirtualPresenterListener::bind(endpoint)?;
        vivid_gateway::VirtualVivid::start_with_events(listener, config, events).map(Self)
    }
}

impl Deref for VirtualVivid {
    type Target = vivid_gateway::VirtualVivid;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
