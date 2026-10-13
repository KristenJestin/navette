//! A PipeWire graph in memory, for the tests: it records what Navette did to it.

use std::io;
use std::sync::{Arc, Mutex};

use super::jitter::Jitter;
use super::{DefaultNode, Device, Graph, OnSound};

#[derive(Default)]
pub struct State {
    /// The default sink.
    pub default: Option<DefaultNode>,
    pub default_source: Option<DefaultNode>,
    pub sinks: Vec<(String, Arc<Mutex<OnSound>>)>,
    pub playbacks: Vec<(String, Arc<Mutex<Jitter>>)>,
    /// What was done, in order.
    pub log: Vec<String>,
    /// Changes of the default fail, as when PipeWire is gone.
    pub refuse_defaults: bool,
    /// How long a change of the default takes.
    pub slow_defaults: std::time::Duration,
    /// How many times PipeWire restarted: a node opened before the last restart is gone.
    pub restarts: u32,
}

#[derive(Clone, Default)]
pub struct FakeGraph(pub Arc<Mutex<State>>);

impl FakeGraph {
    pub fn with_default(name: &str) -> Self {
        let graph = Self::default();
        graph.state().default = Some(DefaultNode::named(name));
        graph
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// PipeWire restarts: every node goes with it.
    pub fn restart(&self) {
        let mut state = self.state();
        state.restarts += 1;
        state.sinks.clear();
        state.playbacks.clear();
        state.log.push("restart".to_owned());
    }
}

pub struct FakeNode {
    graph: FakeGraph,
    name: String,
    restarts: u32,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        let mut state = self.graph.state();
        // A node of an earlier PipeWire went with it: its name may be a new node's now.
        if state.restarts != self.restarts {
            return;
        }
        state.sinks.retain(|(name, _)| *name != self.name);
        state.playbacks.retain(|(name, _)| *name != self.name);
        state.log.push(format!("remove {}", self.name));
    }
}

impl Graph for FakeGraph {
    type Node = FakeNode;

    fn default_node(&self, device: Device) -> io::Result<Option<DefaultNode>> {
        let state = self.state();
        Ok(match device {
            Device::Sink => state.default.clone(),
            Device::Source => state.default_source.clone(),
        })
    }

    fn set_default_node(&self, device: Device, node: Option<&DefaultNode>) -> io::Result<()> {
        let slow = self.state().slow_defaults;
        std::thread::sleep(slow);
        let mut state = self.state();
        if state.refuse_defaults {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PipeWire is gone",
            ));
        }
        let (slot, what) = match device {
            Device::Sink => (&mut state.default, "default"),
            Device::Source => (&mut state.default_source, "default source"),
        };
        *slot = node.cloned();
        let entry = match node {
            Some(node) => format!("{what} {}", node.0),
            None => format!("{what} cleared"),
        };
        state.log.push(entry);
        Ok(())
    }

    fn open_sink(&self, name: &str, _description: &str, on_sound: OnSound) -> io::Result<FakeNode> {
        let mut state = self.state();
        state
            .sinks
            .push((name.to_owned(), Arc::new(Mutex::new(on_sound))));
        state.log.push(format!("sink {name}"));
        Ok(FakeNode {
            graph: self.clone(),
            name: name.to_owned(),
            restarts: state.restarts,
        })
    }

    fn open_playback(
        &self,
        name: &str,
        _description: &str,
        jitter: Arc<Mutex<Jitter>>,
    ) -> io::Result<FakeNode> {
        let mut state = self.state();
        state.playbacks.push((name.to_owned(), jitter));
        state.log.push(format!("playback {name}"));
        Ok(FakeNode {
            graph: self.clone(),
            name: name.to_owned(),
            restarts: state.restarts,
        })
    }

    fn is_gone(&self, node: &FakeNode) -> bool {
        self.state().restarts != node.restarts
    }
}
